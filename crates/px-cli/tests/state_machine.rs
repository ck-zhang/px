use std::fs;

use assert_cmd::cargo::cargo_bin_cmd;

mod common;

use common::{parse_json, prepare_fixture};

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
fn fmt_bypasses_project_lock_env_gating() {
    let (_tmp, project) = prepare_fixture("fmt-bypass");
    let lock = project.join("px.lock");
    fs::remove_file(&lock).expect("remove px.lock");
    fs::remove_dir_all(project.join(".px")).ok();

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["--json", "fmt"])
        .assert()
        .success();

    let payload = parse_json(&assert);
    assert_eq!(
        payload["status"], "ok",
        "fmt should succeed without px.lock"
    );
}

#[test]
fn frozen_test_refuses_autosync_for_missing_env() {
    let (_tmp, project) = prepare_fixture("frozen-missing-env");
    fs::remove_dir_all(project.join(".px")).ok();
    let Some(python) = find_python() else {
        eprintln!("skipping frozen env test (python binary not found)");
        return;
    };

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["--json", "test", "--frozen"])
        .assert()
        .failure();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    let reason = payload["details"]
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    assert_eq!(reason, "missing_env");
}

#[test]
fn test_repairs_missing_env_in_dev_mode() {
    let (_tmp, project) = prepare_fixture("dev-missing-env");
    fs::remove_dir_all(project.join(".px")).ok();
    let Some(python) = find_python() else {
        eprintln!("skipping dev env repair test (python binary not found)");
        return;
    };

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .env("PX_TEST_FALLBACK_STD", "1")
        .args(["--json", "test"])
        .assert()
        .success();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    let autosync = payload["details"]
        .get("autosync")
        .and_then(serde_json::Value::as_object)
        .expect("autosync details present");
    assert_eq!(
        autosync.get("action").and_then(serde_json::Value::as_str),
        Some("env-recreate"),
        "expected env recreation autosync"
    );
}

#[test]
fn test_requires_lock_before_running_tests() {
    let (_tmp, project) = prepare_fixture("test-missing-lock");
    let lock = project.join("px.lock");
    if lock.exists() {
        fs::remove_file(&lock).expect("remove px.lock");
    }

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["--json", "test"])
        .assert()
        .failure();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    assert_eq!(
        payload["details"].get("reason"),
        Some(&serde_json::Value::String("missing_lock".into()))
    );
}

#[test]
fn frozen_sync_reports_lock_drift() {
    let (_tmp, project) = prepare_fixture("drifted-lock");
    let lock_path = project.join("px.lock");
    let contents = fs::read_to_string(&lock_path).expect("read lock");
    let rewritten = contents.replace(
        "manifest_fingerprint = \"2838da4467b85c6e6f67355fc3fa7c216562c042b38910144021cd2b13c8d072\"",
        "manifest_fingerprint = \"ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff\"",
    );
    fs::write(&lock_path, rewritten).expect("write drifted lock");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["--json", "sync", "--frozen"])
        .assert()
        .failure();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    let reason = payload["details"]
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    assert_eq!(reason, "lock_drift");
}
