use std::fs;

use anyhow::{Context, Result};
use serde_json::json;

use crate::{CommandContext, ExecutionOutcome};

use super::metadata::read_metadata;
use super::paths::{normalize_tool_name, tool_root_dir, tools_root};

#[derive(Clone, Debug, Default)]
pub struct ToolListRequest;

#[derive(Clone, Debug)]
pub struct ToolRemoveRequest {
    pub name: String,
}

/// Lists all installed px-managed tools.
///
/// # Errors
/// Returns an error if the tools directory cannot be read.
pub fn tool_list(_ctx: &CommandContext, _request: ToolListRequest) -> Result<ExecutionOutcome> {
    let root = tools_root()?;
    if !root.exists() {
        return Ok(ExecutionOutcome::success(
            "no tools installed",
            json!({ "tools": Vec::<serde_json::Value>::new() }),
        ));
    }
    let mut rows = Vec::new();
    if let Ok(entries) = fs::read_dir(&root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if let Ok(meta) = read_metadata(&path) {
                rows.push(meta);
            }
        }
    }
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    let details: Vec<serde_json::Value> = rows
        .iter()
        .map(|meta| {
            json!({
                "name": meta.name,
                "spec": meta.installed_spec,
                "runtime": meta.runtime_version,
                "entry": meta.entry,
                "console_scripts": meta.console_scripts.keys().collect::<Vec<_>>(),
            })
        })
        .collect();
    if rows.is_empty() {
        return Ok(ExecutionOutcome::success(
            "no tools installed",
            json!({ "tools": details }),
        ));
    }
    let mut lines = Vec::new();
    for meta in &rows {
        lines.push(format!(
            "{}  {}  (Python {})",
            meta.name, meta.installed_spec, meta.runtime_version
        ));
    }
    Ok(ExecutionOutcome::success(
        lines.join("\n"),
        json!({ "tools": details }),
    ))
}

/// Removes an installed px-managed tool.
///
/// # Errors
/// Returns an error if the tool directory cannot be deleted.
pub fn tool_remove(_ctx: &CommandContext, request: &ToolRemoveRequest) -> Result<ExecutionOutcome> {
    let normalized = normalize_tool_name(&request.name);
    let root = tool_root_dir(&normalized)?;
    if !root.exists() {
        return Ok(ExecutionOutcome::user_error(
            format!("tool '{normalized}' is not installed"),
            json!({ "hint": format!("run `px tool install {normalized}` first") }),
        ));
    }
    fs::remove_dir_all(&root).with_context(|| format!("removing {}", root.display()))?;
    Ok(ExecutionOutcome::success(
        format!("removed tool {normalized}"),
        json!({ "tool": normalized }),
    ))
}
