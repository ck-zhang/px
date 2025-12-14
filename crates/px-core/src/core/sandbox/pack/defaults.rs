use std::fs;
use std::path::{Path, PathBuf};

use toml_edit::DocumentMut;

pub(super) fn default_tag(project_name: &str, manifest_path: &Path, profile_oid: &str) -> String {
    let name = sanitize_component(project_name);
    let version = project_version(manifest_path).unwrap_or_else(|| profile_fallback(profile_oid));
    let tag = sanitize_component(&version);
    format!("px.local/{name}:{tag}")
}

pub(super) fn sanitize_component(value: &str) -> String {
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

pub(super) fn default_pxapp_path(
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
