use serde_json::{json, Value};

use crate::ExecutionOutcome;

pub(super) fn mark_reporter_rendered(outcome: &mut ExecutionOutcome) {
    match &mut outcome.details {
        Value::Object(map) => {
            map.insert("reporter_rendered".into(), Value::Bool(true));
        }
        Value::Null => {
            outcome.details = json!({ "reporter_rendered": true });
        }
        other => {
            let prev = other.take();
            outcome.details = json!({ "value": prev, "reporter_rendered": true });
        }
    }
}

pub(super) fn test_success(
    runner: &str,
    output: crate::RunOutput,
    stream_runner: bool,
    args: &[String],
) -> ExecutionOutcome {
    ExecutionOutcome::success(
        format!("{runner} ok"),
        test_details(runner, output, stream_runner, args, None),
    )
}

pub(super) fn test_failure(
    runner: &str,
    output: crate::RunOutput,
    stream_runner: bool,
    args: &[String],
) -> ExecutionOutcome {
    let code = output.code;
    let mut details = test_details(runner, output, stream_runner, args, Some("tests_failed"));
    if let Value::Object(map) = &mut details {
        map.insert("suppress_cli_frame".into(), Value::Bool(true));
    }
    ExecutionOutcome::failure(format!("{runner} failed (exit {code})"), details)
}

pub(super) fn missing_pytest_outcome(
    output: crate::RunOutput,
    args: &[String],
) -> ExecutionOutcome {
    ExecutionOutcome::user_error(
        "pytest is not available in the project environment",
        json!({
            "stdout": output.stdout,
            "stderr": output.stderr,
            "hint": "Install pytest with `px tool install pytest`, then rerun `px test`.",
            "reason": "missing_pytest",
            "code": crate::diag_commands::TEST,
            "runner": "pytest",
            "args": args,
        }),
    )
}

fn test_details(
    runner: &str,
    output: crate::RunOutput,
    stream_runner: bool,
    args: &[String],
    reason: Option<&str>,
) -> serde_json::Value {
    let mut details = json!({
        "runner": runner,
        "stdout": output.stdout,
        "stderr": output.stderr,
        "code": output.code,
        "args": args,
        "streamed": stream_runner,
    });
    if let Some(reason) = reason {
        if let Some(map) = details.as_object_mut() {
            map.insert("reason".to_string(), json!(reason));
        }
    }
    details
}
