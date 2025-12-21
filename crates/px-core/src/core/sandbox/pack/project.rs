use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::json;

use super::super::app_bundle::{write_pxapp_bundle, PxAppMetadata};
use super::super::{
    default_store_root, discover_site_packages, ensure_sandbox_image, env_root_from_site_packages,
    sandbox_error, sandbox_image_tag, sandbox_timestamp_string, SandboxStore,
};
use super::{base_image, defaults, entrypoint, format_capabilities, layers, oci};
use super::{build_oci_image, export_output, load_layer_from_blobs, PackRequest, PackTarget};
use crate::core::runtime::build_pythonpath;
use crate::core::runtime::{load_project_state, ManifestSnapshot};
use crate::project::evaluate_project_state;
use crate::{CommandContext, ExecutionOutcome, PX_VERSION};
use px_domain::api::{load_lockfile_optional, sandbox_config_from_manifest};

pub(super) fn pack_project(
    ctx: &CommandContext,
    request: &PackRequest,
    snapshot: &ManifestSnapshot,
) -> Result<ExecutionOutcome> {
    let pack_label = match request.target {
        PackTarget::Image => "px pack image",
        PackTarget::App => "px pack app",
    };
    let state_report = match evaluate_project_state(ctx, snapshot) {
        Ok(report) => report,
        Err(err) => {
            return Ok(ExecutionOutcome::failure(
                "failed to evaluate project state",
                json!({ "error": err.to_string() }),
            ))
        }
    };
    if !state_report.is_consistent() {
        return Ok(ExecutionOutcome::user_error(
            format!("{pack_label} requires a clean environment"),
            json!({
                "code": "PX201",
                "reason": "env_outdated",
                "hint": "run `px sync` before packing",
                "state": state_report.canonical.as_str(),
            }),
        ));
    }

    if let Ok(Some(changes)) = ctx.git().worktree_changes(&snapshot.root) {
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

    let state = match load_project_state(ctx.fs(), &snapshot.root) {
        Ok(state) => state,
        Err(err) => {
            return Ok(ExecutionOutcome::failure(
                "failed to read project state",
                json!({ "error": err.to_string(), "code": "PX903" }),
            ))
        }
    };
    let env = match state.current_env {
        Some(env) => env,
        None => {
            return Ok(ExecutionOutcome::user_error(
                "project environment missing",
                json!({
                    "code": "PX902",
                    "reason": "missing_env",
                    "hint": "run `px sync` before packing",
                }),
            ))
        }
    };
    let profile_oid = env
        .profile_oid
        .as_deref()
        .unwrap_or(&env.id)
        .trim()
        .to_string();
    if profile_oid.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            "project environment profile is missing",
            json!({ "code": "PX904", "reason": "missing_profile_oid" }),
        ));
    }
    let site_packages = if env.site_packages.trim().is_empty() {
        None
    } else {
        Some(PathBuf::from(&env.site_packages))
    };
    let env_root = env
        .env_path
        .as_ref()
        .filter(|path| !path.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| {
        site_packages
            .as_ref()
            .and_then(|site| env_root_from_site_packages(site))
    });
    let env_root = match env_root {
        Some(path) => path,
        None => {
            return Ok(ExecutionOutcome::user_error(
                "project environment missing",
                json!({
                    "code": "PX902",
                    "reason": "missing_env",
                    "hint": "run `px sync` before packing",
                }),
            ))
        }
    };
    let site_packages = match site_packages {
        Some(path) => Some(path),
        None => discover_site_packages(&env_root),
    };
    let lock = load_lockfile_optional(&snapshot.lock_path)?;
    let Some(lock) = lock.as_ref() else {
        return Ok(ExecutionOutcome::user_error(
            "px.lock not found",
            json!({ "code": "PX900", "reason": "missing_lock" }),
        ));
    };
    let config = match sandbox_config_from_manifest(&snapshot.manifest_path) {
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
        None,
        &profile_oid,
        &env_root,
        site_packages.as_deref(),
    ) {
        Ok(artifacts) => artifacts,
        Err(err) => return Ok(ExecutionOutcome::user_error(err.message, err.details)),
    };
    let site_for_paths = site_packages
        .as_ref()
        .cloned()
        .unwrap_or_else(|| env_root.clone());
    let paths = match build_pythonpath(ctx.fs(), &snapshot.root, Some(site_for_paths)) {
        Ok(info) => info,
        Err(err) => {
            return Ok(ExecutionOutcome::failure(
                "failed to assemble PYTHONPATH for sandbox pack",
                json!({ "error": err.to_string() }),
            ))
        }
    };
    let base_tag = sandbox_image_tag(&artifacts.definition.sbx_id());
    if let Err(err) = base_image::ensure_base_image(
        &mut artifacts,
        &store,
        &snapshot.root,
        &paths.allowed_paths,
        &base_tag,
    ) {
        return Ok(ExecutionOutcome::user_error(err.message, err.details));
    }
    let tag = if matches!(request.target, PackTarget::Image) {
        request.tag.clone().or_else(|| {
            Some(defaults::default_tag(
                &snapshot.name,
                &snapshot.manifest_path,
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
    let env_digest = match artifacts.manifest.env_layer_digest.clone() {
        Some(digest) => digest,
        None => {
            return Ok(ExecutionOutcome::failure(
                "sandbox image metadata missing environment layer",
                json!({ "code": "PX904" }),
            ))
        }
    };
    let env_layer = load_layer_from_blobs(&base_blobs, &env_digest)?;
    let mut layers = Vec::new();
    if let Some(sys_digest) = artifacts.manifest.system_layer_digest.clone() {
        let sys_layer = load_layer_from_blobs(&base_blobs, &sys_digest)?;
        layers.push(sys_layer);
    }
    layers.push(env_layer);
    let mapped_paths = base_image::map_allowed_paths_for_image(
        &paths.allowed_paths,
        &snapshot.root,
        &artifacts.env_root,
    );
    let pythonpath = base_image::join_paths_env(&mapped_paths)?;
    let app_layer = layers::write_app_layer_tar(&snapshot.root, &pack_blobs)?;
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
                export_output(&pack_root, out, &snapshot.root)?;
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
            let entrypoint =
                match entrypoint::resolve_entrypoint(request, &snapshot.root, &snapshot.name) {
                    Ok(ep) => ep,
                    Err(err) => return Ok(ExecutionOutcome::user_error(err.message, err.details)),
                };
            let workdir = request
                .workdir
                .clone()
                .unwrap_or_else(|| PathBuf::from("/app"));
            let out_path = request.out.clone().unwrap_or_else(|| {
                defaults::default_pxapp_path(
                    &snapshot.root,
                    &snapshot.name,
                    &snapshot.manifest_path,
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
