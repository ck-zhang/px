use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use assert_cmd::cargo::cargo_bin_cmd;
use serde_json::Value;
use toml_edit::DocumentMut;

mod common;

use common::{parse_json, prepare_named_fixture};

fn find_python() -> Option<String> {
    let candidates = [
        std::env::var("PYTHON").ok(),
        Some("python3".to_string()),
        Some("python".to_string()),
    ];
    for candidate in candidates.into_iter().flatten() {
        let status = Command::new(&candidate)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if matches!(status, Ok(code) if code.success()) {
            return Some(candidate);
        }
    }
    None
}

#[test]
fn project_status_json_consistent() {
    let (_tmp, root) = common::init_empty_project("status-consistent");
    let Some(python) = find_python() else {
        eprintln!("skipping status test (python binary not found)");
        return;
    };

    cargo_bin_cmd!("px")
        .current_dir(&root)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["sync"])
        .assert()
        .success();

    let assert = cargo_bin_cmd!("px")
        .current_dir(&root)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["--json", "status"])
        .assert()
        .success();
    let payload = parse_json(&assert);
    assert_eq!(payload["context"]["kind"], "project");
    let state = payload["project"]["state"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        state == "Consistent" || state == "InitializedEmpty",
        "expected consistent project state, got {state}"
    );
    assert_eq!(payload["next_action"]["kind"], "none");
    assert_eq!(payload["env"]["status"], "clean");
}

#[test]
fn project_status_detects_manifest_drift() {
    let (_tmp, root) = common::init_empty_project("status-drift");
    let Some(python) = find_python() else {
        eprintln!("skipping status test (python binary not found)");
        return;
    };

    cargo_bin_cmd!("px")
        .current_dir(&root)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["sync"])
        .assert()
        .success();

    let pyproject = root.join("pyproject.toml");
    let mut doc: DocumentMut = fs::read_to_string(&pyproject)
        .expect("read pyproject")
        .parse()
        .expect("parse pyproject");
    if let Some(array) = doc["project"]["dependencies"].as_array_mut() {
        array.push("requests==2.32.3");
    }
    fs::write(&pyproject, doc.to_string()).expect("write pyproject");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&root)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["--json", "status"])
        .assert()
        .failure();
    let payload = parse_json(&assert);
    assert_eq!(payload["project"]["state"], "NeedsLock");
    assert_eq!(payload["next_action"]["kind"], "sync");
    assert_eq!(payload["env"]["status"], "stale");
}

#[test]
fn project_status_detects_missing_env() {
    let (_tmp, root) = common::init_empty_project("status-missing-env");
    let Some(python) = find_python() else {
        eprintln!("skipping status test (python binary not found)");
        return;
    };

    cargo_bin_cmd!("px")
        .current_dir(&root)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["sync"])
        .assert()
        .success();

    let state_path = root.join(".px").join("state.json");
    let state: Value =
        serde_json::from_str(&fs::read_to_string(&state_path).expect("read state.json"))
            .expect("parse state");
    let site = state["current_env"]["site_packages"]
        .as_str()
        .expect("site path");
    if Path::new(site).exists() {
        fs::remove_dir_all(site).expect("remove env dir");
    }

    let assert = cargo_bin_cmd!("px")
        .current_dir(&root)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["--json", "status"])
        .assert()
        .failure();
    let payload = parse_json(&assert);
    assert_eq!(payload["project"]["state"], "NeedsEnv");
    assert_eq!(payload["env"]["status"], "missing");
    assert_eq!(payload["next_action"]["kind"], "sync");
}

#[test]
fn workspace_member_status_consistent() {
    let (_tmp, root) = prepare_named_fixture("workspace_basic", "status-ws-consistent");
    let member_a = root.join("apps").join("a");
    let member_b = root.join("libs").join("b");
    fs::create_dir_all(&member_a).expect("create member a");
    fs::create_dir_all(&member_b).expect("create member b");
    write_member_manifest(&member_a, "member-a");
    write_member_manifest(&member_b, "member-b");

    let Some(python) = find_python() else {
        eprintln!("skipping status test (python binary not found)");
        return;
    };

    cargo_bin_cmd!("px")
        .current_dir(&root)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["sync"])
        .assert()
        .success();

    let assert = cargo_bin_cmd!("px")
        .current_dir(&member_a)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["status", "--json"])
        .assert()
        .success();
    let payload = parse_json(&assert);
    assert_eq!(payload["context"]["kind"], "workspace_member");
    let state = payload["workspace"]["state"]
        .as_str()
        .expect("workspace state");
    assert!(
        state == "WConsistent" || state == "WInitializedEmpty",
        "expected consistent workspace state, got {state}"
    );
    assert_eq!(payload["next_action"]["kind"], "none");
    assert_eq!(payload["env"]["status"], "clean");
}

#[test]
fn workspace_status_detects_member_drift() {
    let (_tmp, root) = prepare_named_fixture("workspace_basic", "status-ws-drift");
    let member_a = root.join("apps").join("a");
    let member_b = root.join("libs").join("b");
    fs::create_dir_all(&member_a).expect("create member a");
    fs::create_dir_all(&member_b).expect("create member b");
    write_member_manifest(&member_a, "member-a");
    write_member_manifest(&member_b, "member-b");

    let Some(python) = find_python() else {
        eprintln!("skipping status test (python binary not found)");
        return;
    };

    cargo_bin_cmd!("px")
        .current_dir(&root)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["sync"])
        .assert()
        .success();

    let manifest = member_a.join("pyproject.toml");
    let mut doc: DocumentMut = fs::read_to_string(&manifest)
        .expect("read pyproject")
        .parse()
        .expect("parse member manifest");
    if let Some(array) = doc["project"]["dependencies"].as_array_mut() {
        array.push("requests==2.32.3");
    }
    fs::write(&manifest, doc.to_string()).expect("write member manifest");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&member_a)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["status", "--json"])
        .assert()
        .failure();
    let payload = parse_json(&assert);
    assert_eq!(payload["workspace"]["state"], "WNeedsLock");
    assert_eq!(payload["next_action"]["kind"], "sync_workspace");
    assert_eq!(payload["next_action"]["kind"], "sync_workspace");
}

#[test]
fn status_brief_emits_one_line() {
    let (_tmp, root) = common::init_empty_project("status-brief");
    let Some(python) = find_python() else {
        eprintln!("skipping status test (python binary not found)");
        return;
    };
    cargo_bin_cmd!("px")
        .current_dir(&root)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["sync"])
        .assert()
        .success();

    let assert = cargo_bin_cmd!("px")
        .current_dir(&root)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["status", "--brief"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.lines().count() == 1,
        "brief output should be a single line: {stdout:?}"
    );
    assert!(stdout.contains("Consistent"));
}

#[test]
fn status_reports_missing_project() {
    let temp = tempfile::tempdir().expect("tempdir");
    let assert = cargo_bin_cmd!("px")
        .current_dir(temp.path())
        .args(["status"])
        .assert()
        .failure();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("PX001"),
        "missing project error should carry PX001 code: {stdout:?}"
    );
}

fn write_member_manifest(root: &Path, name: &str) {
    let manifest = format!(
        r#"[project]
name = "{name}"
version = "0.0.0"
requires-python = ">=3.11"
dependencies = []

[tool.px]
"#
    );
    fs::write(root.join("pyproject.toml"), manifest).expect("write pyproject");
}
