use std::path::Path;

use anyhow::Result;
use serde_json::json;

use super::defaults::sanitize_component;
use super::PackRequest;
use crate::core::sandbox::sandbox_error;
use crate::InstallUserError;

fn default_entrypoint(project_root: &Path, project_name: &str) -> Vec<String> {
    let module = sanitize_component(project_name).replace('-', "_");
    let mut entry = Vec::new();
    entry.push("python".to_string());
    if !module.is_empty() {
        let candidates = [
            project_root.join(&module),
            project_root.join("src").join(&module),
        ];
        for package_dir in candidates {
            if package_dir.join("__main__.py").exists() {
                entry.push("-m".to_string());
                entry.push(module.clone());
                return entry;
            }
            if package_dir.join("cli.py").exists() {
                entry.push("-m".to_string());
                entry.push(format!("{module}.cli"));
                return entry;
            }
        }
        entry.push("-m".to_string());
        entry.push(module);
        return entry;
    }
    entry
}

pub(super) fn resolve_entrypoint(
    request: &PackRequest,
    project_root: &Path,
    project_name: &str,
) -> Result<Vec<String>, InstallUserError> {
    let raw = match &request.entrypoint {
        Some(custom) => custom.clone(),
        None => default_entrypoint(project_root, project_name),
    };
    let entry: Vec<String> = raw
        .into_iter()
        .filter(|item| !item.trim().is_empty())
        .collect();
    if entry.is_empty() {
        return Err(sandbox_error(
            "PX903",
            "px pack app requires an entrypoint command",
            json!({ "reason": "missing_entrypoint" }),
        ));
    }
    Ok(entry)
}
