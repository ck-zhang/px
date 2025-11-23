use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use super::{
    apply_python_env, load_cached_build, persist_metadata,
    wheel::{compute_sha256, ensure_wheel_dist, parse_wheel_tags, wheel_path},
    BuiltWheel, SdistRequest,
};

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
    ensure_build_bootstrap(request.python_path)?;
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

fn run_python_build(python: &str, project_dir: &Path, out_dir: &Path) -> Result<()> {
    match pip_wheel_fallback(python, project_dir, out_dir) {
        Ok(()) => return Ok(()),
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
                return Ok(());
            }
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            bail!("python -m pip wheel failed: {pip_err}\npython -m build failed: {stderr}")
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
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let path = entry.path();
            if path.join("pyproject.toml").exists() {
                return Ok(path);
            }
        }
    }
    Err(anyhow!("unable to find project dir in {}", root.display()))
}

fn find_wheel(dist_dir: &Path) -> Result<PathBuf> {
    let mut found = None;
    for entry in fs::read_dir(dist_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            if entry
                .path()
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("whl"))
            {
                found = Some(entry.path());
                break;
            }
        }
    }
    found.ok_or_else(|| anyhow!("wheel not found in {}", dist_dir.display()))
}
