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

#[test]
fn run_frozen_errors_when_lock_missing() {
    let (_tmp, project) = prepare_fixture("run-json-flag");
    let lock = project.join("px.lock");
    fs::remove_file(&lock).expect("remove lock");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["--json", "run", "--frozen"])
        .assert()
        .failure();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    assert_eq!(
        payload["details"]["reason"], "missing_lock",
        "expected missing lock reason"
    );
    let hint = payload["details"]["hint"].as_str().unwrap_or_default();
    assert!(
        hint.to_ascii_lowercase().contains("px sync"),
        "hint should direct to px sync, got {hint:?}"
    );
}

#[test]
fn run_frozen_errors_when_env_outdated() {
    let (_tmp, project) = prepare_fixture("run-json-flag");
    let px_dir = project.join(".px");
    fs::create_dir_all(&px_dir).expect("px dir");
    write_stale_state(&project, "stale-lock-hash");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["--json", "run", "--frozen"])
        .assert()
        .failure();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    assert_eq!(
        payload["details"]["reason"], "env_outdated",
        "expected env_outdated reason"
    );
    let hint = payload["details"]["hint"].as_str().unwrap_or_default();
    assert!(
        hint.to_ascii_lowercase().contains("px sync"),
        "hint should direct to px sync, got {hint:?}"
    );
}

#[test]
fn run_frozen_errors_on_runtime_mismatch() {
    let (_tmp, project) = prepare_fixture("run-json-flag");
    let lock = project.join("px.lock");
    let lock_id = "52d10cf2634c5817f0e9937a63201c8bc279c9a4e4083e9120a1504afd2a5674";
    let px_dir = project.join(".px");
    fs::create_dir_all(&px_dir).expect("px dir");
    fs::create_dir_all(px_dir.join("site")).expect("site dir");

    // Seed state with matching lock but an incompatible platform to trigger runtime mismatch.
    let state = serde_json::json!({
        "current_env": {
            "id": "test-env",
            "lock_hash": lock_id,
            "platform": "osx_64",
            "site_packages": project.join(".px").join("site").display().to_string(),
            "python": {
                "path": "python3",
                "version": "3.11.0"
            }
        }
    });
    let mut buf = serde_json::to_vec_pretty(&state).expect("serialize state");
    buf.push(b'\n');
    fs::write(px_dir.join("state.json"), buf).expect("write state");
    assert!(lock.exists(), "fixture lockfile should exist");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["--json", "run", "--frozen"])
        .assert()
        .failure();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    assert_eq!(
        payload["details"]["reason"], "runtime_mismatch",
        "expected runtime mismatch when platform differs"
    );
    let hint = payload["details"]["hint"].as_str().unwrap_or_default();
    assert!(
        hint.to_ascii_lowercase().contains("px sync"),
        "hint should recommend px sync, got {hint:?}"
    );
}

fn write_stale_state(project: &Path, lock_hash: &str) {
    let state = serde_json::json!({
        "current_env": {
            "id": "test-env",
            "lock_hash": lock_hash,
            "platform": "any",
            "site_packages": project.join(".px").join("site").display().to_string(),
            "python": {
                "path": "python3",
                "version": "3.11.0"
            }
        }
    });
    let mut buf = serde_json::to_vec_pretty(&state).expect("serialize state");
    buf.push(b'\n');
    fs::write(project.join(".px").join("state.json"), buf).expect("write state");
}

#[test]
fn run_accepts_post_subcommand_json_flag() {
    use common::prepare_named_fixture;
    let (_tmp, project) = prepare_named_fixture("run-json-flag", "run-json-flag");
    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["run", "--json", "sample_px_app.cli", "--", "JsonInline"])
        .assert()
        .success();

    let payload = parse_json(&assert);
    assert_eq!(payload.get("status"), Some(&Value::String("ok".into())));
    let details = payload["details"].as_object().expect("details object");
    assert_eq!(
        details.get("entry"),
        Some(&Value::String("sample_px_app.cli".into()))
    );
    let stdout = payload["details"]["stdout"].as_str().unwrap_or_default();
    assert!(
        stdout.contains("Hello, JsonInline!"),
        "run output should include greeting when --json follows subcommand: {stdout:?}"
    );
}

#[test]
fn run_emits_hint_when_requests_socks_missing_under_proxy() {
    use common::prepare_named_fixture;
    let (_tmp, project) = prepare_named_fixture("run-proxy-traceback", "run-proxy-traceback");
    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("ALL_PROXY", "socks5h://127.0.0.1:9999")
        .args(["--json", "run", "sample_px_app.cli"])
        .assert()
        .failure();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "failure");
    let details = payload["details"].as_object().expect("details");
    assert!(
        details.contains_key("traceback"),
        "proxy failure should include parsed traceback"
    );
    assert!(
        details.get("hint").is_some() || details.get("stderr").is_some(),
        "proxy failure should include remediation hint or stderr"
    );
}

#[test]
fn run_frozen_handles_large_dependency_graph() {
    use common::prepare_named_fixture;
    let (_tmp, project) = prepare_named_fixture("large_graph", "large-graph");
    let lock = project.join("px.lock");
    std::fs::remove_file(&lock).ok(); // force sync to write a fresh lock

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["--json", "sync"])
        .assert()
        .failure();

    let payload = parse_json(&assert);
    let status = payload["status"].as_str().unwrap_or_default();
    assert_ne!(status, "ok", "large graph sync should not succeed");
    let message = payload["message"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    assert!(
        message.contains("resolver") || message.contains("dependency resolution failed"),
        "expected resolver failure for large graph, got {message:?}"
    );
    if let Some(why) = payload["details"]["why"].as_str() {
        let why_lower = why.to_ascii_lowercase();
        assert!(
            why_lower.contains("conflicting") || why_lower.contains("resolver"),
            "details should mention resolver conflict, got {why:?}"
        );
    }
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
