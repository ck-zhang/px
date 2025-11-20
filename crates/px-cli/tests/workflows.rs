use std::{fs, path::Path};

use assert_cmd::cargo::cargo_bin_cmd;
use serde_json::{json, Value};
use toml_edit::{value, DocumentMut, Item, Table};

mod common;

use common::{parse_json, prepare_fixture};

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
fn run_forwards_args_to_default_entry_when_no_entry_passed() {
    let (_tmp, project) = prepare_fixture("run-forward-default");
    set_project_scripts(&project, &[("sample-px-app", "sample_px_app.cli:main")]);

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["--json", "run", "--", "Forwarded"])
        .assert()
        .success();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    let details = payload["details"].as_object().expect("details object");
    assert_eq!(
        details.get("entry"),
        Some(&Value::String("sample_px_app.cli".into()))
    );
    let args = details
        .get("args")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    assert!(
        args.contains(&Value::String("Forwarded".into())),
        "expected forwarded args to include original token: {args:?}"
    );
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
    assert!(hint.contains("px migrate --apply"));
}

#[test]
fn run_frozen_errors_when_environment_missing() {
    let (_tmp, project) = prepare_fixture("run-frozen-missing-env");
    if project.join(".px").exists() {
        fs::remove_dir_all(project.join(".px")).expect("clean .px");
    }

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["--json", "run", "--frozen"])
        .assert()
        .failure();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    let message = payload["message"].as_str().expect("message string");
    assert!(
        message.contains("project environment missing"),
        "expected strict mode to fail when env missing: {message:?}"
    );
    assert_eq!(payload["details"]["reason"], "missing_env");
    let hint = payload["details"]["hint"].as_str().expect("hint field");
    assert!(
        hint.contains("px sync"),
        "strict-mode hint should recommend px sync: {hint:?}"
    );
}

fn write_module(project: &Path, module: &str, body: &str) {
    let module_path = project.join("sample_px_app").join(format!("{module}.py"));
    fs::write(module_path, body).expect("write module");
}

fn set_project_scripts(project: &Path, entries: &[(&str, &str)]) {
    let pyproject = project.join("pyproject.toml");
    let contents = fs::read_to_string(&pyproject).expect("read pyproject");
    let mut doc: DocumentMut = contents.parse().expect("parse pyproject");
    let mut table = Table::new();
    for &(name, target) in entries {
        table.insert(name, value(target));
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
