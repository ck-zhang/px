use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use anyhow::Result;
use serde_json::json;

use crate::{dependency_name, CommandContext, ExecutionOutcome};
use pep508_rs::{Requirement as PepRequirement, VersionOrUrl};
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
            json!({ "hint": "run `px add requests`" }),
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
            json!({ "hint": "run `px add requests`" }),
        ));
    }
    let manifest_path = member_root.join("pyproject.toml");
    let backup = WorkspaceBackup::capture(&manifest_path, &workspace.lock_path)?;
    let mut needs_restore = true;
    let outcome = (|| -> Result<ExecutionOutcome> {
        let mut editor = ManifestEditor::open(&manifest_path)?;
        let report = editor.add_specs(&cleaned_specs)?;
        let dependencies_before = editor.dependencies();
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
        let dependencies_after = ManifestEditor::open(&manifest_path)?.dependencies();
        let manifest_changes = pinned_manifest_changes(
            &report.added,
            &report.updated,
            &dependencies_before,
            &dependencies_after,
        );
        let mut details = json!({
            "pyproject": manifest_path.display().to_string(),
            "lockfile": workspace.lock_path.display().to_string(),
            "added": report.added,
            "updated": report.updated,
        });
        if !manifest_changes.is_empty() {
            details["manifest_changes"] = json!(manifest_changes);
        }
        Ok(ExecutionOutcome::success(
            "updated workspace dependencies",
            details,
        ))
    })();
    if needs_restore {
        backup.restore()?;
    }
    outcome
}

fn pinned_manifest_changes(
    added: &[String],
    updated: &[String],
    before: &[String],
    after: &[String],
) -> Vec<serde_json::Value> {
    let before_map = dependency_spec_map(before);
    let after_map = dependency_spec_map(after);
    let mut changes = Vec::new();
    for name in added.iter().chain(updated) {
        let Some(before_spec) = before_map.get(name) else {
            continue;
        };
        let Some(after_spec) = after_map.get(name) else {
            continue;
        };
        if before_spec == after_spec {
            continue;
        }
        if is_exact_pin(after_spec) {
            changes.push(json!({
                "name": name,
                "before": before_spec,
                "after": after_spec,
            }));
        }
    }
    changes
}

fn dependency_spec_map(specs: &[String]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for spec in specs {
        let name = dependency_name(spec);
        if name.is_empty() {
            continue;
        }
        map.insert(name, spec.clone());
    }
    map
}

fn is_exact_pin(spec: &str) -> bool {
    let trimmed = crate::strip_wrapping_quotes(spec.trim());
    let Ok(requirement) = PepRequirement::from_str(trimmed) else {
        return trimmed.contains("==");
    };
    let Some(VersionOrUrl::VersionSpecifier(specifiers)) = requirement.version_or_url else {
        return false;
    };
    let rendered = specifiers.to_string();
    (rendered.starts_with("==") || rendered.starts_with("===")) && !rendered.contains(',')
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
