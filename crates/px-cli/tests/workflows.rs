use std::{
    fs,
    path::{Path, PathBuf},
};

use assert_cmd::cargo::cargo_bin_cmd;
use serde_json::{json, Value};
use toml_edit::{value, DocumentMut, Item, Table};

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
    assert!(
        stdout.contains("px infra env: pythonpath entries:"),
        "paths output should summarize entry count: {stdout:?}"
    );
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
fn tidy_reports_single_line_and_hint() {
    let (_tmp, project) = prepare_fixture("tidy-human");

    cargo_bin_cmd!("px")
        .current_dir(&project)
        .arg("install")
        .assert()
        .success();

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .arg("tidy")
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("px quality tidy: px.lock matches pyproject"),
        "tidy clean output should mention matches: {stdout:?}"
    );
    assert!(
        !stdout.contains("Hint:"),
        "clean tidy should not emit a hint: {stdout:?}"
    );

    let pyproject = project.join("pyproject.toml");
    let mut doc: DocumentMut = fs::read_to_string(&pyproject)
        .expect("read pyproject")
        .parse()
        .expect("parse pyproject");
    doc["project"]["requires-python"] = value(">=3.13");
    fs::write(&pyproject, doc.to_string()).expect("write pyproject");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .arg("tidy")
        .assert()
        .failure();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("px quality tidy: px.lock is out of date"),
        "tidy drift output should call out stale lock: {stdout:?}"
    );
    assert!(
        stdout.contains("Hint: rerun `px install`"),
        "tidy drift should emit remediation hint: {stdout:?}"
    );
}

#[test]
fn run_defaults_to_first_project_script() {
    let (_tmp, project) = prepare_fixture("run-default-script");
    write_module(
        &project,
        "default_entry",
        "def main():\n    print(\"Script default invoked\")\n\nif __name__ == \"__main__\":\n    main()\n",
    );
    set_project_scripts(
        &project,
        &[
            ("auto-default", "sample_px_app.default_entry:main"),
            ("sample-px-app", "sample_px_app.cli:main"),
        ],
    );

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .arg("run")
        .assert()
        .success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("Script default invoked"),
        "default script should run, got {stdout:?}"
    );
}

#[test]
fn run_falls_back_to_package_cli_when_scripts_missing() {
    let (_tmp, project) = prepare_fixture("run-package-cli");
    remove_scripts_table(&project);

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["--json", "run", "--", "-n", "JsonDefault"])
        .assert()
        .success();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    let details = payload["details"].as_object().expect("details object");
    assert_eq!(
        details.get("entry"),
        Some(&Value::String("sample_px_app.cli".into()))
    );
    assert_eq!(
        details.get("source"),
        Some(&Value::String("package-cli".into()))
    );
    assert_eq!(details.get("defaulted"), Some(&Value::Bool(true)));
}

#[test]
fn run_supports_python_passthrough_invocations() {
    let (_tmp, project) = prepare_fixture("run-passthrough");
    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .args([
            "--json",
            "run",
            "python",
            "--",
            "-m",
            "sample_px_app.cli",
            "-n",
            "Passthrough",
        ])
        .assert()
        .success();

    let payload = parse_json(&assert);
    let details = payload["details"].as_object().expect("details object");
    assert_eq!(
        details.get("mode"),
        Some(&Value::String("passthrough".into()))
    );
    assert_eq!(
        details.get("program"),
        Some(&Value::String("python".into()))
    );
    assert_eq!(details.get("uses_px_python"), Some(&Value::Bool(true)));
    assert_eq!(
        details.get("args"),
        Some(&json!(["-m", "sample_px_app.cli", "-n", "Passthrough"]))
    );
}

#[test]
fn run_respects_explicit_entry_even_with_other_defaults() {
    let (_tmp, project) = prepare_fixture("run-explicit-entry");
    write_module(
        &project,
        "default_entry",
        "def main():\n    print(\"Default fallback\")\n\nif __name__ == \"__main__\":\n    main()\n",
    );
    set_project_scripts(
        &project,
        &[
            ("auto-default", "sample_px_app.default_entry:main"),
            ("sample-px-app", "sample_px_app.cli:main"),
        ],
    );

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["--json", "run", "sample_px_app.cli", "--", "-n", "Explicit"])
        .assert()
        .success();

    let payload = parse_json(&assert);
    let details = payload["details"].as_object().expect("details object");
    assert_eq!(
        details.get("source"),
        Some(&Value::String("explicit".into()))
    );
    assert!(details.get("defaulted").is_none());
}

#[test]
fn run_errors_when_no_default_entry_available() {
    let (_tmp, project) = prepare_fixture("run-missing-default");
    let pyproject = project.join("pyproject.toml");
    fs::remove_file(&pyproject).expect("remove pyproject");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["--json", "run"])
        .assert()
        .failure();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    let message = payload["message"].as_str().expect("message string");
    assert!(message.contains("pyproject.toml"));
    let hint = payload["details"]["hint"].as_str().expect("hint string");
    assert!(hint.contains("px migrate --write"));
}

fn write_module(project: &Path, module: &str, body: &str) {
    let module_path = project.join("sample_px_app").join(format!("{}.py", module));
    fs::write(module_path, body).expect("write module");
}

fn set_project_scripts(project: &Path, entries: &[(&str, &str)]) {
    let pyproject = project.join("pyproject.toml");
    let contents = fs::read_to_string(&pyproject).expect("read pyproject");
    let mut doc: DocumentMut = contents.parse().expect("parse pyproject");
    let mut table = Table::new();
    for (name, target) in entries {
        table.insert(*name, value(*target));
    }
    doc["project"]["scripts"] = Item::Table(table);
    fs::write(pyproject, doc.to_string()).expect("write pyproject");
}

fn remove_scripts_table(project: &Path) {
    let pyproject = project.join("pyproject.toml");
    let contents = fs::read_to_string(&pyproject).expect("read pyproject");
    let mut doc: DocumentMut = contents.parse().expect("parse pyproject");
    if let Some(table) = doc["project"].as_table_mut() {
        table.remove("scripts");
    }
    fs::write(pyproject, doc.to_string()).expect("write pyproject");
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
