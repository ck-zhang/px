use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Result};
use pep508_rs::Requirement as PepRequirement;
use serde_json::{json, Value};
use std::str::FromStr;
use toml_edit::{Array, DocumentMut, Item, Table, Value as TomlValue};

use crate::snapshot::ensure_pyproject_exists;

#[derive(Debug)]
pub struct ManifestEditor {
    path: PathBuf,
    doc: DocumentMut,
}

impl ManifestEditor {
    /// Open `path` and prepare it for manifest edits.
    ///
    /// # Errors
    ///
    /// Returns an error when the manifest cannot be read or parsed.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        ensure_pyproject_exists(&path)?;
        let contents = fs::read_to_string(&path)?;
        let doc: DocumentMut = contents.parse()?;
        Ok(Self { path, doc })
    }

    #[must_use]
    pub fn dependencies(&self) -> Vec<String> {
        read_dependencies_from_doc(&self.doc)
    }

    /// Insert or update direct dependencies.
    ///
    /// # Errors
    ///
    /// Returns an error when the manifest cannot be written.
    pub fn add_specs(&mut self, specs: &[String]) -> Result<ManifestAddReport> {
        if specs.is_empty() {
            return Ok(ManifestAddReport::default());
        }

        let mut deps = self.dependencies();
        let mut added = Vec::new();
        let mut updated = Vec::new();
        for spec in specs {
            let spec = spec.trim();
            if spec.is_empty() {
                continue;
            }
            match upsert_dependency(&mut deps, spec) {
                InsertOutcome::Added(name) => added.push(name),
                InsertOutcome::Updated(name) => updated.push(name),
                InsertOutcome::Unchanged => {}
            }
        }
        sort_and_dedupe(&mut deps);
        if added.is_empty() && updated.is_empty() {
            return Ok(ManifestAddReport::default());
        }
        write_dependencies_array(&mut self.doc, &deps)?;
        self.save()?;
        Ok(ManifestAddReport { added, updated })
    }

    /// Remove direct dependencies from the manifest.
    ///
    /// # Errors
    ///
    /// Returns an error when the manifest cannot be written.
    pub fn remove_specs(&mut self, specs: &[String]) -> Result<ManifestRemoveReport> {
        let mut deps = self.dependencies();
        let targets: HashSet<String> = specs
            .iter()
            .map(|s| dependency_name(s))
            .filter(|name| !name.is_empty())
            .collect();
        if targets.is_empty() {
            return Ok(ManifestRemoveReport {
                removed: Vec::new(),
            });
        }
        let before = deps.len();
        deps.retain(|spec| !targets.contains(&dependency_name(spec)));
        if deps.len() == before {
            return Ok(ManifestRemoveReport {
                removed: Vec::new(),
            });
        }
        sort_and_dedupe(&mut deps);
        write_dependencies_array(&mut self.doc, &deps)?;
        self.save()?;
        Ok(ManifestRemoveReport {
            removed: targets.into_iter().collect(),
        })
    }

    /// Replace the dependencies array with the provided specs.
    ///
    /// # Errors
    ///
    /// Returns an error when the manifest cannot be written.
    pub fn write_dependencies(&mut self, specs: &[String]) -> Result<()> {
        write_dependencies_array(&mut self.doc, specs)?;
        self.save()
    }

    /// Update `[tool.px].python` with the requested version.
    ///
    /// # Errors
    ///
    /// Returns an error when the manifest cannot be written.
    ///
    /// # Panics
    ///
    /// Panics if the TOML structure for `[tool]` or `[tool.px]` is invalid.
    pub fn set_tool_python(&mut self, version: &str) -> Result<bool> {
        let tool_entry = self.doc.entry("tool").or_insert(Item::Table(Table::new()));
        if !tool_entry.is_table() {
            *tool_entry = Item::Table(Table::new());
        }
        let tool_table = tool_entry.as_table_mut().expect("tool table");
        let px_entry = tool_table.entry("px").or_insert(Item::Table(Table::new()));
        if !px_entry.is_table() {
            *px_entry = Item::Table(Table::new());
        }
        let px_table = px_entry.as_table_mut().expect("px table");
        let current = px_table
            .get("python")
            .and_then(Item::as_value)
            .and_then(|value| value.as_str());
        if current == Some(version) {
            return Ok(false);
        }
        px_table.insert("python", Item::Value(TomlValue::from(version)));
        self.save()?;
        Ok(true)
    }

    pub(crate) fn doc(&self) -> &DocumentMut {
        &self.doc
    }

    pub(crate) fn doc_mut(&mut self) -> &mut DocumentMut {
        &mut self.doc
    }

    fn save(&self) -> Result<()> {
        fs::write(&self.path, self.doc.to_string())?;
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct ManifestAddReport {
    pub added: Vec<String>,
    pub updated: Vec<String>,
}

#[derive(Debug, Default)]
pub struct ManifestRemoveReport {
    pub removed: Vec<String>,
}

/// Convert a `pyproject.toml` file into onboarding rows.
///
/// # Errors
///
/// Returns an error if the file cannot be read or parsed.
pub fn collect_pyproject_packages(
    root: &Path,
    path: &Path,
) -> Result<(Value, Vec<OnboardPackagePlan>)> {
    let contents = fs::read_to_string(path)?;
    let doc: DocumentMut = contents.parse()?;
    let deps = read_dependencies_from_doc(&doc);
    let rel = relative_path(root, path);
    let mut rows = Vec::new();
    for dep in deps {
        rows.push(OnboardPackagePlan::new(dep, "prod", rel.clone()));
    }
    Ok((
        json!({ "kind": "pyproject", "path": rel, "count": rows.len() }),
        rows,
    ))
}

/// Convert a requirements file into onboarding rows.
///
/// # Errors
///
/// Returns an error if the file cannot be read or parsed.
pub fn collect_requirement_packages(
    root: &Path,
    path: &Path,
    kind: &str,
    scope: &str,
) -> Result<(Value, Vec<OnboardPackagePlan>)> {
    let specs = read_requirements_file(path)?;
    let rel = relative_path(root, path);
    let mut rows = Vec::new();
    for spec in specs {
        rows.push(OnboardPackagePlan::new(spec, scope, rel.clone()));
    }
    Ok((
        json!({ "kind": kind, "path": rel, "count": rows.len() }),
        rows,
    ))
}

/// Read every requirement entry from `path`.
///
/// # Errors
///
/// Returns an error when the file cannot be read from disk.
pub fn read_requirements_file(path: &Path) -> Result<Vec<String>> {
    let contents = fs::read_to_string(path)?;
    let mut specs = Vec::new();
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let spec = if let Some(idx) = trimmed.find('#') {
            trimmed[..idx].trim()
        } else {
            trimmed
        };
        if !spec.is_empty() {
            specs.push(spec.to_string());
        }
    }
    Ok(specs)
}

pub(crate) fn read_dependencies_from_doc(doc: &DocumentMut) -> Vec<String> {
    doc.get("project")
        .and_then(Item::as_table)
        .and_then(|table| table.get("dependencies"))
        .and_then(Item::as_array)
        .map(|array| {
            array
                .iter()
                .filter_map(|val| val.as_str().map(std::string::ToString::to_string))
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn read_optional_dependency_group(doc: &DocumentMut, group: &str) -> Vec<String> {
    doc.get("project")
        .and_then(Item::as_table)
        .and_then(|project| project.get("optional-dependencies"))
        .and_then(Item::as_table)
        .and_then(|table| table.get(group))
        .and_then(Item::as_array)
        .map(|array| {
            array
                .iter()
                .filter_map(|val| val.as_str().map(std::string::ToString::to_string))
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn write_optional_dependency_group(
    doc: &mut DocumentMut,
    group: &str,
    specs: &[String],
) -> Result<()> {
    let project = project_table_mut(doc)?;
    let optional_table = project
        .entry("optional-dependencies")
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| anyhow!("optional-dependencies must be a table"))?;
    let mut array = Array::new();
    for spec in specs {
        array.push_formatted(TomlValue::from(spec.clone()));
    }
    optional_table.insert(group, Item::Value(TomlValue::Array(array)));
    Ok(())
}

pub(crate) fn project_table(doc: &DocumentMut) -> Result<&Table> {
    doc.get("project")
        .and_then(Item::as_table)
        .ok_or_else(|| anyhow!("[project] must be a table"))
}

pub(crate) fn project_table_mut(doc: &mut DocumentMut) -> Result<&mut Table> {
    doc.entry("project")
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| anyhow!("[project] must be a table"))
}

pub(crate) fn write_dependencies_array(doc: &mut DocumentMut, specs: &[String]) -> Result<()> {
    let table = project_table_mut(doc)?;
    let mut array = Array::new();
    for spec in specs {
        array.push_formatted(TomlValue::from(spec.clone()));
    }
    table.insert("dependencies", Item::Value(TomlValue::Array(array)));
    Ok(())
}

pub(crate) fn upsert_dependency(deps: &mut Vec<String>, spec: &str) -> InsertOutcome {
    let name = dependency_name(spec);
    for existing in deps.iter_mut() {
        if dependency_name(existing) == name {
            if existing.trim() != spec.trim() {
                *existing = spec.to_string();
                return InsertOutcome::Updated(name);
            }
            return InsertOutcome::Unchanged;
        }
    }
    deps.push(spec.to_string());
    InsertOutcome::Added(name)
}

pub(crate) fn sort_and_dedupe(specs: &mut Vec<String>) {
    specs.sort_by(|a, b| dependency_name(a).cmp(&dependency_name(b)).then(a.cmp(b)));
    let mut seen = HashSet::new();
    specs.retain(|spec| seen.insert(dependency_name(spec)));
}

pub(crate) fn dependency_name(spec: &str) -> String {
    let trimmed = strip_wrapping_quotes(spec.trim());
    let mut end = trimmed.len();
    for (idx, ch) in trimmed.char_indices() {
        if ch.is_ascii_whitespace() || matches!(ch, '<' | '>' | '=' | '!' | '~' | ';') {
            end = idx;
            break;
        }
    }
    let head = &trimmed[..end];
    let base = head.split('[').next().unwrap_or(head);
    base.to_lowercase()
}

pub(crate) fn strip_wrapping_quotes(input: &str) -> &str {
    if input.len() >= 2 {
        let bytes = input.as_bytes();
        let first = bytes[0];
        let last = bytes[input.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &input[1..input.len() - 1];
        }
    }
    input
}

pub(crate) fn project_identity(doc: &DocumentMut) -> Result<(String, String)> {
    let project = project_table(doc)?;
    let name = project
        .get("name")
        .and_then(Item::as_str)
        .ok_or_else(|| anyhow!("pyproject missing [project].name"))?
        .to_string();
    let python_requirement = project
        .get("requires-python")
        .and_then(Item::as_str)
        .map_or_else(|| ">=3.12".to_string(), std::string::ToString::to_string);
    Ok((name, python_requirement))
}

pub(crate) fn requirement_display_name(spec: &str) -> String {
    PepRequirement::from_str(spec.trim())
        .map_or_else(|_| spec.trim().to_string(), |req| req.name.to_string())
}

pub(crate) fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

pub(crate) fn normalize_onboard_path(root: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}

#[derive(Clone)]
pub struct OnboardPackagePlan {
    pub name: String,
    pub requested: String,
    pub scope: String,
    pub source: String,
}

impl OnboardPackagePlan {
    fn new(requested: String, scope: &str, source: String) -> Self {
        let name = requirement_display_name(&requested);
        Self {
            name,
            requested,
            scope: scope.to_string(),
            source,
        }
    }
}

pub(crate) fn ensure_optional_dependency_array_mut<'a>(
    doc: &'a mut DocumentMut,
    group: &str,
) -> &'a mut Array {
    let project_entry = doc
        .entry("project")
        .or_insert(Item::Table(Table::default()));
    if !project_entry.is_table() {
        *project_entry = Item::Table(Table::default());
    }
    let project_table = project_entry
        .as_table_mut()
        .expect("[project] must be a table");
    let optional_entry = project_table
        .entry("optional-dependencies")
        .or_insert(Item::Table(Table::default()));
    if !optional_entry.is_table() {
        *optional_entry = Item::Table(Table::default());
    }
    let table = optional_entry
        .as_table_mut()
        .expect("optional-dependencies must be a table");
    let group_entry = table
        .entry(group)
        .or_insert(Item::Value(TomlValue::Array(Array::new())));
    if !group_entry.is_array() {
        *group_entry = Item::Value(TomlValue::Array(Array::new()));
    }
    group_entry.as_array_mut().unwrap()
}

pub(crate) fn merge_dependency_specs(doc: &mut DocumentMut, specs: &[String]) -> bool {
    if specs.is_empty() {
        return false;
    }
    let array = ensure_dependencies_array_mut(doc);
    let mut changed = false;
    for spec in specs {
        if !array.iter().any(|val| val.as_str() == Some(spec.as_str())) {
            array.push(spec.as_str());
            changed = true;
        }
    }
    changed
}

pub(crate) fn merge_dev_dependency_specs(doc: &mut DocumentMut, specs: &[String]) -> bool {
    if specs.is_empty() {
        return false;
    }
    let array = ensure_optional_dependency_array_mut(doc, "px-dev");
    let mut changed = false;
    for spec in specs {
        if !array.iter().any(|val| val.as_str() == Some(spec.as_str())) {
            array.push(spec.as_str());
            changed = true;
        }
    }
    changed
}

pub(crate) fn overwrite_dependency_specs(doc: &mut DocumentMut, specs: &[String]) -> bool {
    let array = ensure_dependencies_array_mut(doc);
    overwrite_array_if_needed(array, specs)
}

pub(crate) fn overwrite_dev_dependency_specs(doc: &mut DocumentMut, specs: &[String]) -> bool {
    let array = ensure_optional_dependency_array_mut(doc, "px-dev");
    overwrite_array_if_needed(array, specs)
}

pub(crate) fn ensure_dependencies_array_mut(doc: &mut DocumentMut) -> &mut Array {
    let project_entry = doc
        .entry("project")
        .or_insert(Item::Table(Table::default()));
    if !project_entry.is_table() {
        *project_entry = Item::Table(Table::default());
    }
    let project_table = project_entry
        .as_table_mut()
        .expect("[project] must be a table");
    let deps_entry = project_table
        .entry("dependencies")
        .or_insert(Item::Value(TomlValue::Array(Array::new())));
    if !deps_entry.is_array() {
        *deps_entry = Item::Value(TomlValue::Array(Array::new()));
    }
    deps_entry
        .as_array_mut()
        .expect("dependencies should be an array")
}

pub(crate) enum InsertOutcome {
    Added(String),
    Updated(String),
    Unchanged,
}

pub(crate) fn canonicalize_marker(raw: &str) -> String {
    raw.split_whitespace()
        .collect::<String>()
        .to_ascii_lowercase()
}

fn overwrite_array_if_needed(array: &mut Array, specs: &[String]) -> bool {
    if array_matches(array, specs) {
        return false;
    }
    array.clear();
    for spec in specs {
        array.push(spec.as_str());
    }
    true
}

fn array_matches(array: &Array, specs: &[String]) -> bool {
    if array.len() != specs.len() {
        return false;
    }
    array
        .iter()
        .zip(specs.iter())
        .all(|(value, spec)| value.as_str() == Some(spec.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write_pyproject(path: &Path, dependencies: &str) -> Result<()> {
        fs::write(
            path,
            format!(
                "[project]\nname = \"demo\"\nversion = \"0.1.0\"\nrequires-python = \">=3.12\"\ndependencies = {dependencies}\n"
            ),
        )?;
        Ok(())
    }

    #[test]
    fn editor_adds_and_removes_specs() -> Result<()> {
        let dir = tempdir()?;
        let pyproject = dir.path().join("pyproject.toml");
        write_pyproject(&pyproject, "[\"requests==2.32.3\"]")?;

        let mut editor = ManifestEditor::open(&pyproject)?;
        let add_report = editor.add_specs(&["httpx==0.27.0".to_string()])?;
        assert_eq!(add_report.added, vec!["httpx".to_string()]);
        assert!(add_report.updated.is_empty());

        let contents = fs::read_to_string(&pyproject)?;
        assert!(contents.contains("httpx==0.27.0"));

        let remove_report = editor.remove_specs(&["requests".to_string()])?;
        assert_eq!(remove_report.removed.len(), 1);
        let contents = fs::read_to_string(&pyproject)?;
        assert!(!contents.contains("requests=="));
        Ok(())
    }
}
