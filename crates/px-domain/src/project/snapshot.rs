use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Context, Result};
use toml_edit::{DocumentMut, Item};

use super::manifest::{
    manifest_fingerprint, project_table, px_options_from_doc, read_build_system_requires,
    read_dependencies_from_doc, resolve_dependency_groups, select_dependency_groups,
    DependencyGroupSource, PxOptions,
};

#[derive(Clone, Debug)]
pub struct ProjectSnapshot {
    pub root: PathBuf,
    pub manifest_path: PathBuf,
    pub lock_path: PathBuf,
    pub name: String,
    pub python_requirement: String,
    pub dependencies: Vec<String>,
    pub dependency_groups: Vec<String>,
    pub declared_dependency_groups: Vec<String>,
    pub dependency_group_source: DependencyGroupSource,
    pub group_dependencies: Vec<String>,
    pub requirements: Vec<String>,
    pub python_override: Option<String>,
    pub px_options: PxOptions,
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
        let doc: DocumentMut = contents
            .parse()
            .with_context(|| format!("failed to parse {}", manifest_path.display()))?;
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
        let selection = select_dependency_groups(&doc);
        let dependency_groups = selection.active.clone();
        let declared_dependency_groups = selection.declared.clone();
        let group_dependencies = resolve_dependency_groups(&doc, &dependency_groups)?;
        let build_requires = read_build_system_requires(&doc);
        let mut requirements = dependencies.clone();
        requirements.extend(group_dependencies.clone());
        super::manifest::sort_and_dedupe(&mut requirements);
        let px_options = px_options_from_doc(&doc);
        let python_override = doc
            .get("tool")
            .and_then(Item::as_table)
            .and_then(|tool| tool.get("px"))
            .and_then(Item::as_table)
            .and_then(|px| px.get("python"))
            .and_then(Item::as_str)
            .map(std::string::ToString::to_string);
        let mut fingerprint_requirements = requirements.clone();
        fingerprint_requirements.extend(build_requires.clone());
        super::manifest::sort_and_dedupe(&mut fingerprint_requirements);
        let manifest_fingerprint = manifest_fingerprint(
            &doc,
            &fingerprint_requirements,
            &dependency_groups,
            &px_options,
        )?;
        Ok(Self {
            root: root.to_path_buf(),
            manifest_path,
            lock_path: root.join("px.lock"),
            name,
            python_requirement,
            dependencies,
            dependency_groups,
            declared_dependency_groups,
            dependency_group_source: selection.source,
            group_dependencies,
            requirements,
            python_override,
            px_options,
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
    let doc: DocumentMut = contents
        .parse()
        .with_context(|| format!("failed to parse {}", path.display()))?;
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
    let doc: DocumentMut = contents
        .parse()
        .with_context(|| format!("failed to parse {}", path.display()))?;
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
        assert!(snapshot.dependency_groups.is_empty());
        assert!(snapshot.declared_dependency_groups.is_empty());
        assert_eq!(
            snapshot.dependency_group_source,
            crate::project::manifest::DependencyGroupSource::None
        );
        assert!(snapshot.group_dependencies.is_empty());
        assert_eq!(snapshot.requirements, snapshot.dependencies);
        assert_eq!(snapshot.manifest_path, pyproject);
        assert_eq!(snapshot.lock_path, root.join("px.lock"));
        Ok(())
    }

    #[test]
    fn build_system_requires_affect_fingerprint() -> Result<()> {
        let dir = tempdir()?;
        let root = dir.path();
        let pyproject = root.join("pyproject.toml");
        fs::write(
            &pyproject,
            r#"[build-system]
requires = ["pdm-backend>=2", "setuptools"]
build-backend = "pdm.backend"

[project]
name = "demo-build"
version = "0.2.0"
requires-python = ">=3.11"
dependencies = ["requests==2.32.3"]
"#,
        )?;

        let snapshot = ProjectSnapshot::read_from(root)?;
        assert!(
            snapshot
                .requirements
                .iter()
                .all(|req| !req.contains("pdm-backend")),
            "build-system requirements should not be installed automatically"
        );
        let base_fingerprint = snapshot.manifest_fingerprint.clone();

        fs::write(
            &pyproject,
            r#"[build-system]
requires = ["flit_core>=3.8.0"]
build-backend = "flit_core.buildapi"

[project]
name = "demo-build"
version = "0.2.0"
requires-python = ">=3.11"
dependencies = ["requests==2.32.3"]
"#,
        )?;
        let updated_snapshot = ProjectSnapshot::read_from(root)?;
        assert_ne!(
            base_fingerprint, updated_snapshot.manifest_fingerprint,
            "build-system requirements should influence the manifest fingerprint"
        );
        Ok(())
    }

    #[test]
    fn dependency_groups_are_resolved_with_includes() -> Result<()> {
        let dir = tempdir()?;
        let root = dir.path();
        let pyproject = root.join("pyproject.toml");
        fs::write(
            &pyproject,
            r#"[project]
name = "demo"
version = "0.1.0"
requires-python = ">=3.11"

[dependency-groups]
extra = ["click==8.1.7"]
test = ["pytest>=7", {include-group = "extra"}]

[tool.px]
"#,
        )?;

        let snapshot = ProjectSnapshot::read_from(root)?;
        assert_eq!(snapshot.dependency_groups, vec!["extra", "test"]);
        assert_eq!(
            snapshot.declared_dependency_groups,
            vec!["extra".to_string(), "test".to_string()]
        );
        assert_eq!(
            snapshot.dependency_group_source,
            crate::project::manifest::DependencyGroupSource::DeclaredDefault
        );
        assert_eq!(
            snapshot.group_dependencies,
            vec!["click==8.1.7", "pytest>=7"]
        );
        assert_eq!(snapshot.requirements, vec!["click==8.1.7", "pytest>=7"]);
        Ok(())
    }

    #[test]
    fn optional_dependencies_with_dev_names_are_selected_by_default() -> Result<()> {
        let dir = tempdir()?;
        let root = dir.path();
        let pyproject = root.join("pyproject.toml");
        fs::write(
            &pyproject,
            r#"[project]
name = "demo"
version = "0.1.0"
requires-python = ">=3.11"

[project.optional-dependencies]
test = ["pytest>=7"]
doc = ["sphinx>=7"]

[tool.px]
"#,
        )?;

        let snapshot = ProjectSnapshot::read_from(root)?;
        assert_eq!(snapshot.dependency_groups, vec!["doc", "test"]);
        assert_eq!(
            snapshot.declared_dependency_groups,
            vec!["doc".to_string(), "test".to_string()]
        );
        assert_eq!(
            snapshot.dependency_group_source,
            crate::project::manifest::DependencyGroupSource::DeclaredDefault
        );
        assert_eq!(snapshot.group_dependencies, vec!["pytest>=7", "sphinx>=7"]);
        assert_eq!(snapshot.requirements, vec!["pytest>=7", "sphinx>=7"]);
        Ok(())
    }

    #[test]
    fn include_groups_config_overrides_declared_defaults() -> Result<()> {
        let dir = tempdir()?;
        let root = dir.path();
        let pyproject = root.join("pyproject.toml");
        fs::write(
            &pyproject,
            r#"[project]
name = "demo"
version = "0.1.0"
requires-python = ">=3.11"

[dependency-groups]
dev = ["ruff==0.6.8"]
test = ["pytest==8.3.3"]

[tool.px]

[tool.px.dependencies]
include-groups = ["test"]
"#,
        )?;

        let snapshot = ProjectSnapshot::read_from(root)?;
        assert_eq!(snapshot.dependency_groups, vec!["test"]);
        assert_eq!(
            snapshot.declared_dependency_groups,
            vec!["dev".to_string(), "test".to_string()]
        );
        assert_eq!(
            snapshot.dependency_group_source,
            crate::project::manifest::DependencyGroupSource::IncludeConfig
        );
        assert_eq!(snapshot.group_dependencies, vec!["pytest==8.3.3"]);
        assert_eq!(snapshot.requirements, vec!["pytest==8.3.3"]);
        Ok(())
    }
}
