use std::path::Path;

use anyhow::Result;
use ignore::WalkBuilder;
use serde_json::json;

use crate::core::sandbox::sandbox_error;
use crate::InstallUserError;

use super::super::LayerTar;
use super::tar::{append_path, finalize_layer, layer_tar_builder};

pub(super) fn write_app_layer_tar(
    source_root: &Path,
    blobs: &Path,
) -> Result<LayerTar, InstallUserError> {
    let mut builder = layer_tar_builder(blobs)?;
    let mut walker = WalkBuilder::new(source_root);
    walker
        .git_ignore(true)
        .git_exclude(true)
        .parents(true)
        .hidden(false)
        .ignore(true)
        .sort_by_file_name(|a, b| a.cmp(b));
    for entry in walker.build() {
        let entry = entry.map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to walk source tree for sandbox pack",
                json!({ "error": err.to_string() }),
            )
        })?;
        let path = entry.path();
        if path == source_root || should_skip(path, source_root) {
            continue;
        }
        let rel = match path.strip_prefix(source_root) {
            Ok(rel) => rel,
            Err(_) => continue,
        };
        let archive_path = Path::new("app").join(rel);
        append_path(&mut builder, &archive_path, path)?;
    }
    finalize_layer(builder, blobs)
}

fn should_skip(path: &Path, root: &Path) -> bool {
    if path == root {
        return false;
    }
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    matches!(
        name,
        ".git"
            | ".px"
            | "__pycache__"
            | "target"
            | "dist"
            | ".mypy_cache"
            | ".pytest_cache"
            | ".nox"
            | ".tox"
            | ".venv"
            | "venv"
            | ".ruff_cache"
    ) || name.ends_with(".pyc")
}
