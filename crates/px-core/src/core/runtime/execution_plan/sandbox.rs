use std::path::Path;

use serde_json::json;

use super::types::SandboxPlan;
use crate::core::sandbox;
use crate::ExecutionOutcome;
use px_domain::api::sandbox_config_from_manifest;

pub(crate) fn sandbox_plan(
    manifest_path: &Path,
    want_sandbox: bool,
    lock: Option<&px_domain::api::LockSnapshot>,
    workspace_lock: Option<&px_domain::api::WorkspaceLock>,
    profile_oid: Option<&str>,
    site_packages: Option<&Path>,
) -> Result<SandboxPlan, ExecutionOutcome> {
    if !want_sandbox {
        return Ok(SandboxPlan {
            enabled: false,
            sbx_id: None,
            base: None,
            capabilities: Vec::new(),
        });
    }
    let config = sandbox_config_from_manifest(manifest_path).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to parse sandbox config",
            json!({ "error": err.to_string() }),
        )
    })?;
    let profile_oid = profile_oid.unwrap_or_default().trim();
    let definition_profile = if profile_oid.is_empty() {
        "unknown"
    } else {
        profile_oid
    };
    let resolution = sandbox::resolve_sandbox_definition(
        &config,
        lock,
        workspace_lock,
        definition_profile,
        site_packages,
    )
    .map_err(|err| ExecutionOutcome::user_error(err.message, err.details))?;
    let sbx_id = if profile_oid.is_empty() {
        None
    } else {
        Some(resolution.definition.sbx_id())
    };
    Ok(SandboxPlan {
        enabled: true,
        sbx_id,
        base: Some(resolution.base.name),
        capabilities: resolution.definition.capabilities.into_iter().collect(),
    })
}
