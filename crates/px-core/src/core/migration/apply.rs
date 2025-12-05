use std::{
    collections::{HashMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};

use anyhow::{Context, Result};
use pep440_rs::{Version, VersionSpecifiers};
use pep508_rs::{MarkerEnvironment, Requirement as PepRequirement};
use serde_json::{json, Value};
use toml_edit::{DocumentMut, Item};

use px_domain::{
    autopin_pin_key, collect_pyproject_packages, collect_requirement_packages,
    collect_setup_cfg_packages, format_specifier, normalize_dist_name, plan_autopin,
    prepare_pyproject_plan, resolve_onboard_path, AutopinEntry, AutopinPending, AutopinState,
    BackupManager, InstallOverride, PinSpec,
};

use crate::runtime_manager;
use crate::{
    discover_project_root, install_snapshot, lock_is_fresh, manifest_snapshot_at,
    refresh_project_site, resolve_dependencies_with_effects, summarize_autopins, CommandContext,
    ExecutionOutcome, InstallState, InstallUserError, ManifestSnapshot,
};

use super::plan::{apply_precedence, apply_python_override};
use super::runtime::fallback_runtime_by_channel;

fn test_migration_crash_hook() -> anyhow::Result<()> {
    if env::var("PX_TEST_MIGRATE_CRASH").ok().as_deref() == Some("1") {
        anyhow::bail!("test crash hook");
    }
    Ok(())
}

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
    pub python: Option<String>,
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
    let mut python_override_value = request.python.clone();

    if let Some(python) = &request.python {
        let selection = match runtime_manager::resolve_runtime(Some(python), ">=0")
            .or_else(|_| fallback_runtime_by_channel(python))
        {
            Ok(selection) => selection,
            Err(err) => {
                return Ok(ExecutionOutcome::user_error(
                    "px migrate: python runtime unavailable",
                    json!({
                        "hint": err.to_string(),
                        "reason": "missing_runtime",
                        "requested": python,
                    }),
                ));
            }
        };
        env::set_var("PX_RUNTIME_PYTHON", &selection.record.path);
        python_override_value = Some(selection.record.version.clone());
    }
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

    let setup_cfg_path = {
        let candidate = root.join("setup.cfg");
        candidate.exists().then_some(candidate)
    };

    let lock_only = request.lock_behavior.is_lock_only();

    if lock_only && !pyproject_exists {
        return Ok(ExecutionOutcome::user_error(
            "px migrate: pyproject.toml required when --lock-only is set",
            json!({
                "hint": "Create pyproject.toml or drop --lock-only to let px write it",
            }),
        ));
    }

    if !pyproject_exists
        && requirements_path.is_none()
        && dev_path.is_none()
        && setup_cfg_path.is_none()
    {
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

    let requirements_rel = requirements_path
        .as_ref()
        .map(|path| crate::relative_path_str(path, &root));
    let dev_rel = dev_path
        .as_ref()
        .map(|path| crate::relative_path_str(path, &root));
    let mut foreign_tools = Vec::new();
    let mut foreign_owners = Vec::new();

    if pyproject_exists {
        let (summary, mut rows) = collect_pyproject_packages(&root, &pyproject_path)?;
        source_summaries.push(summary);
        packages.append(&mut rows);
        foreign_tools = detect_foreign_tool_sections(&pyproject_path)?;
        foreign_owners = detect_foreign_tool_conflicts(&pyproject_path)?;
    }

    if let Some(path) = setup_cfg_path.as_ref() {
        let (summary, mut rows) = collect_setup_cfg_packages(&root, path)?;
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

    let mut project_parts = Vec::new();
    if pyproject_exists {
        project_parts.push("pyproject");
    }
    if setup_cfg_path.is_some() {
        project_parts.push("setup.cfg");
    }
    if requirements_path.is_some() {
        project_parts.push("requirements");
    }
    if dev_path.is_some() {
        project_parts.push("requirements-dev");
    }
    let project_type = if project_parts.is_empty() {
        "bare".to_string()
    } else {
        project_parts.join("+")
    };

    let (packages, conflicts) = apply_precedence(
        &packages,
        requirements_rel.as_ref(),
        dev_rel.as_ref(),
        &source_override,
        &dev_override,
    );

    if !conflicts.is_empty() {
        let conflict_values: Vec<Value> = conflicts
            .iter()
            .map(|conflict| {
                json!({
                    "name": conflict.name,
                    "scope": conflict.scope,
                    "kept": {
                        "source": conflict.kept_source,
                        "spec": conflict.kept_spec,
                    },
                    "dropped": {
                        "source": conflict.dropped_source,
                        "spec": conflict.dropped_spec,
                    },
                })
            })
            .collect();
        let mut details = json!({
            "project_type": project_type,
            "conflicts": conflict_values,
        });
        details["sources"] = json!(source_summaries);

        let single_source_conflict = source_summaries.len() == 1;

        if single_source_conflict {
            details["hint"] = json!("Remove duplicate/conflicting entries in pyproject.toml so each dependency is declared once.");
            return Ok(ExecutionOutcome::user_error(
                "px migrate: conflicting dependency entries in pyproject.toml",
                details,
            ));
        } else {
            details["precedence"] =
                json!("--source/--dev-source > pyproject.toml > requirements.txt");
            details["hint"] = json!("Resolve conflicting specs or rely on explicit --source/--dev-source to pick the right file (pyproject.toml wins over requirements.txt when unspecified).");
            return Ok(ExecutionOutcome::user_error(
                "px migrate: conflicting dependency sources (pyproject takes precedence over requirements)",
                details,
            ));
        }
    }

    let prod_count = packages.iter().filter(|pkg| pkg.scope == "prod").count();
    let dev_count = packages.iter().filter(|pkg| pkg.scope == "dev").count();
    let source_count = source_summaries.len();

    let mut message = format!(
        "px migrate: plan ready (prod: {prod_count}, dev: {dev_count}, sources: {source_count}, project: {project_type})"
    );
    let write_requested = request.mode.is_apply();
    let allow_dirty = request.workspace.allows_dirty();
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

    let foreign_hint = (!foreign_tools.is_empty()).then(|| {
        details["foreign_tools"] = Value::Array(
            foreign_tools
                .iter()
                .map(|t| Value::String(t.clone()))
                .collect(),
        );
        format!(
            "Preserved foreign tool configuration: {}",
            foreign_tools.join(", ")
        )
    });

    let foreign_owner_hint = (!foreign_owners.is_empty()).then(|| {
        details["foreign_owners"] = Value::Array(
            foreign_owners
                .iter()
                .map(|t| Value::String(t.clone()))
                .collect(),
        );
        format!(
            "pyproject declares dependency ownership by {}; preserving those sections while migrating",
            foreign_owners.join(", ")
        )
    });

    if !write_requested {
        let mut hint = "Preview confirmed; rerun with --apply to write changes".to_string();
        if let Some(extra) = foreign_hint.as_ref() {
            hint = format!("{hint} • {extra}");
        }
        if let Some(extra) = foreign_owner_hint.as_ref() {
            hint = format!("{hint} • {extra}");
        }
        details["hint"] = Value::String(hint);
        return Ok(ExecutionOutcome::success(message, details));
    }

    if write_requested && !foreign_owners.is_empty() {
        let owners = foreign_owners.join(", ");
        details["hint"] = Value::String(format!(
            "pyproject is still owned by {owners}; remove or convert those dependency sections before migrating with px.",
        ));
        return Ok(ExecutionOutcome::failure(
            "px migrate: pyproject managed by a foreign tool; aborting",
            details,
        ));
    }

    if write_requested && !ctx.config().network.online {
        details["hint"] = Value::String(
            "PX_ONLINE=1 required for `px migrate --apply`; rerun with network access or drop --apply for preview.".to_string(),
        );
        return Ok(ExecutionOutcome::user_error(
            "px migrate: PX_ONLINE=1 required for apply",
            details,
        ));
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
    let mut created_files: Vec<PathBuf> = Vec::new();
    let mut pyproject_modified = false;
    let mut pyproject_plan = prepare_pyproject_plan(&root, &pyproject_path, lock_only, &packages)?;
    if let Some(python) = &python_override_value {
        pyproject_plan.contents = Some(apply_python_override(&pyproject_plan, python)?.to_string());
    }
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
        pyproject_modified = true;
        if pyproject_plan.created {
            created_files.push(pyproject_plan.path.clone());
        }
    }

    if pyproject_modified {
        if let Err(err) = test_migration_crash_hook() {
            rollback_failed_migration(&backups, &created_files)?;
            return Err(err);
        }
    }

    let mut autopin_entries = Vec::new();
    let mut install_override: Option<InstallOverride> = None;
    let mut autopin_changed_pyproject = false;
    let mut autopin_hint = None;
    let poetry_pins = poetry_lock_versions(&root)?.map(Arc::new);
    let marker_env = Arc::new(ctx.marker_environment()?);
    let reused_from_poetry = Arc::new(std::sync::Mutex::new(Vec::new()));
    let skipped_poetry = Arc::new(std::sync::Mutex::new(Vec::new()));

    if pyproject_path.exists() {
        let autopin_snapshot = px_domain::ProjectSnapshot::read_from(&root)?;
        let effects = ctx.shared_effects();
        let autopin_state = match plan_autopin(
            &autopin_snapshot,
            &pyproject_path,
            lock_only,
            no_autopin,
            &{
                let lock_lookup = poetry_pins.clone();
                let marker_env = marker_env.clone();
                let reused_from_poetry = reused_from_poetry.clone();
                let skipped_poetry = skipped_poetry.clone();
                move |snap: &ManifestSnapshot, specs: &[String]| -> Result<Vec<PinSpec>> {
                    let mut pins = Vec::new();
                    let mut needs_resolution = false;
                    if let Some(locked) = lock_lookup.as_ref() {
                        for spec in specs {
                            match pin_from_poetry_lock(spec, locked, marker_env.as_ref()) {
                                PoetryPinChoice::Reuse { pin, source } => {
                                    if let Ok(mut reused) = reused_from_poetry.lock() {
                                        reused.push(source);
                                    }
                                    pins.push(pin);
                                }
                                PoetryPinChoice::Skip(reason) => {
                                    needs_resolution = true;
                                    if let Ok(mut skipped) = skipped_poetry.lock() {
                                        skipped.push(json!({
                                            "spec": spec,
                                            "reason": reason,
                                        }));
                                    }
                                }
                            }
                        }
                    } else {
                        needs_resolution = true;
                    }
                    if !needs_resolution {
                        return Ok(pins);
                    }
                    let resolved =
                        resolve_dependencies_with_effects(effects.as_ref(), snap, false)?;
                    if pins.is_empty() {
                        return Ok(resolved.pins);
                    }
                    merge_pin_sets(&mut pins, resolved.pins);
                    Ok(pins)
                }
            },
            marker_env.as_ref(),
        ) {
            Ok(state) => state,
            Err(err) => {
                if pyproject_modified {
                    rollback_failed_migration(&backups, &created_files)?;
                }
                let user = match err.downcast::<InstallUserError>() {
                    Ok(user) => user,
                    Err(other) => InstallUserError::new(
                        "px migrate: failed to autopin dependencies",
                        json!({
                            "reason": "autopin_failed",
                            "error": other.to_string(),
                            "hint": "Re-run with --no-autopin to manage pins manually.",
                        }),
                    ),
                };
                return Ok(ExecutionOutcome::user_error(user.message, user.details));
            }
        };
        match autopin_state {
            AutopinState::NotNeeded => {}
            AutopinState::Disabled { pending } => {
                if !pending.is_empty() {
                    details["autopinned"] =
                        Value::Array(pending.iter().map(AutopinPending::to_json).collect());
                }
                details["hint"] = Value::String(
                    "Loose specs remain; drop --no-autopin or pin pyproject manually.".to_string(),
                );
                if pyproject_modified {
                    rollback_failed_migration(&backups, &created_files)?;
                }
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
                    pyproject_modified = true;
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
    let reused_poetry_refs = reused_from_poetry.lock().unwrap();
    if !reused_poetry_refs.is_empty() {
        details["autopin_poetry_lock_used"] = Value::Array(
            reused_poetry_refs
                .iter()
                .cloned()
                .map(Value::String)
                .collect(),
        );
    }
    let skipped_poetry_refs = skipped_poetry.lock().unwrap();
    if !skipped_poetry_refs.is_empty() {
        details["autopin_poetry_lock_skipped"] = Value::Array(skipped_poetry_refs.clone());
    }

    let snapshot = match manifest_snapshot_at(&root) {
        Ok(snapshot) => snapshot,
        Err(err) => {
            if pyproject_modified {
                rollback_failed_migration(&backups, &created_files)?;
            }
            return Err(err);
        }
    };
    let lock_needs_backup = snapshot.lock_path.exists() && !lock_is_fresh(&snapshot)?;
    if lock_needs_backup {
        backups.backup(&snapshot.lock_path)?;
    }
    let install_outcome = match install_snapshot(ctx, &snapshot, false, install_override.as_ref()) {
        Ok(ok) => ok,
        Err(err) => {
            if pyproject_modified {
                rollback_failed_migration(&backups, &created_files)?;
            }
            match err.downcast::<InstallUserError>() {
                Ok(user) => return Ok(ExecutionOutcome::user_error(user.message, user.details)),
                Err(err) => return Err(err),
            }
        }
    };

    if let Err(err) = refresh_project_site(&snapshot, ctx) {
        if pyproject_modified {
            rollback_failed_migration(&backups, &created_files)?;
        }
        return Err(err);
    }

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
        if let Some(extra) = foreign_hint.as_ref() {
            if hint.is_empty() {
                hint = extra.clone();
            } else {
                hint = format!("{hint} • {extra}");
            }
        }
        if let Some(extra) = foreign_owner_hint.as_ref() {
            if hint.is_empty() {
                hint = extra.clone();
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
        if let Some(extra) = foreign_hint.as_ref() {
            hint = format!("{hint} • {extra}");
        }
        if let Some(extra) = foreign_owner_hint.as_ref() {
            hint = format!("{hint} • {extra}");
        }
        details["hint"] = Value::String(hint);
        Ok(ExecutionOutcome::success(
            "px migrate: nothing to apply (already in sync)",
            details,
        ))
    }
}

fn detect_foreign_tool_sections(path: &PathBuf) -> Result<Vec<String>> {
    let contents = fs::read_to_string(path)?;
    let doc: DocumentMut = contents.parse()?;
    let tool_table = doc
        .get("tool")
        .and_then(toml_edit::Item::as_table)
        .map(toml_edit::Table::iter)
        .into_iter()
        .flatten();

    let known = ["poetry", "pdm", "hatch", "flit", "rye"];
    let mut found = Vec::new();
    for (key, _) in tool_table {
        if known.contains(&key) {
            found.push(key.to_string());
        }
    }
    found.sort();
    found.dedup();
    Ok(found)
}

fn item_has_dependencies(item: &toml_edit::Item) -> bool {
    if let Some(array) = item.as_array() {
        return !array.is_empty();
    }
    if let Some(table) = item.as_table() {
        return !table.is_empty();
    }
    false
}

fn table_declares_dependencies(table: &toml_edit::Table) -> bool {
    for key in ["dependencies", "dev-dependencies"] {
        if let Some(entry) = table.get(key) {
            if item_has_dependencies(entry) {
                return true;
            }
        }
    }

    if let Some(group_table) = table.get("group").and_then(toml_edit::Item::as_table) {
        for (_, group_item) in group_table.iter() {
            if let Some(group_entry) = group_item.as_table() {
                if table_declares_dependencies(group_entry) {
                    return true;
                }
            }
        }
    }
    false
}

fn detect_foreign_tool_conflicts(path: &PathBuf) -> Result<Vec<String>> {
    let contents = fs::read_to_string(path)?;
    let doc: DocumentMut = contents.parse()?;
    let Some(tool_table) = doc.get("tool").and_then(toml_edit::Item::as_table) else {
        return Ok(Vec::new());
    };

    let known = ["poetry", "pdm", "hatch", "flit", "rye"];
    let mut owners = Vec::new();
    for (key, value) in tool_table.iter() {
        if !known.contains(&key) {
            continue;
        }
        if let Some(table) = value.as_table() {
            if table_declares_dependencies(table) {
                owners.push(key.to_string());
            }
        }
    }
    owners.sort();
    owners.dedup();
    Ok(owners)
}

fn rollback_failed_migration(backups: &BackupManager, created_files: &[PathBuf]) -> Result<()> {
    backups.restore_all()?;
    for path in created_files {
        if path.exists() {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn poetry_lock_versions(root: &Path) -> Result<Option<HashMap<String, String>>> {
    let path = root.join("poetry.lock");
    if !path.exists() {
        return Ok(None);
    }
    let contents = fs::read_to_string(&path)
        .with_context(|| format!("failed to read poetry.lock at {}", path.display()))?;
    let doc: DocumentMut = contents
        .parse()
        .with_context(|| format!("failed to parse poetry.lock at {}", path.display()))?;
    let Some(packages) = doc.get("package").and_then(Item::as_array_of_tables) else {
        return Ok(None);
    };
    let mut versions = HashMap::new();
    for package in packages.iter() {
        let Some(name) = package.get("name").and_then(Item::as_str) else {
            continue;
        };
        let Some(version) = package.get("version").and_then(Item::as_str) else {
            continue;
        };
        versions
            .entry(normalize_dist_name(name))
            .or_insert_with(|| version.to_string());
    }
    Ok(Some(versions))
}

fn merge_pin_sets(existing: &mut Vec<PinSpec>, extra: Vec<PinSpec>) {
    let mut seen: HashSet<String> = existing.iter().map(autopin_pin_key).collect();
    for pin in extra {
        let key = autopin_pin_key(&pin);
        if seen.insert(key) {
            existing.push(pin);
        }
    }
}

enum PoetryPinChoice {
    Reuse { pin: PinSpec, source: String },
    Skip(String),
}

fn pin_from_poetry_lock(
    spec: &str,
    versions: &HashMap<String, String>,
    marker_env: &MarkerEnvironment,
) -> PoetryPinChoice {
    let requirement = match PepRequirement::from_str(spec.trim()) {
        Ok(req) => req,
        Err(err) => return PoetryPinChoice::Skip(err.to_string()),
    };
    if !requirement.evaluate_markers(marker_env, &[]) {
        return PoetryPinChoice::Skip("markers_do_not_apply".to_string());
    }
    let name = requirement.name.to_string();
    let normalized = normalize_dist_name(&name);
    let Some(version) = versions.get(&normalized) else {
        return PoetryPinChoice::Skip("not_in_poetry_lock".to_string());
    };

    // Ensure the locked version satisfies the current specifiers.
    let satisfies = match &requirement.version_or_url {
        Some(pep508_rs::VersionOrUrl::VersionSpecifier(specifiers)) => {
            VersionSpecifiers::from_str(&specifiers.to_string())
                .ok()
                .and_then(|specs| {
                    Version::from_str(version)
                        .ok()
                        .map(|ver| specs.contains(&ver))
                })
                .unwrap_or(false)
        }
        Some(pep508_rs::VersionOrUrl::Url(_)) => false,
        None => false,
    };
    if !satisfies {
        return PoetryPinChoice::Skip(format!(
            "poetry.lock has {version} which does not satisfy current spec"
        ));
    }

    let extras = px_domain::canonical_extras(
        &requirement
            .extras
            .iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>(),
    );
    let marker = requirement.marker.as_ref().map(ToString::to_string);
    let specifier = format_specifier(&normalized, &extras, version, marker.as_deref());
    let source_normalized = normalized.clone();
    PoetryPinChoice::Reuse {
        pin: PinSpec {
            name,
            specifier,
            version: version.clone(),
            normalized,
            extras,
            marker,
            direct: true,
            requires: Vec::new(),
        },
        source: format!("{source_normalized}=={version} (poetry.lock)"),
    }
}
