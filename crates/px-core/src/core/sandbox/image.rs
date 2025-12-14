use std::path::Path;

use anyhow::Result;
use serde_json::json;

use px_domain::api::{LockSnapshot, SandboxConfig, WorkspaceLock};

use super::errors::sandbox_error;
use super::paths::validate_env_root;
use super::resolve::resolve_sandbox_definition;
use super::store::SandboxStore;
use super::system_deps::pin_missing_apt_versions;
use super::types::SandboxArtifacts;
use crate::InstallUserError;

pub(crate) fn ensure_sandbox_image(
    store: &SandboxStore,
    config: &SandboxConfig,
    lock: Option<&LockSnapshot>,
    workspace_lock: Option<&WorkspaceLock>,
    profile_oid: &str,
    env_root: &Path,
    site_packages: Option<&Path>,
) -> Result<SandboxArtifacts, InstallUserError> {
    let mut resolution =
        resolve_sandbox_definition(config, lock, workspace_lock, profile_oid, site_packages)?;
    pin_missing_apt_versions(&mut resolution.definition)?;
    let env_root = validate_env_root(env_root)?;
    store
        .ensure_base_manifest(&resolution.base)
        .map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to prepare sandbox base",
                json!({
                    "base": resolution.base.name,
                    "error": err.to_string(),
                }),
            )
        })?;
    let manifest = store.ensure_image_manifest(&resolution.definition, &resolution.base)?;
    Ok(SandboxArtifacts {
        base: resolution.base,
        definition: resolution.definition,
        manifest,
        env_root,
    })
}
