use anyhow::{Context, Result};
use px_domain::api::{discover_workspace_root, workspace_member_for_path};

use super::{load_workspace_snapshot, WorkspaceScope};

/// Determine if CWD is inside a workspace (root or member).
pub fn discover_workspace_scope() -> Result<Option<WorkspaceScope>> {
    let Some(root) = discover_workspace_root()? else {
        return Ok(None);
    };
    let snapshot = load_workspace_snapshot(&root)?;
    let cwd = std::env::current_dir().context("unable to determine current directory")?;
    if let Some(member_root) = workspace_member_for_path(&snapshot.config, &cwd) {
        Ok(Some(WorkspaceScope::Member {
            workspace: snapshot,
            member_root,
        }))
    } else {
        Ok(Some(WorkspaceScope::Root(snapshot)))
    }
}
