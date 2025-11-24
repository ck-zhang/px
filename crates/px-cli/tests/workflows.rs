use std::{fs, path::Path};

use assert_cmd::cargo::cargo_bin_cmd;
use serde_json::{json, Value};
use toml_edit::{value, DocumentMut, Item, Table};

mod common;

use common::{parse_json, prepare_fixture};

#[test]
fn run_prints_fixture_output() {
    let (_tmp, project) = prepare_fixture("run-hello");
    set_px_scripts(&project, &[("cli", "sample_px_app.cli:main")]);
    let Some(python) = find_python() else {
        eprintln!("skipping run test (python binary not found)");
        return;
    };
    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["run", "cli", "--", "-n", "PxTest"])
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
    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .arg("run")
        .assert()
        .failure();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("px run requires a target"),
        "expected missing-target error, got {stdout:?}"
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
        .failure();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    let message = payload["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("px run requires a target"),
        "expected missing-target failure, got {message:?}"
    );
}

#[test]
fn run_supports_python_passthrough_invocations() {
    let (_tmp, project) = prepare_fixture("run-passthrough");
    let Some(python) = find_python() else {
        eprintln!("skipping passthrough test (python binary not found)");
        return;
    };
    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
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
    set_px_scripts(
        &project,
        &[
            ("auto-default", "sample_px_app.default_entry:main"),
            ("cli", "sample_px_app.cli:main"),
        ],
    );
    let Some(python) = find_python() else {
        eprintln!("skipping explicit entry test (python binary not found)");
        return;
    };
    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["--json", "run", "cli", "--", "-n", "Explicit"])
        .assert()
        .success();

    let payload = parse_json(&assert);
    let details = payload["details"].as_object().expect("details object");
    assert_eq!(
        details.get("source"),
        Some(&Value::String("px-scripts".into()))
    );
    assert_eq!(
        details.get("entry"),
        Some(&Value::String("sample_px_app.cli:main".into()))
    );
    assert_eq!(details.get("script"), Some(&Value::String("cli".into())));
}

#[test]
fn run_forwards_args_to_default_entry_when_no_entry_passed() {
    let (_tmp, project) = prepare_fixture("run-forward-default");
    set_px_scripts(&project, &[("cli", "sample_px_app.cli:main")]);
    let Some(python) = find_python() else {
        eprintln!("skipping forward args test (python binary not found)");
        return;
    };

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["--json", "run", "cli", "--", "Forwarded"])
        .assert()
        .success();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    let details = payload["details"].as_object().expect("details object");
    assert_eq!(
        details.get("entry"),
        Some(&Value::String("sample_px_app.cli:main".into()))
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
    set_px_scripts(&project, &[("cli", "sample_px_app.cli:main")]);
    let Some(python) = find_python() else {
        eprintln!("skipping frozen env test (python binary not found)");
        return;
    };

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["--json", "run", "cli", "--frozen"])
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
    set_px_scripts(&project, &[("cli", "sample_px_app.cli:main")]);
    let Some(python) = find_python() else {
        eprintln!("skipping frozen missing-lock test (python binary not found)");
        return;
    };

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["--json", "run", "cli", "--frozen"])
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
    set_px_scripts(&project, &[("cli", "sample_px_app.cli:main")]);
    let Some(python) = find_python() else {
        eprintln!("skipping frozen outdated-env test (python binary not found)");
        return;
    };

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["--json", "run", "cli", "--frozen"])
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
    set_px_scripts(&project, &[("cli", "sample_px_app.cli:main")]);
    let Some(python) = find_python() else {
        eprintln!("skipping frozen runtime mismatch test (python binary not found)");
        return;
    };

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["--json", "run", "cli", "--frozen"])
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
    set_px_scripts(&project, &[("cli", "sample_px_app.cli:main")]);
    let Some(python) = find_python() else {
        eprintln!("skipping post-json test (python binary not found)");
        return;
    };
    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["run", "--json", "cli", "--", "JsonInline"])
        .assert()
        .success();

    let payload = parse_json(&assert);
    assert_eq!(payload.get("status"), Some(&Value::String("ok".into())));
    let details = payload["details"].as_object().expect("details object");
    assert_eq!(
        details.get("entry"),
        Some(&Value::String("sample_px_app.cli:main".into()))
    );
    assert_eq!(
        details.get("source"),
        Some(&Value::String("px-scripts".into()))
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
    set_px_scripts(&project, &[("cli", "sample_px_app.cli:main")]);
    let Some(python) = find_python() else {
        eprintln!("skipping proxy traceback test (python binary not found)");
        return;
    };
    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("ALL_PROXY", "socks5h://127.0.0.1:9999")
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["--json", "run", "cli"])
        .assert()
        .failure();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "error");
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
    let Some(python) = find_python() else {
        eprintln!("skipping large graph test (python binary not found)");
        return;
    };

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["--json", "sync"])
        .assert()
        .success();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    assert!(lock.exists(), "sync should emit px.lock");
}

#[test]
fn run_rejects_dry_run_flag() {
    let temp = tempfile::tempdir().expect("tempdir");
    let assert = cargo_bin_cmd!("px")
        .current_dir(temp.path())
        .args(["run", "--dry-run"])
        .assert()
        .failure();
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(assert.get_output().stdout.as_slice()),
        String::from_utf8_lossy(assert.get_output().stderr.as_slice())
    );
    assert!(
        output.contains("Found argument '--dry-run'")
            || output.contains("unexpected argument '--dry-run'"),
        "run should reject unsupported dry-run flag, got output: {output:?}"
    );
}

fn set_px_scripts(project: &Path, entries: &[(&str, &str)]) {
    let pyproject = project.join("pyproject.toml");
    let contents = fs::read_to_string(&pyproject).expect("read pyproject");
    let mut doc: DocumentMut = contents.parse().expect("parse pyproject");
    let tool = doc
        .entry("tool")
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .expect("tool table");
    let px = tool
        .entry("px")
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .expect("px table");
    let mut scripts = Table::new();
    for &(name, target) in entries {
        scripts.insert(name, value(target));
    }
    px.insert("scripts", Item::Table(scripts));
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

#[test]
fn test_command_falls_back_when_pytest_missing() {
    let (_tmp, project) = prepare_fixture("test-fallback");
    let Some(python) = find_python() else {
        eprintln!("skipping test fallback (python binary not found)");
        return;
    };
    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_TEST_FALLBACK_STD", "1")
        .env("PX_RUNTIME_PYTHON", &python)
        .arg("test")
        .assert()
        .success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("px fallback test passed"),
        "fallback runner should report success, got {stdout:?}"
    );
}
