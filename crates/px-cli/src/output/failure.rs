use px_core::api as px_core;
use px_core::{diag_commands, CommandGroup, CommandInfo, ExecutionOutcome};
use serde_json::Value;

use crate::style::Style;

use super::details::hint_from_details;

pub(super) fn render_test_failure(
    style: &Style,
    info: CommandInfo,
    outcome: &ExecutionOutcome,
) -> bool {
    let reason = outcome
        .details
        .as_object()
        .and_then(|map| map.get("reason"))
        .and_then(Value::as_str);
    if reason != Some("tests_failed") {
        return false;
    }
    if outcome
        .details
        .as_object()
        .and_then(|map| map.get("suppress_cli_frame"))
        .and_then(Value::as_bool)
        == Some(true)
    {
        return true;
    }
    let message = px_core::format_status_message(info, &outcome.message);
    println!("{}", style.status(&outcome.status, &message));
    if let Some(hint) = hint_from_details(&outcome.details) {
        println!("{}", style.info(&format!("Hint: {hint}")));
    }
    true
}

pub(super) fn error_code(info: CommandInfo, details: &Value) -> String {
    if let Some(code) = details
        .as_object()
        .and_then(|map| map.get("code"))
        .and_then(Value::as_str)
        .filter(|code| code.starts_with("PX"))
    {
        return code.to_string();
    }
    default_error_code(info).to_string()
}

fn default_error_code(info: CommandInfo) -> &'static str {
    match info.group {
        CommandGroup::Init => diag_commands::INIT,
        CommandGroup::Add => diag_commands::ADD,
        CommandGroup::Remove => diag_commands::REMOVE,
        CommandGroup::Sync => diag_commands::SYNC,
        CommandGroup::Update => diag_commands::UPDATE,
        CommandGroup::Status => diag_commands::STATUS,
        CommandGroup::Run => diag_commands::RUN,
        CommandGroup::Explain => diag_commands::RUN,
        CommandGroup::Test => diag_commands::TEST,
        CommandGroup::Fmt => diag_commands::FMT,
        CommandGroup::Build => diag_commands::BUILD,
        CommandGroup::Publish => diag_commands::PUBLISH,
        CommandGroup::Pack => diag_commands::PACK,
        CommandGroup::Migrate => diag_commands::MIGRATE,
        CommandGroup::Why => diag_commands::WHY,
        CommandGroup::Tool => diag_commands::TOOL,
        CommandGroup::Python => diag_commands::PYTHON,
        CommandGroup::Completions => diag_commands::GENERIC,
    }
}

pub(super) fn collect_why_bullets(details: &Value, fallback: &str) -> Vec<String> {
    use std::collections::HashSet;

    let mut bullets = Vec::new();
    let mut seen_messages = HashSet::new();
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
                Value::String(message) => {
                    if seen_messages.insert(message.clone()) {
                        push_unique(&mut bullets, message.to_string())
                    }
                }
                Value::Object(map) => {
                    let message = map
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if message.is_empty() {
                        continue;
                    }
                    if !seen_messages.insert(message.to_string()) {
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

pub(super) fn collect_fix_bullets(details: &Value) -> Vec<String> {
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
