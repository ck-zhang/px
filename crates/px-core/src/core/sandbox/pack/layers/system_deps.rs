use std::path::Path;

use anyhow::Result;
use serde_json::json;
use walkdir::WalkDir;

use crate::core::sandbox::runner::{BackendKind, ContainerBackend};
use crate::core::sandbox::{sandbox_error, system_deps_mode, SystemDepsMode};
use crate::core::system_deps::SystemDeps;
use crate::InstallUserError;

use super::super::LayerTar;
use super::tar::{append_path, finalize_layer, layer_tar_builder};

pub(crate) fn write_system_deps_layer(
    backend: &ContainerBackend,
    deps: &SystemDeps,
    blobs: &Path,
) -> Result<Option<LayerTar>, InstallUserError> {
    if deps.apt_packages.is_empty() || matches!(system_deps_mode(), SystemDepsMode::Offline) {
        return Ok(None);
    }
    if matches!(backend.kind, BackendKind::Custom) {
        return Ok(None);
    }
    let rootfs = match crate::core::sandbox::ensure_system_deps_rootfs(deps)? {
        Some(path) => path,
        None => return Ok(None),
    };
    let layer = write_rootfs_layer(&rootfs, blobs)?;
    Ok(Some(layer))
}

fn write_rootfs_layer(rootfs: &Path, blobs: &Path) -> Result<LayerTar, InstallUserError> {
    let mut builder = layer_tar_builder(blobs)?;
    let walker = WalkDir::new(rootfs).sort_by(|a, b| a.path().cmp(b.path()));
    for entry in walker {
        let entry = entry.map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to walk system dependency tree",
                json!({ "error": err.to_string() }),
            )
        })?;
        let path = entry.path();
        if path == rootfs {
            continue;
        }
        let rel = match path.strip_prefix(rootfs) {
            Ok(rel) => rel,
            Err(_) => continue,
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        let archive_path = Path::new("").join(rel);
        append_path(&mut builder, &archive_path, path)?;
    }
    finalize_layer(builder, blobs)
}
