use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use pep508_rs::Requirement;
use serde_json::json;
use std::str::FromStr;

use crate::{CommandContext, ExecutionOutcome};

use super::metadata::{read_metadata, ToolMetadata};
use super::paths::{find_env_site, normalize_tool_name, tool_root_dir, tools_root};

#[derive(serde::Deserialize)]
struct ToolState {
    #[serde(default)]
    current_env: Option<ToolEnvironment>,
}

#[derive(serde::Deserialize)]
struct ToolEnvironment {
    site_packages: String,
}

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
    let mut rows: Vec<(ToolMetadata, String)> = Vec::new();
    if let Ok(entries) = fs::read_dir(&root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if let Ok(meta) = read_metadata(&path) {
                let display_spec =
                    resolved_tool_spec(&path, &meta).unwrap_or_else(|| meta.installed_spec.clone());
                rows.push((meta, display_spec));
            }
        }
    }
    rows.sort_by(|a, b| a.0.name.cmp(&b.0.name));
    let details: Vec<serde_json::Value> = rows
        .iter()
        .map(|(meta, display_spec)| {
            json!({
                "name": meta.name,
                "spec": display_spec,
                "metadata_spec": meta.installed_spec,
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
    for (meta, display_spec) in &rows {
        lines.push(format!(
            "{}  {}  (Python {})",
            meta.name, display_spec, meta.runtime_version
        ));
    }
    Ok(ExecutionOutcome::success(
        lines.join("\n"),
        json!({ "tools": details }),
    ))
}

fn resolved_tool_spec(root: &Path, meta: &ToolMetadata) -> Option<String> {
    let package = Requirement::from_str(&meta.installed_spec)
        .ok()
        .map(|req| req.name.to_string())
        .unwrap_or_else(|| meta.name.clone());
    let site = tool_site_packages(root)?;
    let version = dist_info_version(&site, &package).or_else(|| pxpth_version(&site, &package))?;
    Some(format!("{package}=={version}"))
}

fn tool_site_packages(root: &Path) -> Option<PathBuf> {
    let state_path = root.join(".px").join("state.json");
    let state: Option<ToolState> = fs::read_to_string(&state_path)
        .ok()
        .and_then(|contents| serde_json::from_str(&contents).ok());
    if let Some(env) = state.and_then(|s| s.current_env) {
        let site = PathBuf::from(env.site_packages);
        if let Some(packages) = locate_site_packages(&site) {
            return Some(packages);
        }
    }
    let site = find_env_site(root)?;
    locate_site_packages(&site)
}

fn locate_site_packages(site_root: &Path) -> Option<PathBuf> {
    let lib_dir = site_root.join("lib");
    if let Ok(entries) = fs::read_dir(&lib_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_lowercase();
            if !name.starts_with("python") {
                continue;
            }
            let site_packages = path.join("site-packages");
            if site_packages.exists() {
                return Some(site_packages);
            }
        }
    }
    let direct = site_root.join("site-packages");
    if direct.exists() {
        return Some(direct);
    }
    None
}

fn dist_info_version(site_packages: &Path, package: &str) -> Option<String> {
    let normalized = package.replace('_', "-").to_ascii_lowercase();
    let entries = fs::read_dir(site_packages).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        if !name.starts_with(&format!("{normalized}-")) || !name.ends_with(".dist-info") {
            continue;
        }
        let metadata = entry.path().join("METADATA");
        if let Ok(contents) = fs::read_to_string(&metadata) {
            for line in contents.lines() {
                if let Some(rest) = line.strip_prefix("Version:") {
                    let version = rest.trim();
                    if !version.is_empty() {
                        return Some(version.to_string());
                    }
                }
            }
        }
        let remainder = name
            .trim_start_matches(&format!("{normalized}-"))
            .trim_end_matches(".dist-info");
        if !remainder.is_empty() {
            return Some(remainder.to_string());
        }
    }
    None
}

fn pxpth_version(site_packages: &Path, package: &str) -> Option<String> {
    let pth = site_packages.join("px.pth");
    let contents = fs::read_to_string(pth).ok()?;
    let needle = format!("{}-", package.replace('_', "-").to_ascii_lowercase());
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let filename = Path::new(trimmed)
            .file_name()
            .map(|f| f.to_string_lossy().to_ascii_lowercase())?;
        if let Some(rest) = filename.strip_prefix(&needle) {
            let rest = rest.trim_end_matches(".dist");
            if let Some((version, _)) = rest.split_once('-') {
                if !version.is_empty() {
                    return Some(version.to_string());
                }
            }
        }
    }
    None
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
