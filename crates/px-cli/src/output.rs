use atty::Stream;
use color_eyre::Result;
use px_core::{diag_commands, CommandGroup, CommandInfo, CommandStatus, ExecutionOutcome};
use serde_json::Value;

use crate::style::Style;
use crate::traceback;

#[derive(Clone, Copy, Debug)]
pub struct OutputOptions {
    pub quiet: bool,
    pub json: bool,
    pub no_color: bool,
}

pub fn emit_output(
    opts: &OutputOptions,
    subcommand_json: bool,
    info: CommandInfo,
    outcome: &ExecutionOutcome,
) -> Result<i32> {
    let code = match outcome.status {
        CommandStatus::Ok => 0,
        CommandStatus::UserError => 1,
        CommandStatus::Failure => 2,
    };

    let style = Style::new(opts.no_color, atty::is(Stream::Stdout));

    if opts.json || subcommand_json {
        let payload = px_core::to_json_response(info, outcome, code);
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else if !opts.quiet {
        if let Some(note) = autosync_note_from_details(&outcome.details) {
            let line = format!("px {}: {}", info.name, note);
            println!("{}", style.info(&line));
        }
        let migrate_table = render_migrate_table(&style, info, &outcome.details);
        if let CommandStatus::Ok = outcome.status {
            if is_passthrough(&outcome.details) {
                println!("{}", outcome.message);
            } else {
                let message = px_core::format_status_message(info, &outcome.message);
                println!("{}", style.status(&outcome.status, &message));
                let mut hint_emitted = false;
                if let Some(trace) = traceback_from_details(&style, &outcome.details) {
                    println!("{}", trace.body);
                    if let Some(line) = trace.hint_line {
                        println!("{line}");
                        hint_emitted = true;
                    }
                }
                if !hint_emitted {
                    if let Some(hint) = hint_from_details(&outcome.details) {
                        let hint_line = format!("Tip: {hint}");
                        println!("{}", style.info(&hint_line));
                    }
                }
            }
        } else {
            let header = format!("{}  {}", error_code(info), outcome.message);
            println!("{}", style.error_header(&header));
            println!();
            println!("Why:");
            for reason in collect_why_bullets(&outcome.details, &outcome.message) {
                println!("  • {reason}");
            }
            let fixes = collect_fix_bullets(&outcome.details);
            if !fixes.is_empty() {
                println!();
                println!("Fix:");
                for fix in fixes {
                    println!("{}", style.fix_bullet(&format!("  • {fix}")));
                }
            }
            if let Some(trace) = traceback_from_details(&style, &outcome.details) {
                println!();
                println!("{}", trace.body);
                if let Some(line) = trace.hint_line {
                    println!("{line}");
                }
            }
        }
        if let Some(table) = migrate_table {
            println!("{table}");
        }
    }

    Ok(code)
}

fn hint_from_details(details: &Value) -> Option<&str> {
    details
        .as_object()
        .and_then(|map| map.get("hint"))
        .and_then(Value::as_str)
}

fn traceback_from_details(style: &Style, details: &Value) -> Option<traceback::TracebackDisplay> {
    let map = details.as_object()?;
    let traceback_value = map.get("traceback")?;
    traceback::format_traceback(style, traceback_value)
}

fn autosync_note_from_details(details: &Value) -> Option<&str> {
    details
        .as_object()
        .and_then(|map| map.get("autosync"))
        .and_then(Value::as_object)
        .and_then(|map| map.get("note"))
        .and_then(Value::as_str)
}

fn is_passthrough(details: &Value) -> bool {
    details
        .as_object()
        .and_then(|map| map.get("passthrough"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn render_migrate_table(style: &Style, info: CommandInfo, details: &Value) -> Option<String> {
    if info.group != CommandGroup::Migrate {
        return None;
    }
    let packages = details.get("packages")?.as_array()?;
    if packages.is_empty() {
        return None;
    }

    let mut rows = Vec::new();
    for pkg in packages {
        let obj = pkg.as_object()?;
        rows.push(PackageRow {
            name: obj.get("name")?.as_str()?.to_string(),
            source: obj.get("source")?.as_str()?.to_string(),
            requested: obj.get("requested")?.as_str()?.to_string(),
            scope: obj.get("scope")?.as_str()?.to_string(),
        });
    }

    Some(format_package_table(style, &rows))
}

struct PackageRow {
    name: String,
    source: String,
    requested: String,
    scope: String,
}

fn format_package_table(style: &Style, rows: &[PackageRow]) -> String {
    let headers = ["Package", "Source", "Requested", "Scope"];
    let mut widths = [
        headers[0].len(),
        headers[1].len(),
        headers[2].len(),
        headers[3].len(),
    ];

    for row in rows {
        widths[0] = widths[0].max(row.name.len());
        widths[1] = widths[1].max(row.source.len());
        widths[2] = widths[2].max(row.requested.len());
        widths[3] = widths[3].max(row.scope.len());
    }

    let header_line = format!(
        "{:<width0$}  {:<width1$}  {:<width2$}  {:<width3$}",
        headers[0],
        headers[1],
        headers[2],
        headers[3],
        width0 = widths[0],
        width1 = widths[1],
        width2 = widths[2],
        width3 = widths[3],
    );

    let mut lines = Vec::new();
    lines.push(style.table_header(&header_line));
    lines.push(format!(
        "{:-<width0$}  {:-<width1$}  {:-<width2$}  {:-<width3$}",
        "",
        "",
        "",
        "",
        width0 = widths[0],
        width1 = widths[1],
        width2 = widths[2],
        width3 = widths[3],
    ));

    for row in rows {
        lines.push(format!(
            "{:<width0$}  {:<width1$}  {:<width2$}  {:<width3$}",
            row.name,
            row.source,
            row.requested,
            row.scope,
            width0 = widths[0],
            width1 = widths[1],
            width2 = widths[2],
            width3 = widths[3],
        ));
    }

    lines.join("\n")
}

fn error_code(info: CommandInfo) -> &'static str {
    match info.group {
        CommandGroup::Init => diag_commands::INIT,
        CommandGroup::Add => diag_commands::ADD,
        CommandGroup::Remove => diag_commands::REMOVE,
        CommandGroup::Sync => diag_commands::SYNC,
        CommandGroup::Update => diag_commands::UPDATE,
        CommandGroup::Status => diag_commands::STATUS,
        CommandGroup::Run => diag_commands::RUN,
        CommandGroup::Test => diag_commands::TEST,
        CommandGroup::Fmt => diag_commands::FMT,
        CommandGroup::Build => diag_commands::BUILD,
        CommandGroup::Publish => diag_commands::PUBLISH,
        CommandGroup::Migrate => diag_commands::MIGRATE,
        CommandGroup::Why => diag_commands::WHY,
        CommandGroup::Tool => diag_commands::TOOL,
        CommandGroup::Python => diag_commands::PYTHON,
    }
}

fn collect_why_bullets(details: &Value, fallback: &str) -> Vec<String> {
    let mut bullets = Vec::new();
    if let Some(reason) = details.get("reason").and_then(Value::as_str) {
        push_unique(
            &mut bullets,
            reason_display(reason).unwrap_or(reason).to_string(),
        );
    }
    if let Some(status) = details.get("status").and_then(Value::as_str) {
        push_unique(&mut bullets, format!("Status: {status}"));
    }
    if let Some(issues) = details.get("issues").and_then(Value::as_array) {
        for entry in issues {
            match entry {
                Value::String(message) => push_unique(&mut bullets, message.to_string()),
                Value::Object(map) => {
                    let message = map
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if message.is_empty() {
                        continue;
                    }
                    if let Some(id) = map.get("id").and_then(Value::as_str) {
                        push_unique(&mut bullets, format!("{id}: {message}"));
                    } else {
                        push_unique(&mut bullets, message.to_string());
                    }
                }
                _ => {}
            }
        }
    }
    if let Some(drift) = details.get("drift").and_then(Value::as_array) {
        if !drift.is_empty() {
            push_unique(
                &mut bullets,
                format!("Manifest drift detected ({} entries)", drift.len()),
            );
        }
    }
    if bullets.is_empty() {
        bullets.push(fallback.to_string());
    }
    bullets
}

fn collect_fix_bullets(details: &Value) -> Vec<String> {
    let mut fixes = Vec::new();
    if let Some(hint) = hint_from_details(details) {
        push_unique(&mut fixes, hint.to_string());
    }
    if let Some(rec) = details
        .as_object()
        .and_then(|map| map.get("recommendation"))
        .and_then(Value::as_object)
    {
        if let Some(command) = rec.get("command").and_then(Value::as_str) {
            push_unique(&mut fixes, format!("Run `{command}`"));
        }
        if let Some(hint) = rec.get("hint").and_then(Value::as_str) {
            push_unique(&mut fixes, hint.to_string());
        }
    }
    if fixes.is_empty() {
        fixes.push("Re-run with --help for usage or inspect the output above.".to_string());
    }
    fixes
}

fn push_unique(vec: &mut Vec<String>, text: impl Into<String>) {
    let entry = text.into();
    if entry.trim().is_empty() {
        return;
    }
    if !vec.iter().any(|existing| existing == &entry) {
        vec.push(entry);
    }
}

fn reason_display(code: &str) -> Option<&'static str> {
    match code {
        "resolve_no_match" => Some("No compatible release satisfied the requested constraint."),
        "invalid_requirement" => Some("One of the requirements is invalid (PEP 508 parse failed)."),
        "pypi_unreachable" => Some("Unable to reach PyPI while resolving dependencies."),
        "resolve_failed" => Some("Dependency resolver failed."),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn collect_fix_bullets_orders_hint_and_recommendation() {
        let details = json!({
            "hint": "first-hint",
            "recommendation": {
                "command": "px do-thing",
                "hint": "second-hint"
            }
        });
        let fixes = collect_fix_bullets(&details);
        assert!(
            fixes.iter().any(|f| f == "first-hint"),
            "expected primary hint to be present"
        );
        assert!(
            fixes.iter().any(|f| f == "Run `px do-thing`"),
            "expected recommended command"
        );
        assert!(
            fixes.iter().any(|f| f == "second-hint"),
            "expected secondary hint"
        );
    }

    #[test]
    fn collect_why_bullets_dedupes_and_uses_reason_display() {
        let details = json!({
            "reason": "resolve_no_match",
            "issues": [
                { "id": "E1", "message": "inconsistent spec" },
                { "message": "inconsistent spec" }
            ],
            "status": "pending"
        });
        let bullets = collect_why_bullets(&details, "fallback");
        assert!(
            bullets.iter().any(|b| b.contains("No compatible release")),
            "expected reason to be mapped"
        );
        assert!(
            bullets.iter().any(|b| b.contains("Status: pending")),
            "expected status bullet"
        );
        assert_eq!(
            bullets
                .iter()
                .filter(|b| b.contains("inconsistent spec"))
                .count(),
            1,
            "duplicate issues should be collapsed"
        );
    }
}
