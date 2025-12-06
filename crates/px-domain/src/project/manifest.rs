use std::{
    collections::{BTreeMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Result};
use pep508_rs::Requirement as PepRequirement;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::str::FromStr;
use toml_edit::{Array, DocumentMut, Item, Table, Value as TomlValue};

use super::snapshot::ensure_pyproject_exists;

pub(crate) const TOMLI_W_REQUIREMENT: &str = "tomli-w>=1.0.0";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PxOptions {
    pub manage_command: Option<String>,
    pub plugin_imports: Vec<String>,
    pub env_vars: BTreeMap<String, String>,
}

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
    let parsed = read_requirements_file(path)?;
    let rel = relative_path(root, path);
    let mut rows = Vec::new();
    for spec in parsed.specs {
        rows.push(OnboardPackagePlan::new(spec, scope, rel.clone()));
    }
    if !parsed.extras.is_empty() {
        let pyproject = root.join("pyproject.toml");
        if pyproject.exists() {
            if let Ok(contents) = fs::read_to_string(&pyproject) {
                if let Ok(doc) = contents.parse::<DocumentMut>() {
                    let mut seen = HashSet::new();
                    for extra in parsed.extras {
                        if !seen.insert(extra.clone()) {
                            continue;
                        }
                        let deps = read_optional_dependency_group(&doc, &extra);
                        for dep in deps {
                            rows.push(OnboardPackagePlan::new(
                                dep,
                                scope,
                                format!("{rel} [{extra}]"),
                            ));
                        }
                    }
                }
            }
        }
    }
    Ok((
        json!({ "kind": kind, "path": rel, "count": rows.len() }),
        rows,
    ))
}

/// Convert `setup.cfg` metadata dependencies into onboarding rows.
///
/// # Errors
///
/// Returns an error if the file cannot be read or parsed.
pub fn collect_setup_cfg_packages(
    root: &Path,
    path: &Path,
) -> Result<(Value, Vec<OnboardPackagePlan>)> {
    let specs = read_setup_cfg_requires(path)?;
    let rel = relative_path(root, path);
    let mut rows = Vec::new();
    for spec in specs {
        rows.push(OnboardPackagePlan::new(spec, "prod", rel.clone()));
    }
    Ok((
        json!({ "kind": "setup.cfg", "path": rel, "count": rows.len() }),
        rows,
    ))
}

/// Read every requirement entry from `path`.
///
/// # Errors
///
/// Returns an error when the file cannot be read from disk.
#[derive(Debug, Default)]
pub struct RequirementFile {
    pub specs: Vec<String>,
    pub extras: Vec<String>,
}

pub fn read_requirements_file(path: &Path) -> Result<RequirementFile> {
    let mut visited = HashSet::new();
    read_requirements_file_inner(path, &mut visited)
}

fn read_requirements_file_inner(
    path: &Path,
    visited: &mut HashSet<PathBuf>,
) -> Result<RequirementFile> {
    let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(canonical.clone()) {
        return Ok(RequirementFile::default());
    }
    let contents = fs::read_to_string(&canonical)?;
    let base_dir = canonical.parent().unwrap_or_else(|| Path::new("."));
    let mut specs = Vec::new();
    let mut extras = Vec::new();
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let mut spec = if let Some(idx) = trimmed.find('#') {
            trimmed[..idx].trim()
        } else {
            trimmed
        };
        if let Some(rest) = spec.strip_prefix("-r") {
            let target = rest.trim_start_matches([' ', '=']).trim();
            if !target.is_empty() {
                let include = if Path::new(target).is_absolute() {
                    PathBuf::from(target)
                } else {
                    base_dir.join(target)
                };
                let nested = read_requirements_file_inner(&include, visited)?;
                specs.extend(nested.specs);
                extras.extend(nested.extras);
            }
            continue;
        } else if let Some(rest) = spec.strip_prefix("--requirement") {
            let target = rest.trim_start_matches([' ', '=']).trim();
            if !target.is_empty() {
                let include = if Path::new(target).is_absolute() {
                    PathBuf::from(target)
                } else {
                    base_dir.join(target)
                };
                let nested = read_requirements_file_inner(&include, visited)?;
                specs.extend(nested.specs);
                extras.extend(nested.extras);
            }
            continue;
        }
        if let Some(stripped) = spec.strip_prefix("-e ") {
            spec = stripped.trim();
        } else if let Some(stripped) = spec.strip_prefix("--editable ") {
            spec = stripped.trim();
        }
        if let Some(extras_block) = spec.strip_prefix(".[") {
            if let Some(end) = extras_block.find(']') {
                let names = extras_block[..end].split(',');
                for extra in names {
                    let trimmed = extra.trim().to_lowercase();
                    if trimmed == "socks" {
                        specs.push("pysocks".to_string());
                    } else if !trimmed.is_empty() {
                        extras.push(trimmed);
                    }
                }
            }
            continue;
        }
        if spec == "." || spec.starts_with("./") || spec.starts_with(".[") {
            continue;
        }
        if spec.starts_with('-') {
            continue;
        }
        if !spec.is_empty() {
            specs.push(spec.to_string());
        }
    }
    Ok(RequirementFile { specs, extras })
}

/// Read dependency entries from `setup.cfg`.
///
/// # Errors
///
/// Returns an error when the file cannot be read from disk.
pub fn read_setup_cfg_requires(path: &Path) -> Result<Vec<String>> {
    let contents = fs::read_to_string(path)?;
    let mut specs = Vec::new();
    let mut section = String::new();
    let mut collecting = false;

    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            section = trimmed.trim_matches(&['[', ']'][..]).to_ascii_lowercase();
            collecting = false;
            continue;
        }

        if collecting {
            if line.chars().next().is_some_and(char::is_whitespace) {
                if !trimmed.is_empty() && !trimmed.starts_with('#') {
                    specs.push(trimmed.to_string());
                }
                continue;
            }
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            collecting = false;
        }

        if section != "metadata" && section != "options" {
            continue;
        }

        if let Some((raw_key, raw_value)) = line.split_once('=') {
            let key = raw_key.trim().to_ascii_lowercase();
            if key == "requires-dist" || key == "install_requires" {
                let value = raw_value.trim();
                if !value.is_empty() && !value.starts_with('#') {
                    specs.push(value.to_string());
                }
                collecting = true;
            }
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

pub(crate) fn read_build_system_requires(doc: &DocumentMut) -> Vec<String> {
    doc.get("build-system")
        .and_then(Item::as_table)
        .and_then(|table| table.get("requires"))
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
    let normalized = normalize_dependency_group_name(group);
    find_optional_dependency_group(doc, &normalized)
        .map(|array| {
            array
                .iter()
                .filter_map(|val| val.as_str().map(std::string::ToString::to_string))
                .collect()
        })
        .unwrap_or_default()
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
            let incoming = spec.trim();
            let current = existing.trim();

            // Avoid loosening pins when the user re-runs `px add foo` on an already pinned dep.
            let incoming_has_constraint = requirement_has_constraint(incoming);
            let current_has_constraint = requirement_has_constraint(current);
            if current_has_constraint && !incoming_has_constraint {
                return InsertOutcome::Unchanged;
            }

            if current != incoming {
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
    specs.dedup();
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
    head.split('[')
        .next()
        .unwrap_or(head)
        .to_ascii_lowercase()
        .replace(['_', '.'], "-")
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

fn requirement_has_constraint(spec: &str) -> bool {
    let head = strip_wrapping_quotes(spec)
        .split(';')
        .next()
        .unwrap_or(spec);
    head.chars()
        .any(|ch| matches!(ch, '<' | '>' | '=' | '!' | '~' | '@'))
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
        .map_or_else(|| ">=3.11".to_string(), std::string::ToString::to_string);
    Ok((name, python_requirement))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DependencyGroupSource {
    IncludeConfig,
    LegacyConfig,
    DeclaredDefault,
    None,
}

impl DependencyGroupSource {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            DependencyGroupSource::IncludeConfig => "include-groups",
            DependencyGroupSource::LegacyConfig => "legacy",
            DependencyGroupSource::DeclaredDefault => "declared",
            DependencyGroupSource::None => "none",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DependencyGroupSelection {
    pub active: Vec<String>,
    pub declared: Vec<String>,
    pub source: DependencyGroupSource,
}

fn normalize_dependency_group_name(name: &str) -> String {
    dependency_name(name.trim())
}

fn normalize_group_list(groups: Vec<String>) -> Vec<String> {
    let mut normalized: Vec<String> = groups
        .into_iter()
        .map(|name| normalize_dependency_group_name(&name))
        .filter(|name| !name.is_empty())
        .collect();
    normalized.sort();
    normalized.dedup();
    normalized
}

fn parse_env_groups(raw: &str) -> Vec<String> {
    raw.split(|ch: char| ch == ',' || ch.is_whitespace() || ch == ';')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(std::string::ToString::to_string)
        .collect()
}

fn declared_dependency_groups(doc: &DocumentMut) -> Vec<String> {
    let mut groups = Vec::new();
    if let Some(table) = doc.get("dependency-groups").and_then(Item::as_table) {
        for (name, _item) in table.iter() {
            groups.push(name.to_string());
        }
    }
    if let Some(table) = doc
        .get("project")
        .and_then(Item::as_table)
        .and_then(|project| project.get("optional-dependencies"))
        .and_then(Item::as_table)
    {
        for (name, entry) in table.iter() {
            let Some(array) = entry.as_array() else {
                continue;
            };
            if is_common_dev_group(name)
                || array
                    .iter()
                    .filter_map(|val| val.as_str())
                    .any(is_dev_tool_spec)
            {
                groups.push(name.to_string());
            }
        }
    }
    if let Some(table) = doc
        .get("tool")
        .and_then(Item::as_table)
        .and_then(|tool| tool.get("poetry"))
        .and_then(Item::as_table)
        .and_then(|poetry| poetry.get("group"))
        .and_then(Item::as_table)
    {
        for (name, entry) in table.iter() {
            let dependencies = entry
                .as_table()
                .and_then(|group| group.get("dependencies"))
                .and_then(Item::as_table);
            if dependencies.is_some() {
                groups.push(name.to_string());
            }
        }
    }
    normalize_group_list(groups)
}

fn include_group_config(doc: &DocumentMut) -> Option<Vec<String>> {
    doc.get("tool")
        .and_then(Item::as_table)
        .and_then(|tool| tool.get("px"))
        .and_then(Item::as_table)
        .and_then(|px| px.get("dependencies"))
        .and_then(Item::as_table)
        .and_then(|deps| deps.get("include-groups"))
        .and_then(Item::as_array)
        .map(|array| {
            array
                .iter()
                .filter_map(|value| value.as_str().map(std::string::ToString::to_string))
                .collect()
        })
}

fn legacy_dependency_group_config(doc: &DocumentMut) -> Vec<String> {
    doc.get("tool")
        .and_then(Item::as_table)
        .and_then(|tool| tool.get("px"))
        .and_then(Item::as_table)
        .and_then(|px| px.get("dependency-groups"))
        .and_then(Item::as_array)
        .map(|array| {
            array
                .iter()
                .filter_map(|value| value.as_str().map(std::string::ToString::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn configured_dependency_groups(doc: &DocumentMut) -> (Vec<String>, DependencyGroupSource) {
    if let Some(include) = include_group_config(doc) {
        return (
            normalize_group_list(include),
            DependencyGroupSource::IncludeConfig,
        );
    }
    let legacy = legacy_dependency_group_config(doc);
    if !legacy.is_empty() {
        return (
            normalize_group_list(legacy),
            DependencyGroupSource::LegacyConfig,
        );
    }
    (Vec::new(), DependencyGroupSource::None)
}

pub(crate) fn select_dependency_groups(doc: &DocumentMut) -> DependencyGroupSelection {
    let declared = declared_dependency_groups(doc);
    let (mut active, mut source) = configured_dependency_groups(doc);
    if active.is_empty() && !declared.is_empty() {
        active = declared.clone();
        source = DependencyGroupSource::DeclaredDefault;
    }
    if let Ok(raw) = env::var("PX_GROUPS") {
        active.extend(parse_env_groups(&raw));
    }
    DependencyGroupSelection {
        active: normalize_group_list(active),
        declared,
        source,
    }
}

pub fn manifest_fingerprint(
    doc: &DocumentMut,
    requirements: &[String],
    groups: &[String],
    options: &PxOptions,
) -> Result<String> {
    let (name, python_requirement) = project_identity(doc)?;
    let mut deps = requirements.to_vec();
    sort_and_dedupe(&mut deps);
    let mut hasher = Sha256::new();
    hasher.update(name.trim().to_lowercase().as_bytes());
    hasher.update(python_requirement.trim().as_bytes());
    for dep in deps {
        hasher.update(dep.trim().as_bytes());
        hasher.update(b"\n");
    }
    let mut group_names = groups
        .iter()
        .map(|name| normalize_dependency_group_name(name))
        .collect::<Vec<_>>();
    group_names.sort();
    group_names.dedup();
    for group in group_names {
        hasher.update(b"group:");
        hasher.update(group.trim().as_bytes());
        hasher.update(b"\n");
    }
    if let Some(tool_python) = doc
        .get("tool")
        .and_then(Item::as_table)
        .and_then(|tool| tool.get("px"))
        .and_then(Item::as_table)
        .and_then(|px| px.get("python"))
        .and_then(Item::as_str)
    {
        hasher.update(tool_python.trim().as_bytes());
    }
    if let Some(manage) = options.manage_command.as_ref() {
        let trimmed = manage.trim();
        if !trimmed.is_empty() {
            hasher.update(b"manage:");
            hasher.update(trimmed.as_bytes());
        }
    }
    if !options.plugin_imports.is_empty() {
        let mut imports = options.plugin_imports.clone();
        imports.sort();
        imports.dedup();
        for name in imports {
            let trimmed = name.trim();
            if trimmed.is_empty() {
                continue;
            }
            hasher.update(b"plugin:");
            hasher.update(trimmed.as_bytes());
            hasher.update(b"\n");
        }
    }
    if !options.env_vars.is_empty() {
        let mut vars = options.env_vars.clone().into_iter().collect::<Vec<_>>();
        vars.sort_by(|a, b| a.0.cmp(&b.0));
        for (key, value) in vars {
            hasher.update(b"env:");
            hasher.update(key.trim().as_bytes());
            hasher.update(b"=");
            hasher.update(value.trim().as_bytes());
            hasher.update(b"\n");
        }
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn manifest_has_dependency(doc: &DocumentMut, needle: &str) -> bool {
    let target = dependency_name(needle);
    for spec in read_dependencies_from_doc(doc) {
        if dependency_name(&spec) == target {
            return true;
        }
    }
    if let Some(table) = doc
        .get("project")
        .and_then(Item::as_table)
        .and_then(|project| project.get("optional-dependencies"))
        .and_then(Item::as_table)
    {
        for (_, entry) in table.iter() {
            if let Some(array) = entry.as_array() {
                for val in array.iter().filter_map(|v| v.as_str()) {
                    if dependency_name(val) == target {
                        return true;
                    }
                }
            }
        }
    }
    if let Some(table) = doc.get("dependency-groups").and_then(Item::as_table) {
        for (_, entry) in table.iter() {
            if let Some(array) = entry.as_array() {
                for val in array.iter().filter_map(|v| v.as_str()) {
                    if dependency_name(val) == target {
                        return true;
                    }
                }
            }
        }
    }
    false
}

fn uses_hatch(doc: &DocumentMut) -> bool {
    if doc
        .get("tool")
        .and_then(Item::as_table)
        .and_then(|tool| tool.get("hatch"))
        .and_then(Item::as_table)
        .is_some()
    {
        return true;
    }
    doc.get("build-system")
        .and_then(Item::as_table)
        .and_then(|table| table.get("requires"))
        .and_then(Item::as_array)
        .map(|requires| {
            requires.iter().any(|entry| {
                entry
                    .as_str()
                    .map(|value| dependency_name(value) == "hatchling")
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

pub(crate) fn ensure_tooling_requirements(doc: &mut DocumentMut) -> bool {
    if !uses_hatch(doc) {
        return false;
    }
    if manifest_has_dependency(doc, "tomli-w") {
        return false;
    }
    merge_dev_dependency_specs(doc, &[TOMLI_W_REQUIREMENT.to_string()])
}

pub fn px_options_from_doc(doc: &DocumentMut) -> PxOptions {
    let px_table = doc
        .get("tool")
        .and_then(Item::as_table)
        .and_then(|tool| tool.get("px"))
        .and_then(Item::as_table);
    let mut options = PxOptions::default();
    if let Some(px) = px_table {
        if let Some(value) = px.get("manage-command").and_then(Item::as_str) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                options.manage_command = Some(trimmed.to_string());
            }
        }
        if let Some(array) = px.get("plugin-imports").and_then(Item::as_array) {
            let mut imports = Vec::new();
            for entry in array.iter() {
                if let Some(value) = entry.as_str() {
                    let trimmed = value.trim();
                    if !trimmed.is_empty() {
                        imports.push(trimmed.to_string());
                    }
                }
            }
            imports.sort();
            imports.dedup();
            options.plugin_imports = imports;
        }
        if let Some(env_table) = px.get("env").and_then(Item::as_table) {
            for (key, value) in env_table.iter() {
                let key = key.trim();
                if key.is_empty() {
                    continue;
                }
                if let Some(val) = value.as_str() {
                    let trimmed = val.trim();
                    if !trimmed.is_empty() {
                        options
                            .env_vars
                            .insert(key.to_string(), trimmed.to_string());
                    }
                } else if let Some(val) = value.as_value() {
                    let trimmed = val.to_string().trim().to_string();
                    if !trimmed.is_empty() {
                        options.env_vars.insert(key.to_string(), trimmed);
                    }
                }
            }
        }
    }
    options
}

/// Resolves dependency specifications for the selected groups, supporting `include-group` entries.
pub(crate) fn resolve_dependency_groups(
    doc: &DocumentMut,
    groups: &[String],
) -> Result<Vec<String>> {
    let mut collected = Vec::new();
    let mut visiting = HashSet::new();
    for group in groups {
        collect_group_dependencies(doc, group, &mut visiting, &mut collected)?;
    }
    sort_and_dedupe(&mut collected);
    Ok(collected)
}

fn collect_group_dependencies(
    doc: &DocumentMut,
    group: &str,
    visiting: &mut HashSet<String>,
    collected: &mut Vec<String>,
) -> Result<()> {
    let normalized = normalize_dependency_group_name(group);
    if !visiting.insert(normalized.clone()) {
        return Err(anyhow!("dependency group cycle detected at `{group}`"));
    }

    let mut handled = collect_dependency_group_entries(doc, &normalized, visiting, collected)?;
    if let Some(array) = find_optional_dependency_group(doc, &normalized) {
        handled = true;
        for value in array.iter() {
            if let Some(spec) = value.as_str() {
                collected.push(spec.to_string());
            }
        }
    }

    if !handled {
        tracing::debug!("requested dependency group `{group}` not found in manifest");
    }

    visiting.remove(&normalized);
    Ok(())
}

fn collect_dependency_group_entries(
    doc: &DocumentMut,
    normalized_group: &str,
    visiting: &mut HashSet<String>,
    collected: &mut Vec<String>,
) -> Result<bool> {
    if let Some(table) = doc.get("dependency-groups").and_then(Item::as_table) {
        for (name, item) in table.iter() {
            if normalize_dependency_group_name(name) != normalized_group {
                continue;
            }
            if let Some(array) = item.as_array() {
                for value in array.iter() {
                    if let Some(inline) = value.as_inline_table() {
                        if let Some(target) = inline
                            .get("include-group")
                            .and_then(TomlValue::as_str)
                            .map(str::to_string)
                        {
                            collect_group_dependencies(doc, &target, visiting, collected)?;
                            continue;
                        }
                    }
                    if let Some(spec) = value.as_str() {
                        collected.push(spec.to_string());
                    }
                }
            }
            return Ok(true);
        }
    }
    if let Some(table) = doc
        .get("tool")
        .and_then(Item::as_table)
        .and_then(|tool| tool.get("poetry"))
        .and_then(Item::as_table)
        .and_then(|poetry| poetry.get("group"))
        .and_then(Item::as_table)
        .and_then(|groups| groups.get(normalized_group))
        .and_then(Item::as_table)
        .and_then(|group| group.get("dependencies"))
        .and_then(Item::as_table)
    {
        for (name, entry) in table.iter() {
            if let Some(spec) = poetry_dependency_spec(name, entry) {
                collected.push(spec);
            }
        }
        return Ok(true);
    }
    Ok(false)
}

fn normalize_poetry_version_spec(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Some(spec) = trimmed.strip_prefix('^').and_then(caret_version_bounds) {
        return spec;
    }
    trimmed.to_string()
}

fn caret_version_bounds(raw: &str) -> Option<String> {
    let mut parts: Vec<u64> = raw
        .split('.')
        .map(|piece| piece.parse::<u64>().unwrap_or(0))
        .collect();
    while parts.len() < 3 {
        parts.push(0);
    }
    let major = parts.first().copied().unwrap_or(0);
    let minor = parts.get(1).copied().unwrap_or(0);
    let patch = parts.get(2).copied().unwrap_or(0);

    if major > 0 {
        Some(format!(">={raw},<{}.0.0", major + 1))
    } else if minor > 0 {
        Some(format!(">={raw},<0.{}.0", minor + 1))
    } else {
        Some(format!(">={raw},<0.0.{}", patch + 1))
    }
}

fn poetry_python_marker_expr(raw: &str) -> Option<String> {
    let mut clauses = Vec::new();
    for part in raw.split(',') {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }
        let (op, value) = match [">=", "<=", "==", "!=", "~=", ">", "<"]
            .iter()
            .find_map(|op| {
                trimmed
                    .strip_prefix(op)
                    .map(|rest| (*op, rest.trim_start()))
            }) {
            Some(pair) => pair,
            None => continue,
        };
        if value.is_empty() {
            continue;
        }
        clauses.push(format!(r#"python_version {op} "{value}""#));
    }
    if clauses.is_empty() {
        None
    } else {
        Some(clauses.join(" and "))
    }
}

fn poetry_dependency_spec(name: &str, item: &Item) -> Option<String> {
    if let Some(version) = item.as_str() {
        let trimmed = version.trim();
        return Some(if trimmed.is_empty() || trimmed == "*" {
            name.to_string()
        } else {
            format!("{name} {}", normalize_poetry_version_spec(trimmed))
        });
    }
    let (extras, version, python, marker_expr) = if let Some(table) = item.as_table() {
        (
            table.get("extras").and_then(Item::as_array),
            table.get("version").and_then(Item::as_str),
            table.get("python").and_then(Item::as_str),
            table.get("markers").and_then(Item::as_str),
        )
    } else if let Some(inline) = item.as_value().and_then(TomlValue::as_inline_table) {
        (
            inline.get("extras").and_then(TomlValue::as_array),
            inline.get("version").and_then(TomlValue::as_str),
            inline.get("python").and_then(TomlValue::as_str),
            inline.get("markers").and_then(TomlValue::as_str),
        )
    } else {
        return None;
    };
    let mut spec = name.to_string();
    if let Some(extras) = extras {
        let extras: Vec<String> = extras
            .iter()
            .filter_map(|val| val.as_str().map(std::string::ToString::to_string))
            .collect();
        if !extras.is_empty() {
            spec = format!("{spec}[{}]", extras.join(","));
        }
    }
    if let Some(version) = version {
        let trimmed = version.trim();
        if !trimmed.is_empty() && trimmed != "*" {
            spec = format!("{spec} {}", normalize_poetry_version_spec(trimmed));
        }
    }
    let mut markers = Vec::new();
    if let Some(python) = python {
        if let Some(expr) = poetry_python_marker_expr(python) {
            markers.push(expr);
        }
    }
    if let Some(marker) = marker_expr {
        let trimmed = marker.trim();
        if !trimmed.is_empty() {
            markers.push(trimmed.to_string());
        }
    }
    if !markers.is_empty() {
        spec = format!("{spec}; {}", markers.join(" and "));
    }
    Some(spec)
}

fn find_optional_dependency_group<'a>(
    doc: &'a DocumentMut,
    normalized_group: &str,
) -> Option<&'a Array> {
    doc.get("project")
        .and_then(Item::as_table)
        .and_then(|project| project.get("optional-dependencies"))
        .and_then(Item::as_table)
        .and_then(|table| {
            table.iter().find_map(|(name, entry)| {
                if normalize_dependency_group_name(name) == normalized_group {
                    entry.as_array()
                } else {
                    None
                }
            })
        })
}

fn is_common_dev_group(name: &str) -> bool {
    let lowered = name.to_ascii_lowercase();
    matches!(
        lowered.as_str(),
        "dev"
            | "test"
            | "tests"
            | "doc"
            | "docs"
            | "lint"
            | "format"
            | "fmt"
            | "typing"
            | "mypy"
            | "px-dev"
    )
}

fn is_dev_tool_spec(spec: &str) -> bool {
    let name = requirement_display_name(spec).to_ascii_lowercase();
    matches!(
        name.as_str(),
        "pytest"
            | "pytest-cov"
            | "pytest-xdist"
            | "hypothesis"
            | "ruff"
            | "flake8"
            | "mypy"
            | "coverage"
            | "tox"
            | "nox"
            | "black"
            | "isort"
            | "sphinx"
            | "pylint"
            | "bandit"
            | "pre-commit"
    )
}

pub(crate) fn ensure_dependency_group_config(doc: &mut DocumentMut) -> bool {
    if include_group_config(doc).is_some() {
        return false;
    }
    let declared = declared_dependency_groups(doc);
    if declared.is_empty() {
        return false;
    }
    let deps_entry = doc
        .entry("tool")
        .or_insert(Item::Table(Table::default()))
        .as_table_mut()
        .expect("tool table")
        .entry("px")
        .or_insert(Item::Table(Table::default()))
        .as_table_mut()
        .expect("px table")
        .entry("dependencies")
        .or_insert(Item::Table(Table::default()));
    if !deps_entry.is_table() {
        *deps_entry = Item::Table(Table::default());
    }
    let deps_table = deps_entry
        .as_table_mut()
        .expect("[tool.px.dependencies] must be a table");
    let mut array = Array::new();
    for group in declared {
        array.push(group.as_str());
    }
    deps_table.insert("include-groups", Item::Value(TomlValue::Array(array)));
    true
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

#[allow(dead_code)]
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
                "[project]\nname = \"demo\"\nversion = \"0.1.0\"\nrequires-python = \">=3.11\"\ndependencies = {dependencies}\n"
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

    #[test]
    fn add_specs_does_not_loosen_existing_pin() -> Result<()> {
        let dir = tempdir()?;
        let pyproject = dir.path().join("pyproject.toml");
        write_pyproject(&pyproject, "[\"requests==2.32.3\"]")?;

        let mut editor = ManifestEditor::open(&pyproject)?;
        let report = editor.add_specs(&["requests".to_string()])?;
        assert!(report.added.is_empty());
        assert!(report.updated.is_empty());

        let contents = fs::read_to_string(&pyproject)?;
        assert!(contents.contains("requests==2.32.3"));
        Ok(())
    }

    #[test]
    fn setup_cfg_requires_are_parsed() -> Result<()> {
        let dir = tempdir()?;
        let setup_cfg = dir.path().join("setup.cfg");
        fs::write(
            &setup_cfg,
            r#"[metadata]
requires-dist =
  idna>=3.6
  urllib3>=2.0,<3
[options]
install_requires =
  certifi>=2024.0.0
  charset_normalizer>=3.4.0
"#,
        )?;

        let specs = read_setup_cfg_requires(&setup_cfg)?;
        assert!(
            specs.contains(&"idna>=3.6".to_string())
                && specs.contains(&"urllib3>=2.0,<3".to_string()),
            "metadata.requires-dist entries should be collected"
        );
        assert!(
            specs.contains(&"certifi>=2024.0.0".to_string())
                && specs.contains(&"charset_normalizer>=3.4.0".to_string()),
            "options.install_requires entries should be collected"
        );

        Ok(())
    }

    #[test]
    fn requirements_file_resolves_nested_includes() -> Result<()> {
        let dir = tempdir()?;
        let root = dir.path();
        let nested = root.join("nested");
        fs::create_dir_all(&nested)?;
        let inner = nested.join("constraints.txt");
        fs::write(&inner, "anyio==4.0.0\n")?;

        let tests_req = root.join("requirements-tests.txt");
        fs::write(
            &tests_req,
            format!(
                "-r {}\npytest==8.3.3\n",
                inner.strip_prefix(root).unwrap().display()
            ),
        )?;

        let base = root.join("requirements.txt");
        fs::write(&base, "-r requirements-tests.txt\nuvicorn==0.30.0\n")?;

        let parsed = read_requirements_file(&base)?;
        let specs = parsed.specs;
        assert!(
            specs.contains(&"pytest==8.3.3".to_string())
                && specs.contains(&"uvicorn==0.30.0".to_string())
                && specs.contains(&"anyio==4.0.0".to_string()),
            "included requirement files should be expanded"
        );
        Ok(())
    }

    #[test]
    fn requirements_file_handles_recursive_includes() -> Result<()> {
        let dir = tempdir()?;
        let base = dir.path().join("requirements.txt");
        fs::write(&base, "-r requirements.txt\nhttpx==0.27.0\n")?;

        let parsed = read_requirements_file(&base)?;
        let specs = parsed.specs;
        assert_eq!(
            specs,
            vec!["httpx==0.27.0".to_string()],
            "recursive includes should not loop forever"
        );
        Ok(())
    }

    #[test]
    fn requirements_file_collects_local_extras() -> Result<()> {
        let dir = tempdir()?;
        let base = dir.path().join("requirements.txt");
        fs::write(&base, "-e .[test,EXTRA]\n")?;

        let parsed = read_requirements_file(&base)?;
        assert!(
            parsed.extras.contains(&"test".to_string())
                && parsed.extras.contains(&"extra".to_string())
        );
        assert!(parsed.specs.is_empty());
        Ok(())
    }

    #[test]
    fn dependency_group_config_defaults_to_declared_groups() -> Result<()> {
        let mut doc: DocumentMut = r#"[project]
name = "demo"
version = "0.1.0"
requires-python = ">=3.11"
dependencies = []

[dependency-groups]
docs = ["sphinx==7.0.0"]

[project.optional-dependencies]
PX-DEV = ["pytest==8.3.3"]
Test = ["hypothesis==6.0.0"]
"#
        .parse()?;

        let changed = ensure_dependency_group_config(&mut doc);
        assert!(changed, "include-groups should be written when absent");
        let groups = doc["tool"]["px"]["dependencies"]["include-groups"]
            .as_array()
            .expect("include-groups array");
        let values: Vec<_> = groups.iter().filter_map(|val| val.as_str()).collect();
        assert_eq!(
            values,
            vec!["docs", "px-dev", "test"],
            "declared dependency groups should be normalized and sorted"
        );
        // Should be a no-op on subsequent calls.
        assert!(!ensure_dependency_group_config(&mut doc));
        Ok(())
    }

    #[test]
    fn optional_groups_with_dev_tools_are_declared() -> Result<()> {
        let mut doc: DocumentMut = r#"[project]
name = "demo"
version = "0.1.0"
requires-python = ">=3.11"

[project.optional-dependencies]
all = ["pytest>=8.3.2", "ruff>=0.6.2"]
"#
        .parse()?;

        let declared = declared_dependency_groups(&doc);
        assert_eq!(declared, vec!["all"]);

        let changed = ensure_dependency_group_config(&mut doc);
        assert!(
            changed,
            "include-groups should be added for dev-like extras"
        );
        let groups = doc["tool"]["px"]["dependencies"]["include-groups"]
            .as_array()
            .expect("include-groups array");
        let values: Vec<_> = groups.iter().filter_map(|val| val.as_str()).collect();
        assert_eq!(values, vec!["all"]);

        Ok(())
    }

    #[test]
    fn dependency_group_config_handles_poetry_groups() -> Result<()> {
        let mut doc: DocumentMut = r#"[project]
name = "demo"
version = "0.1.0"
requires-python = ">=3.11"

[tool.poetry.group.dev.dependencies]
pytest = "^8.3.3"

[tool.poetry.group.test.dependencies]
pytest-cov = "^5.0.0"

[tool.poetry.group.typing.dependencies]
mypy = "^1.11.0"
"#
        .parse()?;

        let changed = ensure_dependency_group_config(&mut doc);
        assert!(
            changed,
            "include-groups should be written for poetry groups"
        );
        let groups = doc["tool"]["px"]["dependencies"]["include-groups"]
            .as_array()
            .expect("include-groups array");
        let values: Vec<_> = groups.iter().filter_map(|val| val.as_str()).collect();
        assert_eq!(values, vec!["dev", "test", "typing"]);
        Ok(())
    }

    #[test]
    fn poetry_group_dependencies_resolve() -> Result<()> {
        let mut doc: DocumentMut = r#"[project]
name = "demo"
version = "0.1.0"
requires-python = ">=3.11"

[tool.poetry.group.test.dependencies]
pytest = "^8.3.3"
pytest-xdist = { version = ">=3.1.0", extras = ["psutil"] }
pytest-github-actions-annotate-failures = "^0.1.7"
"#
        .parse()?;

        let deps = resolve_dependency_groups(&doc, &[String::from("test")])?;
        assert_eq!(
            deps,
            vec![
                "pytest >=8.3.3,<9.0.0",
                "pytest-github-actions-annotate-failures >=0.1.7,<0.2.0",
                "pytest-xdist[psutil] >=3.1.0"
            ]
        );
        Ok(())
    }

    #[test]
    fn px_options_parse_and_normalize() -> Result<()> {
        let doc: DocumentMut = r#"[project]
name = "demo"
version = "0.1.0"
requires-python = ">=3.11"

[tool.px]
manage-command = " self "
plugin-imports = ["tomli_w", " tomli_w ", "hatchling.builders.plugin"]
 [tool.px.env]
 FOO = "bar"
 COUNT = 1
"#
        .parse()?;

        let options = px_options_from_doc(&doc);
        assert_eq!(options.manage_command.as_deref(), Some("self"));
        assert_eq!(
            options.plugin_imports,
            vec![
                "hatchling.builders.plugin".to_string(),
                "tomli_w".to_string()
            ]
        );
        assert_eq!(
            options.env_vars,
            BTreeMap::from([
                ("COUNT".to_string(), "1".to_string()),
                ("FOO".to_string(), "bar".to_string())
            ])
        );
        Ok(())
    }

    #[test]
    fn manifest_fingerprint_reflects_px_options() -> Result<()> {
        let base: DocumentMut = r#"[project]
name = "demo"
version = "0.1.0"
requires-python = ">=3.11"
dependencies = []
"#
        .parse()?;
        let requirements = Vec::new();
        let groups = Vec::new();
        let base_options = px_options_from_doc(&base);
        let base_fp = manifest_fingerprint(&base, &requirements, &groups, &base_options)?;

        let with_opts: DocumentMut = r#"[project]
name = "demo"
version = "0.1.0"
requires-python = ">=3.11"
dependencies = []

[tool.px]
manage-command = "self"
plugin-imports = ["tomli_w"]
"#
        .parse()?;
        let options = px_options_from_doc(&with_opts);
        let updated_fp = manifest_fingerprint(&with_opts, &requirements, &groups, &options)?;
        assert_ne!(
            base_fp, updated_fp,
            "px options should affect manifest fingerprint"
        );
        Ok(())
    }

    #[test]
    fn ensure_tooling_requirements_adds_tomli_w_for_hatch() -> Result<()> {
        let mut doc: DocumentMut = r#"[project]
name = "demo"
version = "0.1.0"
requires-python = ">=3.11"
dependencies = []

[build-system]
requires = ["hatchling"]
build-backend = "hatchling.build"
"#
        .parse()?;

        let changed = ensure_tooling_requirements(&mut doc);
        assert!(changed, "tomli-w should be added when hatchling is used");
        let dev = doc
            .get("project")
            .and_then(Item::as_table)
            .and_then(|project| project.get("optional-dependencies"))
            .and_then(Item::as_table)
            .and_then(|table| table.get("px-dev"))
            .and_then(Item::as_array)
            .expect("px-dev group written");
        let mut entries: Vec<_> = dev.iter().filter_map(|val| val.as_str()).collect();
        entries.sort();
        assert!(
            entries.contains(&TOMLI_W_REQUIREMENT),
            "px-dev should include tomli-w"
        );
        Ok(())
    }

    #[test]
    fn ensure_tooling_requirements_skips_when_present() -> Result<()> {
        let mut doc: DocumentMut = r#"[project]
name = "demo"
version = "0.1.0"
requires-python = ">=3.11"
dependencies = ["tomli-w==1.0.0"]

[tool.hatch.metadata]
allow-direct-references = true
"#
        .parse()?;

        let changed = ensure_tooling_requirements(&mut doc);
        assert!(!changed, "no-op when tomli-w already present");
        Ok(())
    }
}
