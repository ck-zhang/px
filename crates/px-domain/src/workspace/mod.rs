use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Context, Result};
use sha2::{Digest, Sha256};
use toml_edit::{DocumentMut, Item};

use crate::project::snapshot::ProjectSnapshot;

#[derive(Clone, Debug)]
pub struct WorkspaceConfig {
    pub root: PathBuf,
    pub manifest_path: PathBuf,
    pub members: Vec<PathBuf>,
    pub python: Option<String>,
    pub name: Option<String>,
}

/// Walks upward from CWD to find the nearest workspace root (`pyproject.toml` with [tool.px.workspace]).
pub fn discover_workspace_root() -> Result<Option<PathBuf>> {
    let mut dir = env::current_dir().context("unable to determine workspace root")?;
    loop {
        let pyproject = dir.join("pyproject.toml");
        if pyproject.exists() && pyproject_has_workspace(&pyproject)? {
            return Ok(Some(dir));
        }
        if !dir.pop() {
            break;
        }
    }
    Ok(None)
}

fn pyproject_has_workspace(path: &Path) -> Result<bool> {
    let contents = fs::read_to_string(path)?;
    let doc: DocumentMut = contents
        .parse()
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(manifest_has_workspace(&doc))
}

pub fn manifest_has_workspace(doc: &DocumentMut) -> bool {
    doc.get("tool")
        .and_then(Item::as_table)
        .and_then(|table| table.get("px"))
        .and_then(Item::as_table)
        .and_then(|px| px.get("workspace"))
        .is_some()
}

/// Parses `[tool.px.workspace]` from `pyproject.toml` at `root`.
pub fn read_workspace_config(root: &Path) -> Result<WorkspaceConfig> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let manifest_path = root.join("pyproject.toml");
    if !manifest_path.exists() {
        return Err(anyhow!(
            "pyproject.toml not found in workspace root {}",
            root.display()
        ));
    }
    let contents = fs::read_to_string(&manifest_path)?;
    read_workspace_config_from_str(&root, &contents)
}

pub fn read_workspace_config_from_str(root: &Path, contents: &str) -> Result<WorkspaceConfig> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let manifest_path = root.join("pyproject.toml");
    let doc: DocumentMut = contents
        .parse()
        .with_context(|| format!("failed to parse {}", manifest_path.display()))?;
    workspace_config_from_doc(&root, &manifest_path, &doc)
}

pub fn workspace_config_from_doc(
    root: &Path,
    manifest_path: &Path,
    doc: &DocumentMut,
) -> Result<WorkspaceConfig> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let workspace = doc
        .get("tool")
        .and_then(Item::as_table)
        .and_then(|tool| tool.get("px"))
        .and_then(Item::as_table)
        .and_then(|px| px.get("workspace"))
        .and_then(Item::as_table)
        .ok_or_else(|| anyhow!("[tool.px.workspace] not found"))?;

    let members = workspace
        .get("members")
        .and_then(Item::as_array)
        .map(|array| {
            array
                .iter()
                .filter_map(|value| value.as_str())
                .map(PathBuf::from)
                .collect::<Vec<_>>()
        })
        .ok_or_else(|| anyhow!("workspace.members must be an array of strings"))?;

    let python = workspace
        .get("python")
        .and_then(Item::as_str)
        .map(std::string::ToString::to_string);
    let name = workspace
        .get("name")
        .and_then(Item::as_str)
        .map(std::string::ToString::to_string);

    Ok(WorkspaceConfig {
        root,
        manifest_path: manifest_path.to_path_buf(),
        members,
        python,
        name,
    })
}

/// Returns the absolute member root containing `path`, if any.
pub fn workspace_member_for_path(config: &WorkspaceConfig, path: &Path) -> Option<PathBuf> {
    let Ok(cwd) = path.canonicalize() else {
        return None;
    };
    for member in &config.members {
        let member_root = config.root.join(member);
        let Ok(member_root) = member_root.canonicalize() else {
            continue;
        };
        if cwd.starts_with(&member_root) {
            return Some(member_root);
        }
    }
    None
}

/// Computes a deterministic fingerprint for the workspace manifest + member manifests.
pub fn workspace_manifest_fingerprint(
    config: &WorkspaceConfig,
    members: &[ProjectSnapshot],
) -> Result<String> {
    let mut hasher = Sha256::new();
    if let Some(name) = &config.name {
        hasher.update(name.trim().to_lowercase().as_bytes());
    } else if let Some(dir_name) = config.root.file_name().and_then(|n| n.to_str()) {
        hasher.update(dir_name.as_bytes());
    }
    if let Some(py) = &config.python {
        hasher.update(py.trim().as_bytes());
    }
    let mut entries = members
        .iter()
        .map(|snapshot| {
            let rel = snapshot
                .root
                .strip_prefix(&config.root)
                .unwrap_or(&snapshot.root)
                .display()
                .to_string();
            (rel, snapshot.manifest_fingerprint.clone())
        })
        .collect::<Vec<_>>();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    for (rel, fingerprint) in entries {
        hasher.update(rel.as_bytes());
        hasher.update(b"\n");
        hasher.update(fingerprint.as_bytes());
        hasher.update(b"\n");
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write_member(root: &Path, rel: &str, name: &str, deps: &[&str]) -> ProjectSnapshot {
        let member_root = root.join(rel);
        fs::create_dir_all(&member_root).unwrap();
        let deps_str = deps
            .iter()
            .map(|d| format!("\"{d}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let manifest = format!(
            r#"[project]
name = "{name}"
version = "0.1.0"
requires-python = ">=3.11"
dependencies = [{deps}]

[tool.px]
"#,
            deps = deps_str
        );
        fs::write(member_root.join("pyproject.toml"), manifest).unwrap();
        ProjectSnapshot::read_from(&member_root).unwrap()
    }

    #[test]
    fn fingerprint_stable_regardless_of_member_order() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        let ws_manifest = root.join("pyproject.toml");
        let manifest = r#"[project]
name = "ws"
version = "0.0.0"
requires-python = ">=3.11"

[tool.px.workspace]
members = ["apps/a", "libs/b"]
"#;
        fs::write(&ws_manifest, manifest).unwrap();
        let config = read_workspace_config(root).unwrap();

        let a = write_member(root, "apps/a", "a", &["requests==2.0.0"]);
        let b = write_member(root, "libs/b", "b", &["urllib3==1.26.0"]);

        let fp1 = workspace_manifest_fingerprint(&config, &[a.clone(), b.clone()]).unwrap();
        let fp2 = workspace_manifest_fingerprint(&config, &[b, a]).unwrap();

        assert_eq!(fp1, fp2);
    }

    #[test]
    fn fingerprint_normalizes_root_path() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        let ws_manifest = root.join("pyproject.toml");
        let manifest = r#"[project]
name = "ws"
version = "0.0.0"
requires-python = ">=3.11"

[tool.px.workspace]
members = ["apps/a", "libs/b"]
"#;
        fs::write(&ws_manifest, manifest).unwrap();
        let canonical = read_workspace_config(root).unwrap();
        let noncanonical =
            read_workspace_config(&root.join("..").join(root.file_name().expect("root name")))
                .unwrap();

        let a = write_member(root, "apps/a", "a", &["requests==2.0.0"]);
        let b = write_member(root, "libs/b", "b", &["urllib3==1.26.0"]);

        let fp1 = workspace_manifest_fingerprint(&canonical, &[a.clone(), b.clone()]).unwrap();
        let fp2 = workspace_manifest_fingerprint(&noncanonical, &[a, b]).unwrap();

        assert_eq!(fp1, fp2);
    }
}
