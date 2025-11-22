use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{bail, Result};
use time::{format_description, OffsetDateTime};
use toml_edit::{Array, DocumentMut, Item, Table, Value as TomlValue};

use crate::{
    init::sanitize_package_candidate,
    manifest::{
        merge_dependency_specs, merge_dev_dependency_specs, normalize_onboard_path, relative_path,
        OnboardPackagePlan,
    },
    snapshot::ensure_pyproject_exists,
};

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
    ensure_project_metadata(&mut doc, root);

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
    if prod_specs.is_empty() && !dev_specs.is_empty() {
        prod_specs.append(&mut dev_specs);
    }

    let mut changed = false;
    changed |= merge_dependency_specs(&mut doc, &prod_specs);
    changed |= merge_dev_dependency_specs(&mut doc, &dev_specs);

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

fn ensure_project_metadata(doc: &mut DocumentMut, root: &Path) {
    let project = doc
        .entry("project")
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .expect("project table");
    if !project.contains_key("name") {
        project.insert(
            "name",
            Item::Value(TomlValue::from(sanitize_package_candidate(root))),
        );
    }
    if !project.contains_key("version") {
        project.insert("version", Item::Value(TomlValue::from("0.1.0")));
    }
    if !project.contains_key("requires-python") {
        project.insert("requires-python", Item::Value(TomlValue::from(">=3.11")));
    }
    if !project.contains_key("dependencies") {
        project.insert("dependencies", Item::Value(TomlValue::Array(Array::new())));
    }
    let tool = doc
        .entry("tool")
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .expect("tool table");
    tool.entry("px").or_insert(Item::Table(Table::new()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{
        read_dependencies_from_doc, read_optional_dependency_group, requirement_display_name,
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
}
