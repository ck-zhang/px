use std::fmt::Write as _;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use anyhow::Result;

use crate::workspace::{discover_workspace_scope, workspace_status};
use crate::{
    compute_lock_hash, detect_runtime_metadata, ensure_env_matches_lock, install_snapshot,
    load_project_state, manifest_snapshot, CommandContext, ExecutionOutcome, InstallState,
    InstallUserError, ManifestSnapshot,
};
use px_domain::load_lockfile_optional;

use super::evaluate_project_state;

/// Reports whether the manifest, lockfile, and environment are consistent.
///
/// # Errors
/// Returns an error if project metadata cannot be read or dependency verification fails.
pub fn project_status(ctx: &CommandContext) -> Result<ExecutionOutcome> {
    if let Some(scope) = discover_workspace_scope()? {
        return workspace_status(ctx, scope);
    }

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
        "dependency_groups": {
            "active": snapshot.dependency_groups.clone(),
            "declared": snapshot.declared_dependency_groups.clone(),
            "source": snapshot.dependency_group_source.as_str(),
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
    if let Some(lock_issue) = state_report.lock_issue.clone() {
        details["lock_issue"] = json!(lock_issue);
    }
    if let Some(issue) = state_report.env_issue.clone() {
        details["environment_issue"] = issue;
    }
    details["runtime"] = detect_runtime_details(ctx, &snapshot);
    let env_details =
        collect_environment_status(ctx, &snapshot, outcome.state != InstallState::MissingLock)?;
    details["environment"] = env_details.clone();
    match outcome.state {
        InstallState::UpToDate => {
            let env_status = env_details
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            if !state_report.is_consistent() {
                let canonical = state_report.canonical.as_str().to_string();
                details["status"] = Value::String(canonical);
                if let Some(reason) = env_details.get("reason") {
                    details["reason"] = reason.clone();
                }
                if let Some(hint) = env_details.get("hint") {
                    details["hint"] = hint.clone();
                }
                let message = match env_status {
                    "missing" => "Project environment is missing",
                    "out-of-sync" => "Project environment is out of sync with px.lock",
                    "unknown" => "Project environment status is unknown",
                    _ => "Project environment is not ready",
                };
                return Ok(ExecutionOutcome::user_error(message, details));
            }

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
    let state = load_project_state(ctx.fs(), &snapshot.root)?;
    let Some(env) = state.current_env.clone() else {
        return Ok(json!({
            "status": "missing",
            "reason": "uninitialized",
            "hint": "Run `px sync` to build the px environment.",
        }));
    };
    let lock_id = match lock.lock_id.clone() {
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
    match ensure_env_matches_lock(ctx, snapshot, &lock_id) {
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

pub(crate) fn issue_values(messages: Vec<String>) -> Value {
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

pub(crate) fn issue_id_for(message: &str) -> String {
    let digest = Sha256::digest(message.as_bytes());
    let mut short = String::new();
    for byte in &digest[..6] {
        let _ = write!(&mut short, "{byte:02x}");
    }
    format!("ISS-{}", short.to_ascii_uppercase())
}
