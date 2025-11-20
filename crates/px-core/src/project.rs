use std::fmt::Write as _;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    env, fs,
    path::{Path, PathBuf},
    str::FromStr,
};

use anyhow::{anyhow, Context, Result};
use pep508_rs::{Requirement as PepRequirement, VersionOrUrl};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use toml_edit::{DocumentMut, Item};

use crate::{
    compute_lock_hash, dependency_name, detect_runtime_metadata, ensure_env_matches_lock,
    install_snapshot, load_project_state, manifest_snapshot, manifest_snapshot_at,
    persist_resolved_dependencies, python_context_with_mode, refresh_project_site,
    relative_path_str, resolve_dependencies_with_effects, CommandContext, EnvGuard,
    ExecutionOutcome, InstallOutcome, InstallState, InstallUserError, ManifestSnapshot,
    PythonContext,
};
use px_domain::{
    collect_resolved_dependencies, detect_lock_drift, discover_project_root, infer_package_name,
    load_lockfile_optional, project_name_from_pyproject, state::ProjectStateReport,
    InstallOverride, ManifestEditor, ProjectInitializer,
};

#[derive(Clone, Debug)]
pub struct ProjectInitRequest {
    pub package: Option<String>,
    pub python: Option<String>,
    pub dry_run: bool,
    pub force: bool,
}

#[derive(Clone, Debug)]
pub struct ProjectSyncRequest {
    pub frozen: bool,
}

#[derive(Clone, Debug)]
pub struct ProjectAddRequest {
    pub specs: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct ProjectRemoveRequest {
    pub specs: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct ProjectUpdateRequest {
    pub specs: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct ProjectWhyRequest {
    pub package: Option<String>,
    pub issue: Option<String>,
}

/// Initializes a px project in the current directory.
///
/// # Errors
/// Returns an error if filesystem access or dependency installation fails.
pub fn project_init(
    ctx: &CommandContext,
    request: &ProjectInitRequest,
) -> Result<ExecutionOutcome> {
    let cwd = env::current_dir().context("unable to determine current directory")?;
    if let Some(existing_root) = discover_project_root()? {
        return existing_pyproject_response(&existing_root.join("pyproject.toml"));
    }
    let root = cwd;
    let pyproject_path = root.join("pyproject.toml");
    let pyproject_preexisting = pyproject_path.exists();

    if pyproject_preexisting {
        if let Some(conflict) = detect_init_conflict(&pyproject_path)? {
            return Ok(conflict.into_outcome(&pyproject_path));
        }
    }

    if !request.force {
        if let Some(changes) = ctx.git().worktree_changes(&root)? {
            if !changes.is_empty() {
                return Ok(dirty_worktree_response(&changes));
            }
        }
    }

    let package_arg = request
        .package
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let (package, inferred) = infer_package_name(package_arg, &root)?;
    let python_req = resolve_python_requirement_arg(request.python.as_deref());

    let mut files = ProjectInitializer::scaffold(&root, &package, &python_req)?;
    let snapshot = manifest_snapshot_at(&root)?;
    let actual_name = snapshot.name.clone();
    let lock_existed = snapshot.lock_path.exists();
    match install_snapshot(ctx, &snapshot, false, None) {
        Ok(_) => {}
        Err(err) => match err.downcast::<InstallUserError>() {
            Ok(user) => return Ok(ExecutionOutcome::user_error(user.message, user.details)),
            Err(err) => return Err(err),
        },
    }
    refresh_project_site(&snapshot, ctx)?;
    if !lock_existed {
        files.push(relative_path_str(&snapshot.lock_path, &snapshot.root));
    }

    let mut details = json!({
        "package": actual_name,
        "python": python_req,
        "files_created": files,
        "project_root": root.display().to_string(),
        "lockfile": snapshot.lock_path.display().to_string(),
    });
    if inferred && !pyproject_preexisting {
        details["inferred_package"] = Value::Bool(true);
        details["hint"] = Value::String(
            "Pass --package <name> to override the inferred module name.".to_string(),
        );
    }

    Ok(ExecutionOutcome::success(
        format!("initialized project {actual_name}"),
        details,
    ))
}

/// Reconciles the px environment with the lockfile.
///
/// # Errors
/// Returns an error if dependency installation fails.
pub fn project_sync(
    ctx: &CommandContext,
    request: &ProjectSyncRequest,
) -> Result<ExecutionOutcome> {
    project_sync_outcome(ctx, request.frozen)
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

    let root = ctx.project_root()?;
    let pyproject_path = root.join("pyproject.toml");
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
            needs_restore = false;
            return Ok(ExecutionOutcome::success(
                "dependencies already satisfied",
                json!({ "pyproject": pyproject_path.display().to_string() }),
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

    let root = ctx.project_root()?;
    let pyproject_path = root.join("pyproject.toml");
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
            needs_restore = false;
            return Ok(ExecutionOutcome::user_error(
                "package is not a direct dependency",
                json!({
                    "packages": names,
                    "hint": "Use `px why <package>` to inspect transitive requirements.",
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

/// Reports whether the manifest, lockfile, and environment are consistent.
///
/// # Errors
/// Returns an error if project metadata cannot be read or dependency verification fails.
pub fn project_status(ctx: &CommandContext) -> Result<ExecutionOutcome> {
    let snapshot = manifest_snapshot()?;
    let state_report = evaluate_project_state(ctx, &snapshot)?;
    let outcome = match install_snapshot(ctx, &snapshot, true, None) {
        Ok(outcome) => outcome,
        Err(err) => match err.downcast::<InstallUserError>() {
            Ok(user) => return Ok(ExecutionOutcome::user_error(user.message, user.details)),
            Err(err) => return Err(err),
        },
    };
    let mut details = json!({
        "pyproject": snapshot.manifest_path.display().to_string(),
        "lockfile": snapshot.lock_path.display().to_string(),
        "project": {
            "root": snapshot.root.display().to_string(),
            "name": snapshot.name.clone(),
            "python_requirement": snapshot.python_requirement.clone(),
        },
    });
    details["state"] = Value::String(state_report.canonical.as_str().to_string());
    details["flags"] = state_report.flags_json();
    if let Some(fp) = state_report.manifest_fingerprint.clone() {
        details["manifest_fingerprint"] = Value::String(fp);
    }
    if let Some(fp) = state_report.lock_fingerprint.clone() {
        details["lock_fingerprint"] = Value::String(fp);
    }
    if let Some(id) = state_report.lock_id.clone() {
        details["lock_id"] = Value::String(id);
    }
    if let Some(issue) = state_report.env_issue.clone() {
        details["environment_issue"] = issue;
    }
    details["runtime"] = detect_runtime_details(ctx, &snapshot);
    details["environment"] =
        collect_environment_status(ctx, &snapshot, outcome.state != InstallState::MissingLock)?;
    match outcome.state {
        InstallState::UpToDate => {
            details["status"] = Value::String("in-sync".to_string());
            Ok(ExecutionOutcome::success(
                "Environment is in sync with px.lock",
                details,
            ))
        }
        InstallState::Drift => {
            details["status"] = Value::String("drift".to_string());
            details["issues"] = issue_values(outcome.drift);
            details["hint"] = Value::String("Run `px sync` to refresh px.lock".to_string());
            Ok(ExecutionOutcome::user_error(
                "Environment is out of sync with px.lock",
                details,
            ))
        }
        InstallState::MissingLock => {
            details["status"] = Value::String("missing-lock".to_string());
            details["hint"] = Value::String("Run `px sync` to create px.lock".to_string());
            Ok(ExecutionOutcome::user_error("px.lock not found", details))
        }
        InstallState::Installed => Ok(ExecutionOutcome::failure(
            "Unable to determine project status",
            json!({ "status": "unknown" }),
        )),
    }
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
    let root = ctx.project_root()?;
    let pyproject_path = root.join("pyproject.toml");
    let lock_path = root.join("px.lock");
    if !lock_path.exists() {
        return Ok(ExecutionOutcome::user_error(
            "px update requires an existing px.lock (run `px sync`)",
            json!({
                "lockfile": lock_path.display().to_string(),
                "hint": "run `px sync` to create px.lock before updating",
            }),
        ));
    }

    let snapshot = manifest_snapshot()?;
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
        });
        if !unsupported.is_empty() {
            details["unsupported"] = json!(unsupported);
            details["hint"] = json!("Dependencies pinned via direct URLs must be updated manually");
        }
        let message = if update_all {
            "no dependencies eligible for px update"
        } else {
            "px update: requested packages cannot be updated"
        };
        return Ok(ExecutionOutcome::user_error(message, details));
    }

    let mut override_snapshot = snapshot.clone();
    override_snapshot.dependencies = override_specs;
    let resolved = match resolve_dependencies_with_effects(ctx.effects(), &override_snapshot) {
        Ok(resolved) => resolved,
        Err(err) => match err.downcast::<InstallUserError>() {
            Ok(user) => return Ok(ExecutionOutcome::user_error(user.message, user.details)),
            Err(err) => return Err(err),
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
                Ok(user) => return Ok(ExecutionOutcome::user_error(user.message, user.details)),
                Err(err) => return Err(err),
            },
        };

    refresh_project_site(&updated_snapshot, ctx)?;

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
            Ok(ExecutionOutcome::success(message, details))
        }
        InstallState::Drift | InstallState::MissingLock => Ok(ExecutionOutcome::failure(
            "px update failed to refresh px.lock",
            json!({ "lockfile": snapshot.lock_path.display().to_string() }),
        )),
    }
}

/// Explains why a dependency or issue exists in the project.
///
/// # Errors
/// Returns an error if px.lock cannot be read or dependency graphs are unavailable.
/// # Panics
/// Panics if the resolver returns inconsistent dependency data.
pub fn project_why(ctx: &CommandContext, request: &ProjectWhyRequest) -> Result<ExecutionOutcome> {
    if let Some(issue) = request.issue.as_deref() {
        return explain_issue(ctx, issue);
    }
    let package = match request.package.as_deref() {
        Some(pkg) if !pkg.trim().is_empty() => pkg.trim().to_string(),
        _ => {
            return Ok(ExecutionOutcome::user_error(
                "px why requires a package name",
                json!({ "hint": "run `px why <package>` to inspect dependencies" }),
            ))
        }
    };
    let snapshot = manifest_snapshot()?;
    let target = dependency_name(&package);
    if target.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            "unable to normalize package name",
            json!({
                "package": package,
                "hint": "use names like `rich` or `requests`",
            }),
        ));
    }
    let roots: HashSet<String> = snapshot
        .dependencies
        .iter()
        .map(|spec| dependency_name(spec))
        .filter(|name| !name.is_empty())
        .collect();
    let (py_ctx, _) = match python_context_with_mode(ctx, EnvGuard::Strict) {
        Ok(value) => value,
        Err(outcome) => return Ok(outcome),
    };
    let graph = match collect_dependency_graph(ctx, &py_ctx) {
        Ok(graph) => graph,
        Err(outcome) => return Ok(outcome),
    };
    let entry = graph.packages.get(&target);
    if entry.is_none() {
        return Ok(ExecutionOutcome::user_error(
            format!("{package} is not installed in this project"),
            json!({
                "package": package,
                "hint": "run `px sync` to refresh the environment, then retry",
            }),
        ));
    }
    let entry = entry.unwrap();
    let chains = find_dependency_chains(&graph.reverse, &roots, &target, 5);
    let direct = roots.contains(&target);
    let version = entry.version.clone();
    let message = if direct {
        format!("{}=={} is declared in pyproject.toml", entry.name, version)
    } else if chains.is_empty() {
        format!(
            "{}=={} is present but no dependency chain was found",
            entry.name, version
        )
    } else {
        let chain = chains
            .first()
            .map_or_else(|| entry.name.clone(), |path| path.join(" -> "));
        format!("{}=={} is required by {chain}", entry.name, version)
    };
    let details = json!({
        "package": entry.name,
        "normalized": target,
        "version": version,
        "direct": direct,
        "chains": chains,
    });
    Ok(ExecutionOutcome::success(message, details))
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

fn explain_issue(_ctx: &CommandContext, issue_id: &str) -> Result<ExecutionOutcome> {
    let trimmed = issue_id.trim();
    if trimmed.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            "px why --issue requires an ID",
            json!({ "hint": "Run `px status` to list current issue IDs." }),
        ));
    }
    let snapshot = manifest_snapshot()?;
    let Some(lock) = load_lockfile_optional(&snapshot.lock_path)? else {
        return Ok(ExecutionOutcome::user_error(
            "px why --issue: px.lock not found",
            json!({
                "lockfile": snapshot.lock_path.display().to_string(),
                "hint": "Run `px sync` to create px.lock before inspecting issues.",
            }),
        ));
    };
    let drift = detect_lock_drift(&snapshot, &lock, None);
    let mut normalized = trimmed.to_string();
    normalized.make_ascii_uppercase();
    for message in drift {
        let id = issue_id_for(&message);
        if id.eq_ignore_ascii_case(&normalized) {
            let summary = format!("Issue {id}: {message}");
            let details = json!({
                "id": id,
                "message": message,
                "pyproject": snapshot.manifest_path.display().to_string(),
                "lockfile": snapshot.lock_path.display().to_string(),
            });
            return Ok(ExecutionOutcome::success(summary, details));
        }
    }
    Ok(ExecutionOutcome::user_error(
        format!("issue {issue_id} not found"),
        json!({
            "issue": issue_id,
            "hint": "Run `px status` to list current issue IDs before retrying.",
        }),
    ))
}

fn collect_environment_status(
    ctx: &CommandContext,
    snapshot: &ManifestSnapshot,
    lock_ready: bool,
) -> Result<Value> {
    if !lock_ready {
        return Ok(json!({
            "status": "unknown",
            "reason": "missing-lock",
            "hint": "Run `px sync` to create px.lock before checking the environment.",
        }));
    }
    let lock = match load_lockfile_optional(&snapshot.lock_path)? {
        Some(lock) => lock,
        None => {
            return Ok(json!({
                "status": "unknown",
                "reason": "missing-lock",
                "hint": "Run `px sync` to create px.lock before checking the environment.",
            }))
        }
    };
    let state = load_project_state(ctx.fs(), &snapshot.root);
    let Some(env) = state.current_env.clone() else {
        return Ok(json!({
            "status": "missing",
            "reason": "uninitialized",
            "hint": "Run `px sync` to build the px environment.",
        }));
    };
    let lock_hash = match lock.lock_id.clone() {
        Some(value) => value,
        None => compute_lock_hash(&snapshot.lock_path)?,
    };
    let mut details = json!({
        "status": "in-sync",
        "env": {
            "id": env.id,
            "site": env.site_packages,
            "python": env.python.version,
            "platform": env.platform,
        },
    });
    match ensure_env_matches_lock(ctx, snapshot, &lock_hash) {
        Ok(()) => Ok(details),
        Err(err) => match err.downcast::<InstallUserError>() {
            Ok(user) => {
                let status = match user.details.get("reason").and_then(Value::as_str) {
                    Some("missing_env") => "missing",
                    _ => "out-of-sync",
                };
                details["status"] = Value::String(status.to_string());
                if let Some(reason) = user.details.get("reason") {
                    details["reason"] = reason.clone();
                }
                if let Some(hint) = user.details.get("hint") {
                    details["hint"] = hint.clone();
                }
                Ok(details)
            }
            Err(other) => Err(other),
        },
    }
}

fn issue_values(messages: Vec<String>) -> Value {
    let entries: Vec<Value> = messages
        .into_iter()
        .map(|message| {
            let id = issue_id_for(&message);
            json!({
                "id": id,
                "message": message,
            })
        })
        .collect();
    Value::Array(entries)
}

fn issue_id_for(message: &str) -> String {
    let digest = Sha256::digest(message.as_bytes());
    let mut short = String::new();
    for byte in &digest[..6] {
        let _ = write!(&mut short, "{byte:02x}");
    }
    format!("ISS-{}", short.to_ascii_uppercase())
}

fn evaluate_project_state(
    ctx: &CommandContext,
    snapshot: &ManifestSnapshot,
) -> Result<ProjectStateReport> {
    let manifest_exists = snapshot.manifest_path.exists();
    let manifest_fingerprint = manifest_exists.then(|| snapshot.manifest_fingerprint.clone());
    let lock = load_lockfile_optional(&snapshot.lock_path)?;
    let lock_exists = lock.is_some();
    let lock_fingerprint = lock
        .as_ref()
        .and_then(|lock| lock.manifest_fingerprint.clone());
    let mut manifest_clean = false;
    if manifest_exists && lock_exists {
        manifest_clean = match (&manifest_fingerprint, &lock_fingerprint) {
            (Some(manifest), Some(lock_fp)) => manifest == lock_fp,
            (Some(_), None) => detect_lock_drift(snapshot, lock.as_ref().unwrap(), None).is_empty(),
            _ => false,
        };
    }
    let mut lock_id = None;
    if let Some(lock) = &lock {
        lock_id = match lock.lock_id.clone() {
            Some(id) => Some(id),
            None => Some(compute_lock_hash(&snapshot.lock_path)?),
        };
    }

    let state = load_project_state(ctx.fs(), &snapshot.root);
    let env_exists = state.current_env.is_some();
    let mut env_clean = false;
    let mut env_issue = None;
    if manifest_clean {
        if let Some(lock_id) = lock_id.as_deref() {
            match ensure_env_matches_lock(ctx, snapshot, lock_id) {
                Ok(()) => env_clean = true,
                Err(err) => match err.downcast::<InstallUserError>() {
                    Ok(user) => env_issue = Some(user.details),
                    Err(other) => return Err(other),
                },
            }
        }
    }

    Ok(ProjectStateReport::new(
        manifest_exists,
        lock_exists,
        env_exists,
        manifest_clean,
        env_clean,
        snapshot.dependencies.is_empty(),
        manifest_fingerprint,
        lock_fingerprint,
        lock_id,
        env_issue,
    ))
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

fn resolve_python_requirement_arg(raw: Option<&str>) -> String {
    raw.map(str::trim).filter(|s| !s.is_empty()).map_or_else(
        || ">=3.11".to_string(),
        |s| {
            if s.starts_with('>') {
                s.to_string()
            } else {
                format!(">={s}")
            }
        },
    )
}

#[derive(Debug)]
enum InitConflict {
    OtherTool(String),
    ExistingDependencies,
}

impl InitConflict {
    fn into_outcome(self, pyproject_path: &Path) -> ExecutionOutcome {
        match self {
            InitConflict::OtherTool(tool) => ExecutionOutcome::user_error(
                format!("pyproject managed by {tool}; run `px migrate --apply` to adopt px"),
                json!({
                    "pyproject": pyproject_path.display().to_string(),
                    "tool": tool,
                    "hint": "Run `px migrate --apply` to convert this project to px.",
                }),
            ),
            InitConflict::ExistingDependencies => ExecutionOutcome::user_error(
                "pyproject already declares dependencies",
                json!({
                    "pyproject": pyproject_path.display().to_string(),
                    "hint": "Run `px migrate --apply` to import existing dependencies into px.",
                }),
            ),
        }
    }
}

fn detect_init_conflict(pyproject_path: &Path) -> Result<Option<InitConflict>> {
    let contents = fs::read_to_string(pyproject_path)?;
    let doc: DocumentMut = contents.parse()?;
    if let Some(tool) = detect_foreign_tool(&doc) {
        return Ok(Some(InitConflict::OtherTool(tool)));
    }
    if project_dependencies_declared(&doc) {
        return Ok(Some(InitConflict::ExistingDependencies));
    }
    Ok(None)
}

fn detect_foreign_tool(doc: &DocumentMut) -> Option<String> {
    let tools = doc
        .get("tool")
        .and_then(Item::as_table)
        .map(|table| {
            table
                .iter()
                .filter_map(|(key, _)| {
                    let name = key.to_string();
                    match name.as_str() {
                        "poetry" | "pdm" | "hatch" | "flit" | "rye" => Some(name),
                        _ => None,
                    }
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    tools.into_iter().next()
}

fn project_dependencies_declared(doc: &DocumentMut) -> bool {
    doc.get("project")
        .and_then(Item::as_table)
        .and_then(|table| table.get("dependencies"))
        .and_then(Item::as_array)
        .is_some_and(|array| !array.is_empty())
}

fn existing_pyproject_response(pyproject_path: &Path) -> Result<ExecutionOutcome> {
    let mut details = json!({
        "pyproject": pyproject_path.display().to_string(),
    });
    if let Some(name) = project_name_from_pyproject(pyproject_path)? {
        details["package"] = Value::String(name);
    }
    details["hint"] = Value::String(
        "pyproject.toml already exists; run `px add` or start in an empty directory.".to_string(),
    );
    Ok(ExecutionOutcome::user_error(
        "project already initialized (pyproject.toml present)",
        details,
    ))
}

fn detect_runtime_details(ctx: &CommandContext, snapshot: &ManifestSnapshot) -> Value {
    match detect_runtime_metadata(ctx, snapshot) {
        Ok(meta) => json!({
            "path": meta.path,
            "version": meta.version,
            "platform": meta.platform,
        }),
        Err(err) => json!({
            "hint": format!("failed to detect python runtime: {err}"),
        }),
    }
}

fn dirty_worktree_response(changes: &[String]) -> ExecutionOutcome {
    ExecutionOutcome::user_error(
        "worktree dirty; stash, commit, or rerun with --force",
        json!({
            "changes": changes,
            "hint": "Stash or commit changes, or add --force to bypass this guard.",
        }),
    )
}

struct ManifestLockBackup {
    pyproject_path: PathBuf,
    lock_path: PathBuf,
    pyproject_contents: String,
    lock_contents: Option<String>,
    lock_preexisting: bool,
}

impl ManifestLockBackup {
    fn capture(pyproject_path: &Path, lock_path: &Path) -> Result<Self> {
        let pyproject_contents = fs::read_to_string(pyproject_path)?;
        let lock_preexisting = lock_path.exists();
        let lock_contents = if lock_preexisting {
            Some(fs::read_to_string(lock_path)?)
        } else {
            None
        };
        Ok(Self {
            pyproject_path: pyproject_path.to_path_buf(),
            lock_path: lock_path.to_path_buf(),
            pyproject_contents,
            lock_contents,
            lock_preexisting,
        })
    }

    fn restore(&self) -> Result<()> {
        fs::write(&self.pyproject_path, &self.pyproject_contents)?;
        match (&self.lock_contents, self.lock_preexisting) {
            (Some(contents), _) => {
                fs::write(&self.lock_path, contents)?;
            }
            (None, false) => {
                if self.lock_path.exists() {
                    fs::remove_file(&self.lock_path)?;
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
}

fn sync_manifest_environment(
    ctx: &CommandContext,
) -> Result<(ManifestSnapshot, InstallOutcome), ExecutionOutcome> {
    let snapshot = match manifest_snapshot() {
        Ok(snapshot) => snapshot,
        Err(err) => {
            return Err(ExecutionOutcome::failure(
                "failed to read project manifest",
                json!({ "error": err.to_string() }),
            ))
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

fn project_sync_outcome(ctx: &CommandContext, frozen: bool) -> Result<ExecutionOutcome> {
    let snapshot = manifest_snapshot()?;
    let outcome = match install_snapshot(ctx, &snapshot, frozen, None) {
        Ok(ok) => ok,
        Err(err) => match err.downcast::<InstallUserError>() {
            Ok(user) => return Ok(ExecutionOutcome::user_error(user.message, user.details)),
            Err(err) => return Err(err),
        },
    };
    let mut details = json!({
        "lockfile": outcome.lockfile,
        "project": snapshot.name,
        "python": snapshot.python_requirement,
    });

    match outcome.state {
        InstallState::Installed => {
            refresh_project_site(&snapshot, ctx)?;
            Ok(ExecutionOutcome::success(
                format!("wrote {}", outcome.lockfile),
                details,
            ))
        }
        InstallState::UpToDate => {
            refresh_project_site(&snapshot, ctx)?;
            let message = if frozen && outcome.verified {
                "lockfile verified".to_string()
            } else {
                "px.lock already up to date".to_string()
            };
            Ok(ExecutionOutcome::success(message, details))
        }
        InstallState::Drift => {
            details["drift"] = Value::Array(outcome.drift.iter().map(|d| json!(d)).collect());
            details["hint"] = Value::String("rerun `px sync` to refresh px.lock".to_string());
            Ok(ExecutionOutcome::user_error(
                "px.lock is out of date",
                details,
            ))
        }
        InstallState::MissingLock => Ok(ExecutionOutcome::user_error(
            "px.lock not found (run `px sync`)",
            json!({
                "lockfile": outcome.lockfile,
                "project": snapshot.name,
                "python": snapshot.python_requirement,
                "hint": "run `px sync` to generate a lockfile",
            }),
        )),
    }
}

const WHY_GRAPH_SCRIPT: &str = r"
import importlib.metadata as im
import json

def normalize(name: str) -> str:
    return name.strip().lower()

packages = []
for dist in im.distributions():
    name = dist.metadata.get('Name') or dist.name
    if not name:
        continue
    normalized = normalize(name)
    requires = []
    if dist.requires:
        for req in dist.requires:
            head = req.split(';', 1)[0].strip()
            if not head:
                continue
            token = head.split()[0]
            if not token:
                continue
            base = token.split('[', 1)[0].strip()
            if base:
                requires.append(normalize(base))
    packages.append({
        'name': name,
        'normalized': normalized,
        'version': dist.version or '',
        'requires': requires,
    })

print(json.dumps({'packages': packages}))
";

struct WhyGraph {
    packages: HashMap<String, WhyPackage>,
    reverse: HashMap<String, Vec<String>>,
}

#[derive(Deserialize)]
struct WhyGraphPayload {
    packages: Vec<WhyPackage>,
}

#[derive(Clone, Deserialize)]
struct WhyPackage {
    name: String,
    normalized: String,
    version: String,
    requires: Vec<String>,
}

fn collect_dependency_graph(
    ctx: &CommandContext,
    py_ctx: &PythonContext,
) -> Result<WhyGraph, ExecutionOutcome> {
    let envs = py_ctx
        .base_env(&json!({ "reason": "why" }))
        .map_err(|err| {
            ExecutionOutcome::failure(
                "failed to prepare project environment",
                json!({ "error": err.to_string() }),
            )
        })?;
    let args = vec!["-c".to_string(), WHY_GRAPH_SCRIPT.to_string()];
    let output = ctx
        .python_runtime()
        .run_command(&py_ctx.python, &args, &envs, &py_ctx.project_root)
        .map_err(|err| {
            ExecutionOutcome::failure(
                "failed to inspect dependencies",
                json!({ "error": err.to_string() }),
            )
        })?;
    if output.code != 0 {
        return Err(ExecutionOutcome::failure(
            "python exited with errors while reading metadata",
            json!({
                "stderr": output.stderr,
                "status": output.code,
            }),
        ));
    }
    let payload: WhyGraphPayload = serde_json::from_str(output.stdout.trim()).map_err(|err| {
        ExecutionOutcome::failure(
            "invalid dependency metadata payload",
            json!({ "error": err.to_string() }),
        )
    })?;
    let mut packages = HashMap::new();
    let mut reverse: HashMap<String, Vec<String>> = HashMap::new();
    for package in payload.packages {
        for dep in &package.requires {
            let parents = reverse.entry(dep.clone()).or_default();
            if !parents.iter().any(|p| p == &package.normalized) {
                parents.push(package.normalized.clone());
            }
        }
        packages.insert(package.normalized.clone(), package);
    }

    let lock_path = py_ctx.project_root.join("px.lock");
    if lock_path.exists() {
        let lock = load_lockfile_optional(&lock_path).map_err(|err| {
            ExecutionOutcome::failure(
                "failed to read px.lock",
                json!({ "error": err.to_string(), "lockfile": lock_path.display().to_string() }),
            )
        })?;
        if let Some(lock) = lock {
            for dep in collect_resolved_dependencies(&lock) {
                let normalized = dependency_name(&dep.specifier);
                if normalized.is_empty() {
                    continue;
                }
                let version = version_from_spec(&dep.specifier);
                packages
                    .entry(normalized.clone())
                    .or_insert_with(|| WhyPackage {
                        name: dep.name.clone(),
                        normalized: normalized.clone(),
                        version: version.clone(),
                        requires: dep.requires.clone(),
                    });
                if let Some(pkg) = packages.get_mut(&normalized) {
                    if pkg.version.is_empty() {
                        pkg.version = version.clone();
                    }
                    if pkg.requires.is_empty() && !dep.requires.is_empty() {
                        pkg.requires = dep.requires.clone();
                    }
                }
                for parent in dep.requires {
                    if parent.is_empty() {
                        continue;
                    }
                    let parents = reverse.entry(parent).or_default();
                    if !parents.iter().any(|p| p == &normalized) {
                        parents.push(normalized.clone());
                    }
                }
            }
        }
    }

    Ok(WhyGraph { packages, reverse })
}

fn find_dependency_chains(
    reverse: &HashMap<String, Vec<String>>,
    roots: &HashSet<String>,
    target: &str,
    limit: usize,
) -> Vec<Vec<String>> {
    if limit == 0 {
        return Vec::new();
    }
    let mut results = Vec::new();
    let mut queue = VecDeque::new();
    queue.push_back(vec![target.to_string()]);

    while let Some(path) = queue.pop_front() {
        let current = path.last().cloned().unwrap_or_else(|| target.to_string());
        if roots.contains(&current) {
            let mut chain = path.clone();
            chain.reverse();
            results.push(chain);
            if results.len() >= limit {
                break;
            }
        }
        if let Some(parents) = reverse.get(&current) {
            for parent in parents {
                if path.iter().any(|node| node == parent) {
                    continue;
                }
                let mut next = path.clone();
                next.push(parent.clone());
                queue.push_back(next);
            }
        }
    }
    results
}

fn version_from_spec(spec: &str) -> String {
    let trimmed = spec.trim();
    let head = trimmed.split(';').next().unwrap_or(trimmed);
    if let Some((_, rest)) = head.split_once("==") {
        rest.trim().to_string()
    } else {
        String::new()
    }
}
