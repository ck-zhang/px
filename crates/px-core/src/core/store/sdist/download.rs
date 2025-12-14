use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use hex;
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use super::super::SdistRequest;
use super::super::{http_client, DOWNLOAD_ATTEMPTS};

pub(super) fn download_sdist(request: &SdistRequest<'_>) -> Result<(NamedTempFile, String)> {
    if request.url.starts_with("file://") || Path::new(request.url).exists() {
        let path = if request.url.starts_with("file://") {
            PathBuf::from(request.url.trim_start_matches("file://"))
        } else {
            PathBuf::from(request.url)
        };
        let mut src = File::open(&path)
            .with_context(|| format!("failed to open local sdist at {}", path.display()))?;
        let mut tmp = NamedTempFile::new()?;
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let read = src.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            tmp.write_all(&buffer[..read])?;
        }
        let sha256 = hex::encode(hasher.finalize());
        return Ok((tmp, sha256));
    }

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

pub(super) fn persist_named_tempfile(tmp: NamedTempFile, dest: &Path) -> io::Result<()> {
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

pub(super) fn persist_or_copy(src: &Path, dest: &Path) -> io::Result<()> {
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
