use assert_cmd::cargo::cargo_bin_cmd;
use serde_json::Value;
use std::collections::HashMap;

mod common;

use common::{parse_json, prepare_traceback_fixture};

fn fixture_cases() -> Vec<&'static str> {
    let mut seen = HashMap::new();
    fixtures()
        .into_iter()
        .filter(|name| seen.insert(*name, true).is_none())
        .collect()
}

fn fixtures() -> Vec<&'static str> {
    vec![
        "BaseException",
        "BaseExceptionGroup",
        "Exception",
        "GeneratorExit",
        "KeyboardInterrupt",
        "SystemExit",
        "ArithmeticError",
        "AssertionError",
        "AttributeError",
        "BufferError",
        "EOFError",
        "ImportError",
        "LookupError",
        "MemoryError",
        "NameError",
        "OSError",
        "ReferenceError",
        "RuntimeError",
        "StopAsyncIteration",
        "StopIteration",
        "SyntaxError",
        "SystemError",
        "TypeError",
        "ValueError",
        "Warning",
        "FloatingPointError",
        "OverflowError",
        "ZeroDivisionError",
        "BytesWarning",
        "DeprecationWarning",
        "EncodingWarning",
        "FutureWarning",
        "ImportWarning",
        "PendingDeprecationWarning",
        "ResourceWarning",
        "RuntimeWarning",
        "SyntaxWarning",
        "UnicodeWarning",
        "UserWarning",
        "BlockingIOError",
        "ChildProcessError",
        "ConnectionError",
        "FileExistsError",
        "FileNotFoundError",
        "InterruptedError",
        "IsADirectoryError",
        "NotADirectoryError",
        "PermissionError",
        "ProcessLookupError",
        "TimeoutError",
        "IndentationError",
        "_IncompleteInputError",
        "IndexError",
        "KeyError",
        "ModuleNotFoundError",
        "NotImplementedError",
        "PythonFinalizationError",
        "RecursionError",
        "UnboundLocalError",
        "UnicodeError",
        "BrokenPipeError",
        "ConnectionAbortedError",
        "ConnectionRefusedError",
        "ConnectionResetError",
        "TabError",
        "UnicodeDecodeError",
        "UnicodeEncodeError",
        "UnicodeTranslateError",
        "ExceptionGroup",
    ]
}

fn recommendation_expectations() -> HashMap<&'static str, &'static str> {
    HashMap::from([
        ("ModuleNotFoundError", "missing_import"),
        ("ImportError", "missing_import"),
    ])
}

#[test]
fn px_reports_recommendations_for_builtin_exceptions() {
    let (_tmp, project) = prepare_traceback_fixture("traceback-matrix");
    let Some(python) = find_python() else {
        eprintln!("skipping traceback matrix test (python binary not found)");
        return;
    };
    let expectations = recommendation_expectations();
    for name in fixture_cases() {
        let assert = cargo_bin_cmd!("px")
            .current_dir(&project)
            .env("PX_RUNTIME_PYTHON", &python)
            .args(["--json", "run", "python", "demo_tracebacks.py", name])
            .assert()
            .failure();
        let payload = parse_json(&assert);
        verify_traceback(&payload, name, expectations.get(name).copied());
    }
}

fn verify_traceback(payload: &Value, expected_type: &str, expected_reason: Option<&str>) {
    let details = payload
        .get("details")
        .and_then(Value::as_object)
        .expect("details map");
    let Some(traceback) = details.get("traceback").and_then(Value::as_object) else {
        assert!(
            expected_reason.is_none(),
            "expected recommendation for {expected_type} but no traceback was captured"
        );
        return;
    };
    assert_eq!(
        traceback
            .get("error_type")
            .and_then(Value::as_str)
            .expect("error type"),
        expected_type
    );
    match expected_reason {
        Some(reason) => {
            let recommendation = traceback
                .get("recommendation")
                .and_then(Value::as_object)
                .expect("recommendation payload");
            assert_eq!(
                recommendation
                    .get("reason")
                    .and_then(Value::as_str)
                    .expect("reason"),
                reason
            );
        }
        None => {
            assert!(
                !traceback.contains_key("recommendation"),
                "unexpected recommendation for {expected_type:?}"
            );
        }
    }
}

fn find_python() -> Option<String> {
    let candidates = [
        std::env::var("PYTHON").ok(),
        Some("python3".to_string()),
        Some("python".to_string()),
    ];
    for candidate in candidates.into_iter().flatten() {
        let status = std::process::Command::new(&candidate)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if matches!(status, Ok(code) if code.success()) {
            return Some(candidate);
        }
    }
    None
}
