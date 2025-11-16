use std::{fs, path::Path};

use anyhow::{bail, Result};

use crate::manifest::relative_path;

pub struct ProjectInitializer;

impl ProjectInitializer {
    pub fn scaffold(root: &Path, package: &str, python_req: &str) -> Result<Vec<String>> {
        let mut files = Vec::new();
        let pyproject_path = root.join("pyproject.toml");
        let pyproject = format!(
            "[project]\nname = \"{package}\"\nversion = \"0.1.0\"\nrequires-python = \"{python_req}\"\ndependencies = []\n\n[tool.px]\n\n[build-system]\nrequires = [\"setuptools>=70\", \"wheel\"]\nbuild-backend = \"setuptools.build_meta\"\n"
        );
        fs::write(&pyproject_path, pyproject)?;
        files.push(relative_path(root, &pyproject_path));

        let px_root = root.join(".px");
        ensure_dir(&px_root, root, &mut files)?;
        ensure_dir(&px_root.join("envs"), root, &mut files)?;
        ensure_dir(&px_root.join("logs"), root, &mut files)?;
        let state_path = px_root.join("state.json");
        if !state_path.exists() {
            fs::write(&state_path, "{}\n")?;
            files.push(relative_path(root, &state_path));
        }

        Ok(files)
    }
}

pub fn infer_package_name(explicit: Option<&str>, root: &Path) -> Result<(String, bool)> {
    if let Some(name) = explicit {
        validate_package_name(name)?;
        return Ok((name.to_string(), false));
    }
    let inferred = sanitize_package_candidate(root);
    validate_package_name(&inferred)?;
    Ok((inferred, true))
}

pub fn sanitize_package_candidate(root: &Path) -> String {
    let raw = root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("px_app");
    sanitize_package_name(raw)
}

fn sanitize_package_name(raw: &str) -> String {
    let mut result = String::new();
    let mut last_was_sep = false;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            result.push(ch.to_ascii_lowercase());
            last_was_sep = false;
        } else if matches!(ch, '-' | '_' | ' ' | '.') {
            if !last_was_sep {
                result.push('_');
                last_was_sep = true;
            }
        } else {
            last_was_sep = false;
        }
    }
    while result.starts_with('_') {
        result.remove(0);
    }
    while result.ends_with('_') {
        result.pop();
    }
    if result.is_empty() {
        return "px_app".to_string();
    }
    let first = result.chars().next().unwrap();
    if !first.is_ascii_alphabetic() && first != '_' {
        result = format!("px_{result}");
    }
    result
}

fn ensure_dir(path: &Path, root: &Path, files: &mut Vec<String>) -> Result<()> {
    if !path.exists() {
        fs::create_dir_all(path)?;
        files.push(relative_path(root, path));
    }
    Ok(())
}

fn validate_package_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    match chars.next() {
        Some(ch) if ch.is_ascii_alphabetic() || ch == '_' => {}
        _ => bail!("package name must start with a letter or underscore"),
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        bail!("package name may only contain letters, numbers, or underscores");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn sanitize_infers_reasonable_name() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("Hello-World!");
        fs::create_dir_all(&root).unwrap();
        let name = sanitize_package_candidate(&root);
        assert_eq!(name, "hello_world");
    }
}
