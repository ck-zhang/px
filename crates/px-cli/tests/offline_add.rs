use assert_cmd::cargo::cargo_bin_cmd;
use std::fs;

mod common;

use common::{ensure_test_store_env, parse_json, reset_test_store_env, test_env_guard};

#[test]
fn offline_add_fails_without_mutating_project_files() {
    let _guard = test_env_guard();
    reset_test_store_env();
    ensure_test_store_env();
    // Allow ensurepip in tests so baseline packaging doesn't require network.
    std::env::remove_var("PX_NO_ENSUREPIP");

    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path();

    cargo_bin_cmd!("px")
        .current_dir(project)
        .args(["init"])
        .assert()
        .success();

    let pyproject_path = project.join("pyproject.toml");
    let lock_path = project.join("px.lock");
    let pyproject_before = fs::read_to_string(&pyproject_path).expect("read pyproject");
    let lock_before = fs::read_to_string(&lock_path).expect("read lockfile");

    let assert = cargo_bin_cmd!("px")
        .current_dir(project)
        .args(["--json", "--offline", "add", "pendulum"])
        .assert()
        .failure();
    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    let message = payload["message"].as_str().expect("message string");
    let hint = payload["details"]["hint"].as_str().unwrap_or_default();
    assert!(
        message.contains("PX_ONLINE=1") || hint.contains("PX_ONLINE=1"),
        "expected offline hint to mention PX_ONLINE=1, got message={message:?} hint={hint:?}"
    );

    let pyproject_after = fs::read_to_string(&pyproject_path).expect("read pyproject");
    let lock_after = fs::read_to_string(&lock_path).expect("read lockfile");
    assert_eq!(pyproject_before, pyproject_after);
    assert_eq!(lock_before, lock_after);
}
