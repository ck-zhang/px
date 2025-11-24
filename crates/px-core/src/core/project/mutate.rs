use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use anyhow::{anyhow, Result};
use pep508_rs::{Requirement as PepRequirement, VersionOrUrl};
use serde_json::json;

use crate::{
    dependency_name, install_snapshot, is_missing_project_error, manifest_snapshot,
    persist_resolved_dependencies, refresh_project_site, resolve_dependencies_with_effects,
    CommandContext, ExecutionOutcome, InstallOutcome, InstallOverride, InstallState,
    InstallUserError, ManifestSnapshot,
};
use px_domain::ManifestEditor;

use crate::workspace::WorkspaceScope;
use crate::workspace::{
    discover_workspace_scope, workspace_add, workspace_remove, workspace_update,
};

use super::{ensure_mutation_allowed, evaluate_project_state};

#[derive(Clone, Debug)]
pub struct ProjectAddRequest {
    pub specs: Vec<String>,
    pub dry_run: bool,
}

#[derive(Clone, Debug)]
pub struct ProjectRemoveRequest {
    pub specs: Vec<String>,
    pub dry_run: bool,
}

#[derive(Clone, Debug)]
pub struct ProjectUpdateRequest {
    pub specs: Vec<String>,
    pub dry_run: bool,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum MutationCommand {
    Add,
    Remove,
    Update,
}

/// Adds dependency specifications to the current project.
///
/// # Errors
/// Returns an error if the manifest cannot be updated or installation fails.
pub fn project_add(ctx: &CommandContext, request: &ProjectAddRequest) -> Result<ExecutionOutcome> {
    if request.specs.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            "provide at least one dependency",
            json!({ "hint": "run `px add name==version`" }),
        ));
    }

    if let Some(scope) = discover_workspace_scope()? {
        if let WorkspaceScope::Member { .. } = scope {
            return workspace_add(ctx, request, scope);
        }
    }

    let snapshot = manifest_snapshot()?;
    let state_report = evaluate_project_state(ctx, &snapshot)?;
    if let Err(outcome) = ensure_mutation_allowed(&snapshot, &state_report, MutationCommand::Add) {
        return Ok(outcome);
    }

    let root = snapshot.root.clone();
    let pyproject_path = snapshot.manifest_path.clone();
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

    let lock_path = root.join("px.lock");
    let backup = ManifestLockBackup::capture(&pyproject_path, &lock_path)?;
    let mut needs_restore = true;

    let outcome = (|| -> Result<ExecutionOutcome> {
        let mut editor = ManifestEditor::open(&pyproject_path)?;
        let report = editor.add_specs(&cleaned_specs)?;

        if report.added.is_empty() && report.updated.is_empty() {
            needs_restore = request.dry_run;
            return Ok(ExecutionOutcome::success(
                "dependencies already satisfied",
                json!({
                    "pyproject": pyproject_path.display().to_string(),
                    "dry_run": request.dry_run,
                }),
            ));
        }

        if request.dry_run {
            return Ok(ExecutionOutcome::success(
                "planned dependency changes (dry-run)",
                json!({
                    "pyproject": pyproject_path.display().to_string(),
                    "added": report.added,
                    "updated": report.updated,
                    "dry_run": true,
                }),
            ));
        }

        let (snapshot, _install) = match sync_manifest_environment(ctx) {
            Ok(result) => result,
            Err(outcome) => return Ok(outcome),
        };
        needs_restore = false;
        let message = format!(
            "updated dependencies (added {}, updated {})",
            report.added.len(),
            report.updated.len()
        );
        Ok(ExecutionOutcome::success(
            message,
            json!({
                "pyproject": pyproject_path.display().to_string(),
                "lockfile": snapshot.lock_path.display().to_string(),
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

/// Removes dependency specifications from the project.
///
/// # Errors
/// Returns an error if the manifest cannot be updated or installation fails.
pub fn project_remove(
    ctx: &CommandContext,
    request: &ProjectRemoveRequest,
) -> Result<ExecutionOutcome> {
    if request.specs.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            "provide at least one dependency to remove",
            json!({ "hint": "run `px remove name`" }),
        ));
    }

    if let Some(scope) = discover_workspace_scope()? {
        if let WorkspaceScope::Member { .. } = scope {
            return workspace_remove(ctx, request, scope);
        }
    }

    let snapshot = manifest_snapshot()?;
    let state_report = evaluate_project_state(ctx, &snapshot)?;
    if let Err(outcome) = ensure_mutation_allowed(&snapshot, &state_report, MutationCommand::Remove)
    {
        return Ok(outcome);
    }

    let root = snapshot.root.clone();
    let pyproject_path = snapshot.manifest_path.clone();
    let cleaned_specs: Vec<String> = request
        .specs
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if cleaned_specs.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            "dependencies must contain at least one name",
            json!({ "hint": "use bare names like requests==2.32.3" }),
        ));
    }

    let lock_path = root.join("px.lock");
    let backup = ManifestLockBackup::capture(&pyproject_path, &lock_path)?;
    let mut needs_restore = true;

    let outcome = (|| -> Result<ExecutionOutcome> {
        let mut editor = ManifestEditor::open(&pyproject_path)?;
        let report = editor.remove_specs(&cleaned_specs)?;
        if report.removed.is_empty() {
            let names: Vec<String> = cleaned_specs
                .iter()
                .map(|spec| dependency_name(spec))
                .filter(|name| !name.is_empty())
                .collect();
            needs_restore = request.dry_run;
            return Ok(ExecutionOutcome::user_error(
                "package is not a direct dependency",
                json!({
                    "packages": names,
                    "hint": "Use `px why <package>` to inspect transitive requirements.",
                }),
            ));
        }

        if request.dry_run {
            return Ok(ExecutionOutcome::success(
                "planned dependency removals (dry-run)",
                json!({
                    "pyproject": pyproject_path.display().to_string(),
                    "removed": report.removed,
                    "dry_run": true,
                }),
            ));
        }

        let (snapshot, _install) = match sync_manifest_environment(ctx) {
            Ok(result) => result,
            Err(outcome) => return Ok(outcome),
        };
        needs_restore = false;
        Ok(ExecutionOutcome::success(
            "removed dependencies",
            json!({
                "pyproject": pyproject_path.display().to_string(),
                "lockfile": snapshot.lock_path.display().to_string(),
                "removed": report.removed,
            }),
        ))
    })();

    if needs_restore {
        backup.restore()?;
    }

    outcome
}

/// Updates dependencies to their newest allowed versions.
///
/// # Errors
/// Returns an error if dependency resolution or installation fails.
#[allow(clippy::too_many_lines)]
pub fn project_update(
    ctx: &CommandContext,
    request: &ProjectUpdateRequest,
) -> Result<ExecutionOutcome> {
    if let Some(scope) = discover_workspace_scope()? {
        if let WorkspaceScope::Member { .. } = scope {
            return workspace_update(ctx, request, scope);
        }
    }

    let snapshot = manifest_snapshot()?;
    let state_report = evaluate_project_state(ctx, &snapshot)?;
    if let Err(outcome) = ensure_mutation_allowed(&snapshot, &state_report, MutationCommand::Update)
    {
        return Ok(outcome);
    }
    let pyproject_path = snapshot.manifest_path.clone();
    let backup = ManifestLockBackup::capture(&pyproject_path, &snapshot.lock_path)?;
    let mut needs_restore = true;

    let outcome = (|| -> Result<ExecutionOutcome> {
        if snapshot.dependencies.is_empty() {
            return Ok(ExecutionOutcome::user_error(
                "px update: no dependencies declared",
                json!({
                    "pyproject": pyproject_path.display().to_string(),
                    "hint": "add dependencies with `px add` before running update",
                }),
            ));
        }

        let targets: HashSet<String> = request
            .specs
            .iter()
            .map(|spec| dependency_name(spec))
            .filter(|name| !name.is_empty())
            .collect();
        let update_all = targets.is_empty();

        let mut override_specs = snapshot.dependencies.clone();
        let mut ready = Vec::new();
        let mut ready_seen = HashSet::new();
        let mut unsupported = Vec::new();
        let mut unsupported_seen = HashSet::new();
        let mut observed = HashSet::new();

        for spec in &mut override_specs {
            let name = dependency_name(spec);
            if name.is_empty() {
                continue;
            }
            if !update_all && !targets.contains(&name) {
                continue;
            }
            observed.insert(name.clone());
            match loosen_dependency_spec(spec)? {
                LoosenOutcome::Modified(rewritten) => {
                    *spec = rewritten;
                    if ready_seen.insert(name.clone()) {
                        ready.push(name.clone());
                    }
                }
                LoosenOutcome::AlreadyLoose => {
                    if ready_seen.insert(name.clone()) {
                        ready.push(name.clone());
                    }
                }
                LoosenOutcome::Unsupported => {
                    if unsupported_seen.insert(name.clone()) {
                        unsupported.push(name.clone());
                    }
                }
            }
        }

        if !update_all {
            let missing: Vec<String> = targets.difference(&observed).cloned().collect();
            if !missing.is_empty() {
                return Ok(ExecutionOutcome::user_error(
                    "package is not a direct dependency",
                    json!({
                        "packages": missing,
                        "hint": "use `px add` to declare dependencies before updating",
                    }),
                ));
            }
        }

        if ready.is_empty() {
            let mut details = json!({
                "pyproject": pyproject_path.display().to_string(),
                "dry_run": request.dry_run,
            });
            if !unsupported.is_empty() {
                details["unsupported"] = json!(unsupported);
                details["hint"] =
                    json!("Dependencies pinned via direct URLs must be updated manually");
            }
            let message = if update_all {
                "no dependencies eligible for px update"
            } else {
                "px update: requested packages cannot be updated"
            };
            return Ok(ExecutionOutcome::user_error(message, details));
        }

        if request.dry_run {
            let mut details = json!({
                "pyproject": pyproject_path.display().to_string(),
                "targets": ready,
                "dry_run": true,
            });
            if !unsupported.is_empty() {
                details["skipped"] = json!(unsupported);
                details["hint"] =
                    json!("Dependencies pinned via direct URLs must be updated manually");
            }
            let message = if update_all {
                "planned dependency updates (dry-run)".to_string()
            } else {
                "planned targeted updates (dry-run)".to_string()
            };
            return Ok(ExecutionOutcome::success(message, details));
        }

        let mut override_snapshot = snapshot.clone();
        override_snapshot.dependencies = override_specs;
        let resolved =
            match resolve_dependencies_with_effects(ctx.effects(), &override_snapshot, true) {
                Ok(resolved) => resolved,
                Err(err) => match err.downcast::<InstallUserError>() {
                    Ok(user) => {
                        return Ok(ExecutionOutcome::user_error(user.message, user.details))
                    }
                    Err(err) => {
                        return Ok(ExecutionOutcome::failure(
                            "px update failed",
                            json!({ "error": err.to_string() }),
                        ))
                    }
                },
            };

        persist_resolved_dependencies(&snapshot, &resolved.specs)?;
        let updated_snapshot = manifest_snapshot()?;
        let override_data = InstallOverride {
            dependencies: resolved.specs.clone(),
            pins: resolved.pins.clone(),
        };

        let install_outcome =
            match install_snapshot(ctx, &updated_snapshot, false, Some(&override_data)) {
                Ok(result) => result,
                Err(err) => match err.downcast::<InstallUserError>() {
                    Ok(user) => {
                        return Ok(ExecutionOutcome::user_error(user.message, user.details))
                    }
                    Err(err) => {
                        return Ok(ExecutionOutcome::failure(
                            "px update failed",
                            json!({
                                "error": err.to_string(),
                                "lockfile": snapshot.lock_path.display().to_string(),
                            }),
                        ))
                    }
                },
            };

        if let Err(err) = refresh_project_site(&updated_snapshot, ctx) {
            return Ok(ExecutionOutcome::failure(
                "px update failed to refresh environment",
                json!({ "error": err.to_string() }),
            ));
        }

        let updated_count = ready.len();
        let primary_label = ready.first().cloned();
        let mut details = json!({
            "pyproject": pyproject_path.display().to_string(),
            "lockfile": updated_snapshot.lock_path.display().to_string(),
            "targets": ready,
        });
        if !unsupported.is_empty() {
            details["skipped"] = json!(unsupported);
            details["hint"] = json!("Dependencies pinned via direct URLs must be updated manually");
        }

        let message = if update_all {
            "updated project dependencies".to_string()
        } else if updated_count == 1 {
            format!(
                "updated {}",
                primary_label.unwrap_or_else(|| "dependency".to_string())
            )
        } else {
            format!("updated {updated_count} dependencies")
        };

        match install_outcome.state {
            InstallState::Installed | InstallState::UpToDate => {
                needs_restore = false;
                Ok(ExecutionOutcome::success(message, details))
            }
            InstallState::Drift | InstallState::MissingLock => Ok(ExecutionOutcome::failure(
                "px update failed to refresh px.lock",
                json!({ "lockfile": snapshot.lock_path.display().to_string() }),
            )),
        }
    })();

    if needs_restore {
        backup.restore()?;
    }

    outcome
}

#[derive(Debug)]
enum LoosenOutcome {
    Modified(String),
    AlreadyLoose,
    Unsupported,
}

fn loosen_dependency_spec(spec: &str) -> Result<LoosenOutcome> {
    let trimmed = crate::strip_wrapping_quotes(spec.trim());
    let requirement = PepRequirement::from_str(trimmed)
        .map_err(|err| anyhow!("unable to parse dependency spec `{spec}`: {err}"))?;
    match requirement.version_or_url {
        Some(VersionOrUrl::VersionSpecifier(_)) => {
            let mut unlocked = requirement.clone();
            unlocked.version_or_url = None;
            Ok(LoosenOutcome::Modified(unlocked.to_string()))
        }
        Some(VersionOrUrl::Url(_)) => Ok(LoosenOutcome::Unsupported),
        None => Ok(LoosenOutcome::AlreadyLoose),
    }
}

struct ManifestLockBackup {
    pyproject_path: PathBuf,
    lock_path: PathBuf,
    pyproject_contents: String,
    lock_contents: Option<String>,
    lock_preexisting: bool,
    pyproject_permissions: fs::Permissions,
    lock_permissions: Option<fs::Permissions>,
}

impl ManifestLockBackup {
    fn capture(pyproject_path: &Path, lock_path: &Path) -> Result<Self> {
        let pyproject_contents = fs::read_to_string(pyproject_path)?;
        let pyproject_permissions = fs::metadata(pyproject_path)?.permissions();
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
            pyproject_path: pyproject_path.to_path_buf(),
            lock_path: lock_path.to_path_buf(),
            pyproject_contents,
            lock_contents,
            lock_preexisting,
            pyproject_permissions,
            lock_permissions,
        })
    }

    fn restore(&self) -> Result<()> {
        self.write_with_permissions(
            &self.pyproject_path,
            &self.pyproject_contents,
            &self.pyproject_permissions,
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
        original: &fs::Permissions,
    ) -> Result<()> {
        let writable = Self::writable_permissions(original);
        fs::set_permissions(path, writable)?;
        fs::write(path, contents)?;
        fs::set_permissions(path, original.clone())?;
        Ok(())
    }

    fn remove_with_permissions(&self, path: &Path) -> Result<()> {
        let original = fs::metadata(path)?.permissions();
        let writable = Self::writable_permissions(&original);
        fs::set_permissions(path, writable)?;
        fs::remove_file(path)?;
        Ok(())
    }

    #[cfg(unix)]
    fn writable_permissions(original: &fs::Permissions) -> fs::Permissions {
        fs::Permissions::from_mode(original.mode() | 0o200)
    }

    #[cfg(not(unix))]
    fn writable_permissions(original: &fs::Permissions) -> fs::Permissions {
        let mut perms = original.clone();
        perms.set_readonly(false);
        perms
    }
}

fn sync_manifest_environment(
    ctx: &CommandContext,
) -> Result<(ManifestSnapshot, InstallOutcome), ExecutionOutcome> {
    let snapshot = match manifest_snapshot() {
        Ok(snapshot) => snapshot,
        Err(err) => {
            if is_missing_project_error(&err) {
                return Err(crate::missing_project_outcome());
            }
            return Err(ExecutionOutcome::failure(
                "failed to read project manifest",
                json!({ "error": err.to_string() }),
            ));
        }
    };
    let outcome = match install_snapshot(ctx, &snapshot, false, None) {
        Ok(outcome) => outcome,
        Err(err) => match err.downcast::<InstallUserError>() {
            Ok(user) => return Err(ExecutionOutcome::user_error(user.message, user.details)),
            Err(err) => {
                return Err(ExecutionOutcome::failure(
                    "failed to install dependencies",
                    json!({ "error": err.to_string() }),
                ))
            }
        },
    };
    if let Err(err) = refresh_project_site(&snapshot, ctx) {
        return Err(ExecutionOutcome::failure(
            "failed to update project environment",
            json!({ "error": err.to_string() }),
        ));
    }
    Ok((snapshot, outcome))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loosen_dependency_spec_removes_pin() {
        match loosen_dependency_spec("requests==2.32.0").expect("parsed") {
            LoosenOutcome::Modified(value) => assert_eq!(value, "requests"),
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[test]
    fn loosen_dependency_spec_detects_direct_urls() {
        let spec = "demo @ https://example.invalid/demo-1.0.0.whl";
        assert!(matches!(
            loosen_dependency_spec(spec).expect("parsed"),
            LoosenOutcome::Unsupported
        ));
    }
}
