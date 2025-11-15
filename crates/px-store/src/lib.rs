//! Minimal artifact downloader for pinned installs.

use std::{
    env,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use anyhow::{anyhow, bail, Context, Result};
use dirs_next::home_dir;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json;
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

const USER_AGENT: &str = concat!("px-store/", env!("CARGO_PKG_VERSION"));
const DOWNLOAD_ATTEMPTS: usize = 3;
const HTTP_TIMEOUT: Duration = Duration::from_secs(60);

/// Request describing a wheel that should be cached locally.
pub struct ArtifactRequest<'a> {
    pub name: &'a str,
    pub version: &'a str,
    pub filename: &'a str,
    pub url: &'a str,
    pub sha256: &'a str,
}

/// Result of caching a wheel on disk.
#[derive(Debug, Clone)]
pub struct CachedArtifact {
    pub path: PathBuf,
    pub size: u64,
}

/// Artifact entry captured from `px.lock` for prefetching.
#[derive(Debug, Clone)]
pub struct PrefetchSpec<'a> {
    pub name: &'a str,
    pub version: &'a str,
    pub filename: &'a str,
    pub url: &'a str,
    pub sha256: &'a str,
}

#[derive(Debug, Clone, Copy)]
pub struct PrefetchOptions {
    pub dry_run: bool,
    pub parallel: usize,
}

impl Default for PrefetchOptions {
    fn default() -> Self {
        Self {
            dry_run: false,
            parallel: 4,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PrefetchSummary {
    pub requested: usize,
    pub hit: usize,
    pub fetched: usize,
    pub failed: usize,
    pub bytes_fetched: u64,
    pub errors: Vec<String>,
}

pub fn resolve_cache_store_path() -> Result<CacheLocation> {
    if let Some(override_path) = env::var_os("PX_CACHE_PATH") {
        let path = absolutize(PathBuf::from(override_path))?;
        return Ok(CacheLocation {
            path,
            source: "PX_CACHE_PATH",
        });
    }

    #[cfg(target_os = "windows")]
    let (base, source) = resolve_windows_cache_base()?;
    #[cfg(not(target_os = "windows"))]
    let (base, source) = resolve_unix_cache_base()?;

    Ok(CacheLocation {
        path: base.join("px").join("store"),
        source,
    })
}

pub fn compute_cache_usage(path: &Path) -> Result<CacheUsage> {
    if !path.exists() {
        return Ok(CacheUsage {
            exists: false,
            total_entries: 0,
            total_size_bytes: 0,
        });
    }

    let mut total_entries = 0u64;
    let mut total_size_bytes = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let entry_path = entry.path();
            let metadata = entry.metadata()?;
            if metadata.is_dir() {
                stack.push(entry_path);
            } else if metadata.is_file() {
                total_entries += 1;
                total_size_bytes += metadata.len();
            }
        }
    }

    Ok(CacheUsage {
        exists: true,
        total_entries,
        total_size_bytes,
    })
}

pub fn collect_cache_walk(path: &Path) -> Result<CacheWalk> {
    if !path.exists() {
        return Ok(CacheWalk::default());
    }

    let mut walk = CacheWalk {
        exists: true,
        ..CacheWalk::default()
    };
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let entry_path = entry.path();
            let metadata = entry.metadata()?;
            if metadata.is_dir() {
                stack.push(entry_path.clone());
                if entry_path != path {
                    walk.dirs.push(entry_path);
                }
            } else if metadata.is_file() {
                let size = metadata.len();
                walk.total_bytes += size;
                walk.files.push(CacheEntry {
                    path: entry_path,
                    size,
                });
            }
        }
    }

    walk.files.sort_by(|a, b| a.path.cmp(&b.path));
    walk.dirs.sort();
    Ok(walk)
}

pub fn prune_cache_entries(walk: &CacheWalk) -> CachePruneResult {
    let mut result = CachePruneResult {
        candidate_entries: walk.files.len() as u64,
        candidate_size_bytes: walk.total_bytes,
        ..CachePruneResult::default()
    };

    for entry in &walk.files {
        match fs::remove_file(&entry.path) {
            Ok(_) => {
                result.deleted_entries += 1;
                result.deleted_size_bytes += entry.size;
            }
            Err(err) => result.errors.push(CachePruneError {
                path: entry.path.clone(),
                error: err.to_string(),
            }),
        }
    }

    for dir in walk.dirs.iter().rev() {
        let _ = fs::remove_dir(dir);
    }

    result
}

#[cfg(not(target_os = "windows"))]
fn resolve_unix_cache_base() -> Result<(PathBuf, &'static str)> {
    if let Some(xdg) = env::var_os("XDG_CACHE_HOME") {
        return Ok((PathBuf::from(xdg), "XDG_CACHE_HOME"));
    }
    let home = home_dir().ok_or_else(|| anyhow!("unable to determine home directory"))?;
    Ok((home.join(".cache"), "~/.cache"))
}

#[cfg(target_os = "windows")]
fn resolve_windows_cache_base() -> Result<(PathBuf, &'static str)> {
    if let Some(local) = env::var_os("LOCALAPPDATA") {
        return Ok((PathBuf::from(local), "LOCALAPPDATA"));
    }
    if let Some(user_profile) = env::var_os("USERPROFILE") {
        return Ok((
            PathBuf::from(user_profile).join("AppData").join("Local"),
            "USERPROFILE",
        ));
    }
    let home = home_dir().ok_or_else(|| anyhow!("unable to determine home directory"))?;
    Ok((home.join("AppData").join("Local"), "home/AppData/Local"))
}

fn absolutize(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(env::current_dir()?.join(path))
    }
}

#[derive(Debug, Clone)]
pub struct CacheLocation {
    pub path: PathBuf,
    pub source: &'static str,
}

#[derive(Debug, Clone)]
pub struct CacheUsage {
    pub exists: bool,
    pub total_entries: u64,
    pub total_size_bytes: u64,
}

#[derive(Debug, Clone, Default)]
pub struct CacheWalk {
    pub exists: bool,
    pub files: Vec<CacheEntry>,
    pub dirs: Vec<PathBuf>,
    pub total_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct CacheEntry {
    pub path: PathBuf,
    pub size: u64,
}

#[derive(Debug, Clone)]
pub struct CachePruneError {
    pub path: PathBuf,
    pub error: String,
}

#[derive(Debug, Clone, Default)]
pub struct CachePruneResult {
    pub candidate_entries: u64,
    pub candidate_size_bytes: u64,
    pub deleted_entries: u64,
    pub deleted_size_bytes: u64,
    pub errors: Vec<CachePruneError>,
}

/// Build an sdist into a cached wheel and return its metadata.
pub fn ensure_sdist_build(cache_root: &Path, request: &SdistRequest<'_>) -> Result<BuiltWheel> {
    let precomputed_id = request.sha256.map(|sha| build_identifier(request, sha));
    if let Some(id) = &precomputed_id {
        let meta_path = cache_root.join("sdist-build").join(id).join("meta.json");
        if let Some(built) = load_cached_build(&meta_path)? {
            if built.cached_path.exists() {
                return Ok(built);
            }
        }
    }

    let (temp_file, download_sha) = download_sdist(request)?;
    if let Some(expected) = request.sha256 {
        if expected != download_sha {
            bail!(
                "sdist checksum mismatch for {} (expected {}, got {})",
                request.filename,
                expected,
                download_sha
            );
        }
    }
    let build_id = precomputed_id.unwrap_or_else(|| build_identifier(request, &download_sha));
    let build_root = cache_root.join("sdist-build").join(&build_id);
    let meta_path = build_root.join("meta.json");

    if let Some(built) = load_cached_build(&meta_path)? {
        if built.cached_path.exists() {
            return Ok(built);
        }
    }

    fs::create_dir_all(&build_root)?;
    let sdist_path = build_root.join(request.filename);
    temp_file
        .persist(&sdist_path)
        .map_err(|err| anyhow!("unable to persist sdist download: {err}"))?;

    let src_dir = build_root.join("src");
    let dist_dir = build_root.join("dist");
    if src_dir.exists() {
        fs::remove_dir_all(&src_dir)?;
    }
    if dist_dir.exists() {
        fs::remove_dir_all(&dist_dir)?;
    }
    fs::create_dir_all(&src_dir)?;
    fs::create_dir_all(&dist_dir)?;

    extract_sdist(request.python_path, &sdist_path, &src_dir)?;
    let project_dir = discover_project_dir(&src_dir)?;
    run_python_build(request.python_path, &project_dir, &dist_dir)?;

    let built_wheel_path = find_wheel(&dist_dir)?;
    let filename = built_wheel_path
        .file_name()
        .ok_or_else(|| anyhow!("wheel missing filename"))?
        .to_string_lossy()
        .to_string();
    let (python_tag, abi_tag, platform_tag) = parse_wheel_tags(&filename)
        .ok_or_else(|| anyhow!("unable to parse wheel tags from {filename}"))?;

    let dest = wheel_path(
        cache_root,
        request.normalized_name,
        request.version,
        &filename,
    );
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    if dest.exists() {
        let existing_sha = compute_sha256(&dest)?;
        let fresh_sha = compute_sha256(&built_wheel_path)?;
        if existing_sha != fresh_sha {
            fs::remove_file(&dest)?;
            fs::rename(&built_wheel_path, &dest)?;
        } else {
            fs::remove_file(&built_wheel_path)?;
        }
    } else {
        fs::rename(&built_wheel_path, &dest)?;
    }

    let sha256 = compute_sha256(&dest)?;
    let size = fs::metadata(&dest)?.len();
    let built = BuiltWheel {
        filename,
        url: request.url.to_string(),
        sha256,
        size,
        cached_path: dest.clone(),
        python_tag,
        abi_tag,
        platform_tag,
    };
    persist_metadata(&meta_path, &built)?;

    // Best effort cleanup of extracted sources to keep the cache small.
    let _ = fs::remove_dir_all(&src_dir);
    let _ = fs::remove_dir_all(&dist_dir);

    Ok(built)
}

fn build_identifier(request: &SdistRequest<'_>, sha256: &str) -> String {
    let short_len = sha256.len().min(16);
    format!(
        "{}-{}-{}",
        request.normalized_name,
        request.version,
        &sha256[..short_len]
    )
}

fn download_sdist(request: &SdistRequest<'_>) -> Result<(NamedTempFile, String)> {
    let mut last_err = None;
    for _ in 0..DOWNLOAD_ATTEMPTS {
        match download_sdist_once(request) {
            Ok(result) => return Ok(result),
            Err(err) => last_err = Some(err),
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow!("failed to download sdist {}", request.url)))
}

fn download_sdist_once(request: &SdistRequest<'_>) -> Result<(NamedTempFile, String)> {
    let client = http_client()?;
    let mut response = client
        .get(request.url)
        .send()
        .with_context(|| format!("failed to fetch {}", request.url))?
        .error_for_status()
        .with_context(|| format!("unexpected response for {}", request.url))?;

    let mut tmp = NamedTempFile::new()?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = response
            .read(&mut buffer)
            .with_context(|| format!("stream error for {}", request.filename))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        tmp.write_all(&buffer[..read])?;
    }
    let sha256 = hex::encode(hasher.finalize());
    Ok((tmp, sha256))
}

fn extract_sdist(python: &str, sdist: &Path, dest: &Path) -> Result<()> {
    if dest.exists() {
        fs::remove_dir_all(dest)?;
    }
    fs::create_dir_all(dest)?;
    let script = r#"
import pathlib, sys, tarfile, zipfile
sdist = pathlib.Path(sys.argv[1])
dest = pathlib.Path(sys.argv[2])
dest.mkdir(parents=True, exist_ok=True)
suffix = ''.join(sdist.suffixes).lower()
if suffix.endswith('.zip'):
    with zipfile.ZipFile(sdist) as zf:
        zf.extractall(dest)
else:
    mode = 'r:*'
    with tarfile.open(sdist, mode) as tf:
        tf.extractall(dest)
"#;
    let mut cmd = Command::new(python);
    cmd.arg("-c").arg(script).arg(sdist).arg(dest);
    apply_python_env(&mut cmd);
    let output = cmd
        .output()
        .with_context(|| format!("failed to extract {}", sdist.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("sdist extraction failed: {stderr}");
    }
    Ok(())
}

fn run_python_build(python: &str, project_dir: &Path, out_dir: &Path) -> Result<()> {
    let mut cmd = Command::new(python);
    cmd.arg("-m")
        .arg("build")
        .arg("--wheel")
        .arg("--outdir")
        .arg(out_dir)
        .arg(project_dir);
    apply_python_env(&mut cmd);
    cmd.env("PX_BUILD_FROM_SDIST", "1");
    let output = cmd
        .output()
        .with_context(|| format!("failed to run python -m build in {}", project_dir.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("python -m build failed: {stderr}");
    }
    Ok(())
}

fn discover_project_dir(root: &Path) -> Result<PathBuf> {
    let mut dirs = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            dirs.push(entry.path());
        }
    }
    if dirs.len() == 1 {
        Ok(dirs.remove(0))
    } else {
        Ok(root.to_path_buf())
    }
}

fn find_wheel(dist_dir: &Path) -> Result<PathBuf> {
    let mut wheels = Vec::new();
    for entry in fs::read_dir(dist_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().map(|ext| ext == "whl").unwrap_or(false) {
            wheels.push(path);
        }
    }
    match wheels.len() {
        0 => bail!("python -m build did not produce a wheel"),
        1 => Ok(wheels.remove(0)),
        _ => bail!("python -m build produced multiple wheels"),
    }
}

fn apply_python_env(cmd: &mut Command) {
    cmd.env("PYTHONNOUSERSITE", "1");
    cmd.env("PYTHONDONTWRITEBYTECODE", "1");
    cmd.env("PYTHONPATH", "");
    cmd.env("PIP_DISABLE_PIP_VERSION_CHECK", "1");
    if let Ok(val) = env::var("NO_PROXY") {
        cmd.env("NO_PROXY", val);
    }
    if let Ok(val) = env::var("no_proxy") {
        cmd.env("no_proxy", val);
    }
}

fn load_cached_build(meta_path: &Path) -> Result<Option<BuiltWheel>> {
    if !meta_path.exists() {
        return Ok(None);
    }
    let contents = fs::read_to_string(meta_path)?;
    let meta: BuiltWheelMetadata = serde_json::from_str(&contents)?;
    let path = PathBuf::from(meta.cached_path);
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(BuiltWheel {
        filename: meta.filename,
        url: meta.url,
        sha256: meta.sha256,
        size: meta.size,
        cached_path: path,
        python_tag: meta.python_tag,
        abi_tag: meta.abi_tag,
        platform_tag: meta.platform_tag,
    }))
}

fn persist_metadata(meta_path: &Path, built: &BuiltWheel) -> Result<()> {
    if let Some(parent) = meta_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let meta = BuiltWheelMetadata {
        filename: built.filename.clone(),
        url: built.url.clone(),
        sha256: built.sha256.clone(),
        size: built.size,
        cached_path: built.cached_path.display().to_string(),
        python_tag: built.python_tag.clone(),
        abi_tag: built.abi_tag.clone(),
        platform_tag: built.platform_tag.clone(),
    };
    fs::write(meta_path, serde_json::to_string_pretty(&meta)?)?;
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct BuiltWheelMetadata {
    filename: String,
    url: String,
    sha256: String,
    size: u64,
    cached_path: String,
    python_tag: String,
    abi_tag: String,
    platform_tag: String,
}

fn parse_wheel_tags(filename: &str) -> Option<(String, String, String)> {
    if !filename.ends_with(".whl") {
        return None;
    }
    let trimmed = filename.trim_end_matches(".whl");
    let parts: Vec<&str> = trimmed.split('-').collect();
    if parts.len() < 5 {
        return None;
    }
    Some((
        parts[parts.len() - 3].to_string(),
        parts[parts.len() - 2].to_string(),
        parts[parts.len() - 1].to_string(),
    ))
}

/// Input describing an sdist that should be built into a wheel.
pub struct SdistRequest<'a> {
    pub normalized_name: &'a str,
    pub version: &'a str,
    pub filename: &'a str,
    pub url: &'a str,
    pub sha256: Option<&'a str>,
    pub python_path: &'a str,
}

/// Result of building an sdist into a cached wheel.
#[derive(Debug, Clone)]
pub struct BuiltWheel {
    pub filename: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
    pub cached_path: PathBuf,
    pub python_tag: String,
    pub abi_tag: String,
    pub platform_tag: String,
}

/// Ensure a wheel exists in the cache, downloading and verifying if needed.
pub fn cache_wheel(cache_root: &Path, request: &ArtifactRequest<'_>) -> Result<CachedArtifact> {
    let dest = wheel_path(cache_root, request.name, request.version, request.filename);
    if let Some(existing) = validate_existing(&dest, request.sha256)? {
        return Ok(existing);
    }

    let mut last_err = None;
    for _ in 0..DOWNLOAD_ATTEMPTS {
        match download_once(&dest, request) {
            Ok(artifact) => return Ok(artifact),
            Err(err) => last_err = Some(err),
        }
    }

    Err(last_err
        .unwrap_or_else(|| anyhow!("failed to download {}; no attempts left", request.filename)))
}

fn wheel_path(cache_root: &Path, name: &str, version: &str, filename: &str) -> PathBuf {
    cache_root
        .join("wheels")
        .join(name)
        .join(version)
        .join(filename)
}

fn validate_existing(path: &Path, expected_sha: &str) -> Result<Option<CachedArtifact>> {
    if !path.exists() {
        return Ok(None);
    }

    match compute_sha256(path) {
        Ok(actual) if actual == expected_sha => {
            let size = fs::metadata(path)?.len();
            Ok(Some(CachedArtifact {
                path: path.to_path_buf(),
                size,
            }))
        }
        Ok(_) | Err(_) => {
            // Remove mismatched/corrupt file and re-download.
            let _ = fs::remove_file(path);
            Ok(None)
        }
    }
}

/// Ensure that the provided artifacts exist in the cache.
pub fn prefetch_artifacts(
    cache_root: &Path,
    specs: &[PrefetchSpec<'_>],
    options: PrefetchOptions,
) -> Result<PrefetchSummary> {
    let mut summary = PrefetchSummary {
        requested: specs.len(),
        ..PrefetchSummary::default()
    };

    if specs.is_empty() {
        return Ok(summary);
    }

    let batch_size = options.parallel.max(1);

    for chunk in specs.chunks(batch_size) {
        for spec in chunk {
            let dest = wheel_path(cache_root, spec.name, spec.version, spec.filename);
            match validate_existing(&dest, spec.sha256) {
                Ok(Some(_)) => {
                    summary.hit += 1;
                    continue;
                }
                Ok(None) => {}
                Err(err) => {
                    summary.failed += 1;
                    summary.errors.push(err.to_string());
                    continue;
                }
            }

            if options.dry_run {
                continue;
            }

            let request = ArtifactRequest {
                name: spec.name,
                version: spec.version,
                filename: spec.filename,
                url: spec.url,
                sha256: spec.sha256,
            };

            match cache_wheel(cache_root, &request) {
                Ok(artifact) => {
                    summary.fetched += 1;
                    summary.bytes_fetched += artifact.size;
                }
                Err(err) => {
                    summary.failed += 1;
                    summary.errors.push(err.to_string());
                }
            }
        }
    }

    Ok(summary)
}

fn download_once(dest: &Path, request: &ArtifactRequest<'_>) -> Result<CachedArtifact> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }

    let client = http_client()?;
    let mut response = client
        .get(request.url)
        .send()
        .with_context(|| format!("failed to fetch {}", request.url))?
        .error_for_status()
        .with_context(|| format!("unexpected response for {}", request.url))?;

    let mut tmp = NamedTempFile::new_in(dest.parent().unwrap_or_else(|| Path::new(".")))?;
    let mut hasher = Sha256::new();
    let mut written: u64 = 0;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = response
            .read(&mut buffer)
            .with_context(|| format!("stream error for {}", request.filename))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        tmp.write_all(&buffer[..read])?;
        written += read as u64;
    }

    let actual = hex::encode(hasher.finalize());
    if actual != request.sha256 {
        return Err(anyhow!(
            "sha256 mismatch for {} (expected {}, got {})",
            request.filename,
            request.sha256,
            actual
        ));
    }

    tmp.persist(dest)?;
    Ok(CachedArtifact {
        path: dest.to_path_buf(),
        size: written,
    })
}

fn compute_sha256(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 32 * 1024];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn http_client() -> Result<Client> {
    Client::builder()
        .user_agent(USER_AGENT)
        .timeout(HTTP_TIMEOUT)
        .no_proxy()
        .build()
        .context("failed to build http client")
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::{env, ffi::OsString};

    #[test]
    fn download_packaging_smoke() -> Result<()> {
        if env::var("PX_ONLINE").ok().as_deref() != Some("1") {
            eprintln!("skipping download_packaging_smoke (PX_ONLINE!=1)");
            return Ok(());
        }

        let temp = tempfile::tempdir()?;
        let request = ArtifactRequest {
            name: "packaging",
            version: "24.1",
            filename: "packaging-24.1-py3-none-any.whl",
            url: "https://files.pythonhosted.org/packages/08/aa/cc0199a5f0ad350994d660967a8efb233fe0416e4639146c089643407ce6/packaging-24.1-py3-none-any.whl",
            sha256: "5b8f2217dbdbd2f7f384c41c628544e6d52f2d0f53c6d0c3ea61aa5d1d7ff124",
        };

        let artifact = cache_wheel(temp.path(), &request)?;
        assert!(artifact.path.exists());
        assert_eq!(artifact.size, fs::metadata(&artifact.path)?.len());
        Ok(())
    }

    #[test]
    fn resolves_cache_path_override() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let override_path = temp.path().join("cache-root");
        fs::create_dir_all(&override_path)?;
        let previous: Option<OsString> = env::var_os("PX_CACHE_PATH");
        env::set_var("PX_CACHE_PATH", &override_path);
        let location = resolve_cache_store_path()?;
        match previous {
            Some(value) => env::set_var("PX_CACHE_PATH", value),
            None => env::remove_var("PX_CACHE_PATH"),
        }

        assert_eq!(location.source, "PX_CACHE_PATH");
        assert_eq!(location.path.canonicalize()?, override_path.canonicalize()?);
        Ok(())
    }

    #[test]
    fn prefetch_hits_cached_artifacts() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let cache_root = temp.path();
        let wheel_path = cache_root.join("wheels/demo/1.0.0/demo-1.0.0.whl");
        fs::create_dir_all(wheel_path.parent().unwrap())?;
        fs::write(&wheel_path, b"demo")?;

        let sha = hex::encode(Sha256::digest(b"demo"));
        let name = "demo".to_string();
        let version = "1.0.0".to_string();
        let filename = "demo-1.0.0.whl".to_string();
        let url = "https://example.invalid/demo.whl".to_string();
        let specs = vec![PrefetchSpec {
            name: name.as_str(),
            version: version.as_str(),
            filename: filename.as_str(),
            url: url.as_str(),
            sha256: sha.as_str(),
        }];

        let summary = prefetch_artifacts(cache_root, &specs, PrefetchOptions::default())?;
        assert_eq!(summary.requested, 1);
        assert_eq!(summary.hit, 1);
        assert_eq!(summary.fetched, 0);
        assert_eq!(summary.failed, 0);
        Ok(())
    }
}
