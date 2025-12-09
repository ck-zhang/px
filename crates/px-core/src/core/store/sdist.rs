use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use bzip2::read::BzDecoder;
use hex;
use reqwest;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tar::Archive;
use tempfile::NamedTempFile;
use tracing::debug;
use walkdir::WalkDir;

use super::{
    apply_python_env,
    cas::{
        global_store, pkg_build_lookup_key, source_lookup_key, ObjectKind, ObjectPayload,
        PkgBuildHeader, SourceHeader,
    },
    load_cached_build, persist_metadata,
    wheel::{compute_sha256, ensure_wheel_dist, parse_wheel_tags, wheel_path},
    BuiltWheel, SdistRequest,
};
use std::borrow::Cow;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum BuildMethod {
    PipWheel,
    PythonBuild,
    BuilderWheel,
}

fn copy_native_libs(env_root: &Path, dist_path: &Path) -> Result<()> {
    let mut env_lib_index: HashMap<String, PathBuf> = HashMap::new();
    for dir in ["lib", "lib64"] {
        let lib_root = env_root.join(dir);
        if !lib_root.exists() {
            continue;
        }
        for entry in WalkDir::new(&lib_root) {
            let entry = entry?;
            if entry.file_type().is_file() && is_shared_lib(entry.path()) {
                if let Some(name) = entry
                    .path()
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(str::to_string)
                {
                    env_lib_index
                        .entry(name)
                        .or_insert(entry.path().to_path_buf());
                }
            }
        }
    }
    debug!(
        env_root = %env_root.display(),
        lib_count = env_lib_index.len(),
        "collecting native libs from builder environment"
    );

    let mut queue = VecDeque::new();
    for entry in WalkDir::new(dist_path) {
        let entry = entry?;
        if entry.file_type().is_file() && is_shared_lib(entry.path()) {
            queue.push_back(entry.path().to_path_buf());
        }
    }
    debug!(
        dist = %dist_path.display(),
        seeds = queue.len(),
        "scanning built wheel for native dependencies"
    );

    let mut seen = HashSet::new();
    let mut deps_to_copy: HashSet<PathBuf> = HashSet::new();

    while let Some(target) = queue.pop_front() {
        if !seen.insert(target.clone()) {
            continue;
        }
        let deps = ldd_dependencies(&target, env_root)?;
        for dep in deps {
            let resolved = if dep.starts_with(env_root) {
                Some(dep)
            } else {
                dep.file_name()
                    .and_then(|name| name.to_str())
                    .and_then(|name| env_lib_index.get(name).cloned())
            };
            let Some(dep_path) = resolved else {
                continue;
            };
            if deps_to_copy.insert(dep_path.clone()) {
                queue.push_back(dep_path);
            }
        }
    }

    debug!(
        deps = deps_to_copy.len(),
        "copying resolved native libraries into wheel dist"
    );
    for dep in deps_to_copy {
        let rel = dep.strip_prefix(env_root).unwrap_or(dep.as_path());
        let dest = dist_path.join(rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&dep, &dest).with_context(|| {
            format!(
                "failed to copy native lib {} to {}",
                dep.display(),
                dest.display()
            )
        })?;
    }
    Ok(())
}

fn is_shared_lib(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| {
            let lower = name.to_ascii_lowercase();
            lower.contains(".so") || lower.ends_with(".dylib") || lower.ends_with(".dll")
        })
        .unwrap_or(false)
}

fn ldd_dependencies(target: &Path, env_root: &Path) -> Result<Vec<PathBuf>> {
    let mut cmd = Command::new("ldd");
    cmd.arg(target);
    let mut search_paths = Vec::new();
    for dir in ["lib", "lib64"] {
        let candidate = env_root.join(dir);
        if candidate.exists() {
            search_paths.push(candidate);
        }
    }
    if !search_paths.is_empty() {
        let joined = search_paths
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(":");
        cmd.env("LD_LIBRARY_PATH", joined);
    }
    let output = cmd
        .output()
        .with_context(|| format!("failed to run ldd on {}", target.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("ldd failed for {}: {stderr}", target.display());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut deps = Vec::new();
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut path_candidate: Option<PathBuf> = None;
        if let Some((_, rest)) = trimmed.split_once("=>") {
            let value = rest.trim();
            if value.starts_with("not found") || value == "not found" {
                continue;
            }
            let path_part = value.split_whitespace().next().unwrap_or("");
            if path_part.is_empty() || path_part == "not" {
                continue;
            }
            if path_part.starts_with('/') {
                path_candidate = Some(PathBuf::from(path_part));
            }
        } else if let Some(first) = trimmed.split_whitespace().next() {
            if first.starts_with('/') {
                path_candidate = Some(PathBuf::from(first));
            }
        }
        if let Some(path) = path_candidate {
            deps.push(path);
        }
    }
    Ok(deps)
}

impl Default for BuildMethod {
    fn default() -> Self {
        Self::PipWheel
    }
}

/// Build or retrieve an sdist-derived wheel from the cache.
///
/// # Errors
///
/// Returns an error when the archive cannot be unpacked, hashed, or copied into
/// the cache directory.
pub fn ensure_sdist_build(cache_root: &Path, request: &SdistRequest<'_>) -> Result<BuiltWheel> {
    let cas = global_store();

    let precomputed_id = request.sha256.map(|sha| build_identifier(request, sha));
    if let Some(id) = &precomputed_id {
        let meta_path = cache_root.join("sdist-build").join(id).join("meta.json");
        if let Some(built) = load_cached_build(&meta_path)? {
            if cache_hit_matches(request, &built)? && built.cached_path.exists() {
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
    let sdist_bytes = fs::read(temp_file.path())?;
    let source_header = SourceHeader {
        name: request.normalized_name.to_string(),
        version: request.version.to_string(),
        filename: request.filename.to_string(),
        index_url: request.url.to_string(),
        sha256: download_sha.clone(),
    };
    let source_key = source_lookup_key(&source_header);
    let source_oid = match cas.lookup_key(ObjectKind::Source, &source_key)? {
        Some(oid) => oid,
        None => {
            let payload = ObjectPayload::Source {
                header: source_header.clone(),
                bytes: Cow::Owned(sdist_bytes.clone()),
            };
            let stored = cas.store(&payload)?;
            cas.record_key(ObjectKind::Source, &source_key, &stored.oid)?;
            stored.oid
        }
    };
    let build_id = precomputed_id.unwrap_or_else(|| build_identifier(request, &download_sha));
    let build_root = cache_root.join("sdist-build").join(&build_id);
    let meta_path = build_root.join("meta.json");

    if let Some(built) = load_cached_build(&meta_path)? {
        if cache_hit_matches(request, &built)? && built.cached_path.exists() {
            return Ok(built);
        }
        if built.builder_id != request.builder_id && build_root.exists() {
            let _ = fs::remove_dir_all(&build_root);
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
    ensure_build_bootstrap(request.python_path)?;

    let (build_method, build_python_path, builder_env_root) =
        if needs_builder(request.normalized_name) {
            let (method, py, root) = build_with_micromamba(request, &project_dir, &dist_dir)?;
            (method, py, Some(root))
        } else {
            (
                run_python_build(request.python_path, &project_dir, &dist_dir)?,
                PathBuf::from(request.python_path),
                None,
            )
        };

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
    if let Some(env_root) = &builder_env_root {
        copy_native_libs(env_root, &dist_path)?;
    }
    let runtime_abi = format!("{python_tag}-{abi_tag}-{platform_tag}");
    let mut build_options_hash = compute_build_options_hash(
        build_python_path.to_str().unwrap_or(request.python_path),
        build_method,
    )?;
    if builder_env_root.is_some() {
        build_options_hash = format!("{build_options_hash}-native-libs");
    }
    let archive = super::cas::archive_dir_canonical(&dist_path)?;
    let pkg_header = PkgBuildHeader {
        source_oid,
        runtime_abi,
        builder_id: request.builder_id.to_string(),
        build_options_hash: build_options_hash.clone(),
    };
    let pkg_key = pkg_build_lookup_key(&pkg_header);
    let pkg_payload = ObjectPayload::PkgBuild {
        header: pkg_header.clone(),
        archive: Cow::Owned(archive),
    };
    let stored_pkg = cas.store(&pkg_payload)?;
    cas.record_key(ObjectKind::PkgBuild, &pkg_key, &stored_pkg.oid)?;

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
        build_options_hash,
        build_method,
        builder_id: request.builder_id.to_string(),
    };
    persist_metadata(&meta_path, &built)?;

    let _ = fs::remove_dir_all(&src_dir);
    let _ = fs::remove_dir_all(&dist_dir);

    Ok(built)
}

fn build_identifier(request: &SdistRequest<'_>, sha256: &str) -> String {
    let short_len = sha256.len().min(16);
    let builder = sanitize_builder_id(request.builder_id);
    format!(
        "{}-{}-{}-{}",
        request.normalized_name,
        request.version,
        &sha256[..short_len],
        builder
    )
}

fn sanitize_builder_id(builder_id: &str) -> String {
    builder_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn cache_hit_matches(request: &SdistRequest<'_>, built: &BuiltWheel) -> Result<bool> {
    if !built.builder_id.is_empty() && built.builder_id != request.builder_id {
        return Ok(false);
    }
    if built.build_options_hash.is_empty() {
        return Ok(false);
    }
    let expected = compute_build_options_hash(request.python_path, built.build_method)?;
    Ok(built.build_options_hash == expected)
}

fn needs_builder(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "gdal" | "osgeo" | "rasterio" | "fiona" | "pyproj" | "shapely"
    )
}

fn download_sdist(request: &SdistRequest<'_>) -> Result<(NamedTempFile, String)> {
    let mut last_err = None;
    for _ in 0..super::DOWNLOAD_ATTEMPTS {
        match download_sdist_once(request) {
            Ok(result) => return Ok(result),
            Err(err) => last_err = Some(err),
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow!("failed to download sdist {}", request.url)))
}

fn download_sdist_once(request: &SdistRequest<'_>) -> Result<(NamedTempFile, String)> {
    let client = super::http_client()?;
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
        Ok(_) => Ok(()),
        Err(err) if is_cross_device(&err) => std::fs::copy(src, dest).map(|_| ()),
        Err(err) => Err(err),
    }
}

fn is_cross_device(err: &io::Error) -> bool {
    matches!(err.raw_os_error(), Some(18))
}

fn extract_sdist(python: &str, sdist: &Path, dest: &Path) -> Result<()> {
    let status = Command::new(python)
        .arg("-m")
        .arg("tarfile")
        .arg("-e")
        .arg(sdist)
        .arg(dest)
        .status()
        .with_context(|| format!("failed to unpack {}", sdist.display()))?;
    if !status.success() {
        bail!("tarfile failed to unpack {}", sdist.display());
    }
    Ok(())
}

fn run_python_build(python: &str, project_dir: &Path, out_dir: &Path) -> Result<BuildMethod> {
    match pip_wheel_fallback(python, project_dir, out_dir) {
        Ok(method) => Ok(method),
        Err(pip_err) => {
            let mut cmd = Command::new(python);
            cmd.arg("-m")
                .arg("build")
                .arg("--wheel")
                .arg("--outdir")
                .arg(out_dir)
                .arg(project_dir);
            apply_python_env(&mut cmd);
            cmd.env("PX_BUILD_FROM_SDIST", "1");
            let output = cmd.output().with_context(|| {
                format!("failed to run python -m build in {}", project_dir.display())
            })?;
            if output.status.success() {
                Ok(BuildMethod::PythonBuild)
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                bail!("python -m pip wheel failed: {pip_err}\npython -m build failed: {stderr}")
            }
        }
    }
}

fn pip_wheel_fallback(python: &str, project_dir: &Path, out_dir: &Path) -> Result<BuildMethod> {
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
        return Ok(BuildMethod::PipWheel);
    }
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    bail!("{stderr}");
}

fn build_with_micromamba(
    request: &SdistRequest<'_>,
    project_dir: &Path,
    out_dir: &Path,
) -> Result<(BuildMethod, PathBuf, PathBuf)> {
    let builder_root = request
        .builder_root
        .clone()
        .unwrap_or_else(|| std::env::temp_dir());
    fs::create_dir_all(&builder_root)?;
    let micromamba = ensure_micromamba(&builder_root)?;
    let py_version = python_version(request.python_path)?;
    let env_root = builder_root
        .join("envs")
        .join(sanitize_builder_id(request.builder_id))
        .join(format!("py{py_version}"));
    let conda_meta = env_root.join("conda-meta");
    let env_python = env_root.join("bin").join("python");
    let needs_create = !conda_meta.exists() || !env_python.exists();
    if env_root.exists() && needs_create {
        debug!(
            env_root = %env_root.display(),
            "builder env path exists but is incomplete; recreating"
        );
        let _ = fs::remove_dir_all(&env_root);
    }
    debug!(
        env_root = %env_root.display(),
        builder_id = request.builder_id,
        py_version,
        "provisioning micromamba builder environment",
    );
    if needs_create {
        let create_out = Command::new(&micromamba)
            .arg("create")
            .arg("-y")
            .arg("-p")
            .arg(&env_root)
            .arg(format!("python=={py_version}"))
            .arg("gdal")
            .arg("proj")
            .arg("geos")
            .arg("pysocks")
            .output()
            .context("failed to spawn micromamba to provision builder env")?;
        if !create_out.status.success() {
            let stderr = String::from_utf8_lossy(&create_out.stderr);
            bail!("failed to provision builder environment with micromamba: {stderr}");
        }
    }
    let builder_python = env_root.join("bin").join("python");
    if !builder_python.exists() {
        bail!(
            "builder environment missing python at {}",
            builder_python.display()
        );
    }
    let mut cmd = Command::new(&builder_python);
    cmd.arg("-m")
        .arg("pip")
        .arg("wheel")
        .arg("--no-deps")
        .arg("--wheel-dir")
        .arg(out_dir)
        .arg(project_dir);
    cmd.env("GDAL_CONFIG", env_root.join("bin").join("gdal-config"));
    cmd.env("PROJ_LIB", env_root.join("share").join("proj"));
    cmd.env(
        "PATH",
        format!(
            "{}/bin:{}",
            env_root.display(),
            std::env::var("PATH").unwrap_or_default()
        ),
    );
    for key in [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        cmd.env_remove(key);
    }
    cmd.current_dir(project_dir);
    apply_python_env(&mut cmd);
    let output = cmd.output().context("failed to run builder pip wheel")?;
    if output.status.success() {
        return Ok((BuildMethod::BuilderWheel, builder_python, env_root));
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    bail!(
        "builder pip wheel failed (code {}):\nstdout:\n{}\nstderr:\n{}",
        output.status,
        stdout,
        stderr
    );
}

fn ensure_micromamba(root: &Path) -> Result<PathBuf> {
    let bin = root.join("micromamba");
    if bin.exists() {
        return Ok(bin);
    }
    let url = "https://micromamba.snakepit.net/api/micromamba/linux-64/latest";
    let response = reqwest::blocking::get(url).context("failed to download micromamba")?;
    let status = response.status();
    let bytes = response
        .error_for_status()
        .map_err(|err| anyhow!("micromamba download failed: {err} (status {status})"))?
        .bytes()
        .context("failed to read micromamba download")?;
    let mut decoder = BzDecoder::new(bytes.as_ref());
    let mut extracted = Vec::new();
    decoder
        .read_to_end(&mut extracted)
        .context("failed to decompress micromamba")?;
    let mut archive = Archive::new(std::io::Cursor::new(extracted));
    let mut extracted_bin = Vec::new();
    for entry in archive
        .entries()
        .context("failed to read micromamba archive")?
    {
        let mut entry = entry.context("failed to read micromamba entry")?;
        let path = entry
            .path()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_owned()));
        if let Some(name) = path {
            if name == "micromamba" {
                entry
                    .read_to_end(&mut extracted_bin)
                    .context("failed to extract micromamba binary")?;
                break;
            }
        }
    }
    if extracted_bin.is_empty() {
        bail!("micromamba archive did not contain binary");
    }
    fs::write(&bin, &extracted_bin).context("failed to write micromamba binary")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&bin, fs::Permissions::from_mode(0o755));
    }
    Ok(bin)
}

fn python_version(python: &str) -> Result<String> {
    let output = Command::new(python)
        .arg("-c")
        .arg("import sys; print(f\"{sys.version_info[0]}.{sys.version_info[1]}\")")
        .output()
        .context("failed to query python version")?;
    if !output.status.success() {
        bail!("failed to query python version");
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn ensure_build_bootstrap(python: &str) -> Result<()> {
    let mut cmd = Command::new(python);
    cmd.arg("-m")
        .arg("pip")
        .arg("install")
        .arg("--upgrade")
        .arg("--quiet")
        .arg("pip")
        .arg("build")
        .arg("wheel")
        .arg("pysocks");
    apply_python_env(&mut cmd);
    for key in [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        cmd.env_remove(key);
    }
    let output = cmd
        .output()
        .context("failed to bootstrap build tools (pip/build/wheel/pysocks)")?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    bail!("failed to bootstrap build tools: {stderr}");
}

fn discover_project_dir(root: &Path) -> Result<PathBuf> {
    if is_project_dir(root) {
        return Ok(root.to_path_buf());
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let path = entry.path();
            if is_project_dir(&path) {
                return Ok(path);
            }
        }
    }
    Err(anyhow!("unable to find project dir in {}", root.display()))
}

fn is_project_dir(path: &Path) -> bool {
    path.join("pyproject.toml").exists()
        || path.join("setup.py").exists()
        || path.join("setup.cfg").exists()
}

fn find_wheel(dist_dir: &Path) -> Result<PathBuf> {
    let mut found = None;
    for entry in fs::read_dir(dist_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_file()
            && entry
                .path()
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("whl"))
        {
            found = Some(entry.path());
            break;
        }
    }
    found.ok_or_else(|| anyhow!("wheel not found in {}", dist_dir.display()))
}

pub(crate) fn compute_build_options_hash(python_path: &str, method: BuildMethod) -> Result<String> {
    #[derive(Serialize)]
    struct BuildOptionsFingerprint {
        python: String,
        method: BuildMethod,
        env: BTreeMap<String, String>,
    }

    let python = fs::canonicalize(python_path)
        .unwrap_or_else(|_| PathBuf::from(python_path))
        .display()
        .to_string();
    let fingerprint = BuildOptionsFingerprint {
        python,
        method,
        env: build_env_fingerprint(),
    };
    let bytes = serde_json::to_vec(&fingerprint)?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

/// Compute the build options hash for a wheel-style build/install.
pub(crate) fn wheel_build_options_hash(python_path: &str) -> Result<String> {
    compute_build_options_hash(python_path, BuildMethod::PipWheel)
}

fn build_env_fingerprint() -> BTreeMap<String, String> {
    const BUILD_ENV_VARS: &[&str] = &[
        "ARCHFLAGS",
        "CFLAGS",
        "CPPFLAGS",
        "CXXFLAGS",
        "LDFLAGS",
        "MACOSX_DEPLOYMENT_TARGET",
        "PKG_CONFIG_PATH",
        "PIP_CONFIG_FILE",
        "PIP_DISABLE_PIP_VERSION_CHECK",
        "PIP_EXTRA_INDEX_URL",
        "PIP_FIND_LINKS",
        "PIP_INDEX_URL",
        "PIP_NO_BUILD_ISOLATION",
        "PIP_NO_CACHE_DIR",
        "PIP_PREFER_BINARY",
        "PIP_PROGRESS_BAR",
        "PYTHONDONTWRITEBYTECODE",
        "PYTHONHASHSEED",
        "PYTHONUTF8",
        "PYTHONWARNINGS",
        "SETUPTOOLS_USE_DISTUTILS",
        "SOURCE_DATE_EPOCH",
        "CARGO_BUILD_TARGET",
        "CARGO_HOME",
        "CARGO_TARGET_DIR",
        "MATURIN_BUILD_ARGS",
        "MATURIN_CARGO_FLAGS",
        "MATURIN_CARGO_PROFILE",
        "MATURIN_FEATURES",
        "MATURIN_PEP517_ARGS",
        "MATURIN_PEP517_FEATURES",
        "PYO3_CONFIG_FILE",
        "RUSTFLAGS",
        "RUSTUP_TOOLCHAIN",
    ];
    let mut env = BTreeMap::new();
    for key in BUILD_ENV_VARS {
        if let Ok(value) = std::env::var(key) {
            env.insert((*key).to_string(), value);
        }
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::fs;
    use tempfile::tempdir;

    fn restore_env(key: &str, original: Option<String>) {
        match original {
            Some(value) => env::set_var(key, value),
            None => env::remove_var(key),
        }
    }

    #[test]
    fn build_options_hash_reflects_env_changes() -> Result<()> {
        let key = "CFLAGS";
        let original = env::var(key).ok();
        env::set_var(key, "value-1");
        let temp_python = env::temp_dir().join("python");
        let python = temp_python.display().to_string();

        let first = compute_build_options_hash(&python, BuildMethod::PipWheel)?;
        env::set_var(key, "value-2");
        let second = compute_build_options_hash(&python, BuildMethod::PipWheel)?;

        restore_env(key, original);
        assert_ne!(first, second, "hash should change when build env changes");
        Ok(())
    }

    #[test]
    fn build_options_hash_reflects_rust_env() -> Result<()> {
        let key = "RUSTFLAGS";
        let original = env::var(key).ok();
        env::set_var(key, "value-1");
        let temp_python = env::temp_dir().join("python");
        let python = temp_python.display().to_string();

        let first = compute_build_options_hash(&python, BuildMethod::PipWheel)?;
        env::set_var(key, "value-2");
        let second = compute_build_options_hash(&python, BuildMethod::PipWheel)?;

        restore_env(key, original);
        assert_ne!(
            first, second,
            "hash should change when rust build env changes"
        );
        Ok(())
    }

    #[test]
    fn build_options_hash_varies_by_method() -> Result<()> {
        let temp_python = env::temp_dir().join("python");
        let python = temp_python.display().to_string();
        let pip = compute_build_options_hash(&python, BuildMethod::PipWheel)?;
        let build = compute_build_options_hash(&python, BuildMethod::PythonBuild)?;
        assert_ne!(pip, build, "build method should influence options hash");
        Ok(())
    }

    #[test]
    fn detects_project_at_root_with_pyproject() -> Result<()> {
        let dir = tempdir()?;
        let pyproject = dir.path().join("pyproject.toml");
        fs::write(&pyproject, b"[project]\nname = \"demo\"")?;

        let detected = discover_project_dir(dir.path())?;

        assert_eq!(detected, dir.path());
        Ok(())
    }

    #[test]
    fn detects_project_in_subdir_with_setup_py() -> Result<()> {
        let dir = tempdir()?;
        let nested = dir.path().join("pkg");
        fs::create_dir_all(&nested)?;
        fs::write(
            nested.join("setup.py"),
            b"from setuptools import setup\nsetup()",
        )?;

        let detected = discover_project_dir(dir.path())?;

        assert_eq!(detected, nested);
        Ok(())
    }

    #[test]
    fn errors_when_project_files_missing() {
        let dir = tempdir().unwrap();
        let result = discover_project_dir(dir.path());
        assert!(result.is_err());
    }
}
