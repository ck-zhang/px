use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::json;

use super::app_bundle::{bundle_identity, read_pxapp_bundle};
use super::pack::{export_output, sha256_hex};
use super::runner::{detect_container_backend, run_container, SandboxImageLayout};
use super::{default_store_root, sandbox_error, SandboxDefinition, SandboxStore};
use crate::core::runtime::outcome_from_output;
use crate::core::sandbox::runner::{ContainerRunArgs, RunMode};
use crate::core::sandbox::PackTarget;
use crate::{CommandContext, ExecutionOutcome, InstallUserError};

pub(crate) fn run_pxapp_bundle(
    _ctx: &CommandContext,
    bundle_path: &Path,
    args: &[String],
    interactive: bool,
) -> Result<ExecutionOutcome> {
    if !bundle_path.exists() {
        return Ok(ExecutionOutcome::user_error(
            "pxapp bundle not found",
            json!({
                "code": "PX903",
                "reason": "missing_bundle",
                "path": bundle_path.display().to_string(),
            }),
        ));
    }
    let bundle = match read_pxapp_bundle(bundle_path) {
        Ok(bundle) => bundle,
        Err(err) => return Ok(ExecutionOutcome::user_error(err.message, err.details)),
    };
    if let Err(err) = validate_bundle_identity(&bundle) {
        return Ok(ExecutionOutcome::user_error(err.message, err.details));
    }
    let store_root = match default_store_root() {
        Ok(root) => root,
        Err(err) => {
            return Ok(ExecutionOutcome::failure(
                "failed to resolve sandbox store",
                json!({ "error": err.to_string(), "code": "PX903" }),
            ))
        }
    };
    let store = SandboxStore::new(store_root);
    let bundle_id = bundle_identity(&bundle);
    let tag = format!("px.bundle/{}/{}", bundle.metadata.sbx_id, bundle_id);
    let layout = match ensure_bundle_image(&bundle, &store, &bundle_id, &tag) {
        Ok(layout) => layout,
        Err(err) => return Ok(ExecutionOutcome::user_error(err.message, err.details)),
    };
    let backend = match detect_container_backend() {
        Ok(backend) => backend,
        Err(err) => return Ok(ExecutionOutcome::user_error(err.message, err.details)),
    };
    let entry = bundle.metadata.entrypoint.clone();
    let program = entry
        .first()
        .cloned()
        .unwrap_or_else(|| "python".to_string());
    let mut argv: Vec<String> = entry.into_iter().skip(1).collect();
    argv.extend_from_slice(args);
    let mut env = Vec::new();
    for key in ["HOME", "TERM"] {
        if let Ok(value) = std::env::var(key) {
            if !value.is_empty() {
                env.push((key.to_string(), value));
            }
        }
    }
    env.push(("PX_SANDBOX_ID".to_string(), bundle.metadata.sbx_id.clone()));
    env.push(("PX_SANDBOX_BUNDLE".to_string(), "1".to_string()));
    let workdir = PathBuf::from(&bundle.metadata.workdir);
    let opts = ContainerRunArgs {
        env,
        mounts: Vec::new(),
        workdir,
        program,
        args: argv,
    };
    let mode = if interactive {
        RunMode::Passthrough
    } else {
        RunMode::Capture
    };
    let output = match run_container(&backend, &layout, &opts, mode) {
        Ok(out) => out,
        Err(err) => return Ok(ExecutionOutcome::user_error(err.message, err.details)),
    };
    let mut details = json!({
        "mode": "pxapp",
        "bundle": bundle_path.display().to_string(),
        "sbx_id": bundle.metadata.sbx_id,
        "base_os_oid": bundle.metadata.base_os_oid,
        "profile_oid": bundle.metadata.profile_oid,
        "capabilities": bundle.metadata.capabilities,
        "entrypoint": bundle.metadata.entrypoint,
        "workdir": bundle.metadata.workdir,
        "tag": tag,
        "created_at": bundle.metadata.created_at,
    });
    if let Some(map) = details.as_object_mut() {
        map.insert("target".into(), json!(PackTarget::App));
    }
    let outcome = outcome_from_output(
        "run",
        &bundle_path.display().to_string(),
        &output,
        "px run",
        Some(details),
    );
    Ok(outcome)
}

fn validate_bundle_identity(
    bundle: &super::app_bundle::PxAppBundle,
) -> Result<(), InstallUserError> {
    let definition = SandboxDefinition {
        base_os_oid: bundle.metadata.base_os_oid.clone(),
        capabilities: bundle.metadata.capabilities.clone(),
        system_deps: bundle.metadata.system_deps.clone(),
        profile_oid: bundle.metadata.profile_oid.clone(),
        sbx_version: bundle.metadata.sbx_version,
    };
    if bundle.metadata.entrypoint.is_empty() {
        return Err(sandbox_error(
            "PX903",
            "pxapp bundle is missing an entrypoint",
            json!({ "reason": "missing_entrypoint" }),
        ));
    }
    let computed = definition.sbx_id();
    if computed != bundle.metadata.sbx_id {
        return Err(sandbox_error(
            "PX904",
            "pxapp bundle sandbox identity mismatch",
            json!({
                "expected": computed,
                "found": bundle.metadata.sbx_id,
            }),
        ));
    }
    let config: serde_json::Value =
        serde_json::from_slice(&bundle.config_bytes).map_err(|err| {
            sandbox_error(
                "PX904",
                "pxapp bundle config is invalid",
                json!({ "error": err.to_string() }),
            )
        })?;
    let sbx_from_config = config
        .get("px")
        .and_then(|px| px.get("sbx_id"))
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if sbx_from_config != bundle.metadata.sbx_id {
        return Err(sandbox_error(
            "PX904",
            "pxapp bundle config does not match metadata",
            json!({
                "expected": bundle.metadata.sbx_id,
                "found": sbx_from_config,
            }),
        ));
    }
    Ok(())
}

fn ensure_bundle_image(
    bundle: &super::app_bundle::PxAppBundle,
    store: &SandboxStore,
    bundle_id: &str,
    tag: &str,
) -> Result<SandboxImageLayout, InstallUserError> {
    let bundle_root = store.bundle_dir(&bundle.metadata.sbx_id, bundle_id);
    let oci_dir = bundle_root.join("oci");
    let blobs = oci_dir.join("blobs").join("sha256");
    let archive = bundle_root.join("image.tar");
    let manifest_digest = sha256_hex(&bundle.manifest_bytes);
    let config_digest = sha256_hex(&bundle.config_bytes);
    let mut needs_rebuild = !(manifest_path_exists(&blobs, &manifest_digest)
        && manifest_path_exists(&blobs, &config_digest)
        && archive.exists());
    if !needs_rebuild {
        for digest in &bundle.metadata.layer_digests {
            if !manifest_path_exists(&blobs, digest) {
                needs_rebuild = true;
                break;
            }
        }
    }
    if needs_rebuild {
        if oci_dir.exists() {
            let _ = fs::remove_dir_all(&oci_dir);
        }
        fs::create_dir_all(&blobs).map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to prepare pxapp image directory",
                json!({ "path": blobs.display().to_string(), "error": err.to_string() }),
            )
        })?;
        for layer in &bundle.layers {
            let path = blobs.join(&layer.digest);
            if !path.exists() {
                fs::write(&path, &layer.bytes).map_err(|err| {
                    sandbox_error(
                        "PX903",
                        "failed to write pxapp layer",
                        json!({ "path": path.display().to_string(), "error": err.to_string() }),
                    )
                })?;
            }
        }
        let config_path = blobs.join(&config_digest);
        fs::write(&config_path, &bundle.config_bytes).map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to write pxapp config",
                json!({ "path": config_path.display().to_string(), "error": err.to_string() }),
            )
        })?;
        let manifest_path = blobs.join(&manifest_digest);
        fs::write(&manifest_path, &bundle.manifest_bytes).map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to write pxapp manifest",
                json!({ "path": manifest_path.display().to_string(), "error": err.to_string() }),
            )
        })?;
        let mut annotations = json!({
            "px.sbx_id": bundle.metadata.sbx_id,
            "px.bundle.created_at": bundle.metadata.created_at,
        });
        annotations["org.opencontainers.image.ref.name"] = json!(tag);
        let index = json!({
            "schemaVersion": 2,
            "manifests": [
                {
                    "mediaType": "application/vnd.oci.image.manifest.v1+json",
                    "digest": format!("sha256:{manifest_digest}"),
                    "size": bundle.manifest_bytes.len(),
                    "annotations": annotations,
                }
            ],
        });
        fs::create_dir_all(oci_dir.parent().unwrap_or(&bundle_root)).ok();
        fs::write(
            oci_dir.join("index.json"),
            serde_json::to_vec_pretty(&index).map_err(|err| {
                sandbox_error(
                    "PX903",
                    "failed to encode pxapp index",
                    json!({ "error": err.to_string() }),
                )
            })?,
        )
        .map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to write pxapp index",
                json!({ "path": oci_dir.join("index.json").display().to_string(), "error": err.to_string() }),
            )
        })?;
        fs::write(oci_dir.join("oci-layout"), b"{\"imageLayoutVersion\":\"1.0.0\"}").map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to write pxapp layout file",
                json!({ "path": oci_dir.join("oci-layout").display().to_string(), "error": err.to_string() }),
            )
        })?;
        export_output(&oci_dir, &archive, Path::new("/"))?;
    }
    Ok(SandboxImageLayout {
        oci_dir,
        archive,
        tag: tag.to_string(),
        image_digest: format!("sha256:{manifest_digest}"),
    })
}

fn manifest_path_exists(blobs: &Path, digest: &str) -> bool {
    blobs.join(digest).exists()
}
