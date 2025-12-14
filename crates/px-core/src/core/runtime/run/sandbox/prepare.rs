use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{json, Value};

use crate::core::runtime::facade::{load_project_state, ManifestSnapshot};
use crate::core::sandbox::{
    default_store_root, ensure_sandbox_image, env_root_from_site_packages, SandboxArtifacts,
    SandboxStore,
};
use crate::workspace::WorkspaceStateReport;
use crate::{CommandContext, ExecutionOutcome};
use px_domain::api::{load_lockfile_optional, sandbox_config_from_manifest};

#[derive(Clone, Debug)]
pub(crate) struct SandboxRunContext {
    pub(super) store: SandboxStore,
    pub(super) artifacts: SandboxArtifacts,
}

pub(in super::super) fn sandbox_workspace_env_inconsistent(
    root: &Path,
    state: &WorkspaceStateReport,
) -> ExecutionOutcome {
    let reason = state
        .env_issue
        .as_ref()
        .and_then(|issue| issue.get("reason").and_then(serde_json::Value::as_str))
        .unwrap_or("env_outdated");
    ExecutionOutcome::user_error(
        "sandbox requires a consistent workspace environment",
        json!({
            "code": "PX902",
            "reason": reason,
            "hint": "run `px sync` at the workspace root before using --sandbox",
            "state": state.canonical.as_str(),
            "workspace_root": root.display().to_string(),
        }),
    )
}

pub(in super::super) fn prepare_project_sandbox(
    ctx: &CommandContext,
    snapshot: &ManifestSnapshot,
) -> Result<SandboxRunContext, ExecutionOutcome> {
    let store = sandbox_store()?;
    let state = load_project_state(ctx.fs(), &snapshot.root).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to read project state for sandbox",
            json!({ "error": err.to_string(), "code": "PX903" }),
        )
    })?;
    let env = state.current_env.ok_or_else(|| {
        ExecutionOutcome::user_error(
            "project environment missing for sandbox execution",
            json!({
                "code": "PX902",
                "reason": "missing_env",
                "hint": "run `px sync` before using --sandbox",
            }),
        )
    })?;
    let profile_oid = env
        .profile_oid
        .as_deref()
        .unwrap_or(&env.id)
        .trim()
        .to_string();
    if profile_oid.is_empty() {
        return Err(ExecutionOutcome::user_error(
            "sandbox requires an environment profile",
            json!({
                "code": "PX904",
                "reason": "missing_profile_oid",
            }),
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
        Some(root) => root,
        None => {
            return Err(ExecutionOutcome::user_error(
                "project environment missing for sandbox execution",
                json!({
                    "code": "PX902",
                    "reason": "missing_env",
                    "hint": "run `px sync` before using --sandbox",
                }),
            ))
        }
    };
    let lock = match load_lockfile_optional(&snapshot.lock_path) {
        Ok(lock) => lock,
        Err(err) => {
            return Err(ExecutionOutcome::failure(
                "failed to read px.lock",
                json!({ "error": err.to_string(), "code": "PX900" }),
            ))
        }
    };
    let Some(lock) = lock.as_ref() else {
        return Err(ExecutionOutcome::user_error(
            "px.lock not found for sandbox execution",
            json!({ "code": "PX900", "reason": "missing_lock" }),
        ));
    };
    let config = sandbox_config_from_manifest(&snapshot.manifest_path).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to read sandbox configuration",
            json!({ "error": err.to_string() }),
        )
    })?;
    let artifacts = ensure_sandbox_image(
        &store,
        &config,
        Some(lock),
        None,
        &profile_oid,
        &env_root,
        site_packages.as_deref(),
    )
    .map_err(|err| ExecutionOutcome::user_error(err.message, err.details))?;
    Ok(SandboxRunContext { store, artifacts })
}

pub(in super::super) fn prepare_workspace_sandbox(
    _ctx: &CommandContext,
    ws_ctx: &crate::workspace::WorkspaceRunContext,
) -> Result<SandboxRunContext, ExecutionOutcome> {
    let store = sandbox_store()?;
    let profile_oid = ws_ctx
        .profile_oid
        .as_deref()
        .unwrap_or("")
        .trim()
        .to_string();
    if profile_oid.is_empty() {
        return Err(ExecutionOutcome::user_error(
            "workspace environment missing for sandbox execution",
            json!({
                "code": "PX902",
                "reason": "missing_env",
                "hint": "run `px sync` at the workspace root before using --sandbox",
            }),
        ));
    }
    let lock = match load_lockfile_optional(&ws_ctx.lock_path) {
        Ok(lock) => lock,
        Err(err) => {
            return Err(ExecutionOutcome::failure(
                "failed to read workspace lockfile",
                json!({ "error": err.to_string(), "code": "PX900" }),
            ))
        }
    };
    let Some(lock) = lock.as_ref() else {
        return Err(ExecutionOutcome::user_error(
            "workspace lockfile missing for sandbox execution",
            json!({ "code": "PX900", "reason": "missing_lock" }),
        ));
    };
    let env_root = env_root_from_site_packages(&ws_ctx.site_packages).ok_or_else(|| {
        ExecutionOutcome::user_error(
            "workspace environment missing for sandbox execution",
            json!({
                "code": "PX902",
                "reason": "missing_env",
                "hint": "run `px sync` at the workspace root before using --sandbox",
            }),
        )
    })?;
    let workspace_lock = lock.workspace.as_ref();
    let config = sandbox_config_from_manifest(&ws_ctx.workspace_manifest).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to read workspace sandbox configuration",
            json!({ "error": err.to_string() }),
        )
    })?;
    let artifacts = ensure_sandbox_image(
        &store,
        &config,
        Some(lock),
        workspace_lock,
        &profile_oid,
        &env_root,
        Some(ws_ctx.site_packages.as_path()),
    )
    .map_err(|err| ExecutionOutcome::user_error(err.message, err.details))?;
    Ok(SandboxRunContext { store, artifacts })
}

pub(in super::super) fn prepare_commit_sandbox(
    manifest_path: &Path,
    lock: &px_domain::api::LockSnapshot,
    profile_oid: &str,
    env_root: &Path,
    site_packages: Option<&Path>,
) -> Result<SandboxRunContext, ExecutionOutcome> {
    let store = sandbox_store()?;
    let config = sandbox_config_from_manifest(manifest_path).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to read sandbox configuration at git ref",
            json!({ "error": err.to_string() }),
        )
    })?;
    let artifacts = ensure_sandbox_image(
        &store,
        &config,
        Some(lock),
        lock.workspace.as_ref(),
        profile_oid,
        env_root,
        site_packages,
    )
    .map_err(|err| ExecutionOutcome::user_error(err.message, err.details))?;
    Ok(SandboxRunContext { store, artifacts })
}

fn sandbox_store() -> Result<SandboxStore, ExecutionOutcome> {
    default_store_root().map(SandboxStore::new).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to resolve sandbox store",
            json!({ "error": err.to_string(), "code": "PX903" }),
        )
    })
}

pub(in super::super) fn attach_sandbox_details(
    outcome: &mut ExecutionOutcome,
    sandbox: &SandboxRunContext,
) {
    let details = json!({
        "sbx_id": sandbox.artifacts.definition.sbx_id(),
        "base": sandbox.artifacts.base.name,
        "base_os_oid": sandbox.artifacts.base.base_os_oid,
        "capabilities": sandbox.artifacts.definition.capabilities,
        "profile_oid": sandbox.artifacts.definition.profile_oid,
        "image_digest": sandbox.artifacts.manifest.image_digest,
    });
    match outcome.details {
        Value::Object(ref mut map) => {
            map.insert("sandbox".to_string(), details);
        }
        Value::Null => {
            outcome.details = json!({ "sandbox": details });
        }
        ref mut other => {
            let prev = other.take();
            outcome.details = json!({
                "value": prev,
                "sandbox": details,
            });
        }
    }
}
