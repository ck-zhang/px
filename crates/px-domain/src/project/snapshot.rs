use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Context, Result};
use toml_edit::{DocumentMut, Item};

use super::manifest::{manifest_fingerprint, project_table, read_dependencies_from_doc};

#[derive(Clone, Debug)]
pub struct ProjectSnapshot {
    pub root: PathBuf,
    pub manifest_path: PathBuf,
    pub lock_path: PathBuf,
    pub name: String,
    pub python_requirement: String,
    pub dependencies: Vec<String>,
    pub python_override: Option<String>,
    pub manifest_fingerprint: String,
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
            .map_or_else(|| ">=3.11".to_string(), std::string::ToString::to_string);
        let dependencies = read_dependencies_from_doc(&doc);
        let python_override = doc
            .get("tool")
            .and_then(Item::as_table)
            .and_then(|tool| tool.get("px"))
            .and_then(Item::as_table)
            .and_then(|px| px.get("python"))
            .and_then(Item::as_str)
            .map(std::string::ToString::to_string);
        let manifest_fingerprint = manifest_fingerprint(&doc)?;
        Ok(Self {
            root: root.to_path_buf(),
            manifest_path,
            lock_path: root.join("px.lock"),
            name,
            python_requirement,
            dependencies,
            python_override,
            manifest_fingerprint,
        })
    }
}

pub fn current_project_root() -> Result<PathBuf> {
    match discover_project_root()? {
        Some(root) => Ok(root),
        None => Err(anyhow!(
            "No px project found. Run `px init` in your project directory first."
        )),
    }
}

pub fn discover_project_root() -> Result<Option<PathBuf>> {
    let mut dir = env::current_dir().context("unable to determine project root")?;
    loop {
        if dir.join("px.lock").exists() {
            return Ok(Some(dir));
        }
        let pyproject = dir.join("pyproject.toml");
        if pyproject.exists() && pyproject_has_tool_px(&pyproject)? {
            return Ok(Some(dir));
        }
        if !dir.pop() {
            break;
        }
    }
    Ok(None)
}

fn pyproject_has_tool_px(path: &Path) -> Result<bool> {
    let contents = fs::read_to_string(path)?;
    let doc: DocumentMut = contents.parse()?;
    Ok(doc
        .get("tool")
        .and_then(|item| item.as_table())
        .and_then(|table| table.get("px"))
        .is_some())
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
        .map(std::string::ToString::to_string);
    Ok(name)
}

pub fn ensure_pyproject_exists(path: &Path) -> Result<()> {
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
requires-python = ">=3.11"
dependencies = ["requests==2.32.3"]
"#,
        )?;

        let snapshot = ProjectSnapshot::read_from(root)?;
        assert_eq!(snapshot.name, "demo");
        assert_eq!(snapshot.python_requirement, ">=3.11");
        assert_eq!(snapshot.dependencies, vec!["requests==2.32.3".to_string()]);
        assert_eq!(snapshot.manifest_path, pyproject);
        assert_eq!(snapshot.lock_path, root.join("px.lock"));
        Ok(())
    }
}
