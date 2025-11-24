use serde_json::{json, Value};

use anyhow::Result;

use crate::{
    install_snapshot, manifest_snapshot, refresh_project_site, resolve_dependencies_with_effects,
    CommandContext, ExecutionOutcome, InstallState, InstallUserError,
};

use super::evaluate_project_state;
use crate::state_guard::StateViolation;

#[derive(Clone, Debug)]
pub struct ProjectSyncRequest {
    pub frozen: bool,
    pub dry_run: bool,
}

/// Reconciles the px environment with the lockfile.
///
/// # Errors
/// Returns an error if dependency installation fails.
pub fn project_sync(
    ctx: &CommandContext,
    request: &ProjectSyncRequest,
) -> Result<ExecutionOutcome> {
    if request.dry_run {
        return project_sync_dry_run(ctx, request.frozen);
    }
    project_sync_outcome(ctx, request.frozen)
}

fn project_sync_dry_run(ctx: &CommandContext, frozen: bool) -> Result<ExecutionOutcome> {
    let snapshot = manifest_snapshot()?;
    let state = evaluate_project_state(ctx, &snapshot)?;

    if frozen {
        if !state.lock_exists || !state.manifest_clean {
            if !state.lock_exists {
                return Ok(StateViolation::MissingLock.into_outcome(&snapshot, "sync", &state));
            }
            return Ok(StateViolation::ManifestDrift.into_outcome(&snapshot, "sync", &state));
        }
        let message = if state.env_clean {
            "environment already in sync (dry-run)"
        } else {
            "would refresh environment from px.lock (dry-run)"
        };
        return Ok(ExecutionOutcome::success(
            message.to_string(),
            json!({
                "project": snapshot.name,
                "lockfile": snapshot.lock_path.display().to_string(),
                "python": snapshot.python_requirement,
                "mode": "frozen",
                "state": state.canonical.as_str(),
                "dry_run": true,
            }),
        ));
    }

    let action = if !state.lock_exists || !state.manifest_clean {
        "resolve_lock"
    } else if !state.env_clean {
        "sync_env"
    } else {
        "noop"
    };
    let message = match action {
        "resolve_lock" => "would resolve dependencies and write px.lock (dry-run)",
        "sync_env" => "would rebuild environment from px.lock (dry-run)",
        _ => "environment already in sync (dry-run)",
    };
    if action == "resolve_lock" {
        if let Err(err) = resolve_dependencies_with_effects(ctx.effects(), &snapshot, true) {
            match err.downcast::<InstallUserError>() {
                Ok(user) => {
                    return Ok(ExecutionOutcome::user_error(
                        "px sync: dependency resolution failed (dry-run)",
                        user.details,
                    ))
                }
                Err(other) => return Err(other),
            }
        }
    }

    Ok(ExecutionOutcome::success(
        message.to_string(),
        json!({
            "project": snapshot.name,
            "lockfile": snapshot.lock_path.display().to_string(),
            "python": snapshot.python_requirement,
            "action": action,
            "state": state.canonical.as_str(),
            "dry_run": true,
        }),
    ))
}

fn project_sync_outcome(ctx: &CommandContext, frozen: bool) -> Result<ExecutionOutcome> {
    let snapshot = manifest_snapshot()?;
    if frozen {
        let state = evaluate_project_state(ctx, &snapshot)?;
        if !state.lock_exists || !state.manifest_clean {
            if !state.lock_exists {
                return Ok(StateViolation::MissingLock.into_outcome(&snapshot, "sync", &state));
            }
            return Ok(StateViolation::ManifestDrift.into_outcome(&snapshot, "sync", &state));
        }
        refresh_project_site(&snapshot, ctx)?;
        return Ok(ExecutionOutcome::success(
            "environment synced from existing px.lock",
            json!({
                "project": snapshot.name,
                "lockfile": snapshot.lock_path.display().to_string(),
                "python": snapshot.python_requirement,
                "mode": "frozen",
                "state": "Consistent",
            }),
        ));
    }

    let outcome = match install_snapshot(ctx, &snapshot, false, None) {
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
    // Sync is required to end in Consistent; reflect state in outcome.
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
            Ok(ExecutionOutcome::success(
                "px.lock already up to date".to_string(),
                details,
            ))
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
