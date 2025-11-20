use std::{
    env,
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use anyhow::{anyhow, bail, Context, Result};
use dirs_next::home_dir;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use zip::ZipArchive;

const USER_AGENT: &str = concat!("px-store/", env!("CARGO_PKG_VERSION"));
const DOWNLOAD_ATTEMPTS: usize = 3;
const HTTP_TIMEOUT: Duration = Duration::from_secs(60);
const WHEEL_MARKER_NAME: &str = ".px-wheel.json";

pub struct ArtifactRequest<'a> {
    pub name: &'a str,
    pub version: &'a str,
    pub filename: &'a str,
    pub url: &'a str,
    pub sha256: &'a str,
}

#[derive(Debug, Clone)]
pub struct CachedArtifact {
    pub wheel_path: PathBuf,
    pub dist_path: PathBuf,
    pub size: u64,
}

#[derive(Debug, Clone)]
struct CachedWheelFile {
    path: PathBuf,
    size: u64,
}

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

/// Determine the root directory for the on-disk cache.
///
/// # Errors
///
/// Returns an error if the configured directory cannot be resolved or created.
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

/// Compute aggregate statistics for every file under the cache path.
///
/// # Errors
///
/// Returns an error if the directory tree cannot be traversed.
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

/// Gather every entry under the cache directory.
///
/// # Errors
///
/// Returns an error if reading the directory tree fails at any point.
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

#[must_use]
pub fn prune_cache_entries(walk: &CacheWalk) -> CachePruneResult {
    let mut result = CachePruneResult {
        candidate_entries: walk.files.len() as u64,
        candidate_size_bytes: walk.total_bytes,
        ..CachePruneResult::default()
    };

    for entry in &walk.files {
        match fs::remove_file(&entry.path) {
            Ok(()) => {
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

/// Build or retrieve an sdist-derived wheel from the cache.
///
/// # Errors
///
/// Returns an error when the archive cannot be unpacked, hashed, or copied into
/// the cache directory.
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
        if download_sha != expected {
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
    persist_named_tempfile(temp_file, &sdist_path)
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
    persist_or_copy(&built_wheel_path, &dest)?;

    let sha256 = compute_sha256(&dest)?;
    let size = fs::metadata(&dest)?.len();
    let dist_path = ensure_wheel_dist(&dest, &sha256)?;
    let built = BuiltWheel {
        filename,
        url: request.url.to_string(),
        sha256,
        size,
        cached_path: dest.clone(),
        dist_path: dist_path.clone(),
        python_tag,
        abi_tag,
        platform_tag,
    };
    persist_metadata(&meta_path, &built)?;

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
    let mut buffer = vec![0_u8; 64 * 1024];
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

fn persist_named_tempfile(tmp: NamedTempFile, dest: &Path) -> io::Result<()> {
    match tmp.persist(dest) {
        Ok(_) => Ok(()),
        Err(err) => {
            let file = err.file;
            if is_cross_device(&err.error) {
                let mut reader = file.reopen()?;
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)?;
                }
                let mut writer = File::create(dest)?;
                io::copy(&mut reader, &mut writer)?;
                file.close().ok();
                Ok(())
            } else {
                Err(err.error)
            }
        }
    }
}

fn persist_or_copy(src: &Path, dest: &Path) -> io::Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::rename(src, dest) {
        Ok(()) => Ok(()),
        Err(err) if is_cross_device(&err) => {
            fs::copy(src, dest)?;
            fs::remove_file(src)?;
            Ok(())
        }
        Err(err) => Err(err),
    }
}

fn is_cross_device(err: &io::Error) -> bool {
    matches!(err.raw_os_error(), Some(18))
}

fn extract_sdist(python: &str, sdist: &Path, dest: &Path) -> Result<()> {
    if dest.exists() {
        fs::remove_dir_all(dest)?;
    }
    fs::create_dir_all(dest)?;
    let script = r"
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
";
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
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    match pip_wheel_fallback(python, project_dir, out_dir) {
        Ok(()) => Ok(()),
        Err(pip_err) => {
            bail!("python -m build failed: {stderr}\npython -m pip wheel failed: {pip_err}")
        }
    }
}

fn pip_wheel_fallback(python: &str, project_dir: &Path, out_dir: &Path) -> Result<()> {
    let mut cmd = Command::new(python);
    cmd.arg("-m")
        .arg("pip")
        .arg("wheel")
        .arg("--no-deps")
        .arg("--wheel-dir")
        .arg(out_dir)
        .arg(project_dir);
    apply_python_env(&mut cmd);
    let output = cmd.output().with_context(|| {
        format!(
            "failed to run python -m pip wheel in {}",
            project_dir.display()
        )
    })?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    bail!("{stderr}");
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
        if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("whl"))
        {
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
    let dist = if let Some(dist) = meta.dist_path {
        PathBuf::from(dist)
    } else {
        path.with_extension("dist")
    };
    Ok(Some(BuiltWheel {
        filename: meta.filename,
        url: meta.url,
        sha256: meta.sha256,
        size: meta.size,
        cached_path: path,
        dist_path: dist,
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
        dist_path: Some(built.dist_path.display().to_string()),
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
    #[serde(default)]
    dist_path: Option<String>,
    python_tag: String,
    abi_tag: String,
    platform_tag: String,
}

#[derive(Serialize, Deserialize)]
struct WheelUnpackMetadata {
    sha256: String,
}

fn parse_wheel_tags(filename: &str) -> Option<(String, String, String)> {
    let path = Path::new(filename);
    if !path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("whl"))
    {
        return None;
    }
    let trimmed = path.file_stem()?.to_str()?;
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

pub struct SdistRequest<'a> {
    pub normalized_name: &'a str,
    pub version: &'a str,
    pub filename: &'a str,
    pub url: &'a str,
    pub sha256: Option<&'a str>,
    pub python_path: &'a str,
}

#[derive(Debug, Clone)]
pub struct BuiltWheel {
    pub filename: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
    pub cached_path: PathBuf,
    pub dist_path: PathBuf,
    pub python_tag: String,
    pub abi_tag: String,
    pub platform_tag: String,
}

/// Ensure the requested wheel is available within the cache.
///
/// # Errors
///
/// Returns an error when downloads fail, hashes do not match, or the cache
/// directory cannot be mutated.
pub fn cache_wheel(cache_root: &Path, request: &ArtifactRequest<'_>) -> Result<CachedArtifact> {
    let wheel_dest = wheel_path(cache_root, request.name, request.version, request.filename);
    let wheel = match validate_existing(&wheel_dest, request.sha256)? {
        Some(existing) => existing,
        None => download_with_retry(&wheel_dest, request)?,
    };
    let dist_path = ensure_wheel_dist(&wheel.path, request.sha256)?;
    Ok(CachedArtifact {
        wheel_path: wheel.path,
        dist_path,
        size: wheel.size,
    })
}

fn wheel_path(cache_root: &Path, name: &str, version: &str, filename: &str) -> PathBuf {
    cache_root
        .join("wheels")
        .join(name)
        .join(version)
        .join(filename)
}

fn validate_existing(path: &Path, expected_sha: &str) -> Result<Option<CachedWheelFile>> {
    if !path.exists() {
        return Ok(None);
    }

    match compute_sha256(path) {
        Ok(actual) if actual == expected_sha => {
            let size = fs::metadata(path)?.len();
            Ok(Some(CachedWheelFile {
                path: path.to_path_buf(),
                size,
            }))
        }
        Ok(_) | Err(_) => {
            let _ = fs::remove_file(path);
            Ok(None)
        }
    }
}

/// Fetch every artifact described by `specs` into the cache and summarize the results.
pub fn prefetch_artifacts(
    cache_root: &Path,
    specs: &[PrefetchSpec<'_>],
    options: PrefetchOptions,
) -> PrefetchSummary {
    let mut summary = PrefetchSummary {
        requested: specs.len(),
        ..PrefetchSummary::default()
    };

    if specs.is_empty() {
        return summary;
    }

    let batch_size = options.parallel.max(1);

    for chunk in specs.chunks(batch_size) {
        for spec in chunk {
            let dest = wheel_path(cache_root, spec.name, spec.version, spec.filename);
            let existing = match validate_existing(&dest, spec.sha256) {
                Ok(value) => value,
                Err(err) => {
                    summary.failed += 1;
                    summary.errors.push(err.to_string());
                    continue;
                }
            };

            if options.dry_run {
                if existing.is_some() {
                    summary.hit += 1;
                }
                continue;
            }

            if let Some(file) = existing {
                match ensure_wheel_dist(&file.path, spec.sha256) {
                    Ok(_) => summary.hit += 1,
                    Err(err) => {
                        summary.failed += 1;
                        summary.errors.push(err.to_string());
                    }
                }
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

    summary
}

fn download_with_retry(dest: &Path, request: &ArtifactRequest<'_>) -> Result<CachedWheelFile> {
    let mut last_err = None;
    for _ in 0..DOWNLOAD_ATTEMPTS {
        match download_once(dest, request) {
            Ok(file) => return Ok(file),
            Err(err) => last_err = Some(err),
        }
    }

    Err(last_err
        .unwrap_or_else(|| anyhow!("failed to download {}; no attempts left", request.filename)))
}

fn download_once(dest: &Path, request: &ArtifactRequest<'_>) -> Result<CachedWheelFile> {
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
    let mut buffer = vec![0_u8; 64 * 1024];
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
    Ok(CachedWheelFile {
        path: dest.to_path_buf(),
        size: written,
    })
}

fn ensure_wheel_dist(wheel_path: &Path, expected_sha: &str) -> Result<PathBuf> {
    let dist_dir = wheel_path.with_extension("dist");
    let marker_path = dist_dir.join(WHEEL_MARKER_NAME);
    if dist_dir.exists() && marker_matches(&marker_path, expected_sha) {
        return Ok(dist_dir);
    }

    if dist_dir.exists() {
        fs::remove_dir_all(&dist_dir)
            .with_context(|| format!("failed to clear outdated dist at {}", dist_dir.display()))?;
    }

    let staging = dist_dir.with_extension("tmp");
    if staging.exists() {
        fs::remove_dir_all(&staging)
            .with_context(|| format!("failed to clear staging dir {}", staging.display()))?;
    }
    fs::create_dir_all(&staging)?;
    unpack_wheel(wheel_path, &staging)?;
    write_marker(&staging.join(WHEEL_MARKER_NAME), expected_sha)?;
    if dist_dir.exists() {
        fs::remove_dir_all(&dist_dir)?;
    }
    fs::rename(&staging, &dist_dir)?;
    Ok(dist_dir)
}

fn marker_matches(marker: &Path, expected_sha: &str) -> bool {
    if !marker.exists() {
        return false;
    }
    let Ok(contents) = fs::read_to_string(marker) else {
        return false;
    };
    let Ok(meta) = serde_json::from_str::<WheelUnpackMetadata>(&contents) else {
        return false;
    };
    meta.sha256 == expected_sha
}

fn write_marker(marker: &Path, sha: &str) -> Result<()> {
    let meta = WheelUnpackMetadata {
        sha256: sha.to_string(),
    };
    if let Some(parent) = marker.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(marker, serde_json::to_string(&meta)?)?;
    Ok(())
}

fn unpack_wheel(wheel: &Path, dest: &Path) -> Result<()> {
    let file = File::open(wheel)?;
    let mut archive = ZipArchive::new(file)?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let Some(enclosed) = entry.enclosed_name().map(|p| dest.join(p)) else {
            continue;
        };
        if entry.name().ends_with('/') || entry.is_dir() {
            fs::create_dir_all(&enclosed)?;
            continue;
        }
        if let Some(parent) = enclosed.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut outfile = File::create(&enclosed)?;
        io::copy(&mut entry, &mut outfile)?;
        #[cfg(unix)]
        {
            if let Some(mode) = entry.unix_mode() {
                fs::set_permissions(&enclosed, fs::Permissions::from_mode(mode))?;
            }
        }
    }
    Ok(())
}

fn compute_sha256(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0_u8; 32 * 1024];
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
        .build()
        .context("failed to build http client")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{env, ffi::OsString, fs::File, io::Write};
    use zip::write::FileOptions;

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
        assert!(artifact.wheel_path.exists());
        assert!(artifact.dist_path.exists());
        assert_eq!(artifact.size, fs::metadata(&artifact.wheel_path)?.len());
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
        write_dummy_wheel(&wheel_path, b"print('demo')")?;

        let sha = compute_sha256(&wheel_path)?;
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

        let summary = prefetch_artifacts(cache_root, &specs, PrefetchOptions::default());
        assert_eq!(summary.requested, 1);
        assert_eq!(summary.hit, 1);
        assert_eq!(summary.fetched, 0);
        assert_eq!(summary.failed, 0);
        assert!(wheel_path.with_extension("dist").exists());
        Ok(())
    }

    #[test]
    fn prune_cache_entries_deletes_and_reports_errors() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let present = temp.path().join("wheel/demo-1.0.0.whl");
        fs::create_dir_all(present.parent().unwrap())?;
        fs::write(&present, b"demo")?;
        let missing = temp.path().join("wheel/missing-1.0.0.whl");
        let walk = CacheWalk {
            exists: true,
            files: vec![
                CacheEntry {
                    path: present.clone(),
                    size: 4,
                },
                CacheEntry {
                    path: missing.clone(),
                    size: 9,
                },
            ],
            dirs: Vec::new(),
            total_bytes: 13,
        };

        let result = prune_cache_entries(&walk);

        assert_eq!(result.candidate_entries, 2);
        assert_eq!(result.candidate_size_bytes, 13);
        assert_eq!(result.deleted_entries, 1);
        assert_eq!(result.deleted_size_bytes, 4);
        assert_eq!(result.errors.len(), 1);
        assert!(
            result.errors[0].path.ends_with(missing),
            "expected error to refer to missing path"
        );
        assert!(
            !present.exists(),
            "present file should be removed after prune"
        );
        Ok(())
    }

    #[test]
    fn marker_matches_rejects_mismatched_checksum() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let marker = temp.path().join("wheel/.px-wheel.json");
        write_marker(&marker, "deadbeef")?;
        assert!(
            !marker_matches(&marker, "cafebabe"),
            "marker should reject differing checksum"
        );
        Ok(())
    }

    #[test]
    fn collect_cache_walk_sorts_entries() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();
        let alpha = root.join("a/alpha.whl");
        let beta = root.join("b/beta.whl");
        fs::create_dir_all(alpha.parent().unwrap())?;
        fs::create_dir_all(beta.parent().unwrap())?;
        fs::write(&alpha, b"a")?;
        fs::write(&beta, b"beta-bits")?;

        let walk = collect_cache_walk(root)?;

        assert!(walk.exists);
        assert_eq!(walk.total_bytes, 1 + 9);
        let files: Vec<&Path> = walk.files.iter().map(|entry| entry.path.as_path()).collect();
        assert_eq!(files, vec![alpha.as_path(), beta.as_path()]);
        assert_eq!(walk.dirs.len(), 2, "expected two child directories recorded");
        Ok(())
    }

    fn write_dummy_wheel(path: &Path, contents: &[u8]) -> Result<()> {
        let file = File::create(path)?;
        let mut writer = zip::ZipWriter::new(file);
        let options = FileOptions::default();
        writer.start_file("demo/__init__.py", options)?;
        writer.write_all(contents)?;
        writer.finish()?;
        Ok(())
    }
}
