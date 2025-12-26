use std::{fs, process::Command};

use assert_cmd::cargo::cargo_bin_cmd;
use tempfile::tempdir;

mod common;

use common::{
    detect_host_python_details, ensure_test_store_env, init_empty_project, parse_json,
    prepare_fixture, require_online, reset_test_store_env, test_env_guard, test_temp_root,
};

#[test]
fn fmt_auto_installs_tool_and_preserves_manifest() {
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
    let (_tmp, project) = prepare_fixture("fmt-missing-tool");
    let pyproject = project.join("pyproject.toml");
    let lock = project.join("px.lock");
    let manifest_before = fs::read_to_string(&pyproject).expect("read pyproject");
    let lock_before = fs::read_to_string(&lock).expect("read lock");

    let tools_dir = tempdir().expect("tools dir");
    let store_dir = tempdir().expect("store dir");
    let home_dir = tempdir().expect("home dir");
    let candidates = ["python3.12", "python3", "python"];
    let python = candidates.iter().find_map(|candidate| {
        Command::new(candidate)
            .arg("--version")
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|_| candidate.to_string())
    });
    let Some(python) = python else {
        eprintln!("skipping fmt auto-install test (python binary not found)");
        return;
    };

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("HOME", home_dir.path())
        .env("PX_RUNTIME_PYTHON", &python)
        .env("PX_TOOLS_DIR", tools_dir.path())
        .env("PX_TOOL_STORE", store_dir.path())
        .env_remove("PX_NO_ENSUREPIP")
        .args(["--json", "fmt"])
        .assert()
        .success();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    assert!(
        !project.join(".ruff_cache").exists(),
        "px fmt should keep ruff cache under .px/ to avoid polluting the project root"
    );

    let state_path = tools_dir.path().join("ruff").join(".px").join("state.json");
    let state: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&state_path).expect("read ruff tool state"))
            .expect("parse ruff tool state");
    let env_state = state
        .get("current_env")
        .and_then(|value| value.as_object())
        .expect("current_env object");
    match env_state.get("env_path") {
        None | Some(serde_json::Value::Null) => {}
        Some(serde_json::Value::String(value)) => assert!(
            !value.trim().is_empty(),
            "tool state must not persist an empty-string env_path"
        ),
        Some(other) => panic!("unexpected env_path value: {other:?}"),
    }
    let site = env_state
        .get("site_packages")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    assert!(
        !site.trim().is_empty(),
        "tool state should include a site_packages path"
    );
    assert!(
        std::path::Path::new(site).exists(),
        "tool site_packages should exist: {site:?}"
    );
    let profile_oid = env_state
        .get("profile_oid")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    assert!(
        !profile_oid.trim().is_empty(),
        "tool state should include a non-empty profile_oid"
    );

    assert_eq!(
        manifest_before,
        fs::read_to_string(&pyproject).expect("read pyproject after fmt"),
        "px fmt should not modify pyproject.toml when the formatter is missing"
    );
    assert_eq!(
        lock_before,
        fs::read_to_string(&lock).expect("read lock after fmt"),
        "px fmt should not modify px.lock when the formatter is missing"
    );
}

#[test]
fn fmt_accepts_json_flag_on_subcommand() {
    let (_tmp, project) = prepare_fixture("fmt-json-flag");
    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["fmt", "--json"])
        .assert();

    let payload = parse_json(&assert);
    let status = payload["status"].as_str().expect("status string");
    assert!(
        status == "ok" || status == "user-error",
        "fmt with --json should emit structured status, got {status}"
    );
}

#[test]
fn fmt_frozen_refuses_to_auto_install_tools() {
    let _guard = test_env_guard();
    let (_tmp, project) = prepare_fixture("fmt-missing-tool-frozen");

    let tools_dir = tempdir().expect("tools dir");
    let store_dir = tempdir().expect("store dir");
    let home_dir = tempdir().expect("home dir");
    let candidates = ["python3.12", "python3", "python"];
    let python = candidates.iter().find_map(|candidate| {
        Command::new(candidate)
            .arg("--version")
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|_| candidate.to_string())
    });
    let Some(python) = python else {
        eprintln!("skipping fmt frozen test (python binary not found)");
        return;
    };

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("HOME", home_dir.path())
        .env("PX_RUNTIME_PYTHON", &python)
        .env("PX_TOOLS_DIR", tools_dir.path())
        .env("PX_TOOL_STORE", store_dir.path())
        .args(["--offline", "--json", "fmt", "--frozen"])
        .assert()
        .failure();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    assert_eq!(payload["details"]["reason"], "missing_tool");
    let hint = payload["details"]["hint"].as_str().unwrap_or_default();
    assert!(
        hint.contains("--frozen"),
        "expected hint to explain frozen mode behavior: {hint:?}"
    );
}

#[test]
fn fmt_frozen_overrides_nested_tool_hint() {
    let _guard = test_env_guard();
    let (_tmp, project) = prepare_fixture("fmt-frozen-hint-override");

    let candidates = ["python3.12", "python3", "python"];
    let python = candidates.iter().find_map(|candidate| {
        Command::new(candidate)
            .arg("--version")
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|_| candidate.to_string())
    });
    let Some(python) = python else {
        eprintln!("skipping fmt hint test (python binary not found)");
        return;
    };
    let Some((python_exe, channel, version)) = detect_host_python_details(&python) else {
        eprintln!("skipping fmt hint test (unable to inspect python)");
        return;
    };

    let tools_dir = tempdir().expect("tools dir");
    let tool_root = tools_dir.path().join("ruff");
    fs::create_dir_all(&tool_root).expect("tool root");
    fs::write(
        tool_root.join("pyproject.toml"),
        r#"[project]
name = "ruff"
version = "0.0.0"
requires-python = ">=3.8"
dependencies = []

[tool.px]
"#,
    )
    .expect("write tool pyproject");
    fs::write(
        tool_root.join("tool.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "name": "ruff",
            "spec": "ruff",
            "entry": "ruff",
            "console_scripts": {},
            "runtime_version": channel,
            "runtime_full_version": version,
            "runtime_path": python_exe,
            "installed_spec": "ruff",
            "created_at": "1970-01-01T00:00:00Z",
            "updated_at": "1970-01-01T00:00:00Z",
        }))
        .expect("encode tool.json")
            + "\n",
    )
    .expect("write tool.json");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_TOOLS_DIR", tools_dir.path())
        .args(["--json", "fmt", "--frozen"])
        .assert()
        .failure();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    assert_eq!(payload["details"]["reason"], "tool_not_ready");
    let hint = payload["details"]["hint"].as_str().unwrap_or_default();
    let nested_hint = payload["details"]["details"]["hint"]
        .as_str()
        .unwrap_or_default();
    assert!(
        hint.contains("px tool install ruff"),
        "expected hint to recommend tool install, got {hint:?}"
    );
    assert!(
        nested_hint.contains("px tool install ruff"),
        "expected nested hint to match tool install, got {nested_hint:?}"
    );
    assert!(
        !nested_hint.contains("px sync"),
        "nested hint should not suggest px sync for tools, got {nested_hint:?}"
    );
}

#[test]
fn fmt_empty_project_reports_nothing_to_format() {
    let _guard = test_env_guard();
    reset_test_store_env();
    ensure_test_store_env();

    let temp = tempfile::Builder::new()
        .prefix("fmt-empty-project")
        .tempdir_in(test_temp_root())
        .expect("tempdir");
    let project = temp.path();
    fs::write(
        project.join("pyproject.toml"),
        r#"[project]
name = "fmt-empty-project"
version = "0.1.0"

[tool.px]
"#,
    )
    .expect("write pyproject");

    let assert = cargo_bin_cmd!("px")
        .current_dir(project)
        .args(["fmt"])
        .assert()
        .success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("stdout utf8");
    assert!(
        stdout.to_ascii_lowercase().contains("nothing to format"),
        "expected a no-op fmt message, got: {stdout:?}"
    );
    assert!(
        !stdout.contains("Tip:"),
        "no-op fmt output should avoid boilerplate tips, got: {stdout:?}"
    );
}

#[test]
fn fmt_offline_missing_tool_reports_cache_miss() {
    let _guard = test_env_guard();
    let (_tmp, project) = init_empty_project("fmt-offline-missing-tool");
    fs::write(project.join("hello.py"), "print('hi')\n").expect("write python file");

    let tools_dir = tempdir().expect("tools dir");
    let store_dir = tempdir().expect("store dir");
    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_TOOLS_DIR", tools_dir.path())
        .env("PX_TOOL_STORE", store_dir.path())
        .args(["--offline", "--json", "fmt"])
        .assert()
        .failure();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    assert_eq!(payload["details"]["reason"], "offline");
    let message = payload["message"].as_str().unwrap_or_default();
    assert!(
        message.to_ascii_lowercase().contains("ruff")
            && message.to_ascii_lowercase().contains("offline"),
        "expected tool-specific offline cache miss, got {message:?}"
    );
    assert!(
        !message
            .to_ascii_lowercase()
            .contains("dependency resolution failed"),
        "expected offline tool message, got {message:?}"
    );
}
