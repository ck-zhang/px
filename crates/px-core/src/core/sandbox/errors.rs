use serde_json::{json, Value};

use crate::InstallUserError;

pub(crate) fn sandbox_error(code: &str, message: &str, details: Value) -> InstallUserError {
    let mut merged = details;
    match merged {
        Value::Object(ref mut map) => {
            map.insert("code".into(), Value::String(code.to_string()));
        }
        _ => {
            merged = json!({
                "code": code,
                "details": merged,
            });
        }
    }
    InstallUserError::new(message, merged)
}
