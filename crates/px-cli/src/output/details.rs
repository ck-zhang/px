use serde_json::Value;

use crate::style::Style;
use crate::traceback;

pub(super) fn hint_from_details(details: &Value) -> Option<&str> {
    details
        .as_object()
        .and_then(|map| map.get("hint"))
        .and_then(Value::as_str)
}

pub(super) fn output_from_details<'a>(details: &'a Value, key: &str) -> Option<&'a str> {
    details
        .as_object()
        .and_then(|map| map.get(key))
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
}

pub(super) fn traceback_from_details(
    style: &Style,
    details: &Value,
) -> Option<traceback::TracebackDisplay> {
    let map = details.as_object()?;
    let traceback_value = map.get("traceback")?;
    traceback::format_traceback(style, traceback_value)
}

pub(super) fn autosync_note_from_details(details: &Value) -> Option<&str> {
    details
        .as_object()
        .and_then(|map| map.get("autosync"))
        .and_then(Value::as_object)
        .and_then(|map| map.get("note"))
        .and_then(Value::as_str)
}

pub(super) fn manifest_change_lines_from_details(details: &Value) -> Vec<String> {
    let Some(entries) = details
        .as_object()
        .and_then(|map| map.get("manifest_changes"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    let mut lines = Vec::new();
    for entry in entries {
        let Some(obj) = entry.as_object() else {
            continue;
        };
        let Some(before) = obj.get("before").and_then(Value::as_str) else {
            continue;
        };
        let Some(after) = obj.get("after").and_then(Value::as_str) else {
            continue;
        };
        if before.trim().is_empty() || after.trim().is_empty() {
            continue;
        }
        lines.push(format!("pyproject.toml: {before} -> {after}"));
    }
    lines
}

pub(super) fn is_passthrough(details: &Value) -> bool {
    details
        .as_object()
        .and_then(|map| map.get("passthrough"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}
