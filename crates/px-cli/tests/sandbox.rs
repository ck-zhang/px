use std::fs;
use std::io::Write;
use std::process::Command;

use assert_cmd::cargo::cargo_bin_cmd;

mod common;

use common::{
    fake_sandbox_backend, find_python, parse_json, prepare_named_fixture,
    prepare_traceback_fixture, test_env_guard,
};

fn python_version(python: &str) -> Option<(u32, u32)> {
    let output = Command::new(python)
        .arg("-c")
        .arg("import sys; print(f\"{sys.version_info[0]}.{sys.version_info[1]}\")")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut parts = text.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

#[test]
fn sandbox_run_uses_container_backend() {
    let _guard = test_env_guard();
    let Some(python) = find_python() else {
        eprintln!("skipping sandbox run test (python binary not found)");
        return;
    };
    let (tmp, project) = prepare_traceback_fixture("sandbox-run");
    let (backend, log) = fake_sandbox_backend(tmp.path()).expect("backend script");
    let store = tmp.path().join("sandbox-store");

    cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_SANDBOX_STORE", &store)
        .env("PX_SANDBOX_BACKEND", &backend)
        .env("PX_FAKE_SANDBOX_LOG", &log)
        .env("PX_FAKE_SANDBOX_INSPECT_EXIT", "1")
        .env("PX_RUNTIME_PYTHON", &python)
        .arg("sync")
        .assert()
        .success();

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_SANDBOX_STORE", &store)
        .env("PX_SANDBOX_BACKEND", &backend)
        .env("PX_FAKE_SANDBOX_LOG", &log)
        .env("PX_FAKE_SANDBOX_INSPECT_EXIT", "1")
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["--json", "run", "--sandbox", "python", "-c", "print('hi')"])
        .assert()
        .success();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    assert!(payload["details"]["sandbox"].is_object());
    let log_contents = fs::read_to_string(&log).unwrap_or_default();
    assert!(
        log_contents.contains("run:"),
        "sandbox backend should have handled run: log={log_contents}"
    );
}

#[test]
fn sandbox_test_runs_in_workspace() {
    let _guard = test_env_guard();
    let Some(python) = find_python() else {
        eprintln!("skipping workspace sandbox test (python binary not found)");
        return;
    };
    if let Some((major, minor)) = python_version(&python) {
        if major < 3 || (major == 3 && minor < 11) {
            eprintln!("skipping workspace sandbox test (python {major}.{minor} < 3.11)");
            return;
        }
    }
    let (tmp, workspace) = prepare_named_fixture("workspace_basic", "sandbox-ws");
    let member = workspace.join("apps").join("a");
    let tests_dir = member.join("tests");
    fs::create_dir_all(&tests_dir).expect("create tests dir");
    fs::write(
        tests_dir.join("runtests.py"),
        "print('sandbox workspace tests'); import sys; sys.exit(0)\n",
    )
    .expect("write runtests script");

    let (backend, log) = fake_sandbox_backend(tmp.path()).expect("backend script");
    let store = tmp.path().join("sandbox-store");

    cargo_bin_cmd!("px")
        .current_dir(&workspace)
        .env("PX_SANDBOX_STORE", &store)
        .env("PX_SANDBOX_BACKEND", &backend)
        .env("PX_FAKE_SANDBOX_LOG", &log)
        .env("PX_FAKE_SANDBOX_INSPECT_EXIT", "1")
        .env("PX_RUNTIME_PYTHON", &python)
        .arg("sync")
        .assert()
        .success();

    let assert = cargo_bin_cmd!("px")
        .current_dir(&member)
        .env("PX_SANDBOX_STORE", &store)
        .env("PX_SANDBOX_BACKEND", &backend)
        .env("PX_FAKE_SANDBOX_LOG", &log)
        .env("PX_FAKE_SANDBOX_INSPECT_EXIT", "1")
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["--json", "test", "--sandbox"])
        .assert()
        .success();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    assert!(payload["details"]["sandbox"].is_object());
    let log_contents = fs::read_to_string(&log).unwrap_or_default();
    assert!(
        log_contents.contains("run:"),
        "sandbox backend should have executed test command"
    );
}

#[test]
fn sandbox_backend_unavailable_surfaces_px_error() {
    let _guard = test_env_guard();
    let Some(python) = find_python() else {
        eprintln!("skipping sandbox backend error test (python binary not found)");
        return;
    };
    let (tmp, project) = prepare_traceback_fixture("sandbox-backend-missing");
    let store = tmp.path().join("sandbox-store");

    cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_SANDBOX_STORE", &store)
        .env("PX_RUNTIME_PYTHON", &python)
        .arg("sync")
        .assert()
        .success();

    cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_SANDBOX_STORE", &store)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["run", "python", "-c", "print('ready')"])
        .assert()
        .success();

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_SANDBOX_STORE", &store)
        .env("PX_SANDBOX_BACKEND", "/nonexistent/backend/bin")
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["--json", "run", "--sandbox", "python", "-c", "print('hi')"])
        .assert()
        .failure();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    let code = payload["details"]
        .get("code")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert_eq!(code, "PX903");
}

#[test]
fn sandbox_invalid_base_is_reported() {
    let _guard = test_env_guard();
    let Some(python) = find_python() else {
        eprintln!("skipping sandbox base test (python binary not found)");
        return;
    };
    let (tmp, project) = prepare_traceback_fixture("sandbox-invalid-base");
    let manifest = project.join("pyproject.toml");
    fs::OpenOptions::new()
        .append(true)
        .open(&manifest)
        .expect("open manifest")
        .write_all(b"\n[tool.px.sandbox]\nbase = \"does-not-exist\"\n")
        .expect("write sandbox config");
    let (backend, log) = fake_sandbox_backend(tmp.path()).expect("backend script");
    let store = tmp.path().join("sandbox-store");

    cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_SANDBOX_STORE", &store)
        .env("PX_SANDBOX_BACKEND", &backend)
        .env("PX_FAKE_SANDBOX_LOG", &log)
        .env("PX_RUNTIME_PYTHON", &python)
        .arg("sync")
        .assert()
        .success();

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_SANDBOX_STORE", &store)
        .env("PX_SANDBOX_BACKEND", &backend)
        .env("PX_FAKE_SANDBOX_LOG", &log)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["--json", "run", "--sandbox", "python", "-c", "print('hi')"])
        .assert()
        .failure();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    let code = payload["details"]
        .get("code")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert_eq!(code, "PX900");
}

#[test]
fn sandbox_frozen_requires_existing_env() {
    let _guard = test_env_guard();
    let Some(python) = find_python() else {
        eprintln!("skipping sandbox frozen test (python binary not found)");
        return;
    };
    let (_tmp, project) = prepare_traceback_fixture("sandbox-frozen");
    let store = project.join("sandbox-store");
    fs::remove_dir_all(project.join(".px")).ok();

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_SANDBOX_STORE", &store)
        .env("PX_RUNTIME_PYTHON", &python)
        .args([
            "--json",
            "run",
            "--sandbox",
            "--frozen",
            "python",
            "-c",
            "print('hi')",
        ])
        .assert()
        .failure();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    let reason = payload["details"]
        .get("reason")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(
        !reason.is_empty(),
        "expected frozen sandbox run to fail for missing env"
    );
}

#[test]
fn pack_image_still_builds_layout() {
    let _guard = test_env_guard();
    let Some(python) = find_python() else {
        eprintln!("skipping pack image test (python binary not found)");
        return;
    };
    let (tmp, project) = prepare_traceback_fixture("sandbox-pack");
    let store = tmp.path().join("sandbox-store");
    let out = tmp.path().join("image.tar");

    cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_SANDBOX_STORE", &store)
        .env("PX_ONLINE", "1")
        .env("PX_RUNTIME_PYTHON", &python)
        .arg("sync")
        .assert()
        .success();

    cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_SANDBOX_STORE", &store)
        .env("PX_ONLINE", "1")
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["run", "python", "-c", "print('ready')"])
        .assert()
        .success();

    cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_SANDBOX_STORE", &store)
        .env("PX_ONLINE", "1")
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["sync", "--frozen"])
        .assert()
        .success();

    let status = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_SANDBOX_STORE", &store)
        .env("PX_ONLINE", "1")
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["--json", "status"])
        .assert()
        .success();
    let status_payload = parse_json(&status);
    let state = status_payload["details"]
        .get("state")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if state != "consistent" {
        eprintln!("skipping pack image test due to state={state}");
        return;
    }

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_SANDBOX_STORE", &store)
        .env("PX_ONLINE", "1")
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["--json", "pack", "image", "--out"])
        .arg(&out)
        .assert()
        .success();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    assert!(out.exists(), "pack should write oci archive");
    let sbx = payload["details"]
        .get("sbx_id")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(!sbx.is_empty(), "sbx_id should be present in pack output");
}
