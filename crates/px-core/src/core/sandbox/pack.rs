use std::collections::{BTreeSet, HashSet};
use std::env;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;
use ignore::WalkBuilder;
use oci_distribution::client::{Client, ClientConfig, Config as OciConfig, ImageLayer};
use oci_distribution::manifest::OciImageManifest;
use oci_distribution::secrets::RegistryAuth;
use oci_distribution::Reference;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use tar::{Archive, Builder, EntryType, Header, HeaderMode};
use tempfile::NamedTempFile;
use tokio::runtime::Runtime;
use toml_edit::DocumentMut;
use walkdir::WalkDir;

use super::app_bundle::{write_pxapp_bundle, PxAppMetadata};
use super::runner::{BackendKind, ContainerBackend};
use super::system_deps_mode;
use super::SystemDepsMode;
use super::{
    default_store_root, detect_container_backend, discover_site_packages, ensure_sandbox_image,
    env_root_from_site_packages, sandbox_error, sandbox_image_tag, sandbox_timestamp_string,
    SandboxArtifacts, SandboxImageManifest, SandboxStore,
};
use crate::core::runtime::build_pythonpath;
use crate::core::runtime::{load_project_state, ManifestSnapshot};
use crate::core::system_deps::SystemDeps;
use crate::project::evaluate_project_state;
use crate::workspace::prepare_workspace_run_context;
use crate::{
    is_missing_project_error, manifest_snapshot, missing_project_outcome, CommandContext,
    ExecutionOutcome, InstallUserError, PX_VERSION,
};
use px_domain::{load_lockfile_optional, sandbox_config_from_manifest};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PackTarget {
    Image,
    App,
}

#[derive(Clone, Debug)]
pub struct PackRequest {
    pub target: PackTarget,
    pub tag: Option<String>,
    pub out: Option<PathBuf>,
    pub push: bool,
    pub allow_dirty: bool,
    pub entrypoint: Option<Vec<String>>,
    pub workdir: Option<PathBuf>,
}

pub fn pack_image(ctx: &CommandContext, request: &PackRequest) -> Result<ExecutionOutcome> {
    let mut request = request.clone();
    request.target = PackTarget::Image;
    pack(ctx, &request)
}

pub fn pack_app(ctx: &CommandContext, request: &PackRequest) -> Result<ExecutionOutcome> {
    let mut request = request.clone();
    request.target = PackTarget::App;
    pack(ctx, &request)
}

fn pack(ctx: &CommandContext, request: &PackRequest) -> Result<ExecutionOutcome> {
    if matches!(request.target, PackTarget::App) && request.push {
        return Ok(ExecutionOutcome::user_error(
            "px pack app does not support --push",
            json!({
                "code": "PX903",
                "reason": "push_not_supported",
            }),
        ));
    }
    if let Some(ws_ctx) = match prepare_workspace_run_context(ctx, true, "pack", false) {
        Ok(value) => value,
        Err(outcome) => return Ok(outcome),
    } {
        return pack_workspace_member(ctx, request, ws_ctx);
    }

    let snapshot = match manifest_snapshot() {
        Ok(snapshot) => snapshot,
        Err(err) => {
            if is_missing_project_error(&err) {
                return Ok(missing_project_outcome());
            }
            let msg = err.to_string();
            if msg.contains("pyproject.toml not found") {
                let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                return Ok(crate::tooling::missing_pyproject_outcome("pack", &root));
            }
            return Err(err);
        }
    };
    pack_project(ctx, request, &snapshot)
}

fn pack_project(
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
    let env_root = env.env_path.as_ref().map(PathBuf::from).or_else(|| {
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
    if let Err(err) = ensure_base_image(
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
            Some(default_tag(
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
    let mapped_paths =
        map_allowed_paths_for_image(&paths.allowed_paths, &snapshot.root, &artifacts.env_root);
    let pythonpath = join_paths_env(&mapped_paths)?;
    let app_layer = write_app_layer_tar(&snapshot.root, &pack_blobs)?;
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
                    match push_oci_image(tag_ref, &built) {
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
            let entrypoint = match resolve_entrypoint(request, &snapshot.root, &snapshot.name) {
                Ok(ep) => ep,
                Err(err) => return Ok(ExecutionOutcome::user_error(err.message, err.details)),
            };
            let workdir = request
                .workdir
                .clone()
                .unwrap_or_else(|| PathBuf::from("/app"));
            let out_path = request.out.clone().unwrap_or_else(|| {
                default_pxapp_path(
                    &snapshot.root,
                    &snapshot.name,
                    &snapshot.manifest_path,
                    &profile_oid,
                )
            });
            let metadata = PxAppMetadata {
                format_version: super::PXAPP_VERSION,
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

fn pack_workspace_member(
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
    if let Err(err) = ensure_base_image(
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
            Some(default_tag(
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
    let mapped_paths = map_allowed_paths_for_image(
        &ws_ctx.py_ctx.allowed_paths,
        &ws_ctx.py_ctx.project_root,
        &artifacts.env_root,
    );
    let pythonpath = join_paths_env(&mapped_paths)?;
    let app_layer = write_app_layer_tar(&ws_ctx.py_ctx.project_root, &pack_blobs)?;
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
                    match push_oci_image(tag_ref, &built) {
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
            let entrypoint = match resolve_entrypoint(
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
                default_pxapp_path(
                    &ws_ctx.py_ctx.project_root,
                    &ws_ctx.py_ctx.project_name,
                    &ws_ctx.manifest_path,
                    &profile_oid,
                )
            });
            let metadata = PxAppMetadata {
                format_version: super::PXAPP_VERSION,
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

fn default_tag(project_name: &str, manifest_path: &Path, profile_oid: &str) -> String {
    let name = sanitize_component(project_name);
    let version = project_version(manifest_path).unwrap_or_else(|| profile_fallback(profile_oid));
    let tag = sanitize_component(&version);
    format!("px.local/{name}:{tag}")
}

fn sanitize_component(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || ch == '.' || ch == '-' || ch == '_' {
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "px".to_string()
    } else {
        trimmed.to_string()
    }
}

fn project_version(manifest_path: &Path) -> Option<String> {
    let contents = fs::read_to_string(manifest_path).ok()?;
    let doc: DocumentMut = contents.parse().ok()?;
    let project = doc.get("project")?.as_table()?;
    let version = project.get("version")?.as_str()?.trim().to_string();
    if version.is_empty() {
        None
    } else {
        Some(version)
    }
}

fn profile_fallback(profile_oid: &str) -> String {
    let mut cleaned = sanitize_component(profile_oid);
    if cleaned.is_empty() {
        cleaned = "latest".to_string();
    }
    cleaned.chars().take(32).collect()
}

fn default_pxapp_path(
    project_root: &Path,
    project_name: &str,
    manifest_path: &Path,
    profile_oid: &str,
) -> PathBuf {
    let name = sanitize_component(project_name);
    let version = project_version(manifest_path).unwrap_or_else(|| profile_fallback(profile_oid));
    project_root
        .join("dist")
        .join(format!("{name}-{version}.pxapp"))
}

fn default_entrypoint(project_root: &Path, project_name: &str) -> Vec<String> {
    let module = sanitize_component(project_name).replace('-', "_");
    let mut entry = Vec::new();
    entry.push("python".to_string());
    if !module.is_empty() {
        let candidates = [
            project_root.join(&module),
            project_root.join("src").join(&module),
        ];
        for package_dir in candidates {
            if package_dir.join("__main__.py").exists() {
                entry.push("-m".to_string());
                entry.push(module.clone());
                return entry;
            }
            if package_dir.join("cli.py").exists() {
                entry.push("-m".to_string());
                entry.push(format!("{module}.cli"));
                return entry;
            }
        }
        entry.push("-m".to_string());
        entry.push(module);
        return entry;
    }
    entry
}

fn resolve_entrypoint(
    request: &PackRequest,
    project_root: &Path,
    project_name: &str,
) -> Result<Vec<String>, InstallUserError> {
    let raw = match &request.entrypoint {
        Some(custom) => custom.clone(),
        None => default_entrypoint(project_root, project_name),
    };
    let entry: Vec<String> = raw
        .into_iter()
        .filter(|item| !item.trim().is_empty())
        .collect();
    if entry.is_empty() {
        return Err(sandbox_error(
            "PX903",
            "px pack app requires an entrypoint command",
            json!({ "reason": "missing_entrypoint" }),
        ));
    }
    Ok(entry)
}

fn format_capabilities(artifacts: &SandboxArtifacts) -> String {
    if artifacts.definition.capabilities.is_empty() {
        "none".into()
    } else {
        artifacts
            .definition
            .capabilities
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(",")
    }
}

#[derive(Clone, Debug)]
pub(crate) struct LayerTar {
    pub(crate) digest: String,
    pub(crate) size: u64,
    pub(crate) path: PathBuf,
}

pub(crate) struct BuiltImage {
    pub(crate) manifest_digest: String,
    pub(crate) manifest_bytes: Vec<u8>,
    pub(crate) config_bytes: Vec<u8>,
    pub(crate) layers: Vec<LayerTar>,
}

pub(crate) fn build_oci_image(
    artifacts: &SandboxArtifacts,
    oci_root: &Path,
    layers: Vec<LayerTar>,
    tag: Option<&str>,
    working_dir: &Path,
    pythonpath: Option<&str>,
) -> Result<BuiltImage, InstallUserError> {
    fs::create_dir_all(oci_root).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to prepare sandbox image directory",
            json!({ "path": oci_root.display().to_string(), "error": err.to_string() }),
        )
    })?;
    let blobs = oci_root.join("blobs").join("sha256");
    fs::create_dir_all(&blobs).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to prepare OCI blob directory",
            json!({ "path": blobs.display().to_string(), "error": err.to_string() }),
        )
    })?;
    let mut image_layers = Vec::with_capacity(layers.len());
    for layer in layers {
        let dest = blobs.join(&layer.digest);
        if !dest.exists() {
            link_or_copy(&layer.path, &dest).map_err(|err| {
                sandbox_error(
                    "PX903",
                    "failed to write sandbox layer",
                    json!({
                        "path": dest.display().to_string(),
                        "error": err.to_string(),
                    }),
                )
            })?;
        }
        image_layers.push(LayerTar {
            digest: layer.digest,
            size: layer.size,
            path: dest,
        });
    }
    let diff_ids: Vec<String> = image_layers
        .iter()
        .map(|layer| format!("sha256:{}", layer.digest))
        .collect();
    let mut env_vars = Vec::new();
    env_vars.push(
        "PATH=/px/env/bin:/px/runtime/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/bin"
            .into(),
    );
    if let Some(py_path) = pythonpath {
        env_vars.push(format!("PYTHONPATH={py_path}"));
    }
    env_vars.push(format!("PX_SANDBOX_ID={}", artifacts.definition.sbx_id()));
    let config = json!({
        "created": sandbox_timestamp_string(),
        "architecture": "amd64",
        "os": "linux",
        "rootfs": {
            "type": "layers",
            "diff_ids": diff_ids,
        },
        "config": {
            "WorkingDir": working_dir.display().to_string(),
            "Env": env_vars,
        },
        "px": {
            "sbx_id": artifacts.definition.sbx_id(),
            "base": artifacts.base.name,
            "capabilities": artifacts.definition.capabilities,
            "profile_oid": artifacts.definition.profile_oid,
            "system_deps": artifacts.definition.system_deps.clone(),
        },
    });
    let config_bytes = serde_json::to_vec_pretty(&config).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to encode sandbox config",
            json!({ "error": err.to_string() }),
        )
    })?;
    let config_digest = sha256_hex(&config_bytes);
    let config_path = blobs.join(&config_digest);
    fs::write(&config_path, &config_bytes).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to write sandbox config",
            json!({ "path": config_path.display().to_string(), "error": err.to_string() }),
        )
    })?;
    let manifest_layers: Vec<_> = image_layers
        .iter()
        .map(|layer| {
            json!({
                "mediaType": "application/vnd.oci.image.layer.v1.tar",
                "digest": format!("sha256:{}", layer.digest),
                "size": layer.size,
            })
        })
        .collect();
    let manifest = json!({
        "schemaVersion": 2,
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": format!("sha256:{config_digest}"),
            "size": config_bytes.len(),
        },
        "layers": manifest_layers,
    });
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to encode OCI manifest",
            json!({ "error": err.to_string() }),
        )
    })?;
    let manifest_digest = sha256_hex(&manifest_bytes);
    let manifest_path = blobs.join(&manifest_digest);
    fs::write(&manifest_path, &manifest_bytes).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to write OCI manifest",
            json!({ "path": manifest_path.display().to_string(), "error": err.to_string() }),
        )
    })?;
    let mut annotations = json!({
        "px.sbx_id": artifacts.definition.sbx_id(),
        "px.base": artifacts.base.name,
    });
    if let Some(tag) = tag {
        annotations["org.opencontainers.image.ref.name"] = json!(tag);
    }
    let index = json!({
        "schemaVersion": 2,
        "manifests": [
            {
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "digest": format!("sha256:{manifest_digest}"),
                "size": manifest_bytes.len(),
                "annotations": annotations,
            }
        ],
    });
    fs::write(oci_root.join("index.json"), serde_json::to_vec_pretty(&index).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to encode OCI index",
            json!({ "error": err.to_string() }),
        )
    })?).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to write OCI index",
            json!({ "path": oci_root.join("index.json").display().to_string(), "error": err.to_string() }),
        )
    })?;
    fs::write(oci_root.join("oci-layout"), b"{\"imageLayoutVersion\":\"1.0.0\"}").map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to write OCI layout file",
            json!({ "path": oci_root.join("oci-layout").display().to_string(), "error": err.to_string() }),
        )
    })?;
    Ok(BuiltImage {
        manifest_digest,
        manifest_bytes,
        config_bytes,
        layers: image_layers,
    })
}

pub(crate) fn write_env_layer_tar(
    env_root: &Path,
    runtime_root: Option<&Path>,
    blobs: &Path,
) -> Result<LayerTar, InstallUserError> {
    let mut builder = layer_tar_builder(blobs)?;
    let runtime_root = runtime_root.and_then(|path| path.canonicalize().ok());
    let runtime_root_str = runtime_root
        .as_ref()
        .and_then(|p| p.to_str())
        .map(|s| s.to_string());
    let env_root_canon = env_root
        .canonicalize()
        .unwrap_or_else(|_| env_root.to_path_buf());
    let store_mapping = discover_store_mapping(&env_root_canon)?;
    let mut extra_paths = Vec::new();
    if let Some(runtime_root) = runtime_root.as_ref() {
        let runtime_python = runtime_root.join("bin").join("python");
        for lib in shared_libs(&runtime_python) {
            if lib.starts_with(runtime_root) || lib.starts_with(&env_root_canon) {
                extra_paths.push(lib);
            }
        }
        extra_paths.push(runtime_python);
        extra_paths.sort();
        extra_paths.dedup();
    }
    let mut seen = HashSet::new();
    let walker = WalkDir::new(env_root).sort_by(|a, b| a.path().cmp(b.path()));
    for entry in walker {
        let entry = entry.map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to walk environment tree for sandbox image",
                json!({ "error": err.to_string() }),
            )
        })?;
        let path = entry.path();
        if path == env_root {
            continue;
        }
        let rel = match path.strip_prefix(env_root) {
            Ok(rel) => rel,
            Err(_) => continue,
        };
        let archive_path = Path::new("px").join("env").join(rel);
        if seen.insert(archive_path.clone()) {
            let is_python_shim = runtime_root.is_some()
                && (entry.file_type().is_file() || entry.file_type().is_symlink())
                && archive_path.starts_with(Path::new("px").join("env").join("bin"))
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|name| name.starts_with("python"))
                    .unwrap_or(false);
            if is_python_shim {
                append_rewritten_python(
                    &mut builder,
                    &archive_path,
                    path,
                    env_root,
                    runtime_root_str.as_deref(),
                    store_mapping.as_ref().map(|mapping| mapping.host_root_str.as_str()),
                )?;
            } else if entry.file_type().is_file()
                && path.file_name().and_then(|n| n.to_str()) == Some("pyvenv.cfg")
            {
                append_rewritten_pyvenv(&mut builder, &archive_path, path)?;
            } else if entry.file_type().is_file()
                && path.extension().and_then(|ext| ext.to_str()) == Some("pth")
            {
                append_rewritten_pth(
                    &mut builder,
                    &archive_path,
                    path,
                    store_mapping.as_ref().map(|mapping| mapping.host_root_str.as_str()),
                )?;
            } else {
                append_path(&mut builder, &archive_path, path)?;
            }
        }
    }
    if let Some(mapping) = store_mapping {
        for pkg_root in mapping.pkg_build_roots {
            let walker = WalkDir::new(&pkg_root).sort_by(|a, b| a.path().cmp(b.path()));
            for entry in walker {
                let entry = entry.map_err(|err| {
                    sandbox_error(
                        "PX903",
                        "failed to walk package build tree for sandbox image",
                        json!({ "error": err.to_string() }),
                    )
                })?;
                let path = entry.path();
                if path == pkg_root {
                    continue;
                }
                let rel = match path.strip_prefix(&mapping.host_root) {
                    Ok(rel) => rel,
                    Err(_) => continue,
                };
                let archive_path = Path::new("px").join("store").join(rel);
                if seen.insert(archive_path.clone()) {
                    append_path(&mut builder, &archive_path, path)?;
                }
            }
        }
    }
    if let Some(runtime_root) = runtime_root {
        let walker = WalkDir::new(&runtime_root)
            .sort_by(|a, b| a.path().cmp(b.path()))
            .into_iter()
            .filter_map(Result::ok);
        for entry in walker {
            let path = entry.path();
            if path == runtime_root {
                continue;
            }
            let rel = match path.strip_prefix(&runtime_root) {
                Ok(rel) => rel,
                Err(_) => continue,
            };
            let is_python_related = rel.components().any(|comp| {
                comp.as_os_str()
                    .to_str()
                    .map(|name| name.starts_with("python") || name.starts_with("libpython"))
                    .unwrap_or(false)
            });
            if !is_python_related {
                continue;
            }
            let archive_path = Path::new("px").join("runtime").join(rel);
            if seen.insert(archive_path.clone()) {
                append_path(&mut builder, &archive_path, path)?;
            }
        }
    }
    for host_path in extra_paths {
        if !host_path.exists() {
            continue;
        }
        let rel = host_path
            .strip_prefix("/")
            .unwrap_or(&host_path)
            .to_path_buf();
        if rel.as_os_str().is_empty() {
            continue;
        }
        if rel.components().count() == 0 {
            continue;
        }
        let archive_path = Path::new("").join(rel);
        if seen.insert(archive_path.clone()) {
            append_path(&mut builder, &archive_path, &host_path)?;
        }
    }
    finalize_layer(builder, blobs)
}

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
        .arg(super::SYSTEM_DEPS_IMAGE)
        .output()
        .map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to create base sandbox container",
                json!({ "error": err.to_string(), "image": super::SYSTEM_DEPS_IMAGE }),
            )
        })?;
    if !create.status.success() {
        return Err(sandbox_error(
            "PX903",
            "failed to create base sandbox container",
            json!({
                "image": super::SYSTEM_DEPS_IMAGE,
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
                "image": super::SYSTEM_DEPS_IMAGE,
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
            json!({ "error": err.to_string(), "image": super::SYSTEM_DEPS_IMAGE }),
        )
    })?;
    if !export.status.success() {
        return Err(sandbox_error(
            "PX903",
            "failed to export base sandbox filesystem",
            json!({
                "image": super::SYSTEM_DEPS_IMAGE,
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

fn shared_libs(binary: &Path) -> Vec<PathBuf> {
    let mut libs = Vec::new();
    if !binary.exists() {
        return libs;
    }
    let Ok(output) = Command::new("ldd").arg(binary).output() else {
        return libs;
    };
    if !output.status.success() {
        return libs;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("linux-vdso") {
            continue;
        }
        let parts: Vec<_> = trimmed.split_whitespace().collect();
        let path = if parts.len() >= 3 && parts[1] == "=>" {
            parts[2]
        } else {
            parts.first().copied().unwrap_or_default()
        };
        if path.starts_with('/') && Path::new(path).exists() {
            libs.push(PathBuf::from(path));
        }
    }
    libs
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

fn write_app_layer_tar(source_root: &Path, blobs: &Path) -> Result<LayerTar, InstallUserError> {
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

#[derive(Clone, Debug)]
struct StoreMapping {
    host_root: PathBuf,
    host_root_str: String,
    pkg_build_roots: Vec<PathBuf>,
}

fn discover_store_mapping(env_root: &Path) -> Result<Option<StoreMapping>, InstallUserError> {
    let mut store_roots = BTreeSet::<String>::new();
    let mut pkg_build_roots = Vec::<PathBuf>::new();

    let walker = WalkDir::new(env_root).sort_by(|a, b| a.path().cmp(b.path()));
    for entry in walker {
        let entry = entry.map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to inspect environment for sandbox packaging",
                json!({ "error": err.to_string() }),
            )
        })?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("pth") {
            continue;
        }
        let contents = fs::read_to_string(path).map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to read environment path file for sandbox packaging",
                json!({ "path": path.display().to_string(), "error": err.to_string() }),
            )
        })?;
        for line in contents.lines() {
            let trimmed = line.trim();
            if !trimmed.starts_with('/') {
                continue;
            }
            let Some(index) = trimmed.find("/store/pkg-builds/") else {
                continue;
            };
            let Some(root) = trimmed.get(..index + "/store".len()) else {
                continue;
            };
            store_roots.insert(root.to_string());
            pkg_build_roots.push(PathBuf::from(trimmed));
        }
    }

    if pkg_build_roots.is_empty() {
        return Ok(None);
    }
    if store_roots.len() != 1 {
        return Err(sandbox_error(
            "PX904",
            "sandbox environment references multiple package stores",
            json!({
                "reason": "multiple_store_roots",
                "stores": store_roots.into_iter().collect::<Vec<_>>(),
            }),
        ));
    }
    let root = store_roots
        .into_iter()
        .next()
        .unwrap_or_else(|| "/".to_string());
    let host_root = PathBuf::from(&root)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(&root));
    let host_root_str = host_root.to_string_lossy().to_string();

    let mut canonical_roots = Vec::new();
    for raw in pkg_build_roots {
        let canonical = raw.canonicalize().unwrap_or(raw);
        if !canonical.exists() {
            return Err(sandbox_error(
                "PX903",
                "sandbox environment references a missing package build",
                json!({
                    "reason": "missing_pkg_build",
                    "path": canonical.display().to_string(),
                }),
            ));
        }
        if !canonical.starts_with(&host_root) {
            return Err(sandbox_error(
                "PX903",
                "sandbox environment references a package build outside the store root",
                json!({
                    "reason": "pkg_build_outside_store",
                    "path": canonical.display().to_string(),
                    "store_root": host_root_str,
                }),
            ));
        }
        canonical_roots.push(canonical);
    }
    canonical_roots.sort();
    canonical_roots.dedup();

    Ok(Some(StoreMapping {
        host_root,
        host_root_str,
        pkg_build_roots: canonical_roots,
    }))
}

fn append_path<W: Write>(
    builder: &mut Builder<W>,
    archive_path: &Path,
    path: &Path,
) -> Result<(), InstallUserError> {
    let metadata = fs::symlink_metadata(path).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to read source metadata for sandbox layer",
            json!({ "path": path.display().to_string(), "error": err.to_string() }),
        )
    })?;
    if metadata.is_dir() {
        builder.append_dir(archive_path, path).map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to stage directory for sandbox layer",
                json!({ "path": path.display().to_string(), "error": err.to_string() }),
            )
        })?;
    } else if metadata.file_type().is_symlink() {
        let target = fs::read_link(path).map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to read symlink target for sandbox layer",
                json!({ "path": path.display().to_string(), "error": err.to_string() }),
            )
        })?;
        let mut header = Header::new_gnu();
        header.set_metadata_in_mode(&metadata, HeaderMode::Deterministic);
        header.set_entry_type(EntryType::Symlink);
        header.set_size(0);
        builder
            .append_link(&mut header, archive_path, &target)
            .map_err(|err| {
                sandbox_error(
                    "PX903",
                    "failed to add symlink to sandbox layer",
                    json!({ "path": path.display().to_string(), "error": err.to_string() }),
                )
            })?;
    } else {
        let mut file = File::open(path).map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to read source file for sandbox layer",
                json!({ "path": path.display().to_string(), "error": err.to_string() }),
            )
        })?;
        builder
            .append_file(archive_path, &mut file)
            .map_err(|err| {
                sandbox_error(
                    "PX903",
                    "failed to add file to sandbox layer",
                    json!({ "path": path.display().to_string(), "error": err.to_string() }),
                )
            })?;
    }
    Ok(())
}

fn append_rewritten_python(
    builder: &mut Builder<impl Write>,
    archive_path: &Path,
    path: &Path,
    env_root: &Path,
    runtime_root: Option<&str>,
    store_root: Option<&str>,
) -> Result<(), InstallUserError> {
    let runtime_root = runtime_root.unwrap_or("/px/runtime");
    let env_root = env_root
        .canonicalize()
        .unwrap_or_else(|_| env_root.to_path_buf());
    let env_root_str = env_root.to_string_lossy().to_string();
    let target_name = archive_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("python");
    let target = format!("/px/runtime/bin/{target_name}");
    if path.is_symlink() {
        let shim = "#!/bin/bash\nexec \"/px/env/bin/python\" \"$@\"\n";
        return append_bytes_deterministic(builder, archive_path, shim.as_bytes(), Some(0o755));
    }
    let contents = fs::read_to_string(path).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to read python shim for sandbox layer",
            json!({ "path": path.display().to_string(), "error": err.to_string() }),
        )
    })?;
    let mut rewritten_lines = Vec::new();
    rewritten_lines.push("#!/bin/bash".to_string());
    for line in contents.lines() {
        if line.trim_start().starts_with("#!") {
            continue;
        }
        let mut line = line.replace(&env_root_str, "/px/env");
        line = line.replace(runtime_root, "/px/runtime");
        if let Some(store_root) = store_root {
            line = line.replace(store_root, "/px/store");
        }
        if line.trim_start().starts_with("export LD_LIBRARY_PATH=") {
            if let Some(rewritten) = rewrite_ld_library_path(&line) {
                line = rewritten;
            }
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with("export PX_PYTHON") {
            line = format!(r#"export PX_PYTHON="{target}""#);
        } else if trimmed.starts_with("exec ") {
            line = format!(r#"exec "{target}" "$@""#);
        }
        rewritten_lines.push(line);
    }
    let rewritten = rewritten_lines.join("\n") + "\n";
    append_bytes_deterministic(builder, archive_path, rewritten.as_bytes(), Some(0o755))
}

fn rewrite_ld_library_path(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let rhs = trimmed.strip_prefix("export LD_LIBRARY_PATH=")?;
    let rhs = rhs.trim();
    let (quote, value) = match rhs.chars().next()? {
        '\'' => ('\'', rhs.trim_matches('\'')),
        '"' => ('"', rhs.trim_matches('"')),
        _ => ('\0', rhs),
    };
    let filtered: Vec<&str> = value
        .split(':')
        .map(|part| part.trim())
        .filter(|part| !part.is_empty())
        .filter(|part| !part.contains("/sys-libs"))
        .collect();
    let joined = filtered.join(":");
    if quote == '\0' {
        Some(format!("export LD_LIBRARY_PATH={joined}"))
    } else {
        Some(format!("export LD_LIBRARY_PATH={quote}{joined}{quote}"))
    }
}

fn append_rewritten_pth(
    builder: &mut Builder<impl Write>,
    archive_path: &Path,
    path: &Path,
    store_root: Option<&str>,
) -> Result<(), InstallUserError> {
    let contents = fs::read_to_string(path).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to read .pth file for sandbox layer",
            json!({ "path": path.display().to_string(), "error": err.to_string() }),
        )
    })?;
    let rewritten = if let Some(store_root) = store_root {
        contents.replace(store_root, "/px/store")
    } else {
        contents
    };
    let rewritten = if rewritten.ends_with('\n') {
        rewritten
    } else {
        format!("{rewritten}\n")
    };
    append_bytes_deterministic(builder, archive_path, rewritten.as_bytes(), Some(0o644))
}

fn append_rewritten_pyvenv(
    builder: &mut Builder<impl Write>,
    archive_path: &Path,
    path: &Path,
) -> Result<(), InstallUserError> {
    let contents = fs::read_to_string(path).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to read pyvenv.cfg for sandbox layer",
            json!({ "path": path.display().to_string(), "error": err.to_string() }),
        )
    })?;
    let mut lines = Vec::new();
    for line in contents.lines() {
        if line.trim_start().starts_with("home") {
            lines.push("home = /px/runtime".to_string());
        } else {
            lines.push(line.to_string());
        }
    }
    let rewritten = lines.join("\n") + "\n";
    append_bytes_deterministic(builder, archive_path, rewritten.as_bytes(), Some(0o644))
}

fn append_bytes_deterministic(
    builder: &mut Builder<impl Write>,
    archive_path: &Path,
    data: &[u8],
    mode: Option<u32>,
) -> Result<(), InstallUserError> {
    let mut header = Header::new_gnu();
    header.set_path(archive_path).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to stage sandbox entry",
            json!({ "path": archive_path.display().to_string(), "error": err.to_string() }),
        )
    })?;
    header.set_size(data.len() as u64);
    header.set_mode(mode.unwrap_or(0o644));
    header.set_mtime(0);
    header.set_uid(0);
    header.set_gid(0);
    header.set_cksum();
    builder.append(&header, data).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to write sandbox entry",
            json!({ "path": archive_path.display().to_string(), "error": err.to_string() }),
        )
    })
}

struct HashingWriter<W> {
    inner: W,
    hasher: Sha256,
    bytes_written: u64,
}

impl<W> HashingWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            bytes_written: 0,
        }
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.hasher.update(&buf[..written]);
        self.bytes_written = self
            .bytes_written
            .saturating_add(written.try_into().unwrap_or(u64::MAX));
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn layer_tar_builder(blobs: &Path) -> Result<Builder<HashingWriter<NamedTempFile>>, InstallUserError> {
    fs::create_dir_all(blobs).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to prepare layer directory",
            json!({ "path": blobs.display().to_string(), "error": err.to_string() }),
        )
    })?;
    let file = NamedTempFile::new_in(blobs).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to create sandbox layer",
            json!({ "path": blobs.display().to_string(), "error": err.to_string() }),
        )
    })?;
    Ok(Builder::new(HashingWriter::new(file)))
}

fn finalize_layer(
    builder: Builder<HashingWriter<NamedTempFile>>,
    blobs: &Path,
) -> Result<LayerTar, InstallUserError> {
    let writer = builder.into_inner().map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to finalize sandbox layer",
            json!({ "error": err.to_string() }),
        )
    })?;
    let HashingWriter {
        inner: temp,
        hasher,
        bytes_written: size,
    } = writer;
    let digest = format!("{:x}", hasher.finalize());
    let layer_path = blobs.join(&digest);
    if !layer_path.exists() {
        match temp.persist_noclobber(&layer_path) {
            Ok(_) => {}
            Err(err) => {
                if err.error.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(sandbox_error(
                        "PX903",
                        "failed to write sandbox layer",
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

fn link_or_copy(from: &Path, to: &Path) -> std::io::Result<()> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::hard_link(from, to) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(_) => {
            fs::copy(from, to)?;
            Ok(())
        }
    }
}

pub(crate) fn export_output(
    oci_root: &Path,
    out: &Path,
    cwd: &Path,
) -> Result<(), InstallUserError> {
    if out.extension().map(|ext| ext == "tar").unwrap_or(false) {
        let target = if out.is_absolute() {
            out.to_path_buf()
        } else {
            cwd.join(out)
        };
        let file = File::create(&target).map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to create OCI tarball",
                json!({ "path": target.display().to_string(), "error": err.to_string() }),
            )
        })?;
        let mut builder = Builder::new(file);
        builder.append_dir_all(".", oci_root).map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to write OCI tarball",
                json!({ "path": target.display().to_string(), "error": err.to_string() }),
            )
        })?;
        builder.into_inner().map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to finalize OCI tarball",
                json!({ "path": target.display().to_string(), "error": err.to_string() }),
            )
        })?;
        return Ok(());
    }

    let target_dir = if out.is_absolute() {
        out.to_path_buf()
    } else {
        cwd.join(out)
    };
    if target_dir.exists() {
        let _ = fs::remove_dir_all(&target_dir);
    }
    copy_dir(oci_root, &target_dir)?;
    Ok(())
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

fn map_allowed_paths_for_image(
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

fn join_paths_env(paths: &[PathBuf]) -> Result<String, InstallUserError> {
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

fn ensure_base_image(
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

pub(crate) fn load_layer_from_blobs(
    blobs: &Path,
    digest: &str,
) -> Result<LayerTar, InstallUserError> {
    let path = blobs.join(digest);
    let mut file = File::open(&path).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to read sandbox layer",
            json!({ "path": path.display().to_string(), "error": err.to_string() }),
        )
    })?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to read sandbox layer",
                json!({ "path": path.display().to_string(), "error": err.to_string() }),
            )
        })?;
        if read == 0 {
            break;
        }
        size = size.saturating_add(read as u64);
        hasher.update(&buffer[..read]);
    }
    let computed = format!("{:x}", hasher.finalize());
    if computed != digest {
        return Err(sandbox_error(
            "PX904",
            "sandbox layer digest mismatch",
            json!({
                "expected": digest,
                "computed": computed,
                "path": path.display().to_string(),
            }),
        ));
    }
    Ok(LayerTar {
        digest: computed,
        size,
        path,
    })
}

fn push_oci_image(tag: &str, built: &BuiltImage) -> Result<(), InstallUserError> {
    let reference = Reference::try_from(tag).map_err(|err| {
        sandbox_error(
            "PX903",
            "invalid image reference for push",
            json!({ "reference": tag, "error": err.to_string() }),
        )
    })?;
    let manifest: OciImageManifest =
        serde_json::from_slice(&built.manifest_bytes).map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to encode OCI manifest for push",
                json!({ "error": err.to_string() }),
            )
        })?;
    let mut client = Client::new(ClientConfig::default());
    let auth = registry_auth_from_env()?;
    let layers = built
        .layers
        .iter()
        .map(|layer| {
            let bytes = fs::read(&layer.path).map_err(|err| {
                sandbox_error(
                    "PX903",
                    "failed to read sandbox layer for push",
                    json!({ "path": layer.path.display().to_string(), "error": err.to_string() }),
                )
            })?;
            Ok(ImageLayer::new(
                bytes,
                "application/vnd.oci.image.layer.v1.tar".to_string(),
                None,
            ))
        })
        .collect::<Result<Vec<_>, InstallUserError>>()?;
    let config = OciConfig::new(
        built.config_bytes.clone(),
        "application/vnd.oci.image.config.v1+json".to_string(),
        None,
    );
    let rt = Runtime::new().map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to initialize registry client",
            json!({ "error": err.to_string() }),
        )
    })?;
    rt.block_on(async {
        client
            .push(&reference, &layers, config, &auth, Some(manifest))
            .await
    })
    .map_err(|err| {
        let message = err.to_string();
        let mut details = json!({ "reference": tag, "error": message });
        if message.contains("401") || message.contains("Unauthorized") {
            details["reason"] = json!("unauthorized");
            details["hint"] =
                json!("check PX_REGISTRY_USERNAME/PX_REGISTRY_PASSWORD or registry permissions");
        }
        sandbox_error("PX903", "failed to push sandbox image", details)
    })?;
    Ok(())
}

fn registry_auth_from_env() -> Result<RegistryAuth, InstallUserError> {
    let username = env::var("PX_REGISTRY_USERNAME").unwrap_or_default();
    let password = env::var("PX_REGISTRY_PASSWORD").unwrap_or_default();
    if username.is_empty() && password.is_empty() {
        return Ok(RegistryAuth::Anonymous);
    }
    if username.is_empty() || password.is_empty() {
        return Err(sandbox_error(
            "PX903",
            "registry credentials incomplete",
            json!({
                "code": "PX903",
                "reason": "registry_auth_missing",
                "hint": "set PX_REGISTRY_USERNAME and PX_REGISTRY_PASSWORD",
            }),
        ));
    }
    Ok(RegistryAuth::Basic(username, password))
}

fn copy_dir(from: &Path, to: &Path) -> Result<(), InstallUserError> {
    fs::create_dir_all(to).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to prepare output directory",
            json!({ "path": to.display().to_string(), "error": err.to_string() }),
        )
    })?;
    for entry in WalkDir::new(from) {
        let entry = entry.map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to walk OCI layout",
                json!({ "error": err.to_string() }),
            )
        })?;
        let path = entry.path();
        let rel = path.strip_prefix(from).unwrap_or(path);
        let dest = to.join(rel);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&dest).map_err(|err| {
                sandbox_error(
                    "PX903",
                    "failed to create output directory",
                    json!({ "path": dest.display().to_string(), "error": err.to_string() }),
                )
            })?;
        } else {
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent).map_err(|err| {
                    sandbox_error(
                        "PX903",
                        "failed to create output directory",
                        json!({ "path": parent.display().to_string(), "error": err.to_string() }),
                    )
                })?;
            }
            fs::copy(path, &dest).map_err(|err| {
                sandbox_error(
                    "PX903",
                    "failed to copy OCI file",
                    json!({ "path": dest.display().to_string(), "error": err.to_string() }),
                )
            })?;
        }
    }
    Ok(())
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

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::sandbox::{SandboxBase, SandboxDefinition, SandboxImageManifest, SBX_VERSION};
    use crate::core::system_deps::resolve_system_deps;
    use serde_json;
    use std::collections::BTreeSet;
    use std::env;
    use std::fs;
    use tar::Archive;
    use tempfile::tempdir;

    #[test]
    fn oci_builder_writes_layer_with_sources() -> Result<()> {
        let temp = tempdir()?;
        let source = temp.path().join("app");
        fs::create_dir_all(&source)?;
        fs::write(source.join("app.py"), b"print('hi')\n")?;
        let env_root = temp.path().join("env");
        fs::create_dir_all(env_root.join("bin"))?;
        fs::write(
            env_root.join("bin").join("python"),
            b"#!/usr/bin/env python\n",
        )?;
        let oci_root = temp.path().join("oci");

        let mut caps = BTreeSet::new();
        caps.insert("postgres".to_string());
        let system_deps = resolve_system_deps(&caps, None);
        let definition = SandboxDefinition {
            base_os_oid: "base".into(),
            capabilities: caps.clone(),
            system_deps: system_deps.clone(),
            profile_oid: "profile".into(),
            sbx_version: SBX_VERSION,
        };
        let artifacts = SandboxArtifacts {
            base: SandboxBase {
                name: "demo".into(),
                base_os_oid: "base".into(),
                supported_capabilities: caps.clone(),
            },
            definition: definition.clone(),
            manifest: SandboxImageManifest {
                sbx_id: definition.sbx_id(),
                base_os_oid: "base".into(),
                profile_oid: "profile".into(),
                capabilities: caps,
                system_deps,
                image_digest: String::new(),
                base_layer_digest: None,
                env_layer_digest: None,
                system_layer_digest: None,
                created_at: String::new(),
                px_version: "test".into(),
                sbx_version: SBX_VERSION,
            },
            env_root: env_root.clone(),
        };

        let blobs = oci_root.join("blobs").join("sha256");
        let env_layer = write_env_layer_tar(&env_root, None, &blobs)?;
        let app_layer = write_app_layer_tar(&source, &blobs)?;
        build_oci_image(
            &artifacts,
            &oci_root,
            vec![env_layer, app_layer],
            Some("demo:latest"),
            Path::new("/app"),
            Some("/app"),
        )?;
        let index_path = oci_root.join("index.json");
        assert!(index_path.exists(), "index.json missing");
        let index: serde_json::Value = serde_json::from_slice(&fs::read(&index_path)?)?;
        let manifest_digest = index["manifests"][0]["digest"]
            .as_str()
            .unwrap()
            .trim_start_matches("sha256:")
            .to_string();
        let manifest_path = oci_root.join("blobs").join("sha256").join(&manifest_digest);
        let manifest: serde_json::Value = serde_json::from_slice(&fs::read(&manifest_path)?)?;
        let layers = manifest["layers"].as_array().expect("layers array");
        let mut found = false;
        for layer in layers {
            let digest = layer["digest"]
                .as_str()
                .unwrap()
                .trim_start_matches("sha256:");
            let layer_path = oci_root.join("blobs").join("sha256").join(digest);
            let file = File::open(layer_path)?;
            let mut archive = Archive::new(file);
            for entry in archive.entries()? {
                let entry = entry?;
                if entry.path()? == Path::new("app/app.py") {
                    found = true;
                    break;
                }
            }
            if found {
                break;
            }
        }
        assert!(found, "app layer should include app/app.py");
        Ok(())
    }

    #[test]
    fn default_tag_uses_manifest_version_when_available() -> Result<()> {
        let temp = tempdir()?;
        let manifest = temp.path().join("pyproject.toml");
        fs::write(
            &manifest,
            r#"[project]
name = "demo"
version = "1.2.3"
requires-python = ">=3.11"
"#,
        )?;
        let tag = default_tag("demo", &manifest, "profile");
        assert_eq!(tag, "px.local/demo:1.2.3");
        Ok(())
    }

    #[test]
    fn default_tag_falls_back_to_profile_when_version_missing() {
        let temp = tempdir().unwrap();
        let manifest = temp.path().join("pyproject.toml");
        fs::write(
            &manifest,
            r#"[project]
name = "demo"
requires-python = ">=3.11"
"#,
        )
        .unwrap();
        let tag = default_tag("demo", &manifest, "profile-oid-1234567890");
        assert!(
            tag.starts_with("px.local/demo:profile-oid"),
            "fallback tag should use profile oid"
        );
    }

    #[test]
    fn registry_auth_requires_username_and_password() {
        let prev_user = env::var("PX_REGISTRY_USERNAME").ok();
        let prev_pass = env::var("PX_REGISTRY_PASSWORD").ok();
        env::set_var("PX_REGISTRY_USERNAME", "demo");
        env::remove_var("PX_REGISTRY_PASSWORD");
        let err = registry_auth_from_env().expect_err("missing password should error");
        let reason = err
            .details
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        assert_eq!(reason, "registry_auth_missing");
        if let Some(val) = prev_user {
            env::set_var("PX_REGISTRY_USERNAME", val);
        } else {
            env::remove_var("PX_REGISTRY_USERNAME");
        }
        if let Some(val) = prev_pass {
            env::set_var("PX_REGISTRY_PASSWORD", val);
        } else {
            env::remove_var("PX_REGISTRY_PASSWORD");
        }
    }
}
