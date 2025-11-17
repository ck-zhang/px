use anyhow::Result;
use serde_json::json;

use crate::{
    detect_lock_drift, load_lockfile_optional, manifest_snapshot, CommandContext, ExecutionOutcome,
};

#[derive(Clone, Debug, Default)]
pub struct QualityTidyRequest;

#[derive(Clone, Debug)]
pub struct ToolCommandRequest {
    pub args: Vec<String>,
}

pub fn quality_tidy(
    _ctx: &CommandContext,
    _request: QualityTidyRequest,
) -> Result<ExecutionOutcome> {
    quality_tidy_outcome()
}

pub fn quality_fmt(_ctx: &CommandContext, request: ToolCommandRequest) -> Result<ExecutionOutcome> {
    Ok(ExecutionOutcome::success(
        "stubbed quality fmt",
        json!({ "args": request.args }),
    ))
}

pub fn quality_lint(
    _ctx: &CommandContext,
    request: ToolCommandRequest,
) -> Result<ExecutionOutcome> {
    Ok(ExecutionOutcome::success(
        "stubbed quality lint",
        json!({ "args": request.args }),
    ))
}

fn quality_tidy_outcome() -> Result<ExecutionOutcome> {
    let snapshot = manifest_snapshot()?;

    let lock = match load_lockfile_optional(&snapshot.lock_path)? {
        Some(lock) => lock,
        None => {
            return Ok(ExecutionOutcome::user_error(
                "px tidy: px.lock not found (run `px sync`)",
                json!({
                    "lockfile": snapshot.lock_path.display().to_string(),
                    "hint": "run `px sync` to generate px.lock before running tidy",
                }),
            ))
        }
    };

    let drift = detect_lock_drift(&snapshot, &lock, None);
    if drift.is_empty() {
        Ok(ExecutionOutcome::success(
            "px.lock matches pyproject",
            json!({
                "status": "clean",
                "lockfile": snapshot.lock_path.display().to_string(),
            }),
        ))
    } else {
        Ok(ExecutionOutcome::user_error(
            "px.lock is out of date",
            json!({
                "status": "drift",
                "lockfile": snapshot.lock_path.display().to_string(),
                "drift": drift,
                "hint": "rerun `px sync` to refresh the lockfile",
            }),
        ))
    }
}
