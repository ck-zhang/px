use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{bail, Result};
use time::{format_description, OffsetDateTime};
use toml_edit::DocumentMut;

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
    pub fn needs_backup(&self) -> bool {
        self.contents.is_some() && !self.created
    }

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
}

impl BackupManager {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            dir: None,
            entries: Vec::new(),
        }
    }

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
        Ok(())
    }

    pub fn finish(self) -> BackupSummary {
        let dir_rel = self.dir.map(|dir| relative_path(&self.root, &dir));
        BackupSummary {
            files: self.entries,
            directory: dir_rel,
        }
    }

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
        + "requires-python = \">=3.12\"\n"
        + "dependencies = []\n\n[build-system]\n"
        + "requires = [\"setuptools>=70\", \"wheel\"]\n"
        + "build-backend = \"setuptools.build_meta\"\n";
    Ok(template.parse()?)
}
