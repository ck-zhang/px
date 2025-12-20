use std::fs;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use anyhow::Result;
use serde_json::json;

use crate::{CommandContext, ExecutionOutcome};
use px_domain::api::ManifestEditor;

use super::{workspace_sync, WorkspaceScope, WorkspaceSyncRequest};

pub fn workspace_add(
    ctx: &CommandContext,
    request: &crate::project::ProjectAddRequest,
    scope: WorkspaceScope,
) -> Result<ExecutionOutcome> {
    let WorkspaceScope::Member {
        workspace,
        member_root,
    } = scope
    else {
        return Ok(ExecutionOutcome::user_error(
            "px add: not inside a workspace member",
            json!({ "hint": "run inside a configured workspace member" }),
        ));
    };
    if request.specs.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            "provide at least one dependency",
            json!({ "hint": "run `px add name==version`" }),
        ));
    }
    let cleaned_specs: Vec<String> = request
        .specs
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if cleaned_specs.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            "provide at least one dependency",
            json!({ "hint": "run `px add name==version`" }),
        ));
    }
    let manifest_path = member_root.join("pyproject.toml");
    let backup = WorkspaceBackup::capture(&manifest_path, &workspace.lock_path)?;
    let mut needs_restore = true;
    let outcome = (|| -> Result<ExecutionOutcome> {
        let mut editor = ManifestEditor::open(&manifest_path)?;
        let report = editor.add_specs(&cleaned_specs)?;
        if report.added.is_empty() && report.updated.is_empty() {
            needs_restore = request.dry_run;
            return Ok(ExecutionOutcome::success(
                "dependencies already satisfied",
                json!({
                    "pyproject": manifest_path.display().to_string(),
                    "dry_run": request.dry_run,
                }),
            ));
        }
        if request.dry_run {
            return Ok(ExecutionOutcome::success(
                "planned dependency changes (dry-run)",
                json!({
                    "pyproject": manifest_path.display().to_string(),
                    "added": report.added,
                    "updated": report.updated,
                    "dry_run": true,
                }),
            ));
        }
        workspace_sync(
            ctx,
            WorkspaceScope::Member {
                workspace: workspace.clone(),
                member_root,
            },
            &WorkspaceSyncRequest {
                frozen: false,
                dry_run: false,
                force_resolve: true,
            },
        )?;
        needs_restore = false;
        Ok(ExecutionOutcome::success(
            "updated workspace dependencies",
            json!({
                "pyproject": manifest_path.display().to_string(),
                "lockfile": workspace.lock_path.display().to_string(),
                "added": report.added,
                "updated": report.updated,
            }),
        ))
    })();
    if needs_restore {
        backup.restore()?;
    }
    outcome
}

pub fn workspace_remove(
    ctx: &CommandContext,
    request: &crate::project::ProjectRemoveRequest,
    scope: WorkspaceScope,
) -> Result<ExecutionOutcome> {
    let WorkspaceScope::Member {
        workspace,
        member_root,
    } = scope
    else {
        return Ok(ExecutionOutcome::user_error(
            "px remove: not inside a workspace member",
            json!({ "hint": "run inside a configured workspace member" }),
        ));
    };
    if request.specs.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            "provide at least one dependency",
            json!({ "hint": "run `px remove name`" }),
        ));
    }
    let manifest_path = member_root.join("pyproject.toml");
    let backup = WorkspaceBackup::capture(&manifest_path, &workspace.lock_path)?;
    let mut needs_restore = true;
    let outcome = (|| -> Result<ExecutionOutcome> {
        let mut editor = ManifestEditor::open(&manifest_path)?;
        let report = editor.remove_specs(&request.specs)?;
        if report.removed.is_empty() {
            needs_restore = request.dry_run;
            return Ok(ExecutionOutcome::user_error(
                "none of the requested dependencies are direct dependencies",
                json!({
                    "pyproject": manifest_path.display().to_string(),
                    "requested": request.specs,
                }),
            ));
        }
        if request.dry_run {
            return Ok(ExecutionOutcome::success(
                "planned dependency removals (dry-run)",
                json!({
                    "pyproject": manifest_path.display().to_string(),
                    "removed": report.removed,
                    "dry_run": true,
                }),
            ));
        }
        workspace_sync(
            ctx,
            WorkspaceScope::Member {
                workspace: workspace.clone(),
                member_root,
            },
            &WorkspaceSyncRequest {
                frozen: false,
                dry_run: false,
                force_resolve: true,
            },
        )?;
        needs_restore = false;
        Ok(ExecutionOutcome::success(
            "removed dependencies and updated workspace",
            json!({
                "pyproject": manifest_path.display().to_string(),
                "lockfile": workspace.lock_path.display().to_string(),
                "removed": report.removed,
            }),
        ))
    })();
    if needs_restore {
        backup.restore()?;
    }
    outcome
}

pub fn workspace_update(
    ctx: &CommandContext,
    _request: &crate::project::ProjectUpdateRequest,
    scope: WorkspaceScope,
) -> Result<ExecutionOutcome> {
    let workspace = match scope {
        WorkspaceScope::Root(ws) | WorkspaceScope::Member { workspace: ws, .. } => ws,
    };
    workspace_sync(
        ctx,
        WorkspaceScope::Root(workspace.clone()),
        &WorkspaceSyncRequest {
            frozen: false,
            dry_run: false,
            force_resolve: true,
        },
    )
}

struct WorkspaceBackup {
    manifest_path: PathBuf,
    lock_path: PathBuf,
    manifest_contents: String,
    lock_contents: Option<String>,
    manifest_permissions: fs::Permissions,
    lock_permissions: Option<fs::Permissions>,
    lock_preexisting: bool,
}

impl WorkspaceBackup {
    fn capture(manifest_path: &Path, lock_path: &Path) -> Result<Self> {
        let manifest_contents = fs::read_to_string(manifest_path)?;
        let manifest_permissions = fs::metadata(manifest_path)?.permissions();
        let lock_preexisting = lock_path.exists();
        let lock_contents = if lock_preexisting {
            Some(fs::read_to_string(lock_path)?)
        } else {
            None
        };
        let lock_permissions = if lock_preexisting {
            Some(fs::metadata(lock_path)?.permissions())
        } else {
            None
        };
        Ok(Self {
            manifest_path: manifest_path.to_path_buf(),
            lock_path: lock_path.to_path_buf(),
            manifest_contents,
            lock_contents,
            manifest_permissions,
            lock_permissions,
            lock_preexisting,
        })
    }

    fn restore(&self) -> Result<()> {
        self.write_with_permissions(
            &self.manifest_path,
            &self.manifest_contents,
            &self.manifest_permissions,
        )?;
        match (&self.lock_contents, self.lock_preexisting) {
            (Some(contents), _) => {
                let permissions = if let Some(perms) = &self.lock_permissions {
                    perms.clone()
                } else {
                    fs::metadata(&self.lock_path)?.permissions()
                };
                self.write_with_permissions(&self.lock_path, contents, &permissions)?;
            }
            (None, false) => {
                if self.lock_path.exists() {
                    self.remove_with_permissions(&self.lock_path)?;
                }
            }
            (None, true) => {
                debug_assert!(
                    false,
                    "lock_preexisting implies lock contents should have been captured"
                );
            }
        }
        Ok(())
    }

    fn write_with_permissions(
        &self,
        path: &Path,
        contents: &str,
        _permissions: &fs::Permissions,
    ) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, contents)?;
        #[cfg(unix)]
        {
            fs::set_permissions(path, _permissions.clone())?;
        }
        Ok(())
    }

    fn remove_with_permissions(&self, path: &Path) -> Result<()> {
        #[cfg(unix)]
        {
            let perms = fs::metadata(path)?.permissions();
            let mut writable = perms.clone();
            writable.set_mode(0o644);
            fs::set_permissions(path, writable)?;
        }
        fs::remove_file(path)?;
        Ok(())
    }
}
