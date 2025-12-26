use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::json;

use super::super::app_bundle::{write_pxapp_bundle, PxAppMetadata};
use super::super::{
    default_store_root, ensure_sandbox_image, env_root_from_site_packages, sandbox_error,
    sandbox_image_tag, sandbox_timestamp_string, SandboxStore,
};
use super::{base_image, defaults, entrypoint, format_capabilities, layers, oci};
use super::{build_oci_image, export_output, load_layer_from_blobs, PackRequest, PackTarget};
use crate::{CommandContext, ExecutionOutcome, PX_VERSION};
use px_domain::api::{load_lockfile_optional, sandbox_config_from_manifest};

pub(super) fn pack_workspace_member(
    ctx: &CommandContext,
    request: &PackRequest,
    ws_ctx: crate::workspace::WorkspaceRunContext,
) -> Result<ExecutionOutcome> {
    let profile_oid = ws_ctx
        .profile_oid
        .clone()
        .unwrap_or_default()
        .trim()
        .to_string();
    if profile_oid.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            "workspace environment missing",
            json!({
                "code": "PX902",
                "reason": "missing_env",
                "hint": "run `px sync` at the workspace root before packing",
            }),
        ));
    }
    if let Ok(Some(changes)) = ctx.git().worktree_changes(&ws_ctx.py_ctx.project_root) {
        if !changes.is_empty() && !request.allow_dirty {
            return Ok(ExecutionOutcome::user_error(
                "working tree has uncommitted changes",
                json!({
                    "code": "PX903",
                    "reason": "dirty_worktree",
                    "hint": "commit changes or re-run with --allow-dirty",
                    "changes": changes,
                }),
            ));
        }
    }
    let lock = load_lockfile_optional(&ws_ctx.lock_path)?;
    let Some(lock) = lock.as_ref() else {
        return Ok(ExecutionOutcome::user_error(
            "workspace lockfile missing",
            json!({ "code": "PX900", "reason": "missing_lock" }),
        ));
    };
    let env_root = match env_root_from_site_packages(&ws_ctx.site_packages) {
        Some(root) => root,
        None => {
            return Ok(ExecutionOutcome::user_error(
                "workspace environment missing",
                json!({
                    "code": "PX902",
                    "reason": "missing_env",
                    "hint": "run `px sync` at the workspace root before packing",
                }),
            ))
        }
    };
    let config = match sandbox_config_from_manifest(&ws_ctx.workspace_manifest) {
        Ok(cfg) => cfg,
        Err(err) => {
            return Ok(ExecutionOutcome::failure(
                "failed to read sandbox configuration",
                json!({ "error": err.to_string() }),
            ))
        }
    };
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
    let mut artifacts = match ensure_sandbox_image(
        &store,
        &config,
        Some(lock),
        lock.workspace.as_ref(),
        &profile_oid,
        &env_root,
        Some(&ws_ctx.site_packages),
    ) {
        Ok(artifacts) => artifacts,
        Err(err) => return Ok(ExecutionOutcome::user_error(err.message, err.details)),
    };
    let base_tag = sandbox_image_tag(&artifacts.definition.sbx_id());
    if let Err(err) = base_image::ensure_base_image(
        &mut artifacts,
        &store,
        &ws_ctx.py_ctx.project_root,
        &ws_ctx.py_ctx.allowed_paths,
        &base_tag,
    ) {
        return Ok(ExecutionOutcome::user_error(err.message, err.details));
    }
    let tag = if matches!(request.target, PackTarget::Image) {
        request.tag.clone().or_else(|| {
            Some(defaults::default_tag(
                &ws_ctx.py_ctx.project_name,
                &ws_ctx.manifest_path,
                &profile_oid,
            ))
        })
    } else {
        None
    };
    let pack_root = store.pack_dir(&artifacts.definition.sbx_id());
    if pack_root.exists() {
        fs::remove_dir_all(&pack_root).map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to prepare sandbox pack directory",
                json!({
                    "path": pack_root.display().to_string(),
                    "error": err.to_string(),
                }),
            )
        })?;
    }
    let pack_blobs = pack_root.join("blobs").join("sha256");
    let base_blobs = store
        .oci_dir(&artifacts.definition.sbx_id())
        .join("blobs")
        .join("sha256");
    let base_digest = match artifacts.manifest.base_layer_digest.clone() {
        Some(digest) => digest,
        None => {
            return Ok(ExecutionOutcome::failure(
                "sandbox image metadata missing base layer",
                json!({ "code": "PX904" }),
            ))
        }
    };
    let env_digest = match artifacts.manifest.env_layer_digest.clone() {
        Some(digest) => digest,
        None => {
            return Ok(ExecutionOutcome::failure(
                "sandbox image metadata missing environment layer",
                json!({ "code": "PX904" }),
            ))
        }
    };
    let base_layer = load_layer_from_blobs(&base_blobs, &base_digest)?;
    let env_layer = load_layer_from_blobs(&base_blobs, &env_digest)?;
    let mut layers = Vec::new();
    layers.push(base_layer);
    if let Some(sys_digest) = artifacts.manifest.system_layer_digest.clone() {
        let sys_layer = load_layer_from_blobs(&base_blobs, &sys_digest)?;
        layers.push(sys_layer);
    }
    layers.push(env_layer);
    let mapped_paths = base_image::map_allowed_paths_for_image(
        &ws_ctx.py_ctx.allowed_paths,
        &ws_ctx.py_ctx.project_root,
        &artifacts.env_root,
    );
    let pythonpath = base_image::join_paths_env(&mapped_paths)?;
    let app_layer = layers::write_app_layer_tar(&ws_ctx.py_ctx.project_root, &pack_blobs)?;
    layers.push(app_layer);
    let built = build_oci_image(
        &artifacts,
        &pack_root,
        layers,
        tag.as_deref(),
        Path::new("/app"),
        Some(&pythonpath),
    )?;
    match request.target {
        PackTarget::Image => {
            if let Some(out) = &request.out {
                export_output(&pack_root, out, &ws_ctx.py_ctx.project_root)?;
            }
            let mut details = json!({
                "sbx_id": artifacts.definition.sbx_id(),
                "base": artifacts.base.name,
                "capabilities": artifacts.definition.capabilities,
                "system_deps": artifacts.definition.system_deps.clone(),
                "profile_oid": artifacts.definition.profile_oid,
                "image_digest": format!("sha256:{}", built.manifest_digest),
                "store": pack_root.display().to_string(),
                "allow_dirty": request.allow_dirty,
                "pushed": false,
                "workspace_root": ws_ctx.workspace_root.display().to_string(),
            });
            if let Some(tag) = &tag {
                details["tag"] = json!(tag);
            }
            if let Some(out) = &request.out {
                details["out"] = json!(out.display().to_string());
            }
            if request.push {
                if let Some(tag_ref) = tag.as_deref() {
                    match oci::push_oci_image(tag_ref, &built) {
                        Ok(_) => {
                            details["pushed"] = json!(true);
                        }
                        Err(err) => {
                            return Ok(ExecutionOutcome::failure(
                                "failed to push sandbox image",
                                err.details,
                            ))
                        }
                    }
                } else {
                    return Ok(ExecutionOutcome::user_error(
                        "image tag is required to push",
                        json!({ "code": "PX903", "reason": "missing_tag" }),
                    ));
                }
            }
            let message = format!(
                "px pack image: sbx_id={} (base={}, capabilities={})",
                artifacts.definition.sbx_id(),
                artifacts.base.name,
                format_capabilities(&artifacts),
            );
            Ok(ExecutionOutcome::success(message, details))
        }
        PackTarget::App => {
            let entrypoint = match entrypoint::resolve_entrypoint(
                request,
                &ws_ctx.py_ctx.project_root,
                &ws_ctx.py_ctx.project_name,
            ) {
                Ok(ep) => ep,
                Err(err) => return Ok(ExecutionOutcome::user_error(err.message, err.details)),
            };
            let workdir = request
                .workdir
                .clone()
                .unwrap_or_else(|| PathBuf::from("/app"));
            let out_path = request.out.clone().unwrap_or_else(|| {
                defaults::default_pxapp_path(
                    &ws_ctx.py_ctx.project_root,
                    &ws_ctx.py_ctx.project_name,
                    &ws_ctx.manifest_path,
                    &profile_oid,
                )
            });
            let metadata = PxAppMetadata {
                format_version: super::super::PXAPP_VERSION,
                sbx_version: artifacts.definition.sbx_version,
                sbx_id: artifacts.definition.sbx_id(),
                base_os_oid: artifacts.base.base_os_oid.clone(),
                profile_oid: artifacts.definition.profile_oid.clone(),
                capabilities: artifacts.definition.capabilities.clone(),
                system_deps: artifacts.definition.system_deps.clone(),
                entrypoint: entrypoint.clone(),
                workdir: workdir.display().to_string(),
                manifest_digest: String::new(),
                config_digest: String::new(),
                layer_digests: vec![],
                created_at: sandbox_timestamp_string(),
                px_version: PX_VERSION.to_string(),
            };
            write_pxapp_bundle(
                &out_path,
                metadata,
                &built.manifest_bytes,
                &built.config_bytes,
                &built.layers,
            )?;
            let mut details = json!({
                "sbx_id": artifacts.definition.sbx_id(),
                "base": artifacts.base.name,
                "capabilities": artifacts.definition.capabilities,
                "system_deps": artifacts.definition.system_deps.clone(),
                "profile_oid": artifacts.definition.profile_oid,
                "bundle": out_path.display().to_string(),
                "allow_dirty": request.allow_dirty,
                "entrypoint": entrypoint,
                "workdir": workdir.display().to_string(),
                "store": pack_root.display().to_string(),
                "workspace_root": ws_ctx.workspace_root.display().to_string(),
            });
            if let Some(tag) = &tag {
                details["tag"] = json!(tag);
            }
            let message = format!(
                "px pack app: sbx_id={} (base={}, capabilities={})",
                artifacts.definition.sbx_id(),
                artifacts.base.name,
                format_capabilities(&artifacts),
            );
            Ok(ExecutionOutcome::success(message, details))
        }
    }
}
