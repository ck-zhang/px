use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Result};
use serde_json::json;

use super::errors::sandbox_error;
use crate::InstallUserError;

pub(crate) fn env_root_from_site_packages(site: &Path) -> Option<PathBuf> {
    site.parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .map(PathBuf::from)
}

pub(crate) fn discover_site_packages(env_root: &Path) -> Option<PathBuf> {
    let lib_dir = env_root.join("lib");
    if let Ok(entries) = fs::read_dir(&lib_dir) {
        let mut candidates: Vec<PathBuf> = entries
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                if !path.is_dir() {
                    return None;
                }
                let name = path.file_name()?.to_str()?;
                if !name.starts_with("python") {
                    return None;
                }
                let site = path.join("site-packages");
                if site.exists() {
                    Some(site)
                } else {
                    None
                }
            })
            .collect();
        candidates.sort();
        if let Some(site) = candidates.into_iter().next() {
            return Some(site);
        }
    }
    let fallback = env_root.join("site-packages");
    if fallback.exists() {
        return Some(fallback);
    }
    None
}

pub(super) fn validate_env_root(env_root: &Path) -> Result<PathBuf, InstallUserError> {
    if env_root.as_os_str().is_empty() {
        return Err(sandbox_error(
            "PX902",
            "sandbox requires an environment",
            json!({ "reason": "missing_env" }),
        ));
    }
    let path = env_root
        .canonicalize()
        .unwrap_or_else(|_| env_root.to_path_buf());
    if !path.exists() {
        return Err(sandbox_error(
            "PX902",
            "sandbox environment path is missing",
            json!({
                "reason": "missing_env",
                "env_root": env_root.display().to_string(),
            }),
        ));
    }
    Ok(path)
}

pub(crate) fn default_store_root() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("PX_SANDBOX_STORE") {
        return Ok(PathBuf::from(path));
    }
    dirs_next::home_dir()
        .map(|home| home.join(".px").join("sandbox"))
        .ok_or_else(|| anyhow!("unable to determine sandbox store location"))
}

pub(crate) fn sandbox_image_tag(sbx_id: &str) -> String {
    format!("px.sbx.local/{sbx_id}:latest")
}
