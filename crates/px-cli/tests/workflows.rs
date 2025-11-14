use std::path::{Path, PathBuf};

use assert_cmd::cargo::cargo_bin_cmd;
use serde_json::Value;

mod common;

use common::{parse_json, prepare_fixture};

#[test]
fn env_python_prints_interpreter_path() {
    let (_tmp, project) = prepare_fixture("env-python");
    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["env", "python"])
        .assert()
        .success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone())
        .expect("utf8")
        .trim()
        .to_string();

    assert!(
        stdout.starts_with("px infra env:"),
        "expected prefixed output: {stdout}"
    );
    let path_segment = stdout
        .split_once(':')
        .map(|(_, rest)| rest.trim())
        .unwrap_or(&stdout);
    let interpreter = Path::new(path_segment);
    assert!(interpreter.is_absolute(), "path must be absolute: {stdout}");
    assert!(interpreter.exists(), "interpreter does not exist: {stdout}");
    assert!(
        path_segment.to_lowercase().contains("python"),
        "path should reference python: {stdout}"
    );
}

#[test]
fn env_info_json_contains_core_keys() {
    let (_tmp, project) = prepare_fixture("env-info-json");
    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["--json", "env", "info"])
        .assert()
        .success();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    let details = payload["details"].as_object().expect("details object");
    let interpreter = details
        .get("interpreter")
        .and_then(Value::as_str)
        .expect("interpreter field");
    assert!(Path::new(interpreter).is_absolute());

    let project_root = details
        .get("project_root")
        .and_then(Value::as_str)
        .expect("project_root field");
    let root_path = Path::new(project_root);
    assert!(root_path.is_absolute());
    assert!(root_path.exists());

    let pythonpath = details
        .get("pythonpath")
        .and_then(Value::as_str)
        .expect("pythonpath field");
    assert!(
        pythonpath.contains(project_root),
        "pythonpath should reference project root"
    );
}

#[test]
fn env_paths_prints_human_lines() {
    let (_tmp, project) = prepare_fixture("env-paths");
    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["env", "paths"])
        .assert()
        .success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(stdout.contains("px infra env: Interpreter:"));
    assert!(stdout.contains("Project root:"));
    assert!(stdout.contains("PYTHONPATH:"));
    let project_str = project.display().to_string();
    assert!(stdout.contains(&project_str));
}

#[test]
fn run_prints_fixture_output() {
    let (_tmp, project) = prepare_fixture("run-hello");
    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["run", "sample_px_app.cli", "--", "-n", "PxTest"])
        .assert()
        .success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("Hello, PxTest!"),
        "run should print greeting, got {stdout:?}"
    );
}

#[test]
fn test_command_falls_back_when_pytest_missing() {
    let (_tmp, project) = prepare_fixture("test-fallback");
    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_TEST_FALLBACK_STD", "1")
        .arg("test")
        .assert()
        .success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("px fallback test passed"),
        "fallback runner should report success, got {stdout:?}"
    );
}

#[test]
fn cache_path_resolves_to_override_directory() {
    let temp = tempfile::Builder::new()
        .prefix("px-cache-root")
        .tempdir()
        .expect("tempdir");
    let custom_store = temp.path().join("store");
    let assert = cargo_bin_cmd!("px")
        .env("PX_CACHE_PATH", custom_store.as_os_str())
        .args(["--json", "cache", "path"])
        .assert()
        .success();

    let payload = parse_json(&assert);
    let path = payload["details"]["path"].as_str().expect("path field");
    let path_buf = PathBuf::from(path);
    assert!(path_buf.is_absolute(), "cache path should be absolute");
    assert!(path_buf.exists(), "cache path should exist");
    assert!(
        path_buf.starts_with(temp.path()),
        "cache path should honor PX_CACHE_PATH override"
    );
}
