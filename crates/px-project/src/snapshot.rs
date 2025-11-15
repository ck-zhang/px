use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Context, Result};
use toml_edit::DocumentMut;

use crate::manifest::{project_table, read_dependencies_from_doc};

#[derive(Clone, Debug)]
pub struct ProjectSnapshot {
    pub root: PathBuf,
    pub manifest_path: PathBuf,
    pub lock_path: PathBuf,
    pub name: String,
    pub python_requirement: String,
    pub dependencies: Vec<String>,
}

impl ProjectSnapshot {
    pub fn read_current() -> Result<Self> {
        let root = current_project_root()?;
        Self::read_from(&root)
    }

    pub fn read_from(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        let manifest_path = root.join("pyproject.toml");
        ensure_pyproject_exists(&manifest_path)?;
        let contents = fs::read_to_string(&manifest_path)?;
        let doc: DocumentMut = contents.parse()?;
        let project = project_table(&doc)?;
        let name = project
            .get("name")
            .and_then(|item| item.as_str())
            .ok_or_else(|| anyhow!("pyproject missing [project].name"))?
            .to_string();
        let python_requirement = project
            .get("requires-python")
            .and_then(|item| item.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| ">=3.12".to_string());
        let dependencies = read_dependencies_from_doc(&doc);
        Ok(Self {
            root: root.to_path_buf(),
            manifest_path,
            lock_path: root.join("px.lock"),
            name,
            python_requirement,
            dependencies,
        })
    }
}

pub fn current_project_root() -> Result<PathBuf> {
    env::current_dir().context("unable to determine project root")
}

pub fn project_name_from_pyproject(path: &Path) -> Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }
    let contents = fs::read_to_string(path)?;
    let doc: DocumentMut = contents.parse()?;
    let name = doc
        .get("project")
        .and_then(|item| item.as_table())
        .and_then(|table| table.get("name"))
        .and_then(|item| item.as_str())
        .map(|s| s.to_string());
    Ok(name)
}

pub(crate) fn ensure_pyproject_exists(path: &Path) -> Result<()> {
    if path.exists() {
        Ok(())
    } else {
        let parent = path.parent().unwrap_or(path);
        Err(anyhow!("pyproject.toml not found in {}", parent.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn reads_snapshot_from_disk() -> Result<()> {
        let dir = tempdir()?;
        let root = dir.path();
        let pyproject = root.join("pyproject.toml");
        fs::write(
            &pyproject,
            r#"[project]
name = "demo"
version = "0.1.0"
requires-python = ">=3.12"
dependencies = ["requests==2.32.3"]
"#,
        )?;

        let snapshot = ProjectSnapshot::read_from(root)?;
        assert_eq!(snapshot.name, "demo");
        assert_eq!(snapshot.python_requirement, ">=3.12");
        assert_eq!(snapshot.dependencies, vec!["requests==2.32.3".to_string()]);
        assert_eq!(snapshot.manifest_path, pyproject);
        assert_eq!(snapshot.lock_path, root.join("px.lock"));
        Ok(())
    }
}
