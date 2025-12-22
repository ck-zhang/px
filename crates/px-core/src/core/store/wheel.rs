use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use anyhow::{anyhow, Context, Result};
use serde_json::json;
use sha2::{Digest, Sha256};
use zip::ZipArchive;

use super::{
    cas::{global_store, source_lookup_key, LoadedObject, ObjectKind, ObjectPayload, SourceHeader},
    ArtifactRequest, CachedArtifact, CachedWheelFile, WheelUnpackMetadata, HTTP_TIMEOUT,
    USER_AGENT, WHEEL_MARKER_NAME,
};
use crate::InstallUserError;
use std::borrow::Cow;

/// Ensure the requested wheel is available within the cache.
///
/// # Errors
///
/// Returns an error when downloads fail, hashes do not match, or the cache
/// directory cannot be mutated.
pub fn cache_wheel(cache_root: &Path, request: &ArtifactRequest<'_>) -> Result<CachedArtifact> {
    let cas = global_store();
    let header = SourceHeader {
        name: request.name.to_string(),
        version: request.version.to_string(),
        filename: request.filename.to_string(),
        index_url: request.url.to_string(),
        sha256: request.sha256.to_string(),
    };
    let lookup_key = source_lookup_key(&header);
    if let Some(oid) = cas.lookup_key(ObjectKind::Source, &lookup_key)? {
        match cas.load(&oid) {
            Ok(LoadedObject::Source { bytes, .. }) => {
                let wheel_dest =
                    wheel_path(cache_root, request.name, request.version, request.filename);
                if let Some(parent) = wheel_dest.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&wheel_dest, &bytes)?;
                let sha = compute_sha256(&wheel_dest)?;
                if sha != request.sha256 {
                    return Err(anyhow!(
                        "CAS source digest mismatch for {} (expected {}, got {})",
                        request.filename,
                        request.sha256,
                        sha
                    ));
                }
                let dist_path = ensure_wheel_dist(&wheel_dest, request.sha256)?;
                return Ok(CachedArtifact {
                    wheel_path: wheel_dest,
                    dist_path,
                    size: bytes.len() as u64,
                });
            }
            Ok(_) => {
                // Wrong kind stored under key; fall through to download.
            }
            Err(_) => {
                // Corrupt or missing; allow download path to refresh.
            }
        }
    }

    let wheel_dest = wheel_path(cache_root, request.name, request.version, request.filename);
    let wheel = match validate_existing(&wheel_dest, request.sha256)? {
        Some(existing) => existing,
        None => download_with_retry(&wheel_dest, request)?,
    };
    let artifact_size = wheel.size;
    let dist_path = ensure_wheel_dist(&wheel.path, request.sha256)?;
    let wheel_bytes = fs::read(&wheel.path)?;
    let payload = ObjectPayload::Source {
        header,
        bytes: Cow::Owned(wheel_bytes.clone()),
    };
    let stored = cas.store(&payload)?;
    cas.record_key(ObjectKind::Source, &lookup_key, &stored.oid)?;
    Ok(CachedArtifact {
        wheel_path: wheel.path,
        dist_path,
        size: artifact_size,
    })
}

pub fn parse_wheel_tags(filename: &str) -> Option<(String, String, String)> {
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

pub fn wheel_path(cache_root: &Path, name: &str, version: &str, filename: &str) -> PathBuf {
    cache_root
        .join("wheels")
        .join(name)
        .join(version)
        .join(filename)
}

pub fn validate_existing(path: &Path, expected_sha: &str) -> Result<Option<CachedWheelFile>> {
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

pub fn ensure_wheel_dist(wheel_path: &Path, expected_sha: &str) -> Result<PathBuf> {
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

pub fn marker_matches(marker: &Path, expected_sha: &str) -> bool {
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

pub fn write_marker(marker: &Path, sha: &str) -> Result<()> {
    let meta = WheelUnpackMetadata {
        sha256: sha.to_string(),
    };
    if let Some(parent) = marker.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(marker, serde_json::to_string(&meta)?)?;
    Ok(())
}

pub fn unpack_wheel(wheel: &Path, dest: &Path) -> Result<()> {
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

pub fn compute_sha256(path: &Path) -> Result<String> {
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

pub fn http_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(HTTP_TIMEOUT)
        .build()
        .context("failed to build http client")
}

pub fn download_with_retry(dest: &Path, request: &ArtifactRequest<'_>) -> Result<CachedWheelFile> {
    if std::env::var("PX_ONLINE").ok().is_some_and(|value| {
        matches!(
            value.to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off" | ""
        )
    }) {
        return Err(InstallUserError::new(
            "PX_ONLINE=1 required to download packages",
            json!({
                "reason": "offline",
                "url": request.url,
                "filename": request.filename,
                "hint": "Re-run with --online / set PX_ONLINE=1, or populate the cache while online.",
            }),
        )
        .into());
    }
    let mut last_err = None;
    for _ in 0..super::DOWNLOAD_ATTEMPTS {
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

    let mut tmp = tempfile::NamedTempFile::new_in(dest.parent().unwrap_or_else(|| Path::new(".")))?;
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

    match tmp.persist(dest) {
        Ok(_) => Ok(CachedWheelFile {
            path: dest.to_path_buf(),
            size: written,
        }),
        Err(err) => {
            let io_err = err.error;
            if io_err.kind() == io::ErrorKind::AlreadyExists {
                match validate_existing(dest, request.sha256) {
                    Ok(Some(existing)) => return Ok(existing),
                    Ok(None) => {
                        let tmp = err.file;
                        let _ = fs::remove_file(dest);
                        tmp.persist(dest)?;
                        return Ok(CachedWheelFile {
                            path: dest.to_path_buf(),
                            size: written,
                        });
                    }
                    Err(check_err) => return Err(check_err),
                }
            }
            Err(io_err.into())
        }
    }
}

#[cfg(test)]
pub(crate) fn write_dummy_wheel(path: &Path, contents: &[u8]) -> Result<()> {
    let file = File::create(path)?;
    let mut writer = zip::ZipWriter::new(file);
    let options = zip::write::FileOptions::default();
    writer.start_file("demo/__init__.py", options)?;
    writer.write_all(contents)?;
    writer.finish()?;
    Ok(())
}
