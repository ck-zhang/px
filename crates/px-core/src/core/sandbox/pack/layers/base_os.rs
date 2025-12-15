use std::fs::{self, File};
use std::io::Read;
use std::path::Path;
use std::process::Command;

use anyhow::Result;
use serde_json::json;
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use crate::core::sandbox::runner::ContainerBackend;
use crate::core::sandbox::{sandbox_error, SYSTEM_DEPS_IMAGE};
use crate::InstallUserError;

use super::super::LayerTar;

pub(crate) fn write_base_os_layer(
    backend: &ContainerBackend,
    blobs: &Path,
) -> Result<LayerTar, InstallUserError> {
    fs::create_dir_all(blobs).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to prepare OCI blob directory",
            json!({ "path": blobs.display().to_string(), "error": err.to_string() }),
        )
    })?;

    let create = Command::new(&backend.program)
        .arg("create")
        .arg(SYSTEM_DEPS_IMAGE)
        .output()
        .map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to create base sandbox container",
                json!({ "error": err.to_string(), "image": SYSTEM_DEPS_IMAGE }),
            )
        })?;
    if !create.status.success() {
        return Err(sandbox_error(
            "PX903",
            "failed to create base sandbox container",
            json!({
                "image": SYSTEM_DEPS_IMAGE,
                "code": create.status.code(),
                "stdout": String::from_utf8_lossy(&create.stdout).to_string(),
                "stderr": String::from_utf8_lossy(&create.stderr).to_string(),
            }),
        ));
    }
    let id = String::from_utf8_lossy(&create.stdout).trim().to_string();
    if id.is_empty() {
        return Err(sandbox_error(
            "PX903",
            "failed to create base sandbox container",
            json!({
                "image": SYSTEM_DEPS_IMAGE,
                "reason": "missing_container_id",
                "stdout": String::from_utf8_lossy(&create.stdout).to_string(),
            }),
        ));
    }

    let temp = NamedTempFile::new_in(blobs).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to create base sandbox layer",
            json!({ "path": blobs.display().to_string(), "error": err.to_string() }),
        )
    })?;
    let out_file = temp.reopen().map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to prepare base sandbox layer",
            json!({ "error": err.to_string() }),
        )
    })?;
    let export = Command::new(&backend.program)
        .arg("export")
        .arg(&id)
        .stdout(out_file)
        .stderr(std::process::Stdio::piped())
        .output();

    let _ = Command::new(&backend.program)
        .arg("rm")
        .arg("-f")
        .arg(&id)
        .output();

    let export = export.map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to export base sandbox filesystem",
            json!({ "error": err.to_string(), "image": SYSTEM_DEPS_IMAGE }),
        )
    })?;
    if !export.status.success() {
        return Err(sandbox_error(
            "PX903",
            "failed to export base sandbox filesystem",
            json!({
                "image": SYSTEM_DEPS_IMAGE,
                "code": export.status.code(),
                "stdout": String::from_utf8_lossy(&export.stdout).to_string(),
                "stderr": String::from_utf8_lossy(&export.stderr).to_string(),
            }),
        ));
    }

    let mut file = File::open(temp.path()).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to read base sandbox layer",
            json!({ "path": temp.path().display().to_string(), "error": err.to_string() }),
        )
    })?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to read base sandbox layer",
                json!({ "path": temp.path().display().to_string(), "error": err.to_string() }),
            )
        })?;
        if read == 0 {
            break;
        }
        size = size.saturating_add(read as u64);
        hasher.update(&buffer[..read]);
    }
    let digest = format!("{:x}", hasher.finalize());
    let layer_path = blobs.join(&digest);
    if !layer_path.exists() {
        match temp.persist_noclobber(&layer_path) {
            Ok(_) => {}
            Err(err) => {
                if err.error.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(sandbox_error(
                        "PX903",
                        "failed to write base sandbox layer",
                        json!({
                            "path": layer_path.display().to_string(),
                            "error": err.error.to_string(),
                        }),
                    ));
                }
            }
        }
    }

    Ok(LayerTar {
        digest,
        size,
        path: layer_path,
    })
}
