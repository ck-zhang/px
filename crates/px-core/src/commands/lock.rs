use std::fs;

use anyhow::Result;
use serde_json::{json, Value};

use crate::{
    analyze_lock_diff, load_lockfile_optional, manifest_snapshot, render_lockfile_v2,
    CommandContext, ExecutionOutcome,
};

#[derive(Clone, Debug, Default)]
pub struct LockDiffRequest;

#[derive(Clone, Debug, Default)]
pub struct LockUpgradeRequest;

pub fn lock_diff(_ctx: &CommandContext, _request: LockDiffRequest) -> Result<ExecutionOutcome> {
    lock_diff_outcome()
}

pub fn lock_upgrade(
    ctx: &CommandContext,
    _request: LockUpgradeRequest,
) -> Result<ExecutionOutcome> {
    lock_upgrade_outcome(ctx)
}

fn lock_diff_outcome() -> Result<ExecutionOutcome> {
    let snapshot = manifest_snapshot()?;
    match load_lockfile_optional(&snapshot.lock_path)? {
        Some(lock) => {
            let report = analyze_lock_diff(&snapshot, &lock, None);
            let mut details = report.to_json(&snapshot);
            if report.is_clean() {
                Ok(ExecutionOutcome::success(report.summary(), details))
            } else {
                details["hint"] = Value::String(
                    "run `px sync` (or `px lock upgrade`) to regenerate the lock".to_string(),
                );
                Ok(ExecutionOutcome::user_error(report.summary(), details))
            }
        }
        None => {
            let details = json!({
                "status": "missing_lock",
                "pyproject": snapshot.manifest_path.display().to_string(),
                "lockfile": snapshot.lock_path.display().to_string(),
                "added": [],
                "removed": [],
                "changed": [],
                "version_mismatch": Value::Null,
                "python_mismatch": Value::Null,
                "mode_mismatch": Value::Null,
                "hint": "run `px sync` to generate px.lock before diffing",
            });
            Ok(ExecutionOutcome::user_error(
                format!(
                    "missing px.lock at {} (run `px sync` first)",
                    snapshot.lock_path.display()
                ),
                details,
            ))
        }
    }
}

fn lock_upgrade_outcome(_ctx: &CommandContext) -> Result<ExecutionOutcome> {
    let snapshot = manifest_snapshot()?;
    let lock_path = snapshot.lock_path.clone();
    let lock = match load_lockfile_optional(&lock_path)? {
        Some(lock) => lock,
        None => {
            return Ok(ExecutionOutcome::user_error(
                "missing px.lock (run `px sync` first)",
                json!({
                    "status": "missing_lock",
                    "lockfile": lock_path.display().to_string(),
                    "hint": "run `px sync` to create a lock before upgrading",
                }),
            ))
        }
    };

    if lock.version >= 2 {
        return Ok(ExecutionOutcome::success(
            "lock already at version 2",
            json!({
                "lockfile": lock_path.display().to_string(),
                "version": lock.version,
                "status": "unchanged",
            }),
        ));
    }

    let upgraded = render_lockfile_v2(&snapshot, &lock, crate::PX_VERSION)?;
    fs::write(&lock_path, upgraded)?;

    Ok(ExecutionOutcome::success(
        "upgraded lock to version 2",
        json!({
            "lockfile": lock_path.display().to_string(),
            "version": 2,
            "status": "upgraded",
        }),
    ))
}
