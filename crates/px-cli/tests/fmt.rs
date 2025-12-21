use std::{fs, process::Command};

use assert_cmd::cargo::cargo_bin_cmd;
use tempfile::tempdir;

mod common;

use common::{parse_json, prepare_fixture, require_online, test_env_guard};

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
        .env("PX_RUNTIME_PYTHON", &python)
        .env("PX_TOOLS_DIR", tools_dir.path())
        .env("PX_TOOL_STORE", store_dir.path())
        .env_remove("PX_NO_ENSUREPIP")
        .args(["--json", "fmt"])
        .assert()
        .success();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");

    let state_path = tools_dir.path().join("ruff").join(".px").join("state.json");
    let state: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&state_path).expect("read ruff tool state"),
    )
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
