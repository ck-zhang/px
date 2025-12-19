use std::fs;

use assert_cmd::cargo::cargo_bin_cmd;
use tempfile::tempdir;

mod common;

use common::{detect_host_python, parse_json, prepare_fixture, require_online, test_env_guard};

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
    let registry_dir = tempdir().expect("registry dir");
    let registry = registry_dir.path().join("runtimes.json");
    let Some(python) = common::find_python() else {
        eprintln!("skipping fmt auto-install test (python binary not found)");
        return;
    };
    let Some((python_path, channel)) = detect_host_python(&python) else {
        eprintln!("skipping fmt auto-install test (unable to inspect python interpreter)");
        return;
    };
    cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_REGISTRY", &registry)
        .args(["python", "install", &channel, "--path", &python_path])
        .assert()
        .success();

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .env("PX_RUNTIME_REGISTRY", &registry)
        .env("PX_TOOLS_DIR", tools_dir.path())
        .env("PX_TOOL_STORE", store_dir.path())
        .args(["--json", "fmt"])
        .assert()
        .success();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
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
