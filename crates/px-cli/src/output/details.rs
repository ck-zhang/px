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

pub(super) fn is_passthrough(details: &Value) -> bool {
    details
        .as_object()
        .and_then(|map| map.get("passthrough"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}
