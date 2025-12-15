use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};

pub(crate) fn default_envs_root() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("PX_ENVS_PATH") {
        return Ok(PathBuf::from(path));
    }
    let home = dirs_next::home_dir().ok_or_else(|| anyhow!("HOME not set"))?;
    Ok(home.join(".px").join("envs"))
}

pub(crate) fn project_env_owner_id(
    project_root: &Path,
    lock_id: &str,
    runtime_version: &str,
) -> Result<String> {
    Ok(format!(
        "project-env:{}:{}:{}",
        crate::core::store::cas::root_fingerprint(project_root)?,
        lock_id,
        runtime_version
    ))
}

pub(crate) fn workspace_env_owner_id(
    workspace_root: &Path,
    lock_id: &str,
    runtime_version: &str,
) -> Result<String> {
    Ok(format!(
        "workspace-env:{}:{}:{}",
        crate::core::store::cas::root_fingerprint(workspace_root)?,
        lock_id,
        runtime_version
    ))
}
