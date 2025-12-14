use anyhow::Error;
use serde_json::{json, Value};

use super::cas_native::{CasNativeFallback, CasNativeFallbackReason};
use crate::ExecutionOutcome;

pub(crate) fn install_error_outcome(err: Error, context: &str) -> ExecutionOutcome {
    match err.downcast::<crate::InstallUserError>() {
        Ok(user) => {
            ExecutionOutcome::user_error(user.message().to_string(), user.details().clone())
        }
        Err(other) => {
            let mut details = json!({ "error": other.to_string() });
            if let Some(code) = store_error_code(&other) {
                if let Value::Object(map) = &mut details {
                    map.insert("code".into(), json!(code));
                }
            }
            ExecutionOutcome::failure(context, details)
        }
    }
}

pub(super) fn error_details_with_code(err: &Error) -> Value {
    let mut details = json!({ "error": err.to_string() });
    if let Some(code) = store_error_code(err) {
        if let Value::Object(map) = &mut details {
            map.insert("code".into(), json!(code));
        }
    }
    details
}

fn store_error_code(err: &Error) -> Option<&'static str> {
    err.chain().find_map(|cause| {
        cause
            .downcast_ref::<crate::StoreError>()
            .map(crate::StoreError::code)
    })
}

pub(super) fn attach_cas_native_fallback(
    outcome: &mut ExecutionOutcome,
    fallback: &CasNativeFallback,
) {
    let payload = json!({
        "code": fallback.reason.as_str(),
        "error": fallback.summary,
    });
    match &mut outcome.details {
        Value::Object(map) => {
            map.insert("cas_native_fallback".into(), payload);
        }
        Value::Null => {
            outcome.details = json!({ "cas_native_fallback": payload });
        }
        other => {
            let prev = other.take();
            outcome.details = json!({ "value": prev, "cas_native_fallback": payload });
        }
    }
}

pub(super) fn cas_native_fallback_reason(
    outcome: &ExecutionOutcome,
) -> Option<CasNativeFallbackReason> {
    let reason = outcome.details.get("reason").and_then(Value::as_str)?;
    match reason {
        "ambiguous_console_script" => Some(CasNativeFallbackReason::AmbiguousConsoleScript),
        "cas_native_console_script_index_failed" => {
            Some(CasNativeFallbackReason::ConsoleScriptIndexFailed)
        }
        "missing_artifacts" => Some(CasNativeFallbackReason::MissingArtifacts),
        "cas_native_unresolved_console_script" => {
            Some(CasNativeFallbackReason::UnresolvedConsoleScript)
        }
        "cas_native_site_setup_failed" => Some(CasNativeFallbackReason::NativeSiteSetupFailed),
        _ => None,
    }
}

pub(super) fn cas_native_fallback_summary(outcome: &ExecutionOutcome) -> String {
    let mut summary = outcome.message.clone();
    if let Some(err) = outcome.details.get("error").and_then(Value::as_str) {
        let sanitized = err.replace('\n', " ");
        if !sanitized.trim().is_empty() {
            summary.push_str(": ");
            summary.push_str(sanitized.trim());
        }
    }
    summary
}

pub(super) fn is_integrity_failure(outcome: &ExecutionOutcome) -> bool {
    outcome
        .details
        .get("code")
        .and_then(Value::as_str)
        .is_some_and(cas_integrity_code)
}

fn cas_integrity_code(code: &str) -> bool {
    matches!(
        code,
        crate::diagnostics::cas::MISSING_OR_CORRUPT
            | crate::diagnostics::cas::STORE_WRITE_FAILURE
            | crate::diagnostics::cas::INDEX_CORRUPT
            | crate::diagnostics::cas::FORMAT_INCOMPATIBLE
    )
}
