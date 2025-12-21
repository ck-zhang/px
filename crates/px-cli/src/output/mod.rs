mod details;
mod failure;
mod migrate;
mod run;
mod status;

use atty::Stream;
use color_eyre::Result;
use px_core::api as px_core;
use px_core::{CommandGroup, CommandInfo, CommandStatus, ExecutionOutcome, StatusPayload};
use serde_json::Value;

use crate::style::Style;

#[derive(Clone, Copy, Debug)]
pub struct OutputOptions {
    pub quiet: bool,
    pub json: bool,
    pub no_color: bool,
    pub verbose: u8,
    pub debug: bool,
    pub trace: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct StatusRenderOptions {
    pub brief: bool,
}

pub fn emit_output(
    opts: &OutputOptions,
    subcommand_json: bool,
    status_opts: Option<StatusRenderOptions>,
    info: CommandInfo,
    outcome: &ExecutionOutcome,
) -> Result<i32> {
    let mut code = match outcome.status {
        CommandStatus::Ok => 0,
        CommandStatus::UserError => 1,
        CommandStatus::Failure => 2,
    };
    if matches!(info.group, CommandGroup::Run | CommandGroup::Test) {
        if let Some(exit_code) = outcome
            .details
            .as_object()
            .and_then(|map| map.get("code"))
            .and_then(Value::as_i64)
        {
            code = i32::try_from(exit_code).unwrap_or(code);
        }
    }

    let style = Style::new(opts.no_color, atty::is(Stream::Stdout));

    if info.group == CommandGroup::Status {
        if let Ok(payload) = serde_json::from_value::<StatusPayload>(outcome.details.clone()) {
            let render = status_opts.unwrap_or_default();
            return status::emit_status_output(opts, &style, render, &payload, code);
        }
    }

    if opts.json || subcommand_json {
        let payload = px_core::to_json_response(info, outcome, code);
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else if !opts.quiet {
        if info.group == CommandGroup::Explain && matches!(outcome.status, CommandStatus::Ok) {
            println!("{}", outcome.message);
            return Ok(code);
        }
        if run::handle_run_output(opts, &style, info, outcome) {
            return Ok(code);
        }
        if let Some(note) = details::autosync_note_from_details(&outcome.details) {
            let line = format!("px {}: {}", info.name, note);
            println!("{}", style.info(&line));
        }
        let migrate_table = migrate::render_migrate_table(&style, info, &outcome.details);
        let reporter_rendered = outcome
            .details
            .as_object()
            .and_then(|map| map.get("reporter_rendered"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if info.group == CommandGroup::Test && reporter_rendered {
            return Ok(code);
        }
        if info.group == CommandGroup::Test
            && outcome.status == CommandStatus::Failure
            && failure::render_test_failure(&style, info, outcome)
        {
            if let Some(table) = migrate_table {
                println!("{table}");
            }
            return Ok(code);
        }
        if let CommandStatus::Ok = outcome.status {
            if details::is_passthrough(&outcome.details) {
                println!("{}", outcome.message);
            } else {
                let message = px_core::format_status_message(info, &outcome.message);
                println!("{}", style.status(&outcome.status, &message));
                let mut hint_emitted = false;
                if let Some(trace) = details::traceback_from_details(&style, &outcome.details) {
                    println!("{}", trace.body);
                    if let Some(line) = trace.hint_line {
                        println!("{line}");
                        hint_emitted = true;
                    }
                }
                if !hint_emitted {
                    if let Some(hint) = details::hint_from_details(&outcome.details) {
                        let hint_line = format!("Tip: {hint}");
                        println!("{}", style.info(&hint_line));
                    }
                }
            }
        } else {
            let trace = details::traceback_from_details(&style, &outcome.details);
            if info.group == CommandGroup::Test
                && outcome
                    .details
                    .as_object()
                    .and_then(|map| map.get("suppress_cli_frame"))
                    .and_then(Value::as_bool)
                    == Some(true)
            {
                return Ok(code);
            }
            let header = format!("{}  {}", failure::error_code(info), outcome.message);
            println!("{}", style.error_header(&header));
            println!();
            println!("Why:");
            for reason in failure::collect_why_bullets(&outcome.details, &outcome.message) {
                println!("  • {reason}");
            }
            let fixes = failure::collect_fix_bullets(&outcome.details);
            if !fixes.is_empty() {
                println!();
                println!("Fix:");
                for fix in fixes {
                    println!("{}", style.fix_bullet(&format!("  • {fix}")));
                }
            }
            let stdout = details::output_from_details(&outcome.details, "stdout");
            let stderr = details::output_from_details(&outcome.details, "stderr");
            if let Some(stdout) = stdout {
                println!();
                println!("stdout:");
                println!("{stdout}");
            }
            if let Some(trace) = trace.as_ref() {
                println!();
                println!("{}", trace.body);
                if let Some(line) = trace.hint_line.as_ref() {
                    println!("{line}");
                }
            } else if let Some(stderr) = stderr {
                if !stderr.trim().is_empty() {
                    println!();
                    println!("stderr:");
                    println!("{stderr}");
                }
            }
        }
        if let Some(table) = migrate_table {
            println!("{table}");
        }
    }

    Ok(code)
}
