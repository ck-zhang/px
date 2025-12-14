use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Result};
use sha2::{Digest, Sha256};

pub(crate) fn default_envs_root() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("PX_ENVS_PATH") {
        return Ok(PathBuf::from(path));
    }
    let home = dirs_next::home_dir().ok_or_else(|| anyhow!("HOME not set"))?;
    Ok(home.join(".px").join("envs"))
}

fn project_root_fingerprint(root: &Path) -> Result<String> {
    let canonical = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    Ok(hex::encode(Sha256::digest(
        canonical.display().to_string().as_bytes(),
    )))
}

pub(crate) fn project_env_owner_id(
    project_root: &Path,
    lock_id: &str,
    runtime_version: &str,
) -> Result<String> {
    Ok(format!(
        "project-env:{}:{}:{}",
        project_root_fingerprint(project_root)?,
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
        project_root_fingerprint(workspace_root)?,
        lock_id,
        runtime_version
    ))
}
