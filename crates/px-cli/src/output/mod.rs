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

    let style_out = Style::new(opts.no_color, atty::is(Stream::Stdout));
    let style_err = Style::new(opts.no_color, atty::is(Stream::Stderr));

    if info.group == CommandGroup::Status {
        if let Ok(payload) = serde_json::from_value::<StatusPayload>(outcome.details.clone()) {
            let render = status_opts.unwrap_or_default();
            return status::emit_status_output(opts, &style_out, render, &payload, code);
        }
    }

    if opts.json || subcommand_json {
        let payload = px_core::to_json_response(info, outcome, code);
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        if info.group == CommandGroup::Explain && matches!(outcome.status, CommandStatus::Ok) {
            if !opts.quiet {
                println!("{}", outcome.message);
            }
            return Ok(code);
        }

        if run::handle_run_output(opts, &style_out, info, outcome) {
            return Ok(code);
        }

        if !opts.quiet {
            if info.group == CommandGroup::Init
                && matches!(outcome.status, CommandStatus::Ok)
                && !details::is_passthrough(&outcome.details)
            {
                if let Some(note) = details::gitignore_note_from_details(&outcome.details) {
                    let line = format!("px {}: {}", info.name, note);
                    println!("{}", style_out.info(&line));
                }
            }
            if info.group == CommandGroup::Add && matches!(outcome.status, CommandStatus::Ok) {
                for change in details::manifest_add_lines_from_details(&outcome.details) {
                    let line = format!("px {}: {}", info.name, change);
                    println!("{}", style_out.info(&line));
                }
                for change in details::manifest_change_lines_from_details(&outcome.details) {
                    let line = format!("px {}: {}", info.name, change);
                    println!("{}", style_out.info(&line));
                }
                if let Some(lock) = details::lock_change_summary_line_from_details(&outcome.details) {
                    let line = format!("px {}: {}", info.name, lock);
                    println!("{}", style_out.info(&line));
                }
                for change in details::lock_direct_change_lines_from_details(&outcome.details, opts.verbose) {
                    let line = format!("px {}: {}", info.name, change);
                    println!("{}", style_out.info(&line));
                }
            }
            if info.group == CommandGroup::Update && matches!(outcome.status, CommandStatus::Ok) {
                if let Some(lock) = details::lock_change_summary_line_from_details(&outcome.details) {
                    let line = format!("px {}: {}", info.name, lock);
                    println!("{}", style_out.info(&line));
                }
                for change in details::lock_updated_version_lines_from_details(&outcome.details, opts.verbose) {
                    let line = format!("px {}: {}", info.name, change);
                    println!("{}", style_out.info(&line));
                }
            }
            if matches!(outcome.status, CommandStatus::Ok) {
                for preview in
                    details::dry_run_preview_lines_from_details(&outcome.details, opts.verbose)
                {
                    let line = format!("px {}: {}", info.name, preview);
                    println!("{}", style_out.info(&line));
                }
            }
            if let Some(note) = details::autosync_note_from_details(&outcome.details) {
                let line = format!("px {}: {}", info.name, note);
                println!("{}", style_out.info(&line));
            }
        }

        let migrate_table = if opts.quiet {
            None
        } else {
            migrate::render_migrate_table(&style_out, info, &outcome.details)
        };

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
            && failure::render_test_failure(&style_out, info, outcome)
        {
            if let Some(table) = migrate_table {
                println!("{table}");
            }
            return Ok(code);
        }

        match outcome.status {
            CommandStatus::Ok => {
                if opts.quiet {
                    return Ok(code);
                }
                if details::is_passthrough(&outcome.details) {
                    println!("{}", outcome.message);
                } else {
                    let message = px_core::format_status_message(info, &outcome.message);
                    println!("{}", style_out.status(&outcome.status, &message));
                    let mut hint_emitted = false;
                    if let Some(trace) =
                        details::traceback_from_details(&style_out, &outcome.details)
                    {
                        println!("{}", trace.body);
                        if let Some(line) = trace.hint_line {
                            println!("{line}");
                            hint_emitted = true;
                        }
                    }
                    if !hint_emitted {
                        if let Some(hint) = details::hint_from_details(&outcome.details) {
                            let hint_line = format!("Tip: {hint}");
                            println!("{}", style_out.info(&hint_line));
                        }
                    }
                }
                if let Some(table) = migrate_table {
                    println!("{table}");
                }
            }
            CommandStatus::UserError | CommandStatus::Failure => {
                let trace = details::traceback_from_details(&style_err, &outcome.details);
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
                let header = format!(
                    "{}  {}",
                    failure::error_code(info, &outcome.details),
                    outcome.message
                );
                eprintln!("{}", style_err.error_header(&header));
                eprintln!();
                eprintln!("Why:");
                for reason in failure::collect_why_bullets(&outcome.details, &outcome.message) {
                    eprintln!("  • {reason}");
                }
                let fixes = failure::collect_fix_bullets(&outcome.details);
                if !fixes.is_empty() {
                    eprintln!();
                    eprintln!("Fix:");
                    for fix in fixes {
                        eprintln!("{}", style_err.fix_bullet(&format!("  • {fix}")));
                    }
                }
                let stdout = details::output_from_details(&outcome.details, "stdout");
                let stderr = details::output_from_details(&outcome.details, "stderr");
                if let Some(stdout) = stdout {
                    eprintln!();
                    eprintln!("stdout:");
                    eprintln!("{stdout}");
                }
                if let Some(trace) = trace.as_ref() {
                    eprintln!();
                    eprintln!("{}", trace.body);
                    if let Some(line) = trace.hint_line.as_ref() {
                        eprintln!("{line}");
                    }
                } else if let Some(stderr) = stderr {
                    if !stderr.trim().is_empty() {
                        eprintln!();
                        eprintln!("stderr:");
                        eprintln!("{stderr}");
                    }
                }
                if let Some(table) = migrate_table {
                    eprintln!("{table}");
                }
            }
        }
    }

    Ok(code)
}
