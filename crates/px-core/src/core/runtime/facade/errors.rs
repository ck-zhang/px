use crate::context::CommandInfo;
use crate::outcome::{CommandStatus, ExecutionOutcome};
use px_domain::api::{missing_project_guidance, MissingProjectGuidance};
use serde_json::{json, Value};
use toml_edit::TomlError;

use super::{MISSING_PROJECT_HINT, MISSING_PROJECT_MESSAGE};

pub fn missing_project_outcome() -> ExecutionOutcome {
    let guidance = missing_project_guidance().unwrap_or_else(|_| MissingProjectGuidance {
        message: MISSING_PROJECT_MESSAGE.to_string(),
        hint: MISSING_PROJECT_HINT.to_string(),
    });
    let searched = std::env::current_dir()
        .ok()
        .map(|dir| dir.display().to_string());
    let mut details = json!({
        "code": "PX001",
        "reason": "missing_project",
        "issues": ["No pyproject.toml with [tool.px] and no px.lock found in parent directories."],
        "hint": guidance.hint,
    });
    if let (Some(searched), Some(map)) = (searched, details.as_object_mut()) {
        map.insert("searched".to_string(), Value::String(searched));
    }
    ExecutionOutcome::user_error(guidance.message.clone(), details)
}

pub fn is_missing_project_error(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|cause| cause.to_string().contains("No px project found"))
}

pub fn manifest_error_outcome(err: &anyhow::Error) -> Option<ExecutionOutcome> {
    if err
        .chain()
        .any(|cause| cause.to_string().contains("pyproject.toml not found"))
    {
        return Some(ExecutionOutcome::user_error(
            "pyproject.toml not found",
            json!({
                "reason": "missing_manifest",
                "hint": "Run `px init` to create pyproject.toml, or restore it from version control.",
            }),
        ));
    }

    let parse_error = err
        .chain()
        .find_map(|cause| cause.downcast_ref::<TomlError>().map(ToString::to_string))?;

    let mut target = "pyproject.toml";
    for cause in err.chain() {
        let msg = cause.to_string();
        if msg.contains("px.lock") {
            target = "px.lock";
            break;
        }
        if msg.contains("pyproject.toml") {
            target = "pyproject.toml";
            break;
        }
    }

    let (reason, hint) = if target == "px.lock" {
        (
            "invalid_lock",
            "Delete or fix px.lock, then run `px sync` to regenerate it.",
        )
    } else {
        (
            "invalid_manifest",
            "Fix pyproject.toml syntax and rerun the command.",
        )
    };

    Some(ExecutionOutcome::user_error(
        format!("{target} is not valid TOML"),
        json!({
            "reason": reason,
            "target": target,
            "error": parse_error,
            "hint": hint,
        }),
    ))
}

#[must_use]
pub fn to_json_response(info: CommandInfo, outcome: &ExecutionOutcome, _code: i32) -> Value {
    let status = match outcome.status {
        CommandStatus::Ok => "ok",
        CommandStatus::UserError => "user-error",
        CommandStatus::Failure => "error",
    };
    let details = match &outcome.details {
        Value::Object(_) => outcome.details.clone(),
        Value::Null => json!({}),
        other => json!({ "value": other }),
    };
    json!({
        "status": status,
        "message": format_status_message(info, &outcome.message),
        "details": details,
    })
}

#[must_use]
pub fn format_status_message(info: CommandInfo, message: &str) -> String {
    let group_name = info.group.to_string();
    let prefix = if group_name == info.name {
        format!("px {}", info.name)
    } else {
        format!("px {} {}", group_name, info.name)
    };
    if message.is_empty() {
        prefix
    } else if message.starts_with(&prefix) {
        message.to_string()
    } else {
        format!("{prefix}: {message}")
    }
}
