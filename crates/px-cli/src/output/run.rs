use px_core::api::{
    format_status_message, CommandGroup, CommandInfo, CommandStatus, ExecutionOutcome,
};
use serde_json::Value;

use crate::style::Style;

use super::details::{is_passthrough, output_from_details, traceback_from_details};
use super::OutputOptions;

pub(super) fn handle_run_output(
    opts: &OutputOptions,
    style: &Style,
    info: CommandInfo,
    outcome: &ExecutionOutcome,
) -> bool {
    let trace = traceback_from_details(style, &outcome.details);
    let stdout = output_from_details(&outcome.details, "stdout");
    let stderr = output_from_details(&outcome.details, "stderr");
    if info.group != CommandGroup::Run {
        return false;
    }
    match outcome.status {
        CommandStatus::Ok => {
            if is_passthrough(&outcome.details) {
                if !outcome.message.is_empty() {
                    println!("{}", outcome.message);
                }
            } else if opts.verbose > 0 || opts.debug || opts.trace {
                let message = format_status_message(info, &outcome.message);
                println!("{}", style.status(&outcome.status, &message));
            }
            true
        }
        CommandStatus::Failure => {
            let reason = outcome
                .details
                .as_object()
                .and_then(|map| map.get("reason"))
                .and_then(Value::as_str);
            if reason == Some("internal_error") {
                return false;
            }
            if let Some(stdout) = stdout {
                if !stdout.trim().is_empty() {
                    println!("{stdout}");
                }
            }
            let mut printed_trace = false;
            if let Some(trace) = trace.as_ref() {
                println!("{}", trace.body);
                if let Some(line) = trace.hint_line.as_ref() {
                    println!("{line}");
                }
                printed_trace = true;
            }
            if let Some(stderr) = stderr {
                if !(stderr.trim().is_empty()
                    || (printed_trace && stderr.contains("Traceback (most recent call last):")))
                {
                    println!("{stderr}");
                }
            }
            let summary = format_run_failure_summary(info, outcome);
            println!("{}", style.status(&CommandStatus::Failure, &summary));
            true
        }
        CommandStatus::UserError => false,
    }
}

fn format_run_failure_summary(info: CommandInfo, outcome: &ExecutionOutcome) -> String {
    let target = outcome
        .details
        .as_object()
        .and_then(|map| map.get("target"))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("target");
    let code = outcome
        .details
        .as_object()
        .and_then(|map| map.get("code"))
        .and_then(Value::as_i64);
    let mut summary = format!("px {} {target} failed", info.name);
    if let Some(code) = code {
        summary.push_str(&format!(" (exit code {code})"));
    }
    summary
}
