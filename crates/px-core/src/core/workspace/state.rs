use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::json;

use crate::core::runtime::validate_cas_environment;
use crate::core::runtime::FileSystem;
use crate::core::tooling::diagnostics;
use crate::{
    compute_lock_hash, detect_runtime_metadata, CommandContext, ExecutionOutcome, InstallUserError,
    StoredEnvironment, StoredRuntime,
};
use px_domain::api::{detect_lock_drift, load_lockfile_optional};

use super::{WorkspaceSnapshot, WorkspaceStateKind, WorkspaceStateReport};

fn workspace_state_path(root: &Path) -> PathBuf {
    root.join(".px").join("workspace-state.json")
}

pub(crate) fn load_workspace_state(
    filesystem: &dyn FileSystem,
    root: &Path,
) -> Result<ProjectState> {
    let path = workspace_state_path(root);
    match filesystem.read_to_string(&path) {
        Ok(contents) => {
            let state: ProjectState = serde_json::from_str(&contents)
                .with_context(|| format!("failed to parse {}", path.display()))?;
            validate_project_state(&state)?;
            Ok(state)
        }
        Err(err) => {
            if filesystem.metadata(&path).is_ok() {
                Err(err)
            } else {
                Ok(ProjectState::default())
            }
        }
    }
}

pub(super) fn persist_workspace_state(
    filesystem: &dyn FileSystem,
    root: &Path,
    env: StoredEnvironment,
    runtime: StoredRuntime,
) -> Result<()> {
    let mut state = load_workspace_state(filesystem, root)?;
    state.current_env = Some(env);
    state.runtime = Some(runtime);
    write_project_state(filesystem, &workspace_state_path(root), &state)
}

fn write_project_state(
    filesystem: &dyn FileSystem,
    path: &Path,
    state: &ProjectState,
) -> Result<()> {
    let mut contents = serde_json::to_vec_pretty(state)?;
    contents.push(b'\n');
    if let Some(dir) = path.parent() {
        filesystem.create_dir_all(dir)?;
    }
    let tmp_path = path.with_extension("tmp");
    filesystem.write(&tmp_path, &contents)?;
    match fs::rename(&tmp_path, path) {
        Ok(_) => Ok(()),
        Err(_err) if path.exists() => {
            fs::remove_file(path)?;
            fs::rename(&tmp_path, path).with_context(|| format!("writing {}", path.display()))
        }
        Err(err) => Err(err).with_context(|| format!("writing {}", path.display())),
    }
}

pub(crate) fn evaluate_workspace_state(
    ctx: &CommandContext,
    workspace: &WorkspaceSnapshot,
) -> Result<WorkspaceStateReport> {
    let manifest_exists = workspace.config.manifest_path.exists();
    let lock_exists = workspace.lock_path.exists();
    let mut manifest_clean = false;
    let mut env_clean = false;
    let mut lock_fingerprint = None;
    let mut lock_issue = None;
    let mut lock_id = None;
    let mut env_issue = None;
    let env_state = load_workspace_state(ctx.fs(), &workspace.config.root)?;

    if lock_exists {
        if let Some(lock) = load_lockfile_optional(&workspace.lock_path)? {
            lock_fingerprint = lock.manifest_fingerprint.clone();
            let marker_env = ctx.marker_environment().ok();
            let drift = detect_lock_drift(&workspace.lock_snapshot(), &lock, marker_env.as_ref());
            if drift.is_empty()
                && lock
                    .manifest_fingerprint
                    .as_deref()
                    .is_some_and(|fp| fp == workspace.manifest_fingerprint)
            {
                manifest_clean = true;
            } else if !drift.is_empty() {
                lock_issue = Some(drift);
            }
            lock_id = Some(
                lock.lock_id
                    .clone()
                    .unwrap_or(compute_lock_hash(&workspace.lock_path)?),
            );
            if let Some(lock_id) = lock_id.as_deref() {
                match ensure_workspace_env_matches(ctx, workspace, &env_state, lock_id) {
                    Ok(()) => env_clean = true,
                    Err(user) => env_issue = Some(user.details),
                }
            }
        }
    }

    let deps_empty = workspace.deps_empty();
    let env_exists = env_state
        .current_env
        .as_ref()
        .map(|env| {
            env.env_path
                .as_ref()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(&env.site_packages))
                .exists()
        })
        .unwrap_or(false);
    let canonical = if !manifest_exists {
        WorkspaceStateKind::Uninitialized
    } else if !lock_exists || !manifest_clean {
        WorkspaceStateKind::NeedsLock
    } else if !env_clean {
        WorkspaceStateKind::NeedsEnv
    } else if deps_empty {
        WorkspaceStateKind::InitializedEmpty
    } else {
        WorkspaceStateKind::Consistent
    };

    Ok(WorkspaceStateReport {
        manifest_exists,
        lock_exists,
        env_exists,
        manifest_clean,
        env_clean,
        deps_empty,
        canonical,
        manifest_fingerprint: Some(workspace.manifest_fingerprint.clone()),
        lock_fingerprint,
        lock_id,
        lock_issue,
        env_issue,
    })
}

fn ensure_workspace_env_matches(
    ctx: &CommandContext,
    workspace: &WorkspaceSnapshot,
    env_state: &ProjectState,
    lock_id: &str,
) -> Result<(), InstallUserError> {
    let Some(env) = env_state.current_env.as_ref() else {
        return Err(InstallUserError::new(
            "workspace environment missing",
            json!({
                "hint": "run `px sync` to build the workspace environment",
                "reason": "missing_env",
            }),
        ));
    };
    let site_dir = PathBuf::from(&env.site_packages);
    if env.lock_id != lock_id {
        return Err(InstallUserError::new(
            "workspace environment is out of date",
            json!({
                "expected_lock_id": lock_id,
                "current_lock_id": env.lock_id,
                "hint": "run `px sync` to rebuild the workspace environment",
                "reason": "env_outdated",
            }),
        ));
    }
    if !site_dir.exists() {
        return Err(InstallUserError::new(
            "workspace environment missing",
            json!({
                "site": env.site_packages,
                "hint": "run `px sync` to rebuild the workspace environment",
                "reason": "missing_env",
            }),
        ));
    }
    let snapshot = workspace.lock_snapshot();
    let runtime_selection = crate::prepare_project_runtime(&snapshot).map_err(|err| {
        InstallUserError::new(
            "workspace runtime unavailable",
            json!({
                "error": err.to_string(),
                "hint": "install or select a compatible Python runtime, then rerun",
                "reason": "runtime_unavailable",
            }),
        )
    })?;
    let runtime = match env_state.runtime.as_ref().filter(|stored| {
        stored.path == runtime_selection.record.path
            && stored.version == runtime_selection.record.full_version
            && !stored.platform.trim().is_empty()
    }) {
        Some(stored) => crate::RuntimeMetadata {
            path: stored.path.clone(),
            version: stored.version.clone(),
            platform: stored.platform.clone(),
        },
        None => detect_runtime_metadata(ctx, &snapshot).map_err(|err| {
            InstallUserError::new(
                "workspace runtime unavailable",
                json!({
                    "error": err.to_string(),
                    "hint": "install or select a compatible Python runtime, then rerun",
                    "reason": "runtime_unavailable",
                }),
            )
        })?,
    };
    if runtime.version != env.python.version || runtime.platform != env.platform {
        return Err(InstallUserError::new(
            format!(
                "workspace environment targets Python {} ({}) but {} ({}) is active",
                env.python.version, env.platform, runtime.version, runtime.platform
            ),
            json!({
                "expected_python": env.python.version,
                "current_python": runtime.version,
                "expected_platform": env.platform,
                "current_platform": runtime.platform,
                "hint": "run `px sync` to rebuild for the current runtime",
                "reason": "runtime_mismatch",
            }),
        ));
    }
    if env.profile_oid.is_none() {
        return Err(InstallUserError::new(
            "workspace environment CAS profile missing",
            json!({
                "reason": "missing_env",
                "code": diagnostics::cas::MISSING_OR_CORRUPT,
                "hint": "run `px sync` to rebuild the workspace environment",
            }),
        ));
    }
    if let Err(err) = validate_cas_environment(env) {
        return match err.downcast::<InstallUserError>() {
            Ok(user) => Err(user),
            Err(other) => Err(InstallUserError::new(
                "workspace environment verification failed",
                json!({
                    "reason": "env_outdated",
                    "code": diagnostics::cas::MISSING_OR_CORRUPT,
                    "error": other.to_string(),
                }),
            )),
        };
    }
    Ok(())
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct ProjectState {
    #[serde(default)]
    pub(crate) current_env: Option<StoredEnvironment>,
    #[serde(default)]
    pub(crate) runtime: Option<StoredRuntime>,
}

fn validate_project_state(state: &ProjectState) -> Result<()> {
    if let Some(env) = &state.current_env {
        if env.id.trim().is_empty() || env.lock_id.trim().is_empty() {
            anyhow::bail!("invalid workspace state: missing environment identity");
        }
        if env.site_packages.trim().is_empty() {
            anyhow::bail!("invalid workspace state: missing site-packages path");
        }
        if env.python.path.trim().is_empty() || env.python.version.trim().is_empty() {
            anyhow::bail!("invalid workspace state: missing python metadata");
        }
    }
    if let Some(runtime) = &state.runtime {
        if runtime.path.trim().is_empty()
            || runtime.version.trim().is_empty()
            || runtime.platform.trim().is_empty()
        {
            anyhow::bail!("invalid workspace runtime metadata");
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum StateViolation {
    MissingLock,
    LockDrift,
    EnvDrift,
}

pub(crate) fn workspace_violation(
    command: &str,
    snapshot: &WorkspaceSnapshot,
    state_report: &WorkspaceStateReport,
    violation: StateViolation,
) -> ExecutionOutcome {
    let mut base = json!({
        "workspace": snapshot.config.root.display().to_string(),
        "manifest": snapshot.config.manifest_path.display().to_string(),
        "lockfile": snapshot.lock_path.display().to_string(),
        "state": state_report.canonical.as_str(),
    });
    match violation {
        StateViolation::MissingLock => {
            let hint = if command == "sync" {
                "Run `px sync` without --frozen to generate px.workspace.lock before syncing."
                    .to_string()
            } else {
                format!("Run `px sync` before `px {command}`.")
            };
            base["hint"] = json!(hint);
            base["code"] = json!("PX120");
            base["reason"] = json!("missing_lock");
            ExecutionOutcome::user_error("workspace lock not found", base)
        }
        StateViolation::LockDrift => {
            let mut details = base;
            details["hint"] = json!("Run `px sync` to update the workspace lock and environment.");
            details["code"] = json!("PX120");
            details["reason"] = json!("lock_drift");
            if let Some(fp) = &state_report.lock_fingerprint {
                details["lock_fingerprint"] = json!(fp);
            }
            if let Some(fp) = &state_report.manifest_fingerprint {
                details["manifest_fingerprint"] = json!(fp);
            }
            if let Some(lock_id) = &state_report.lock_id {
                details["lock_id"] = json!(lock_id);
            }
            if let Some(issues) = &state_report.lock_issue {
                details["lock_issue"] = json!(issues);
            }
            ExecutionOutcome::user_error(
                "Workspace manifest has changed since px.workspace.lock was created",
                details,
            )
        }
        StateViolation::EnvDrift => {
            let mut reason = "env_outdated".to_string();
            let mut details = base;
            details["hint"] = json!(format!(
                "Run `px sync` before `px {command}` (environment is out of sync)."
            ));
            details["code"] = json!("PX201");
            if let Some(issue) = &state_report.env_issue {
                details["environment_issue"] = issue.clone();
                if let Some(r) = issue.get("reason").and_then(serde_json::Value::as_str) {
                    reason = r.to_string();
                }
            }
            if let Some(lock_id) = &state_report.lock_id {
                details["lock_id"] = json!(lock_id);
            }
            details["reason"] = json!(reason);
            let message = if reason == "missing_env" {
                "workspace environment missing"
            } else {
                "Workspace environment is out of sync with px.workspace.lock"
            };
            ExecutionOutcome::user_error(message, details)
        }
    }
}
