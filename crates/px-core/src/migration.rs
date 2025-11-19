use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{env, fs};

use px_domain::{
    collect_pyproject_packages, collect_requirement_packages, plan_autopin, prepare_pyproject_plan,
    resolve_onboard_path, AutopinEntry, AutopinPending, AutopinState, BackupManager,
    InstallOverride, PinSpec,
};

use crate::{
    discover_project_root, install_snapshot, lock_is_fresh, manifest_snapshot_at,
    resolve_dependencies_with_effects, summarize_autopins, CommandContext, ExecutionOutcome,
    InstallState, InstallUserError, ManifestSnapshot,
};

#[derive(Clone, Copy, Debug)]
pub enum MigrationMode {
    Preview,
    Apply,
}

impl MigrationMode {
    const fn is_apply(self) -> bool {
        matches!(self, Self::Apply)
    }
}

#[derive(Clone, Copy, Debug)]
pub enum WorkspacePolicy {
    CleanOnly,
    AllowDirty,
}

impl WorkspacePolicy {
    const fn allows_dirty(self) -> bool {
        matches!(self, Self::AllowDirty)
    }
}

#[derive(Clone, Copy, Debug)]
pub enum LockBehavior {
    Full,
    LockOnly,
}

impl LockBehavior {
    const fn is_lock_only(self) -> bool {
        matches!(self, Self::LockOnly)
    }
}

#[derive(Clone, Copy, Debug)]
pub enum AutopinPreference {
    Enabled,
    Disabled,
}

impl AutopinPreference {
    const fn autopin_enabled(self) -> bool {
        matches!(self, Self::Enabled)
    }
}

#[derive(Clone, Debug)]
pub struct MigrateRequest {
    pub source: Option<String>,
    pub dev_source: Option<String>,
    pub mode: MigrationMode,
    pub workspace: WorkspacePolicy,
    pub lock_behavior: LockBehavior,
    pub autopin: AutopinPreference,
}

/// Migrates an existing Python project into px format.
///
/// # Errors
/// Returns an error if project files cannot be read or write operations fail.
#[allow(clippy::too_many_lines)]
pub fn migrate(ctx: &CommandContext, request: &MigrateRequest) -> Result<ExecutionOutcome> {
    let root = match discover_project_root()? {
        Some(path) => path,
        None => env::current_dir().context("unable to determine current directory")?,
    };
    let pyproject_path = root.join("pyproject.toml");
    let pyproject_exists = pyproject_path.exists();

    let source_override = request.source.clone();
    let dev_override = request.dev_source.clone();

    let requirements_path = match resolve_onboard_path(
        &root,
        source_override.as_deref(),
        "requirements.txt",
    ) {
        Ok(path) => path,
        Err(err) => {
            return Ok(ExecutionOutcome::user_error(
                "px migrate: override path invalid",
                json!({
                    "error": err.to_string(),
                    "hint": "Override path invalid; specify a repo-relative file before retrying.",
                }),
            ))
        }
    };
    let dev_path = match resolve_onboard_path(
        &root,
        dev_override.as_deref(),
        "requirements-dev.txt",
    ) {
        Ok(path) => path,
        Err(err) => {
            return Ok(ExecutionOutcome::user_error(
                "px migrate: override path invalid",
                json!({
                    "error": err.to_string(),
                    "hint": "Override path invalid; specify a repo-relative file before retrying.",
                }),
            ))
        }
    };

    if !pyproject_exists && requirements_path.is_none() && dev_path.is_none() {
        return Ok(ExecutionOutcome::user_error(
            "px migrate: no project files found",
            json!({
                "project_type": "bare",
                "sources": [],
                "hint": "add pyproject.toml or requirements.txt before running px migrate",
            }),
        ));
    }

    let mut packages = Vec::new();
    let mut source_summaries = Vec::new();

    if pyproject_exists {
        let (summary, mut rows) = collect_pyproject_packages(&root, &pyproject_path)?;
        source_summaries.push(summary);
        packages.append(&mut rows);
    }

    if let Some(path) = requirements_path.as_ref() {
        let (summary, mut rows) =
            collect_requirement_packages(&root, path, "requirements", "prod")?;
        source_summaries.push(summary);
        packages.append(&mut rows);
    }

    if let Some(path) = dev_path.as_ref() {
        let (summary, mut rows) =
            collect_requirement_packages(&root, path, "requirements-dev", "dev")?;
        source_summaries.push(summary);
        packages.append(&mut rows);
    }

    let project_type = if pyproject_exists {
        if requirements_path.is_some() || dev_path.is_some() {
            "pyproject+requirements"
        } else {
            "pyproject"
        }
    } else if requirements_path.is_some() || dev_path.is_some() {
        "requirements"
    } else {
        "bare"
    };

    let prod_count = packages.iter().filter(|pkg| pkg.scope == "prod").count();
    let dev_count = packages.iter().filter(|pkg| pkg.scope == "dev").count();
    let source_count = source_summaries.len();

    let mut message = format!(
        "px migrate: plan ready (prod: {prod_count}, dev: {dev_count}, sources: {source_count}, project: {project_type})"
    );
    let write_requested = request.mode.is_apply();
    let allow_dirty = request.workspace.allows_dirty();
    let lock_only = request.lock_behavior.is_lock_only();
    let no_autopin = !request.autopin.autopin_enabled();

    if lock_only && !pyproject_exists {
        return Ok(ExecutionOutcome::user_error(
            "px migrate: pyproject.toml required when --lock-only is set",
            json!({
                "hint": "Create pyproject.toml or drop --lock-only to let px write it",
            }),
        ));
    }

    let package_values: Vec<Value> = packages
        .iter()
        .map(|pkg| {
            json!({
                "name": pkg.name,
                "requested": pkg.requested,
                "scope": pkg.scope,
                "source": pkg.source,
            })
        })
        .collect();

    let mut details = json!({
        "project_type": project_type,
        "sources": source_summaries,
        "packages": package_values,
        "write_requested": write_requested,
        "dry_run": !write_requested,
        "actions": {
            "pyproject_updated": false,
            "lock_written": false,
            "backups": [],
        },
    });

    if !write_requested {
        details["hint"] =
            Value::String("Preview confirmed; rerun with --apply to write changes".to_string());
        return Ok(ExecutionOutcome::success(message, details));
    }

    if !allow_dirty {
        if let Some(changes) = ctx.git().worktree_changes(&root)? {
            if !changes.is_empty() {
                details["changes"] =
                    Value::Array(changes.iter().map(|c| Value::String(c.clone())).collect());
                details["hint"] = Value::String(
                    "Repo dirty—stash, commit, or use --allow-dirty before retrying.".to_string(),
                );
                return Ok(ExecutionOutcome::user_error(
                    "px migrate: worktree dirty (stash, commit, or use --allow-dirty)",
                    details,
                ));
            }
        }
    }

    let mut backups = BackupManager::new(&root);
    let pyproject_plan = prepare_pyproject_plan(&root, &pyproject_path, lock_only, &packages)?;
    let mut pyproject_backed_up = false;
    if pyproject_plan.needs_backup() {
        backups.backup(&pyproject_plan.path)?;
        pyproject_backed_up = true;
    }
    if let Some(contents) = &pyproject_plan.contents {
        if let Some(parent) = pyproject_plan.path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&pyproject_plan.path, contents)?;
    }

    let mut autopin_entries = Vec::new();
    let mut install_override: Option<InstallOverride> = None;
    let mut autopin_changed_pyproject = false;
    let mut autopin_hint = None;

    if pyproject_path.exists() {
        let marker_env = ctx.marker_environment()?;
        let autopin_snapshot = px_domain::ProjectSnapshot::read_from(&root)?;
        let effects = ctx.shared_effects();
        let resolver = move |snap: &ManifestSnapshot, specs: &[String]| -> Result<Vec<PinSpec>> {
            let mut override_snapshot = snap.clone();
            override_snapshot.dependencies = specs.to_vec();
            let resolved = resolve_dependencies_with_effects(effects.as_ref(), &override_snapshot)?;
            Ok(resolved.pins)
        };
        match plan_autopin(
            &autopin_snapshot,
            &pyproject_path,
            lock_only,
            no_autopin,
            &resolver,
            &marker_env,
        )? {
            AutopinState::NotNeeded => {}
            AutopinState::Disabled { pending } => {
                if !pending.is_empty() {
                    details["autopinned"] =
                        Value::Array(pending.iter().map(AutopinPending::to_json).collect());
                }
                details["hint"] = Value::String(
                    "Loose specs remain; drop --no-autopin or pin pyproject manually.".to_string(),
                );
                return Ok(ExecutionOutcome::user_error(
                    "px migrate: automatic pinning disabled but loose specs remain",
                    details,
                ));
            }
            AutopinState::Planned(plan) => {
                autopin_entries = plan.autopinned;
                if let Some(contents) = plan.doc_contents {
                    if !pyproject_plan.created && !pyproject_backed_up {
                        backups.backup(&pyproject_plan.path)?;
                    }
                    if let Some(parent) = pyproject_plan.path.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::write(&pyproject_plan.path, contents)?;
                    autopin_changed_pyproject = true;
                }
                install_override = plan.install_override;
                autopin_hint = summarize_autopins(&autopin_entries);
            }
        }
    }

    if !autopin_entries.is_empty() {
        details["autopinned"] =
            Value::Array(autopin_entries.iter().map(AutopinEntry::to_json).collect());
        if autopin_hint.is_none() {
            autopin_hint = summarize_autopins(&autopin_entries);
        }
    }

    let snapshot = manifest_snapshot_at(&root)?;
    let lock_needs_backup = snapshot.lock_path.exists() && !lock_is_fresh(&snapshot)?;
    if lock_needs_backup {
        backups.backup(&snapshot.lock_path)?;
    }
    let install_outcome = match install_snapshot(ctx, &snapshot, false, install_override.as_ref()) {
        Ok(ok) => ok,
        Err(err) => match err.downcast::<InstallUserError>() {
            Ok(user) => return Ok(ExecutionOutcome::user_error(user.message, user.details)),
            Err(err) => return Err(err),
        },
    };

    let backup_summary = backups.finish();
    let pyproject_updated = pyproject_plan.updated() || autopin_changed_pyproject;
    let lock_written = matches!(install_outcome.state, InstallState::Installed);

    details["actions"]["pyproject_updated"] = Value::Bool(pyproject_updated);
    details["actions"]["lock_written"] = Value::Bool(lock_written);
    details["actions"]["backups"] = Value::Array(
        backup_summary
            .files
            .iter()
            .map(|entry| Value::String(entry.clone()))
            .collect(),
    );
    if let Some(dir) = backup_summary.directory.as_ref() {
        details["actions"]["backup_dir"] = Value::String(dir.clone());
    }

    let changes_applied = pyproject_updated || lock_written;
    if changes_applied {
        let mut hint = if let Some(dir) = backup_summary.directory.as_ref() {
            format!("Backups stored under {dir}")
        } else {
            "No backups created (new files only)".to_string()
        };
        if let Some(extra) = autopin_hint {
            if hint.is_empty() {
                hint = extra;
            } else {
                hint = format!("{hint} • {extra}");
            }
        }
        if !hint.is_empty() {
            details["hint"] = Value::String(hint);
        }
        message = format!("px migrate: plan applied (prod: {prod_count}, dev: {dev_count})");
        Ok(ExecutionOutcome::success(message, details))
    } else {
        let mut hint =
            "No changes detected; nothing to write. Run again if you expect updates.".to_string();
        if let Some(extra) = autopin_hint {
            hint = format!("{hint} • {extra}");
        }
        details["hint"] = Value::String(hint);
        Ok(ExecutionOutcome::success(
            "px migrate: nothing to apply (already in sync)",
            details,
        ))
    }
}
