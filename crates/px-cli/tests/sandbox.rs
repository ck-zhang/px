#![cfg(not(windows))]

use std::fs;
use std::io::{Cursor, Read, Write};
use std::process::Command;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use assert_cmd::cargo::cargo_bin_cmd;
use serde_json::Value;
use tar::{Archive, Builder, Header};

mod common;

use common::{
    fake_sandbox_backend, find_python, parse_json, prepare_fixture, prepare_named_fixture,
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
        .env("PX_FAKE_SANDBOX_PROJECT_ROOT", &project)
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
        .env("PX_FAKE_SANDBOX_PROJECT_ROOT", &project)
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
        .env("PX_FAKE_SANDBOX_PROJECT_ROOT", &workspace)
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
        .env("PX_FAKE_SANDBOX_PROJECT_ROOT", &member)
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
        .env("PX_FAKE_SANDBOX_PROJECT_ROOT", &project)
        .env("PX_RUNTIME_PYTHON", &python)
        .arg("sync")
        .assert()
        .success();

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_SANDBOX_STORE", &store)
        .env("PX_SANDBOX_BACKEND", &backend)
        .env("PX_FAKE_SANDBOX_LOG", &log)
        .env("PX_FAKE_SANDBOX_PROJECT_ROOT", &project)
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

#[test]
fn pack_app_builds_bundle_and_runs() {
    let _guard = test_env_guard();
    let Some(python) = find_python() else {
        eprintln!("skipping pack app test (python binary not found)");
        return;
    };
    let (tmp, project) = prepare_fixture("sandbox-pack-app");
    let (backend, log) = fake_sandbox_backend(tmp.path()).expect("backend script");
    let store_root = common::workspace_root().join("target").join("px-test-sandbox-store");
    fs::create_dir_all(&store_root).expect("sandbox store root");
    let store_dir = tempfile::Builder::new()
        .prefix("sandbox-store")
        .tempdir_in(&store_root)
        .expect("sandbox store dir");
    let store = store_dir.path().to_path_buf();
    let bundle = tmp.path().join("sample-bundle.pxapp");

    cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_SANDBOX_STORE", &store)
        .env("PX_ONLINE", "1")
        .env("PX_RUNTIME_PYTHON", &python)
        .arg("sync")
        .assert()
        .success();

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_SANDBOX_STORE", &store)
        .env("PX_ONLINE", "1")
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["--json", "pack", "app", "--entrypoint", "python", "--out"])
        .arg(&bundle)
        .assert()
        .success();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    assert!(bundle.exists(), "bundle should be written to disk");

    let shim_dir = tmp.path().join("shim");
    fs::create_dir_all(&shim_dir).expect("shim dir");
    let shim = shim_dir.join("python");
    fs::write(
        &shim,
        format!("#!/bin/sh\n\"{}\" \"$@\"\n", python).as_bytes(),
    )
    .expect("write shim");
    #[cfg(unix)]
    {
        let mut perms = fs::metadata(&shim).expect("shim metadata").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&shim, perms).expect("chmod shim");
    }

    let assert = cargo_bin_cmd!("px")
        .current_dir(tmp.path())
        .env("PX_SANDBOX_STORE", &store)
        .env("PX_SANDBOX_BACKEND", &backend)
        .env("PX_FAKE_SANDBOX_LOG", &log)
        .env("PX_FAKE_SANDBOX_INSPECT_EXIT", "1")
        .env("PX_RUNTIME_PYTHON", &python)
        .env(
            "PATH",
            format!(
                "{}:{}",
                shim_dir.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .args(["--json", "run", "--non-interactive"])
        .arg(&bundle)
        .arg("--")
        .arg("-c")
        .arg("print('bundle-ok')")
        .assert()
        .success();

    let run_payload = parse_json(&assert);
    assert_eq!(run_payload["status"], "ok");
    let message = run_payload["message"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        message.contains("bundle-ok"),
        "run output should include python output: {message}"
    );
    assert!(
        run_payload["details"]
            .get("sbx_id")
            .and_then(|v| v.as_str())
            .map(|v| !v.is_empty())
            .unwrap_or(false),
        "sbx_id should be present in run details"
    );
    let log_contents = fs::read_to_string(&log).unwrap_or_default();
    assert!(
        log_contents.contains("run:"),
        "sandbox backend should have executed bundle"
    );
}

#[test]
fn pack_app_is_deterministic() {
    let _guard = test_env_guard();
    let Some(python) = find_python() else {
        eprintln!("skipping pack app deterministic test (python binary not found)");
        return;
    };
    let (tmp, project) = prepare_fixture("sandbox-pack-app-deterministic");
    let store = tmp.path().join("sandbox-store");
    let bundle_a = tmp.path().join("a.pxapp");
    let bundle_b = tmp.path().join("b.pxapp");

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
        .args(["pack", "app", "--out"])
        .arg(&bundle_a)
        .assert()
        .success();

    cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_SANDBOX_STORE", &store)
        .env("PX_ONLINE", "1")
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["pack", "app", "--out"])
        .arg(&bundle_b)
        .assert()
        .success();

    let a = fs::read(&bundle_a).expect("bundle a");
    let b = fs::read(&bundle_b).expect("bundle b");
    assert_eq!(a, b, "identical inputs should produce identical bundles");
}

#[test]
fn run_pxapp_rejects_corrupted_bundle() {
    let _guard = test_env_guard();
    let (tmp, _project) = prepare_fixture("sandbox-pack-app-corrupt");
    let store = tmp.path().join("sandbox-store");
    let bundle = tmp.path().join("bad.pxapp");
    fs::write(&bundle, b"not a valid pxapp").expect("write corrupted bundle");

    let assert = cargo_bin_cmd!("px")
        .current_dir(tmp.path())
        .env("PX_SANDBOX_STORE", &store)
        .args(["--json", "run"])
        .arg(&bundle)
        .assert()
        .failure();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    let code = payload["details"]
        .get("code")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(
        code.starts_with("PX90"),
        "expected sandbox error code, got {code}"
    );
}

#[test]
fn run_pxapp_rejects_incompatible_bundle_version() {
    let _guard = test_env_guard();
    let Some(python) = find_python() else {
        eprintln!("skipping pxapp version test (python binary not found)");
        return;
    };
    let (tmp, project) = prepare_fixture("sandbox-pack-app-incompatible");
    let store = tmp.path().join("sandbox-store");
    let bundle = tmp.path().join("orig.pxapp");
    let broken = tmp.path().join("broken.pxapp");

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
        .args(["pack", "app", "--out"])
        .arg(&bundle)
        .assert()
        .success();

    let data = fs::read(&bundle).expect("read bundle");
    let cursor = Cursor::new(data);
    let mut archive = Archive::new(cursor);
    let mut entries = Vec::new();
    for entry in archive.entries().expect("tar entries") {
        let mut entry = entry.expect("tar entry");
        let mut buf = Vec::new();
        entry.read_to_end(&mut buf).expect("read entry");
        let path = entry.path().expect("path").into_owned();
        if path.as_os_str() == "metadata.json" {
            let mut meta: Value = serde_json::from_slice(&buf).expect("metadata json");
            meta["format_version"] = Value::Number(999.into());
            buf = serde_json::to_vec_pretty(&meta).expect("encode metadata");
        }
        entries.push((path, buf));
    }
    let mut builder = Builder::new(Vec::new());
    for (path, buf) in entries {
        let mut header = Header::new_gnu();
        header.set_path(&path).expect("set path");
        header.set_size(buf.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_uid(0);
        header.set_gid(0);
        header.set_cksum();
        builder
            .append(&header, buf.as_slice())
            .expect("append entry");
    }
    fs::write(&broken, builder.into_inner().expect("bundle bytes")).expect("write broken bundle");

    let assert = cargo_bin_cmd!("px")
        .current_dir(tmp.path())
        .env("PX_SANDBOX_STORE", &store)
        .args(["--json", "run"])
        .arg(&broken)
        .assert()
        .failure();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    let code = payload["details"]
        .get("code")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert_eq!(code, "PX904");
}
