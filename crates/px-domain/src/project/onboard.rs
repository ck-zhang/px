use std::{
    collections::BTreeMap,
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{bail, Result};
use time::{format_description, OffsetDateTime};
use toml_edit::{Array, DocumentMut, Item, Table, Value as TomlValue};

use super::init::sanitize_package_candidate;
use super::manifest::{
    ensure_dependency_group_config, ensure_tooling_requirements, merge_dependency_specs,
    merge_dev_dependency_specs, normalize_onboard_path, read_build_system_requires, relative_path,
    OnboardPackagePlan,
};
use super::snapshot::ensure_pyproject_exists;

#[derive(Clone)]
pub struct PyprojectPlan {
    pub path: PathBuf,
    pub contents: Option<String>,
    pub created: bool,
}

impl PyprojectPlan {
    #[must_use]
    pub fn needs_backup(&self) -> bool {
        self.contents.is_some() && !self.created
    }

    #[must_use]
    pub fn updated(&self) -> bool {
        self.contents.is_some()
    }
}

pub struct BackupSummary {
    pub files: Vec<String>,
    pub directory: Option<String>,
}

pub struct BackupManager {
    root: PathBuf,
    dir: Option<PathBuf>,
    entries: Vec<String>,
    copies: Vec<(PathBuf, PathBuf)>,
}

impl BackupManager {
    #[must_use]
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            dir: None,
            entries: Vec::new(),
            copies: Vec::new(),
        }
    }

    /// Copy `path` into the backup directory if it exists.
    ///
    /// # Errors
    ///
    /// Returns an error when the backup path cannot be created or when copying
    /// the file fails.
    pub fn backup(&mut self, path: &Path) -> Result<()> {
        if !path.exists() {
            return Ok(());
        }
        let dir = self.ensure_dir()?;
        let rel = relative_path(&self.root, path);
        let dest = dir.join(&rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(path, &dest)?;
        self.entries.push(relative_path(&self.root, &dest));
        self.copies.push((path.to_path_buf(), dest));
        Ok(())
    }

    /// Restore every backed up file to its original location.
    ///
    /// # Errors
    ///
    /// Returns an error when a restore copy fails.
    pub fn restore_all(&self) -> Result<()> {
        for (original, backup) in &self.copies {
            if backup.exists() {
                if let Some(parent) = original.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::copy(backup, original)?;
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn finish(self) -> BackupSummary {
        let dir_rel = self.dir.map(|dir| relative_path(&self.root, &dir));
        BackupSummary {
            files: self.entries,
            directory: dir_rel,
        }
    }

    /// Lazily create the backup directory and return its path.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory cannot be created.
    fn ensure_dir(&mut self) -> Result<PathBuf> {
        if self.dir.is_none() {
            let fmt = format_description::parse("[year][month][day]T[hour][minute][second]")?;
            let stamp = OffsetDateTime::now_utc().format(&fmt)?;
            let dir = self.root.join(".px").join("onboard-backups").join(stamp);
            fs::create_dir_all(&dir)?;
            self.dir = Some(dir);
        }
        Ok(self.dir.clone().unwrap())
    }
}

pub fn prepare_pyproject_plan(
    root: &Path,
    pyproject_path: &Path,
    lock_only: bool,
    packages: &[OnboardPackagePlan],
) -> Result<PyprojectPlan> {
    if lock_only {
        ensure_pyproject_exists(pyproject_path)?;
        return Ok(PyprojectPlan {
            path: pyproject_path.to_path_buf(),
            contents: None,
            created: false,
        });
    }

    let mut created = false;
    let mut doc: DocumentMut = if pyproject_path.exists() {
        fs::read_to_string(pyproject_path)?.parse()?
    } else {
        created = true;
        create_minimal_pyproject_doc(root)?
    };
    let mut changed = ensure_project_metadata(&mut doc, root);

    let mut prod_specs = Vec::new();
    let mut dev_specs = Vec::new();
    for pkg in packages {
        if pkg.source.ends_with("pyproject.toml") {
            continue;
        }
        if pkg.scope == "dev" {
            dev_specs.push(pkg.requested.clone());
        } else {
            prod_specs.push(pkg.requested.clone());
        }
    }

    changed |= merge_dependency_specs(&mut doc, &prod_specs);
    changed |= merge_dev_dependency_specs(&mut doc, &dev_specs);
    if changed {
        let build_requires = read_build_system_requires(&doc);
        changed |= merge_dev_dependency_specs(&mut doc, &build_requires);
    }
    changed |= ensure_dependency_group_config(&mut doc);
    changed |= ensure_tooling_requirements(&mut doc);
    changed |= dedupe_dependency_arrays(&mut doc);

    if changed || created {
        Ok(PyprojectPlan {
            path: pyproject_path.to_path_buf(),
            contents: Some(doc.to_string()),
            created,
        })
    } else {
        Ok(PyprojectPlan {
            path: pyproject_path.to_path_buf(),
            contents: None,
            created: false,
        })
    }
}

fn dedupe_dependency_arrays(doc: &mut DocumentMut) -> bool {
    fn dedupe(array: &mut Array) -> bool {
        let mut seen = HashSet::new();
        let mut duplicates = Vec::new();
        for (idx, value) in array.iter().enumerate() {
            let Some(spec) = value.as_str() else {
                continue;
            };
            if !seen.insert(spec.to_string()) {
                duplicates.push(idx);
            }
        }
        if duplicates.is_empty() {
            return false;
        }
        for idx in duplicates.into_iter().rev() {
            let _ = array.remove(idx);
        }
        true
    }

    let mut changed = false;
    if let Some(deps) = doc
        .get_mut("project")
        .and_then(Item::as_table_mut)
        .and_then(|project| project.get_mut("dependencies"))
        .and_then(Item::as_array_mut)
    {
        changed |= dedupe(deps);
    }
    if let Some(dev) = doc
        .get_mut("project")
        .and_then(Item::as_table_mut)
        .and_then(|project| project.get_mut("optional-dependencies"))
        .and_then(Item::as_table_mut)
        .and_then(|optional| optional.get_mut("px-dev"))
        .and_then(Item::as_array_mut)
    {
        changed |= dedupe(dev);
    }
    changed
}

pub fn resolve_onboard_path(
    root: &Path,
    override_value: Option<&str>,
    default_name: &str,
) -> Result<Option<PathBuf>> {
    if let Some(raw) = override_value {
        let candidate = normalize_onboard_path(root, PathBuf::from(raw));
        if !candidate.exists() {
            bail!("path not found: {}", candidate.display());
        }
        return Ok(Some(candidate));
    }
    let candidate = root.join(default_name);
    if candidate.exists() {
        Ok(Some(candidate))
    } else {
        Ok(None)
    }
}

fn create_minimal_pyproject_doc(root: &Path) -> Result<DocumentMut> {
    let name = sanitize_package_candidate(root);
    let template = format!("[project]\nname = \"{name}\"\nversion = \"0.1.0\"\n",)
        + "description = \"Onboarded by px\"\n"
        + "requires-python = \">=3.11\"\n"
        + "dependencies = []\n\n[tool.px]\n"
        + "\n[build-system]\n"
        + "requires = [\"setuptools>=70\", \"wheel\"]\n"
        + "build-backend = \"setuptools.build_meta\"\n";
    Ok(template.parse()?)
}

#[derive(Default)]
struct SetupCfgProjectInfo {
    name: Option<String>,
    version: Option<String>,
    description: Option<String>,
    requires_python: Option<String>,
    scripts: BTreeMap<String, String>,
    gui_scripts: BTreeMap<String, String>,
}

fn read_setup_cfg_project_info(path: &Path) -> Result<SetupCfgProjectInfo> {
    let contents = fs::read_to_string(path)?;
    let mut info = SetupCfgProjectInfo::default();
    let mut section = String::new();
    let mut collecting: Option<String> = None;

    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            section = trimmed
                .trim_matches(&['[', ']'][..])
                .to_ascii_lowercase();
            collecting = None;
            continue;
        }
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        match section.as_str() {
            "metadata" => {
                let Some((raw_key, raw_value)) = line.split_once('=') else {
                    continue;
                };
                let key = raw_key.trim().to_ascii_lowercase();
                let value = raw_value.trim();
                if value.is_empty() || value.starts_with('#') {
                    continue;
                }
                match key.as_str() {
                    "name" => info.name = Some(value.to_string()),
                    "version" => info.version = Some(value.to_string()),
                    "description" => info.description = Some(value.to_string()),
                    _ => {}
                }
            }
            "options" => {
                let Some((raw_key, raw_value)) = line.split_once('=') else {
                    continue;
                };
                let key = raw_key.trim().to_ascii_lowercase();
                if key != "python_requires" {
                    continue;
                }
                let value = raw_value.trim();
                if value.is_empty() || value.starts_with('#') {
                    continue;
                }
                info.requires_python = Some(value.to_string());
            }
            "options.entry_points" => {
                if let Some(group) = collecting.as_deref() {
                    if line.chars().next().is_some_and(char::is_whitespace) {
                        if trimmed.starts_with('#') {
                            continue;
                        }
                        if let Some((raw_name, raw_value)) = trimmed.split_once('=') {
                            let name = raw_name.trim();
                            let value = raw_value.trim();
                            if name.is_empty() || value.is_empty() {
                                continue;
                            }
                            match group {
                                "console_scripts" => {
                                    info.scripts.insert(name.to_string(), value.to_string());
                                }
                                "gui_scripts" => {
                                    info.gui_scripts.insert(name.to_string(), value.to_string());
                                }
                                _ => {}
                            }
                        }
                        continue;
                    }
                    collecting = None;
                }

                let Some((raw_key, raw_value)) = line.split_once('=') else {
                    continue;
                };
                let key = raw_key.trim().to_ascii_lowercase();
                let value = raw_value.trim();
                if value.is_empty() {
                    collecting = Some(key);
                }
            }
            _ => {}
        }
    }

    Ok(info)
}

fn ensure_project_metadata(doc: &mut DocumentMut, root: &Path) -> bool {
    let mut changed = false;
    if !doc.contains_key("project") {
        changed = true;
    }
    let project = doc
        .entry("project")
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .expect("project table");
    let dynamic_fields: Vec<String> = project
        .get("dynamic")
        .and_then(Item::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(toml_edit::Value::as_str)
                .map(std::string::ToString::to_string)
                .collect()
        })
        .unwrap_or_default();
    let is_dynamic = |field: &str| dynamic_fields.iter().any(|entry| entry == field);

    // When onboarding legacy setuptools projects (setup.py / setup.cfg), prefer leaving
    // existing metadata to the build backend by marking select fields as dynamic when
    // we can detect that metadata is provided outside `pyproject.toml`.
    //
    // This avoids setuptools treating existing metadata as "defined outside pyproject"
    // (and in some cases crashing while normalizing missing values), while also
    // avoiding declaring dynamic fields that setuptools cannot satisfy.
    let setup_cfg_path = root.join("setup.cfg");
    if setup_cfg_path.exists() {
        let mut inferred = Vec::new();
        if let Ok(contents) = fs::read_to_string(&setup_cfg_path) {
            let mut section = String::new();
            for line in contents.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with('[') && trimmed.ends_with(']') {
                    section = trimmed
                        .trim_matches(&['[', ']'][..])
                        .to_ascii_lowercase();
                    continue;
                }
                if section != "metadata" {
                    continue;
                }
                let Some((raw_key, _)) = trimmed.split_once('=') else {
                    continue;
                };
                let key = raw_key.trim().to_ascii_lowercase();
                match key.as_str() {
                    "long_description" | "description_file" | "long_description_content_type" => {
                        inferred.push("readme");
                    }
                    "license" | "license_files" => inferred.push("license"),
                    "classifier" | "classifiers" => inferred.push("classifiers"),
                    "url" | "project_urls" => inferred.push("urls"),
                    "author" | "author_email" => inferred.push("authors"),
                    "maintainer" | "maintainer_email" => inferred.push("maintainers"),
                    _ => {}
                }
            }
        }
        inferred.sort();
        inferred.dedup();

        let mut missing = Vec::new();
        for field in inferred {
            if project.contains_key(field) || is_dynamic(field) {
                continue;
            }
            missing.push(field);
        }
        if !missing.is_empty() {
            let dynamic = project
                .entry("dynamic")
                .or_insert_with(|| Item::Value(TomlValue::Array(Array::new())))
                .as_array_mut()
                .expect("project.dynamic array");
            changed = true;
            for field in missing {
                dynamic.push(field);
            }
        }

        if let Ok(info) = read_setup_cfg_project_info(&setup_cfg_path) {
            let should_override = project
                .get("description")
                .and_then(Item::as_str)
                .is_some_and(|desc| desc == "Onboarded by px");
            if should_override {
                if !is_dynamic("name") {
                    if let Some(name) = info.name.as_deref() {
                        if project.get("name").and_then(Item::as_str) != Some(name) {
                            changed = true;
                            project.insert("name", Item::Value(TomlValue::from(name)));
                        }
                    }
                }
                if !is_dynamic("version") {
                    if let Some(version) = info.version.as_deref() {
                        let looks_static =
                            !version.contains(':') && !version.contains(' ') && !version.is_empty();
                        if looks_static && project.get("version").and_then(Item::as_str) != Some(version) {
                            changed = true;
                            project.insert("version", Item::Value(TomlValue::from(version)));
                        }
                    }
                }
                if !is_dynamic("requires-python") {
                    if let Some(req) = info.requires_python.as_deref() {
                        if project.get("requires-python").and_then(Item::as_str) != Some(req) {
                            changed = true;
                            project.insert("requires-python", Item::Value(TomlValue::from(req)));
                        }
                    }
                }
                if !is_dynamic("description") {
                    if let Some(description) = info.description.as_deref() {
                        if project.get("description").and_then(Item::as_str) != Some(description) {
                            changed = true;
                            project.insert("description", Item::Value(TomlValue::from(description)));
                        }
                    }
                }
            }

            if !info.scripts.is_empty() {
                let scripts = project
                    .entry("scripts")
                    .or_insert_with(|| Item::Table(Table::new()))
                    .as_table_mut()
                    .expect("project.scripts table");
                for (name, value) in info.scripts {
                    if !scripts.contains_key(&name) {
                        changed = true;
                        scripts.insert(&name, Item::Value(TomlValue::from(value)));
                    }
                }
            }
            if !info.gui_scripts.is_empty() {
                let gui_scripts = project
                    .entry("gui-scripts")
                    .or_insert_with(|| Item::Table(Table::new()))
                    .as_table_mut()
                    .expect("project.gui-scripts table");
                for (name, value) in info.gui_scripts {
                    if !gui_scripts.contains_key(&name) {
                        changed = true;
                        gui_scripts.insert(&name, Item::Value(TomlValue::from(value)));
                    }
                }
            }
        }
    }

    if !is_dynamic("name") && !project.contains_key("name") {
        changed = true;
        project.insert(
            "name",
            Item::Value(TomlValue::from(sanitize_package_candidate(root))),
        );
    }
    if !is_dynamic("version") && !project.contains_key("version") {
        changed = true;
        project.insert("version", Item::Value(TomlValue::from("0.1.0")));
    }
    if !is_dynamic("requires-python") && !project.contains_key("requires-python") {
        changed = true;
        project.insert("requires-python", Item::Value(TomlValue::from(">=3.11")));
    }
    if !is_dynamic("dependencies") && !project.contains_key("dependencies") {
        changed = true;
        project.insert("dependencies", Item::Value(TomlValue::Array(Array::new())));
    }
    if !doc.contains_key("tool") {
        changed = true;
    }
    let tool = doc
        .entry("tool")
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .expect("tool table");
    if !tool.contains_key("px") {
        changed = true;
    }
    tool.entry("px").or_insert(Item::Table(Table::new()));
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::manifest::{
        read_dependencies_from_doc, read_optional_dependency_group, requirement_display_name,
        TOMLI_W_REQUIREMENT,
    };
    use tempfile::tempdir;
    use toml_edit::DocumentMut;

    fn pkg(spec: &str, scope: &str, source: &str) -> OnboardPackagePlan {
        OnboardPackagePlan {
            name: requirement_display_name(spec),
            requested: spec.to_string(),
            scope: scope.to_string(),
            source: source.to_string(),
        }
    }

    fn pyproject_with_backend(build_block: &str) -> String {
        format!(
            r#"[project]
name = "backend-demo"
version = "0.1.0"
requires-python = ">=3.11"
dependencies = ["base-project==1.0.0"]

{build_block}
"#
        )
    }

    #[test]
    fn prepare_plan_preserves_various_build_backends() -> Result<()> {
        let cases = vec![
            (
                "setuptools",
                r#"[build-system]
requires = ["setuptools>=70", "wheel"]
build-backend = "setuptools.build_meta"
"#,
                "setuptools.build_meta",
            ),
            (
                "hatchling",
                r#"[build-system]
requires = ["hatchling>=1.22"]
build-backend = "hatchling.build"
"#,
                "hatchling.build",
            ),
            (
                "flit",
                r#"[build-system]
requires = ["flit-core>=3.9.0"]
build-backend = "flit_core.buildapi"
"#,
                "flit_core.buildapi",
            ),
            (
                "poetry",
                r#"[build-system]
requires = ["poetry-core>=1.9.0"]
build-backend = "poetry.core.masonry.api"
"#,
                "poetry.core.masonry.api",
            ),
            (
                "pdm",
                r#"[build-system]
requires = ["pdm-backend"]
build-backend = "pdm.backend"
"#,
                "pdm.backend",
            ),
        ];

        for (label, build_block, expected_backend) in cases {
            let dir = tempdir()?;
            let pyproject_path = dir.path().join("pyproject.toml");
            fs::write(&pyproject_path, pyproject_with_backend(build_block))?;

            let packages = vec![
                pkg("httpx==0.27.0", "prod", "requirements.txt"),
                pkg("pytest==8.2.0", "dev", "requirements-dev.txt"),
            ];

            let plan = prepare_pyproject_plan(dir.path(), &pyproject_path, false, &packages)?;
            assert!(plan.needs_backup(), "expected backup for {label}");
            assert!(!plan.created, "pyproject should already exist for {label}");

            let contents = plan.contents.expect("pyproject should be rewritten");
            let doc: DocumentMut = contents.parse()?;

            let deps = read_dependencies_from_doc(&doc);
            assert!(
                deps.contains(&"base-project==1.0.0".to_string()),
                "base dependency missing for {label}"
            );
            assert!(
                deps.contains(&"httpx==0.27.0".to_string()),
                "merged prod dependency missing for {label}"
            );

            let dev_specs = read_optional_dependency_group(&doc, "px-dev");
            assert!(
                dev_specs.contains(&"pytest==8.2.0".to_string()),
                "dev dependency missing for {label}"
            );

            let backend = doc["build-system"]["build-backend"].as_str().unwrap_or("");
            assert_eq!(
                backend, expected_backend,
                "build backend altered for {label}"
            );

            let requires = doc["build-system"]["requires"]
                .as_array()
                .expect("build-system.requires should remain an array");
            assert!(
                !requires.is_empty(),
                "build-system.requires should be preserved for {label}"
            );
        }

        Ok(())
    }

    #[test]
    fn ensure_metadata_respects_dynamic_version() -> Result<()> {
        let dir = tempdir()?;
        let mut doc: DocumentMut = r#"[project]
name = "flit"
dynamic = ["version", "description"]
requires-python = ">=3.8"
dependencies = ["flit_core>=3.11,<4"]

[build-system]
requires = ["flit_core>=3.11,<4"]
build-backend = "flit_core.buildapi"
"#
        .parse()?;

        ensure_project_metadata(&mut doc, dir.path());

        let project = doc["project"].as_table().expect("project table");
        assert!(
            project.get("version").is_none(),
            "version should remain dynamic"
        );
        let dynamic = project["dynamic"]
            .as_array()
            .expect("dynamic should remain an array");
        assert!(
            dynamic
                .iter()
                .any(|item| item.as_str().is_some_and(|value| value == "version")),
            "dynamic version entry should be preserved"
        );
        Ok(())
    }

    #[test]
    fn prepare_plan_writes_metadata_when_pyproject_missing_project_table() -> Result<()> {
        let dir = tempdir()?;
        let pyproject_path = dir.path().join("pyproject.toml");
        fs::write(
            &pyproject_path,
            r#"[build-system]
requires = ["setuptools>=42", "packaging"]
"#,
        )?;

        let plan = prepare_pyproject_plan(dir.path(), &pyproject_path, false, &[])?;
        let contents = plan.contents.expect("pyproject should be updated");
        let doc: DocumentMut = contents.parse()?;
        assert!(
            doc.get("project").and_then(Item::as_table).is_some(),
            "pyproject should include a [project] table"
        );
        assert!(
            doc["project"]["name"].as_str().is_some(),
            "[project].name should be populated"
        );
        assert!(
            doc.get("tool")
                .and_then(Item::as_table)
                .and_then(|tool| tool.get("px"))
                .and_then(Item::as_table)
                .is_some(),
            "pyproject should include [tool.px]"
        );
        let dev_specs = read_optional_dependency_group(&doc, "px-dev");
        assert!(
            dev_specs.contains(&"setuptools>=42".to_string()),
            "build-system.requires should be added to px-dev group"
        );
        assert!(
            dev_specs.contains(&"packaging".to_string()),
            "build-system.requires should be added to px-dev group"
        );
        Ok(())
    }

    #[test]
    fn legacy_setuptools_dynamic_fields_are_inferred_from_setup_cfg() -> Result<()> {
        let dir = tempdir()?;
        fs::write(
            dir.path().join("setup.cfg"),
            r#"[metadata]
long_description = file: README.md
maintainer = Example Maintainer
"#,
        )?;

        let mut doc: DocumentMut = r#"[build-system]
requires = ["setuptools>=70", "wheel"]
build-backend = "setuptools.build_meta"
"#
        .parse()?;
        ensure_project_metadata(&mut doc, dir.path());
        let dynamic = doc["project"]["dynamic"]
            .as_array()
            .expect("project.dynamic array");
        let entries: Vec<_> = dynamic.iter().filter_map(|item| item.as_str()).collect();
        assert!(
            entries.iter().any(|entry| *entry == "readme"),
            "readme should be marked dynamic when setup.cfg provides long_description"
        );
        assert!(
            entries.iter().any(|entry| *entry == "maintainers"),
            "maintainers should only be marked dynamic when setup.cfg provides maintainer metadata"
        );

        let dir = tempdir()?;
        fs::write(
            dir.path().join("setup.cfg"),
            r#"[metadata]
long_description = file: README.md
"#,
        )?;
        let mut doc: DocumentMut = r#"[build-system]
requires = ["setuptools>=70", "wheel"]
build-backend = "setuptools.build_meta"
"#
        .parse()?;
        ensure_project_metadata(&mut doc, dir.path());
        let dynamic = doc["project"]["dynamic"]
            .as_array()
            .expect("project.dynamic array");
        let entries: Vec<_> = dynamic.iter().filter_map(|item| item.as_str()).collect();
        assert!(
            entries.iter().any(|entry| *entry == "readme"),
            "readme should be marked dynamic when setup.cfg provides long_description"
        );
        assert!(
            !entries.iter().any(|entry| *entry == "maintainers"),
            "maintainers should not be marked dynamic when setup.cfg omits maintainer metadata"
        );
        Ok(())
    }

    #[test]
    fn prepare_plan_reads_setup_cfg_entry_points() -> Result<()> {
        let dir = tempdir()?;
        fs::write(
            dir.path().join("setup.cfg"),
            r#"[metadata]
name = pre_commit
version = 4.5.0
description = A demo

[options]
python_requires = >=3.10

[options.entry_points]
console_scripts =
    pre-commit = pre_commit.main:main
"#,
        )?;

        let pyproject_path = dir.path().join("pyproject.toml");
        let packages = vec![pkg("cfgv==3.5.0", "prod", "setup.cfg")];
        let plan = prepare_pyproject_plan(dir.path(), &pyproject_path, false, &packages)?;
        assert!(plan.created);
        let contents = plan.contents.expect("pyproject should be created");
        let doc: DocumentMut = contents.parse()?;

        assert_eq!(doc["project"]["name"].as_str(), Some("pre_commit"));
        assert_eq!(doc["project"]["version"].as_str(), Some("4.5.0"));
        assert_eq!(doc["project"]["requires-python"].as_str(), Some(">=3.10"));
        assert_eq!(doc["project"]["scripts"]["pre-commit"].as_str(), Some("pre_commit.main:main"));

        Ok(())
    }

    #[test]
    fn lock_only_plan_leaves_existing_pyproject_unmodified() -> Result<()> {
        let dir = tempdir()?;
        let pyproject_path = dir.path().join("pyproject.toml");
        fs::write(
            &pyproject_path,
            pyproject_with_backend(
                r#"[build-system]
requires = ["hatchling>=1.22"]
build-backend = "hatchling.build"
"#,
            ),
        )?;

        let packages = vec![pkg("httpx==0.27.0", "prod", "requirements.txt")];

        let plan = prepare_pyproject_plan(dir.path(), &pyproject_path, true, &packages)?;
        assert!(
            plan.contents.is_none(),
            "lock-only should not rewrite pyproject"
        );
        assert!(
            !plan.created,
            "existing pyproject should not be marked created"
        );
        assert!(
            !plan.needs_backup(),
            "no backup needed when no contents are written"
        );

        Ok(())
    }

    #[test]
    fn prepare_plan_adds_tomli_w_for_hatch_projects() -> Result<()> {
        let dir = tempdir()?;
        let pyproject_path = dir.path().join("pyproject.toml");
        fs::write(
            &pyproject_path,
            r#"[project]
name = "demo"
version = "0.1.0"
requires-python = ">=3.11"
dependencies = []

[build-system]
requires = ["hatchling>=1.22"]
build-backend = "hatchling.build"
"#,
        )?;

        let plan = prepare_pyproject_plan(dir.path(), &pyproject_path, false, &[])?;
        let contents = plan.contents.expect("pyproject should be updated");
        let doc: DocumentMut = contents.parse()?;
        let dev_specs = read_optional_dependency_group(&doc, "px-dev");
        assert!(
            dev_specs.contains(&TOMLI_W_REQUIREMENT.to_string()),
            "hatch projects should record tomli-w"
        );
        Ok(())
    }
}
