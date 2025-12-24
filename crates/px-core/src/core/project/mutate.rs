use std::collections::{HashMap, HashSet};
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
    manifest_snapshot_at, persist_resolved_dependencies, refresh_project_site,
    resolve_dependencies_with_effects, CommandContext, ExecutionOutcome, InstallOutcome,
    InstallOverride, InstallState, InstallUserError, ManifestSnapshot,
};
use px_domain::api::{
    autopin_pin_key, autopin_spec_key, load_lockfile_optional, marker_applies,
    merge_resolved_dependencies, parse_lockfile, spec_requires_pin, ManifestEditor, PinSpec,
};

use crate::workspace::{
    discover_workspace_scope, workspace_add, workspace_remove, workspace_update,
};

use super::{ensure_mutation_allowed, evaluate_project_state, ProjectLock};

#[derive(Clone, Debug)]
pub struct ProjectAddRequest {
    pub specs: Vec<String>,
    pub pin: bool,
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
            json!({ "hint": "run `px add requests`" }),
        ));
    }

    if let Some(scope) = discover_workspace_scope()? {
        return workspace_add(ctx, request, scope);
    }

    let snapshot = manifest_snapshot()?;
    let manifest_before = snapshot.dependencies.clone();
    let Some(_lock) = ProjectLock::try_acquire(&snapshot.root)? else {
        return Ok(project_locked_outcome("add"));
    };
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
            json!({ "hint": "run `px add requests`" }),
        ));
    }
    let allow_targeted_pin = request.pin && !snapshot.px_options.pin_manifest;
    let pin_targets = allow_targeted_pin.then(|| dependency_target_names(&cleaned_specs));

    let lock_path = root.join("px.lock");
    let backup = ManifestLockBackup::capture(&pyproject_path, &lock_path)?;
    let mut needs_restore = true;

    let outcome = (|| -> Result<ExecutionOutcome> {
        let mut editor = ManifestEditor::open(&pyproject_path)?;
        let report = editor.add_specs(&cleaned_specs)?;
        let dependencies_after_edit = editor.dependencies();
        let pin_needed = pin_targets.as_ref().is_some_and(|targets| {
            let after_map = dependency_spec_map(&dependencies_after_edit);
            targets.iter().any(|name| {
                after_map
                    .get(name)
                    .is_some_and(|spec| !is_exact_pin(spec))
            })
        });

        if report.added.is_empty() && report.updated.is_empty() && !pin_needed {
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
            let updated_snapshot = manifest_snapshot_at(&root)?;
            let resolved = match resolve_dependencies_with_effects(ctx.effects(), &updated_snapshot, true) {
                Ok(resolved) => resolved,
                Err(err) => match err.downcast::<InstallUserError>() {
                    Ok(user) => {
                        return Ok(ExecutionOutcome::user_error(user.message, user.details))
                    }
                    Err(err) => {
                        return Ok(ExecutionOutcome::failure(
                            "px add failed while planning changes",
                            json!({ "error": err.to_string() }),
                        ))
                    }
                },
            };
            let marker_env = ctx.marker_environment()?;
            let planned_manifest = if updated_snapshot.px_options.pin_manifest {
                merge_resolved_dependencies(&dependencies_after_edit, &resolved.specs, &marker_env)
            } else if let Some(targets) = pin_targets.as_ref() {
                pin_dependencies_for_targets(
                    &dependencies_after_edit,
                    targets,
                    &resolved.pins,
                    &marker_env,
                )
                .0
            } else {
                dependencies_after_edit.clone()
            };
            let lock = load_lockfile_optional(&lock_path)?;
            let lock_preview = super::lock_preview(lock.as_ref(), &resolved.pins);
            let env_would_rebuild = lock_preview
                .get("would_change")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true);
            return Ok(ExecutionOutcome::success(
                "planned dependency changes (dry-run)",
                json!({
                    "pyproject": pyproject_path.display().to_string(),
                    "added": report.added,
                    "updated": report.updated,
                    "preview": json!({
                        "pyproject": {
                            "path": pyproject_path.display().to_string(),
                            "changes": [
                                super::dependency_group_changes("dependencies", &manifest_before, &planned_manifest),
                            ],
                        },
                        "lock": lock_preview,
                        "env": { "would_rebuild": env_would_rebuild },
                        "tools": { "would_rebuild": false },
                    }),
                    "dry_run": true,
                }),
            ));
        }

        let snapshot = if pin_needed {
            let updated_snapshot = manifest_snapshot_at(&root)?;
            let resolved =
                match resolve_dependencies_with_effects(ctx.effects(), &updated_snapshot, true) {
                    Ok(resolved) => resolved,
                    Err(err) => match err.downcast::<InstallUserError>() {
                        Ok(user) => return Ok(ExecutionOutcome::user_error(user.message, user.details)),
                        Err(err) => {
                            return Ok(ExecutionOutcome::failure(
                                "px add failed",
                                json!({ "error": err.to_string() }),
                            ))
                        }
                    },
                };
            let marker_env = ctx.marker_environment()?;
            let targets = pin_targets.as_ref().expect("pin_targets when pin_needed");
            let (pinned_deps, _) =
                pin_dependencies_for_targets(&updated_snapshot.dependencies, targets, &resolved.pins, &marker_env);
            persist_resolved_dependencies(&updated_snapshot, &pinned_deps)?;
            let updated_snapshot = manifest_snapshot_at(&root)?;
            let override_data = InstallOverride {
                dependencies: pinned_deps,
                pins: resolved.pins.clone(),
            };
            let install_outcome =
                match install_snapshot(ctx, &updated_snapshot, false, false, Some(&override_data)) {
                    Ok(outcome) => outcome,
                    Err(err) => match err.downcast::<InstallUserError>() {
                        Ok(user) => return Ok(ExecutionOutcome::user_error(user.message, user.details)),
                        Err(err) => {
                            return Ok(ExecutionOutcome::failure(
                                "px add failed",
                                json!({ "error": err.to_string() }),
                            ))
                        }
                    },
                };
            if let Err(err) = refresh_project_site(&updated_snapshot, ctx) {
                return Ok(ExecutionOutcome::failure(
                    "px add failed to refresh environment",
                    json!({ "error": err.to_string() }),
                ));
            }
            match install_outcome.state {
                InstallState::Installed | InstallState::UpToDate => {}
                InstallState::Drift | InstallState::MissingLock => {
                    return Ok(ExecutionOutcome::failure(
                        "px add failed to refresh px.lock",
                        json!({ "lockfile": updated_snapshot.lock_path.display().to_string() }),
                    ))
                }
            }
            updated_snapshot
        } else {
            let (snapshot, _install) = match sync_manifest_environment(ctx) {
                Ok(result) => result,
                Err(outcome) => return Ok(outcome),
            };
            snapshot
        };
        needs_restore = false;
        let dependencies_after =
            ManifestEditor::open(&pyproject_path)?.dependencies();
        let added_specs = {
            let after_map = dependency_spec_map(&dependencies_after);
            let mut out = Vec::new();
            for name in &report.added {
                if let Some(spec) = after_map.get(name) {
                    out.push(spec.clone());
                }
            }
            out.sort();
            out.dedup();
            out
        };
        let manifest_changes = pinned_manifest_changes(
            &report.added,
            &report.updated,
            pin_targets.as_ref(),
            &dependencies_after_edit,
            &dependencies_after,
        );
        let message = if report.added.is_empty() && report.updated.is_empty() {
            "pinned dependencies".to_string()
        } else {
            format!(
                "updated dependencies (added {}, updated {})",
                report.added.len(),
                report.updated.len()
            )
        };
        let mut details = json!({
            "pyproject": pyproject_path.display().to_string(),
            "lockfile": snapshot.lock_path.display().to_string(),
            "added": report.added,
            "updated": report.updated,
        });
        if !added_specs.is_empty() {
            details["manifest_added"] = json!(added_specs);
        }
        if !manifest_changes.is_empty() {
            details["manifest_changes"] = json!(manifest_changes);
        }
        let before_lock =
            backup
                .lock_contents
                .as_deref()
                .and_then(|contents| parse_lockfile(contents).ok());
        let after_lock = load_lockfile_optional(&snapshot.lock_path)?;
        details["lock_changes"] = super::lock_changes(before_lock.as_ref(), after_lock.as_ref());
        Ok(ExecutionOutcome::success(
            message,
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
    pinned: Option<&HashSet<String>>,
    before: &[String],
    after: &[String],
) -> Vec<serde_json::Value> {
    let before_map = dependency_spec_map(before);
    let after_map = dependency_spec_map(after);
    let mut changes = Vec::new();
    let mut targets = Vec::new();
    for name in added.iter().chain(updated) {
        targets.push(name);
    }
    if let Some(extra) = pinned {
        for name in extra {
            targets.push(name);
        }
    }
    targets.sort();
    targets.dedup();
    for name in targets {
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

fn dependency_target_names(specs: &[String]) -> HashSet<String> {
    let mut targets = HashSet::new();
    for spec in specs {
        let name = dependency_name(spec);
        if name.is_empty() {
            continue;
        }
        targets.insert(name);
    }
    targets
}

fn pin_dependencies_for_targets(
    dependencies: &[String],
    targets: &HashSet<String>,
    pins: &[PinSpec],
    marker_env: &pep508_rs::MarkerEnvironment,
) -> (Vec<String>, bool) {
    let mut pinned_lookup = HashMap::new();
    for pin in pins {
        pinned_lookup.insert(autopin_pin_key(pin), pin.specifier.clone());
    }
    let mut updated = Vec::with_capacity(dependencies.len());
    let mut changed = false;
    for spec in dependencies {
        let name = dependency_name(spec);
        if name.is_empty() || !targets.contains(&name) {
            updated.push(spec.clone());
            continue;
        }
        if !spec_requires_pin(spec) || !marker_applies(spec, marker_env) {
            updated.push(spec.clone());
            continue;
        }
        let key = autopin_spec_key(spec);
        if let Some(pinned) = pinned_lookup.get(&key) {
            if pinned != spec {
                changed = true;
                updated.push(pinned.clone());
            } else {
                updated.push(spec.clone());
            }
        } else {
            updated.push(spec.clone());
        }
    }
    (updated, changed)
}

fn update_exact_pins_for_targets(
    dependencies: &[String],
    targets: &HashSet<String>,
    pins: &[PinSpec],
    marker_env: &pep508_rs::MarkerEnvironment,
) -> (Vec<String>, bool) {
    let mut pinned_lookup = HashMap::new();
    for pin in pins {
        pinned_lookup.insert(autopin_pin_key(pin), pin.specifier.clone());
    }
    let mut updated = Vec::with_capacity(dependencies.len());
    let mut changed = false;
    for spec in dependencies {
        let name = dependency_name(spec);
        if name.is_empty() || !targets.contains(&name) || !is_exact_pin(spec) {
            updated.push(spec.clone());
            continue;
        }
        if !marker_applies(spec, marker_env) {
            updated.push(spec.clone());
            continue;
        }
        let key = autopin_spec_key(spec);
        if let Some(pinned) = pinned_lookup.get(&key) {
            if pinned != spec {
                changed = true;
                updated.push(pinned.clone());
            } else {
                updated.push(spec.clone());
            }
        } else {
            updated.push(spec.clone());
        }
    }
    (updated, changed)
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
        return workspace_remove(ctx, request, scope);
    }

    let snapshot = manifest_snapshot()?;
    let Some(_lock) = ProjectLock::try_acquire(&snapshot.root)? else {
        return Ok(project_locked_outcome("remove"));
    };
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
        return workspace_update(ctx, request, scope);
    }

    let snapshot = manifest_snapshot()?;
    let manifest_before = snapshot.dependencies.clone();
    let Some(_lock) = ProjectLock::try_acquire(&snapshot.root)? else {
        return Ok(project_locked_outcome("update"));
    };
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
            let mut override_snapshot = snapshot.clone();
            override_snapshot.dependencies = override_specs;
            override_snapshot.group_dependencies = snapshot.group_dependencies.clone();
            override_snapshot.dependency_groups = snapshot.dependency_groups.clone();
            override_snapshot.declared_dependency_groups = snapshot.declared_dependency_groups.clone();
            override_snapshot.dependency_group_source = snapshot.dependency_group_source;
            override_snapshot.requirements = override_snapshot.dependencies.clone();
            override_snapshot
                .requirements
                .extend(override_snapshot.group_dependencies.clone());
            override_snapshot.requirements.sort();
            override_snapshot.requirements.dedup();

            let resolved =
                match resolve_dependencies_with_effects(ctx.effects(), &override_snapshot, true) {
                    Ok(resolved) => resolved,
                    Err(err) => match err.downcast::<InstallUserError>() {
                        Ok(user) => {
                            return Ok(ExecutionOutcome::user_error(user.message, user.details))
                        }
                        Err(err) => {
                            return Ok(ExecutionOutcome::failure(
                                "px update failed while planning changes",
                                json!({ "error": err.to_string() }),
                            ))
                        }
                    },
                };
            let marker_env = ctx.marker_environment()?;
            let planned_manifest = if snapshot.px_options.pin_manifest {
                merge_resolved_dependencies(&override_snapshot.dependencies, &resolved.specs, &marker_env)
            } else {
                let mut pinned_targets = HashSet::new();
                for spec in &snapshot.dependencies {
                    if !is_exact_pin(spec) {
                        continue;
                    }
                    let name = dependency_name(spec);
                    if name.is_empty() {
                        continue;
                    }
                    if update_all || targets.contains(&name) {
                        pinned_targets.insert(name);
                    }
                }
                update_exact_pins_for_targets(
                    &snapshot.dependencies,
                    &pinned_targets,
                    &resolved.pins,
                    &marker_env,
                )
                .0
            };
            let lock = load_lockfile_optional(&snapshot.lock_path)?;
            let lock_preview = super::lock_preview(lock.as_ref(), &resolved.pins);
            let env_would_rebuild = lock_preview
                .get("would_change")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true);
            let mut details = json!({
                "pyproject": pyproject_path.display().to_string(),
                "targets": ready,
                "preview": json!({
                    "pyproject": {
                        "path": pyproject_path.display().to_string(),
                        "changes": [
                            super::dependency_group_changes("dependencies", &manifest_before, &planned_manifest),
                        ],
                    },
                    "lock": lock_preview,
                    "env": { "would_rebuild": env_would_rebuild },
                    "tools": { "would_rebuild": false },
                }),
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
        override_snapshot.group_dependencies = snapshot.group_dependencies.clone();
        override_snapshot.dependency_groups = snapshot.dependency_groups.clone();
        override_snapshot.declared_dependency_groups = snapshot.declared_dependency_groups.clone();
        override_snapshot.dependency_group_source = snapshot.dependency_group_source;
        override_snapshot.requirements = override_snapshot.dependencies.clone();
        override_snapshot
            .requirements
            .extend(override_snapshot.group_dependencies.clone());
        override_snapshot.requirements.sort();
        override_snapshot.requirements.dedup();
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

        let marker_env = ctx.marker_environment()?;
        let (updated_deps, manifest_written) = if snapshot.px_options.pin_manifest {
            let deps = merge_resolved_dependencies(&override_snapshot.dependencies, &resolved.specs, &marker_env);
            persist_resolved_dependencies(&snapshot, &deps)?;
            (deps, true)
        } else {
            let mut pinned_targets = HashSet::new();
            for spec in &snapshot.dependencies {
                if !is_exact_pin(spec) {
                    continue;
                }
                let name = dependency_name(spec);
                if name.is_empty() {
                    continue;
                }
                if update_all || targets.contains(&name) {
                    pinned_targets.insert(name);
                }
            }
            let (deps, changed) =
                update_exact_pins_for_targets(&snapshot.dependencies, &pinned_targets, &resolved.pins, &marker_env);
            if changed {
                persist_resolved_dependencies(&snapshot, &deps)?;
            }
            (deps, changed)
        };
        let updated_snapshot = if manifest_written {
            manifest_snapshot()?
        } else {
            snapshot.clone()
        };
        let override_data = InstallOverride {
            dependencies: updated_deps,
            pins: resolved.pins.clone(),
        };

        let install_outcome =
            match install_snapshot(ctx, &updated_snapshot, false, true, Some(&override_data)) {
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
        let before_lock =
            backup
                .lock_contents
                .as_deref()
                .and_then(|contents| parse_lockfile(contents).ok());
        let after_lock = load_lockfile_optional(&updated_snapshot.lock_path)?;
        details["lock_changes"] = super::lock_changes(before_lock.as_ref(), after_lock.as_ref());
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

fn project_locked_outcome(action: &str) -> ExecutionOutcome {
    ExecutionOutcome::user_error(
        format!("px {action}: another px command is already running for this project"),
        json!({
            "reason": "project_locked",
            "hint": "Wait for the other px command to finish, then retry.",
        }),
    )
}

fn loosen_dependency_spec(spec: &str) -> Result<LoosenOutcome> {
    let trimmed = crate::strip_wrapping_quotes(spec.trim());
    let requirement = PepRequirement::from_str(trimmed)
        .map_err(|err| anyhow!("unable to parse dependency spec `{spec}`: {err}"))?;
    match requirement.version_or_url {
        Some(VersionOrUrl::VersionSpecifier(ref specifiers)) => {
            let rendered = specifiers.to_string();
            let exact = (rendered.starts_with("==") || rendered.starts_with("==="))
                && !rendered.contains(',');
            if !exact {
                return Ok(LoosenOutcome::AlreadyLoose);
            }
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
    let outcome = match install_snapshot(ctx, &snapshot, false, false, None) {
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
