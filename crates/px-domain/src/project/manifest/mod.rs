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
mod dependency_groups;
mod fingerprint;
mod normalize;
mod options;
mod packages;

pub use dependency_groups::DependencyGroupSource;
pub(crate) use dependency_groups::{
    ensure_dependency_group_config, ensure_tooling_requirements, read_optional_dependency_group,
    resolve_dependency_groups, select_dependency_groups,
};
pub use fingerprint::manifest_fingerprint;
pub use normalize::{canonicalize_package_name, canonicalize_spec};
pub(crate) use normalize::{dependency_name, strip_wrapping_quotes};
pub use options::{
    px_options_from_doc, sandbox_config_from_doc, sandbox_config_from_manifest, PxOptions,
    SandboxConfig,
};
pub use packages::{
    collect_pyproject_packages, collect_requirement_packages, collect_setup_cfg_packages,
    collect_setup_py_packages, read_setup_cfg_requires, read_setup_py_requires, OnboardPackagePlan,
};
pub(crate) use packages::{normalize_onboard_path, relative_path, requirement_display_name};

pub type RequirementFile = packages::RequirementFile;

pub fn read_requirements_file(path: &Path) -> Result<RequirementFile> {
    packages::read_requirements_file(path)
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
            let spec = canonicalize_spec(spec);
            if spec.is_empty() {
                continue;
            }
            match upsert_dependency(&mut deps, &spec) {
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

fn requirement_has_constraint(spec: &str) -> bool {
    let head = strip_wrapping_quotes(spec)
        .split(';')
        .next()
        .unwrap_or(spec);
    head.chars()
        .any(|ch| matches!(ch, '<' | '>' | '=' | '!' | '~' | '@'))
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

        let selection = select_dependency_groups(&doc);
        assert_eq!(selection.declared, vec!["all"]);

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
    fn optional_all_group_with_non_dev_deps_is_not_declared() -> Result<()> {
        let mut doc: DocumentMut = r#"[project]
name = "demo"
version = "0.1.0"
requires-python = ">=3.11"

[project.optional-dependencies]
all = ["pytest>=8.3.2", "requests>=2.0"]
	"#
        .parse()?;

        let selection = select_dependency_groups(&doc);
        assert!(selection.declared.is_empty());
        assert!(
            !ensure_dependency_group_config(&mut doc),
            "include-groups should not be written when only non-dev extras are detected"
        );

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
        let doc: DocumentMut = r#"[project]
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

    #[test]
    fn dependency_name_applies_aliases() {
        assert_eq!(dependency_name("osgeo"), "gdal");
        assert_eq!(dependency_name("OSGEO>=1.0"), "gdal");
    }

    #[test]
    fn canonicalize_spec_rewrites_alias() {
        assert_eq!(canonicalize_spec("osgeo>=1.0"), "gdal>=1.0");
        assert_eq!(canonicalize_spec("gdal==3.12.0"), "gdal==3.12.0");
    }
}
