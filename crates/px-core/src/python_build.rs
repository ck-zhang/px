use std::collections::HashMap;
use std::env;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use dirs_next::home_dir;
use flate2::read::GzDecoder;
use reqwest::blocking::Client;
use serde::Deserialize;
use tar::Archive;
use tempfile::{tempdir_in, NamedTempFile, TempDir, TempPath};
use zip::ZipArchive;

use crate::build_http_client;

const DEFAULT_DOWNLOADS_URL: &str =
    "https://raw.githubusercontent.com/astral-sh/uv/main/crates/uv-python/download-metadata.json";

enum ManifestSource {
    Http(String),
    File(PathBuf),
}

#[derive(Clone, Copy, Debug)]
struct HostTarget {
    label: &'static str,
    os: &'static str,
    arch: &'static str,
    libc: &'static str,
}

#[derive(Clone, Copy, Debug)]
enum ArchiveKind {
    TarGz,
    Zip,
}

#[derive(Clone, Debug)]
struct SelectedAsset {
    target: HostTarget,
    name: String,
    url: String,
    kind: ArchiveKind,
}

#[derive(Deserialize, Clone)]
struct PythonDownload {
    name: String,
    arch: PythonDownloadArch,
    os: String,
    libc: String,
    major: u8,
    minor: u8,
    patch: u8,
    #[serde(default)]
    prerelease: Option<String>,
    url: String,
    #[serde(default)]
    variant: Option<String>,
    #[serde(default)]
    build: Option<String>,
}

#[derive(Deserialize, Clone)]
struct PythonDownloadArch {
    family: String,
    #[serde(default)]
    variant: Option<String>,
}

impl PythonDownload {
    fn matches(&self, major: u8, minor: u8, target: &HostTarget) -> bool {
        let prerelease = self
            .prerelease
            .as_deref()
            .map(|value| value.is_empty())
            .unwrap_or(true);
        let variant = self
            .variant
            .as_deref()
            .map(|value| value.is_empty())
            .unwrap_or(true);
        let arch_variant = self
            .arch
            .variant
            .as_deref()
            .map(|value| value.is_empty())
            .unwrap_or(true);
        self.name == "cpython"
            && self.major == major
            && self.minor == minor
            && prerelease
            && variant
            && arch_variant
            && self.arch.family == target.arch
            && self.os == target.os
            && self.libc == target.libc
    }

    fn rank(&self) -> (u8, u64) {
        let build = self
            .build
            .as_deref()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0);
        (self.patch, build)
    }
}

/// Download and install a python-build-standalone runtime.
///
/// # Errors
/// Returns an error if the requested channel cannot be located or the archive
/// cannot be extracted.
pub(crate) fn install_python(channel: &str) -> Result<PathBuf> {
    let targets = detect_host_targets()?;
    let client = build_http_client()?;
    let downloads = load_download_manifest(&client)?;
    let asset = select_release_asset(channel, &targets, &downloads)?;
    let download = download_asset(&client, &asset)?;
    let runtimes_root = ensure_runtimes_root()?;
    let stage = create_stage_dir(&runtimes_root)?;
    extract_archive(download.as_ref(), stage.path(), asset.kind)?;
    let interpreter = locate_python_binary(stage.path(), channel)?;
    let relative = interpreter
        .strip_prefix(stage.path())
        .context("python binary must live under install root")?
        .to_path_buf();
    let stage_path = stage.keep();
    let install_path = runtimes_root.join(format!("{}-{}", channel, asset.target.label));
    if install_path.exists() {
        fs::remove_dir_all(&install_path)
            .with_context(|| format!("removing previous runtime at {}", install_path.display()))?;
    }
    if let Err(err) = fs::rename(&stage_path, &install_path) {
        // Clean the staging directory on failure for predictable retries.
        let _ = fs::remove_dir_all(&stage_path);
        return Err(err).with_context(|| {
            format!(
                "moving python runtime into place at {}",
                install_path.display()
            )
        });
    }
    Ok(install_path.join(relative))
}

fn detect_host_targets() -> Result<Vec<HostTarget>> {
    match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => Ok(vec![HostTarget {
            label: "x86_64-unknown-linux-gnu",
            os: "linux",
            arch: "x86_64",
            libc: "gnu",
        }]),
        ("linux", "aarch64") => Ok(vec![HostTarget {
            label: "aarch64-unknown-linux-gnu",
            os: "linux",
            arch: "aarch64",
            libc: "gnu",
        }]),
        ("macos", "x86_64") => Ok(vec![HostTarget {
            label: "x86_64-apple-darwin",
            os: "darwin",
            arch: "x86_64",
            libc: "none",
        }]),
        ("macos", "aarch64") => Ok(vec![HostTarget {
            label: "aarch64-apple-darwin",
            os: "darwin",
            arch: "aarch64",
            libc: "none",
        }]),
        ("windows", "x86_64") => Ok(vec![HostTarget {
            label: "x86_64-pc-windows-msvc",
            os: "windows",
            arch: "x86_64",
            libc: "none",
        }]),
        (os, arch) => bail!("unsupported host platform {os}-{arch} for python-build"),
    }
}

fn load_download_manifest(client: &Client) -> Result<Vec<PythonDownload>> {
    let raw_source =
        env::var("PX_PYTHON_DOWNLOADS_URL").unwrap_or_else(|_| DEFAULT_DOWNLOADS_URL.to_string());
    let source = if let Some(path) = raw_source.strip_prefix("file://") {
        ManifestSource::File(PathBuf::from(path))
    } else if raw_source.starts_with("http://") || raw_source.starts_with("https://") {
        ManifestSource::Http(raw_source)
    } else {
        ManifestSource::File(PathBuf::from(raw_source))
    };

    let bytes = match fetch_manifest_bytes(client, &source) {
        Ok(bytes) => bytes,
        Err(err) => {
            let cache = read_manifest_cache()?;
            if let Some(bytes) = cache {
                bytes
            } else {
                return Err(err.context("failed to fetch python download manifest"));
            }
        }
    };
    let downloads = parse_manifest(&bytes)?;
    if matches!(source, ManifestSource::Http(_)) {
        let _ = write_manifest_cache(&bytes);
    }
    Ok(downloads)
}

fn fetch_manifest_bytes(client: &Client, source: &ManifestSource) -> Result<Vec<u8>> {
    match source {
        ManifestSource::Http(url) => {
            let response = client
                .get(url)
                .send()
                .with_context(|| format!("failed to download python manifest from {url}"))?
                .error_for_status()
                .with_context(|| format!("python downloads manifest request failed ({url})"))?;
            response
                .bytes()
                .map(|bytes| bytes.to_vec())
                .context("failed to read python downloads manifest body")
        }
        ManifestSource::File(path) => fs::read(path)
            .with_context(|| format!("reading python downloads manifest at {}", path.display())),
    }
}

fn parse_manifest(bytes: &[u8]) -> Result<Vec<PythonDownload>> {
    let map: HashMap<String, PythonDownload> =
        serde_json::from_slice(bytes).context("invalid python downloads manifest")?;
    Ok(map.into_values().collect())
}

fn manifest_cache_path() -> Result<PathBuf> {
    let home = home_dir().ok_or_else(|| anyhow!("home directory not found"))?;
    let cache_dir = home.join(".px").join("cache");
    fs::create_dir_all(&cache_dir)
        .with_context(|| format!("creating cache directory at {}", cache_dir.display()))?;
    Ok(cache_dir.join("python-downloads.json"))
}

fn write_manifest_cache(bytes: &[u8]) -> Result<()> {
    let path = manifest_cache_path()?;
    fs::write(path, bytes).context("writing cached python downloads manifest")
}

fn read_manifest_cache() -> Result<Option<Vec<u8>>> {
    let path = manifest_cache_path()?;
    if path.exists() {
        fs::read(&path).map(Some).with_context(|| {
            format!(
                "reading cached python downloads manifest at {}",
                path.display()
            )
        })
    } else {
        Ok(None)
    }
}

fn select_release_asset(
    channel: &str,
    targets: &[HostTarget],
    downloads: &[PythonDownload],
) -> Result<SelectedAsset> {
    let (major, minor) = parse_channel_pair(channel)?;
    for target in targets {
        if let Some(entry) = find_download(downloads, major, minor, target) {
            let name = filename_from_url(&entry.url);
            let kind = archive_kind(&name)?;
            return Ok(SelectedAsset {
                target: *target,
                name,
                url: entry.url.clone(),
                kind,
            });
        }
    }
    bail!("python {channel} is not available for this platform");
}

fn find_download<'a>(
    downloads: &'a [PythonDownload],
    major: u8,
    minor: u8,
    target: &HostTarget,
) -> Option<&'a PythonDownload> {
    downloads
        .iter()
        .filter(|download| download.matches(major, minor, target))
        .max_by(|left, right| left.rank().cmp(&right.rank()))
}

fn filename_from_url(url: &str) -> String {
    url.rsplit('/')
        .next()
        .unwrap_or("python.tar.gz")
        .to_string()
}

fn archive_kind(name: &str) -> Result<ArchiveKind> {
    if name.ends_with(".tar.gz") {
        Ok(ArchiveKind::TarGz)
    } else if name.ends_with(".zip") {
        Ok(ArchiveKind::Zip)
    } else {
        bail!("unsupported archive format for {name}")
    }
}

fn download_asset(client: &Client, asset: &SelectedAsset) -> Result<TempPath> {
    let mut response = client
        .get(&asset.url)
        .send()
        .with_context(|| format!("failed to download {}", asset.name))?
        .error_for_status()
        .with_context(|| format!("download failed for {}", asset.name))?;
    let mut file = NamedTempFile::new().context("creating temporary file for python runtime")?;
    response
        .copy_to(file.as_file_mut())
        .with_context(|| format!("failed to write {}", asset.name))?;
    Ok(file.into_temp_path())
}

fn create_stage_dir(root: &Path) -> Result<TempDir> {
    tempdir_in(root).with_context(|| format!("creating staging directory under {}", root.display()))
}

fn extract_archive(archive: &Path, dest: &Path, kind: ArchiveKind) -> Result<()> {
    match kind {
        ArchiveKind::TarGz => {
            let file = File::open(archive)
                .with_context(|| format!("opening python archive {}", archive.display()))?;
            let decoder = GzDecoder::new(file);
            let mut tar = Archive::new(decoder);
            tar.unpack(dest)
                .with_context(|| format!("extracting archive into {}", dest.display()))?;
        }
        ArchiveKind::Zip => {
            let file = File::open(archive)
                .with_context(|| format!("opening python archive {}", archive.display()))?;
            let mut archive = ZipArchive::new(file)
                .with_context(|| format!("reading zip archive {}", archive.display()))?;
            archive
                .extract(dest)
                .with_context(|| format!("extracting zip archive into {}", dest.display()))?;
        }
    }
    Ok(())
}

fn locate_python_binary(root: &Path, channel: &str) -> Result<PathBuf> {
    let windows = env::consts::OS == "windows";
    let candidates = interpreter_names(channel, windows)?;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry
                .file_type()
                .with_context(|| format!("inspecting {}", path.display()))?;
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let normalized = if windows {
                name.to_ascii_lowercase()
            } else {
                name.to_string()
            };
            if candidates.iter().any(|candidate| candidate == &normalized) {
                return Ok(path);
            }
        }
    }
    bail!("unable to locate python binary in installed runtime")
}

fn interpreter_names(channel: &str, windows: bool) -> Result<Vec<String>> {
    let (major, minor) = parse_channel_pair(channel)?;
    if windows {
        let mut names = Vec::new();
        names.push(format!("python{major}{minor}.exe"));
        names.push(format!("python{major}.{minor}.exe"));
        names.push(format!("python{major}.exe"));
        names.push("python3.exe".to_string());
        names.push("python.exe".to_string());
        Ok(names
            .into_iter()
            .map(|name| name.to_ascii_lowercase())
            .collect())
    } else {
        Ok(vec![
            format!("python{major}.{minor}"),
            format!("python{major}"),
            "python3".to_string(),
            "python".to_string(),
        ])
    }
}

fn parse_channel_pair(input: &str) -> Result<(u8, u8)> {
    let mut parts = input.split('.');
    let major = parts
        .next()
        .ok_or_else(|| anyhow!("python version missing major component"))?
        .parse()?;
    let minor = parts
        .next()
        .ok_or_else(|| anyhow!("python version missing minor component"))?
        .parse()?;
    Ok((major, minor))
}

fn ensure_runtimes_root() -> Result<PathBuf> {
    let home = home_dir().ok_or_else(|| anyhow!("home directory not found"))?;
    let root = home.join(".px").join("runtimes");
    fs::create_dir_all(&root)
        .with_context(|| format!("creating px runtimes directory at {}", root.display()))?;
    Ok(root)
}
