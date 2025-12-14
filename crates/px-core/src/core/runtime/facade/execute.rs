use crate::outcome::ExecutionOutcome;
use crate::process::RunOutput;
use crate::traceback::{analyze_python_traceback, TracebackContext};
use serde_json::{json, Value};

pub(crate) fn outcome_from_output(
    command_name: &str,
    target: &str,
    output: &RunOutput,
    prefix: &str,
    extra: Option<Value>,
) -> ExecutionOutcome {
    let mut extra_details = extra;
    let context = TracebackContext::new(command_name, target, extra_details.as_ref());
    let mut details = json!({
        "stdout": output.stdout.clone(),
        "stderr": output.stderr.clone(),
        "code": output.code,
        "target": target,
    });

    if let Some(extra_value) = extra_details.take() {
        if let Value::Object(map) = extra_value {
            if let Some(details_map) = details.as_object_mut() {
                for (key, value) in map {
                    details_map.insert(key, value);
                }
            }
        } else {
            details["extra"] = extra_value;
        }
    }

    let mut has_traceback = false;
    if output.code != 0 {
        if let Some(report) = analyze_python_traceback(&output.stderr, &context) {
            has_traceback = true;
            let recommendation = report.recommendation.clone();
            let trace_value = serde_json::to_value(&report).expect("traceback serialization");
            if let Some(map) = details.as_object_mut() {
                map.insert("traceback".to_string(), trace_value);
            }
            if let Some(rec) = recommendation {
                let hint_text = rec.hint.clone();
                let rec_value = serde_json::to_value(&rec).expect("traceback recommendation");
                if let Some(map) = details.as_object_mut() {
                    map.insert("recommendation".to_string(), rec_value);
                    if !map.contains_key("hint") {
                        map.insert("hint".to_string(), Value::String(hint_text));
                    }
                }
            }
        }
    }

    if output.code == 0 {
        let stdout = output.stdout.trim_end();
        if !stdout.is_empty() {
            details["passthrough"] = Value::Bool(true);
            return ExecutionOutcome::success(stdout.to_string(), details);
        }
        let stderr = output.stderr.trim_end();
        if !stderr.is_empty() {
            details["passthrough"] = Value::Bool(true);
            return ExecutionOutcome::success(stderr.to_string(), details);
        }
        let message = format!("{prefix} {command_name}({target}) succeeded");
        ExecutionOutcome::success(message, details)
    } else {
        let trimmed_stderr = output.stderr.trim();
        let message = if trimmed_stderr.is_empty() || has_traceback {
            format!(
                "{prefix} {command_name}({target}) exited with {}",
                output.code
            )
        } else {
            details["passthrough"] = Value::Bool(true);
            output.stderr.trim_end().to_string()
        };
        ExecutionOutcome::failure(message, details)
    }
}
