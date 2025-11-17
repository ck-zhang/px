use serde::Deserialize;
use serde_json::Value;

use crate::style::Style;

const DEFAULT_HEADER: &str = "Traceback (most recent call last):";

#[derive(Debug, Deserialize)]
pub struct TracebackDisplay {
    pub body: String,
    pub hint_line: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TracebackPayload {
    #[serde(default = "default_header")]
    header: String,
    frames: Vec<TracebackFrame>,
    #[serde(rename = "error_type")]
    error_type: String,
    #[serde(rename = "error_message", default)]
    error_message: String,
    #[serde(default)]
    recommendation: Option<TracebackRecommendation>,
}

#[derive(Debug, Deserialize)]
struct TracebackFrame {
    file: String,
    line: u32,
    function: String,
    #[serde(default)]
    code: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TracebackRecommendation {
    hint: String,
}

fn default_header() -> String {
    DEFAULT_HEADER.to_string()
}

pub fn format_traceback(style: &Style, value: &Value) -> Option<TracebackDisplay> {
    let payload: TracebackPayload = serde_json::from_value(value.clone()).ok()?;
    let mut lines = Vec::new();
    lines.push(style.traceback_header(payload.header.trim()));
    for frame in payload.frames {
        lines.push(style.traceback_location(&frame.file, frame.line, &frame.function));
        if let Some(code) = frame.code.as_deref() {
            lines.push(style.traceback_code(code));
        }
    }
    lines.push(style.traceback_error(&payload.error_type, payload.error_message.trim()));
    let hint_line = payload.recommendation.and_then(|rec| {
        if rec.hint.trim().is_empty() {
            None
        } else {
            Some(style.traceback_hint(rec.hint.trim()))
        }
    });
    Some(TracebackDisplay {
        body: lines.join("\n"),
        hint_line,
    })
}
