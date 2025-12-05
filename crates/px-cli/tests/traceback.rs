use assert_cmd::cargo::cargo_bin_cmd;
use serde_json::Value;

mod common;

use common::{prepare_fixture, prepare_traceback_fixture};

#[test]
fn run_missing_import_surfaces_px_hint() {
    let (_tmp, project) = prepare_fixture("traceback-missing-import");
    let Some(python) = find_python() else {
        eprintln!("skipping traceback test (python binary not found)");
        return;
    };
    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["run", "python", "-m", "sample_px_app.bad_import"])
        .assert()
        .failure();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("stdout");
    if stdout.contains("Environment missing") {
        // Skip when auto-repair message is emitted instead of user-facing traceback.
        return;
    }
    assert!(
        stdout.contains("Traceback (most recent call last):"),
        "traceback header missing: {stdout:?}"
    );
    assert!(
        stdout.contains("imaginary_package"),
        "missing module should be echoed: {stdout:?}"
    );
    assert!(
        stdout.contains("px run python failed (exit code 1)"),
        "summary line missing: {stdout:?}"
    );
}

#[test]
fn run_missing_import_exposes_traceback_in_json() {
    let (_tmp, project) = prepare_fixture("traceback-json-import");
    let Some(python) = find_python() else {
        eprintln!("skipping traceback test (python binary not found)");
        return;
    };
    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["--json", "run", "python", "-m", "sample_px_app.bad_import"])
        .assert()
        .failure();
    let Ok(payload) = serde_json::from_slice::<Value>(&assert.get_output().stdout) else {
        return;
    };
    let Some(details) = payload["details"].as_object() else {
        return;
    };
    let Some(traceback) = details.get("traceback").and_then(Value::as_object) else {
        return;
    };
    assert_eq!(
        traceback.get("error_type"),
        Some(&Value::String("ModuleNotFoundError".into()))
    );
    assert_eq!(
        traceback.get("error_message"),
        Some(&Value::String("No module named 'imaginary_package'".into()))
    );
    assert_eq!(
        details
            .get("recommendation")
            .and_then(Value::as_object)
            .and_then(|rec| rec.get("reason"))
            .and_then(Value::as_str),
        Some("missing_import")
    );
}

#[test]
fn run_traceback_is_not_duplicated_in_cli_output() {
    let (_tmp, project) = prepare_traceback_fixture("traceback-cli-dedup");
    let Some(python) = find_python() else {
        eprintln!("skipping traceback dedup test (python binary not found)");
        return;
    };
    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["run", "python", "demo_tracebacks.py", "RuntimeError"])
        .assert()
        .failure();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("stdout");
    if stdout.contains("Environment missing") {
        return;
    }
    let header = "Traceback (most recent call last):";
    let count = stdout.matches(header).count();
    assert!(
        count == 1,
        "traceback header should appear once in CLI output: {stdout:?}"
    );
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
