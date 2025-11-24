use std::{fs, path::Path};

use anyhow::{bail, Result};
use toml_edit::{Array, DocumentMut, Item, Table, Value as TomlValue};

use super::manifest::{ensure_dependencies_array_mut, relative_path};

pub struct ProjectInitializer;

impl ProjectInitializer {
    /// Ensure the standard px project files exist under `root`.
    ///
    /// # Errors
    ///
    /// Returns an error when existing files cannot be read or the generated
    /// files cannot be written to disk.
    pub fn scaffold(root: &Path, package: &str, python_req: &str) -> Result<Vec<String>> {
        let mut files = Vec::new();
        let pyproject_path = root.join("pyproject.toml");
        let mut doc = if pyproject_path.exists() {
            let contents = fs::read_to_string(&pyproject_path)?;
            contents.parse::<DocumentMut>()?
        } else {
            DocumentMut::new()
        };
        let mut pyproject_changed = false;

        ensure_project_table(&mut doc);
        pyproject_changed |= ensure_project_identity(&mut doc, package, python_req);
        pyproject_changed |= ensure_project_dependencies(&mut doc);
        pyproject_changed |= ensure_tool_px_section(&mut doc);
        pyproject_changed |= ensure_build_system(&mut doc);

        if pyproject_changed {
            fs::write(&pyproject_path, doc.to_string())?;
            files.push(relative_path(root, &pyproject_path));
        }

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

fn ensure_project_table(doc: &mut DocumentMut) {
    let needs_reset = match doc.get("project") {
        Some(item) => !item.is_table(),
        None => true,
    };
    if needs_reset {
        doc.as_table_mut()
            .insert("project", Item::Table(Table::new()));
    }
}

fn ensure_project_identity(doc: &mut DocumentMut, package: &str, python_req: &str) -> bool {
    let mut changed = false;
    let table = doc["project"].as_table_mut().expect("project table");
    if !table.contains_key("name") {
        table["name"] = Item::Value(TomlValue::from(package));
        changed = true;
    }
    if !table.contains_key("version") {
        table["version"] = Item::Value(TomlValue::from("0.1.0"));
        changed = true;
    }
    if !table.contains_key("requires-python") {
        table["requires-python"] = Item::Value(TomlValue::from(python_req));
        changed = true;
    }
    changed
}

fn ensure_project_dependencies(doc: &mut DocumentMut) -> bool {
    let deps = ensure_dependencies_array_mut(doc);
    if deps.is_empty() {
        return false;
    }
    deps.clear();
    true
}

fn ensure_tool_px_section(doc: &mut DocumentMut) -> bool {
    let tool_entry = doc.entry("tool").or_insert(Item::Table(Table::new()));
    if !tool_entry.is_table() {
        *tool_entry = Item::Table(Table::new());
    }
    let tool_table = tool_entry.as_table_mut().expect("tool table");
    if tool_table.contains_key("px") {
        return false;
    }
    tool_table.insert("px", Item::Table(Table::new()));
    true
}

fn ensure_build_system(doc: &mut DocumentMut) -> bool {
    if doc
        .get("build-system")
        .is_some_and(toml_edit::Item::is_table)
    {
        return false;
    }
    let mut requires = Array::new();
    requires.push("setuptools>=70");
    requires.push("wheel");
    let mut table = Table::new();
    table.insert("requires", Item::Value(TomlValue::Array(requires)));
    table.insert(
        "build-backend",
        Item::Value(TomlValue::from("setuptools.build_meta")),
    );
    doc.as_table_mut()
        .insert("build-system", Item::Table(table));
    true
}

/// Determine the canonical package name for a project root.
///
/// # Errors
///
/// Returns an error when the explicit name or inferred name is not valid.
pub fn infer_package_name(explicit: Option<&str>, root: &Path) -> Result<(String, bool)> {
    if let Some(name) = explicit {
        validate_package_name(name)?;
        return Ok((name.to_string(), false));
    }
    let inferred = sanitize_package_candidate(root);
    validate_package_name(&inferred)?;
    Ok((inferred, true))
}

#[must_use]
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
