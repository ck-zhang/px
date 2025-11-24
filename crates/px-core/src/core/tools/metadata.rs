use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::InstallUserError;

use super::paths::{normalize_tool_name, tool_root_dir};

pub(crate) const MIN_PYTHON_REQUIREMENT: &str = ">=3.8";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ToolMetadata {
    pub name: String,
    pub spec: String,
    pub entry: String,
    pub console_scripts: BTreeMap<String, String>,
    pub runtime_version: String,
    pub runtime_full_version: String,
    pub runtime_path: String,
    pub installed_spec: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug)]
pub(crate) struct InstalledTool {
    pub name: String,
    pub runtime_version: String,
    pub root: PathBuf,
}

pub(crate) fn load_installed_tool(name: &str) -> Result<InstalledTool, InstallUserError> {
    let normalized = normalize_tool_name(name);
    let root = tool_root_dir(&normalized).map_err(|err| {
        InstallUserError::new(
            "failed to resolve tool directory",
            serde_json::json!({
                "tool": normalized,
                "error": err.to_string(),
            }),
        )
    })?;
    let metadata = read_metadata(&root).map_err(|err| {
        InstallUserError::new(
            format!("tool '{normalized}' is not installed"),
            serde_json::json!({
                "error": err.to_string(),
                "tool": normalized,
                "root": root.display().to_string(),
                "hint": format!("run `px tool install {normalized}` first"),
            }),
        )
    })?;

    Ok(InstalledTool {
        name: metadata.name,
        runtime_version: metadata.runtime_version,
        root,
    })
}

pub(crate) fn write_metadata(root: &Path, metadata: &ToolMetadata) -> Result<()> {
    let path = root.join("tool.json");
    let mut json = serde_json::to_vec_pretty(metadata)?;
    json.push(b'\n');
    fs::write(path, json)?;
    Ok(())
}

pub(crate) fn read_metadata(root: &Path) -> Result<ToolMetadata> {
    let path = root.join("tool.json");
    let contents =
        fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&contents).context("invalid tool metadata")
}

pub(crate) fn timestamp_string() -> Result<String> {
    let now = OffsetDateTime::now_utc();
    Ok(now.format(&time::format_description::well_known::Rfc3339)?)
}
