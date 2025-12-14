use std::env;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::json;
use tar::Archive;

use super::super::runner::BackendKind;
use super::super::{
    detect_container_backend, sandbox_error, sandbox_timestamp_string, SandboxArtifacts,
    SandboxImageManifest, SandboxStore,
};
use super::{build_oci_image, write_base_os_layer, write_env_layer_tar, write_system_deps_layer};
use crate::{InstallUserError, PX_VERSION};

pub(crate) fn runtime_home_from_env(env_root: &Path) -> Option<PathBuf> {
    let cfg = env_root.join("pyvenv.cfg");
    let contents = fs::read_to_string(&cfg).ok()?;
    for line in contents.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("home") {
            if let Some((_, value)) = rest.split_once('=') {
                let path = value.trim();
                if !path.is_empty() {
                    return Some(PathBuf::from(path));
                }
            }
        }
    }
    None
}

fn layer_contains_runtime(blobs: &Path, digest: &str) -> Result<bool, InstallUserError> {
    let path = blobs.join(digest);
    let file = File::open(&path).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to read sandbox layer",
            json!({ "path": path.display().to_string(), "error": err.to_string() }),
        )
    })?;
    let mut archive = Archive::new(file);
    for entry in archive.entries().map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to inspect sandbox layer",
            json!({ "path": path.display().to_string(), "error": err.to_string() }),
        )
    })? {
        let entry = entry.map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to read sandbox layer entry",
                json!({ "path": path.display().to_string(), "error": err.to_string() }),
            )
        })?;
        let entry_path = entry.path().map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to resolve sandbox layer entry path",
                json!({ "path": path.display().to_string(), "error": err.to_string() }),
            )
        })?;
        if entry_path.starts_with("px/runtime/") || entry_path == Path::new("px/runtime") {
            return Ok(true);
        }
    }
    Ok(false)
}

fn persist_manifest(
    store: &SandboxStore,
    manifest: &SandboxImageManifest,
) -> Result<(), InstallUserError> {
    let path = store.image_manifest_path(&manifest.sbx_id);
    let encoded = serde_json::to_vec_pretty(manifest).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to encode sandbox image metadata",
            json!({ "error": err.to_string() }),
        )
    })?;
    fs::write(&path, encoded).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to write sandbox image metadata",
            json!({ "path": path.display().to_string(), "error": err.to_string() }),
        )
    })
}

pub(super) fn map_allowed_paths_for_image(
    allowed_paths: &[PathBuf],
    project_root: &Path,
    env_root: &Path,
) -> Vec<PathBuf> {
    let container_project = Path::new("/app");
    let container_env = Path::new("/px/env");
    let mut mapped = Vec::new();
    for path in allowed_paths {
        let mapped_path = if path.starts_with(project_root) {
            Some(
                container_project.join(
                    path.strip_prefix(project_root)
                        .unwrap_or_else(|_| Path::new("")),
                ),
            )
        } else if path.starts_with(env_root) {
            Some(
                container_env.join(
                    path.strip_prefix(env_root)
                        .unwrap_or_else(|_| Path::new("")),
                ),
            )
        } else {
            Some(path.clone())
        };
        if let Some(mapped_path) = mapped_path {
            if !mapped.iter().any(|p| p == &mapped_path) {
                mapped.push(mapped_path);
            }
        }
    }
    if !mapped.iter().any(|p| p == container_project) {
        mapped.insert(0, container_project.to_path_buf());
    }
    mapped
}

pub(super) fn join_paths_env(paths: &[PathBuf]) -> Result<String, InstallUserError> {
    let joined = env::join_paths(paths).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to assemble sandbox python path",
            json!({ "error": err.to_string(), "paths": paths }),
        )
    })?;
    joined.into_string().map_err(|_| {
        sandbox_error(
            "PX903",
            "sandbox python path contains non-utf8 entries",
            json!({ "paths": paths }),
        )
    })
}

pub(super) fn ensure_base_image(
    artifacts: &mut SandboxArtifacts,
    store: &SandboxStore,
    project_root: &Path,
    allowed_paths: &[PathBuf],
    tag: &str,
) -> Result<(), InstallUserError> {
    let backend = detect_container_backend()?;
    let oci_dir = store.oci_dir(&artifacts.definition.sbx_id());
    let blobs = oci_dir.join("blobs").join("sha256");
    let runtime_root = runtime_home_from_env(&artifacts.env_root);
    let runtime_required = runtime_root.is_some();
    let needs_system = !artifacts.definition.system_deps.apt_packages.is_empty()
        && !matches!(backend.kind, BackendKind::Custom);
    let mut rebuild = !oci_dir.join("index.json").exists();
    if !rebuild {
        let digest = artifacts
            .manifest
            .image_digest
            .trim_start_matches("sha256:")
            .to_string();
        let manifest_path = blobs.join(digest);
        let base_ok = artifacts
            .manifest
            .base_layer_digest
            .as_ref()
            .map(|d| blobs.join(d).exists())
            .unwrap_or(false);
        let env_ok = artifacts
            .manifest
            .env_layer_digest
            .as_ref()
            .map(|d| blobs.join(d).exists())
            .unwrap_or(false);
        let sys_ok = match &artifacts.manifest.system_layer_digest {
            Some(d) => blobs.join(d).exists(),
            None => !needs_system,
        };
        rebuild = !manifest_path.exists() || !base_ok || !env_ok || !sys_ok;
    }
    if !rebuild && runtime_required {
        if let Some(env_digest) = artifacts.manifest.env_layer_digest.as_ref() {
            match layer_contains_runtime(&blobs, env_digest) {
                Ok(true) => {}
                Ok(false) => rebuild = true,
                Err(_) => rebuild = true,
            }
        } else {
            rebuild = true;
        }
    }
    if rebuild {
        if let Some(parent) = oci_dir.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                sandbox_error(
                    "PX903",
                    "failed to prepare sandbox image directory",
                    json!({ "path": parent.display().to_string(), "error": err.to_string() }),
                )
            })?;
        }
        fs::create_dir_all(&blobs).map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to prepare sandbox image directory",
                json!({ "path": blobs.display().to_string(), "error": err.to_string() }),
            )
        })?;
        let mut layers = Vec::new();
        let base_layer = write_base_os_layer(&backend, &blobs)?;
        artifacts.manifest.base_layer_digest = Some(base_layer.digest.clone());
        layers.push(base_layer);
        if let Some(system_layer) =
            write_system_deps_layer(&backend, &artifacts.definition.system_deps, &blobs)?
        {
            artifacts.manifest.system_layer_digest = Some(system_layer.digest.clone());
            layers.push(system_layer);
        } else {
            artifacts.manifest.system_layer_digest = None;
        }
        let env_layer = write_env_layer_tar(&artifacts.env_root, runtime_root.as_deref(), &blobs)?;
        layers.push(env_layer.clone());
        let mapped = map_allowed_paths_for_image(allowed_paths, project_root, &artifacts.env_root);
        let pythonpath = join_paths_env(&mapped)?;
        let built = build_oci_image(
            artifacts,
            &oci_dir,
            layers,
            Some(tag),
            Path::new("/app"),
            Some(&pythonpath),
        )?;
        artifacts.manifest.image_digest = format!("sha256:{}", built.manifest_digest);
        artifacts.manifest.env_layer_digest = Some(env_layer.digest);
        artifacts.manifest.created_at = sandbox_timestamp_string();
        artifacts.manifest.px_version = PX_VERSION.to_string();
        persist_manifest(store, &artifacts.manifest)?;
    }
    Ok(())
}
