use std::fs;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use toml_edit::DocumentMut;

use px_domain::api::px_options_from_doc;
use px_domain::api::{
    read_workspace_config, workspace_manifest_fingerprint, ProjectSnapshot, WorkspaceConfig,
};

use super::{WorkspaceMember, WorkspaceSnapshot};

pub(crate) fn load_workspace_snapshot(root: &Path) -> Result<WorkspaceSnapshot> {
    let config = read_workspace_config(root)?;
    let mut members = Vec::new();
    for rel in &config.members {
        let member_root = config.root.join(rel);
        let abs = member_root.canonicalize().with_context(|| {
            format!("workspace member {} does not exist", member_root.display())
        })?;
        let snapshot = ProjectSnapshot::read_from(&abs)?;
        let rel_path = abs
            .strip_prefix(&config.root)
            .unwrap_or(&abs)
            .display()
            .to_string();
        members.push(WorkspaceMember {
            rel_path,
            root: abs,
            snapshot,
        });
    }
    let python_override = config.python.clone();
    let python_requirement = derive_workspace_python(&config, &members)?;
    let manifest_fingerprint = workspace_manifest_fingerprint(
        &config,
        &members
            .iter()
            .map(|m| m.snapshot.clone())
            .collect::<Vec<_>>(),
    )?;
    let mut dependencies = Vec::new();
    for member in &members {
        dependencies.extend(member.snapshot.requirements.clone());
    }
    dependencies.retain(|dep| !dep.trim().is_empty());

    let name = config
        .name
        .clone()
        .or_else(|| {
            config
                .root
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| "workspace".to_string());
    let px_options = {
        let contents = fs::read_to_string(&config.manifest_path)
            .with_context(|| format!("failed to read {}", config.manifest_path.display()))?;
        let doc: DocumentMut = contents
            .parse()
            .with_context(|| format!("failed to parse {}", config.manifest_path.display()))?;
        px_options_from_doc(&doc)
    };

    Ok(WorkspaceSnapshot {
        lock_path: config.root.join("px.workspace.lock"),
        config,
        members,
        manifest_fingerprint,
        python_requirement,
        python_override,
        dependencies,
        name,
        px_options,
    })
}

pub(crate) fn derive_workspace_python(
    config: &WorkspaceConfig,
    members: &[WorkspaceMember],
) -> Result<String> {
    if let Some(py) = &config.python {
        return Ok(py.clone());
    }
    if members.is_empty() {
        return Ok(">=3.11".to_string());
    }
    let mut requirements = members
        .iter()
        .map(|m| m.snapshot.python_requirement.clone())
        .collect::<Vec<_>>();
    requirements.sort();
    requirements.dedup();
    if requirements.len() == 1 {
        Ok(requirements[0].clone())
    } else {
        Err(anyhow!(
            "workspace members disagree on requires-python; set [tool.px.workspace].python"
        ))
    }
}
