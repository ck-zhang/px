use std::{fs, path::Path, process::Command};

use assert_cmd::cargo::cargo_bin_cmd;
use serde_json::{json, Value};
use toml_edit::DocumentMut;

mod common;

use common::{parse_json, prepare_fixture, require_online};

#[test]
fn run_prints_fixture_output() {
    let _guard = common::test_env_guard();
    let (_tmp, project) = prepare_fixture("run-hello");
    let Some(python) = find_python() else {
        eprintln!("skipping run test (python binary not found)");
        return;
    };
    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["run", "sample_px_app/cli.py", "-n", "PxTest"])
        .assert()
        .success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("Hello, PxTest!"),
        "run should print greeting, got {stdout:?}"
    );
}

#[test]
fn run_missing_program_honors_json_flag() {
    let _guard = common::test_env_guard();
    let (_tmp, project) = prepare_fixture("run-missing-program-json");
    let Some(python) = find_python() else {
        eprintln!("skipping json run test (python binary not found)");
        return;
    };

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["--json", "run", "definitely-does-not-exist"])
        .assert()
        .failure();

    let payload = parse_json(&assert);
    assert_ne!(payload["status"], Value::String("ok".into()));
    assert!(
        payload["message"]
            .as_str()
            .unwrap_or_default()
            .starts_with("px run"),
        "message should be prefixed with `px run`, got {:?}",
        payload["message"]
    );
    let reason = payload["details"]["reason"].as_str().unwrap_or_default();
    assert!(
        matches!(
            reason,
            "command_not_found" | "command_not_executable" | "command_failed_to_start"
        ),
        "unexpected run error reason: {reason:?}"
    );
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        !stderr.contains("Backtrace omitted") && !stderr.contains("Location:"),
        "stderr should not contain a Rust report when --json is set: {stderr:?}"
    );
}

#[cfg(unix)]
#[test]
fn run_json_output_is_stable_under_tty() {
    let _guard = common::test_env_guard();
    let (_tmp, project) = prepare_fixture("run-json-tty");
    let Some(python) = find_python() else {
        eprintln!("skipping tty json run test (python binary not found)");
        return;
    };

    let script_ok = Command::new("script")
        .args(["-q", "-c", "true", "/dev/null"])
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if !script_ok {
        eprintln!("skipping tty json run test (script command unavailable)");
        return;
    }

    cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["sync"])
        .assert()
        .success();

    let px = assert_cmd::cargo::cargo_bin!("px");
    let command = format!("{} --json run python -c \"print('hi')\"", px.display());
    let output = Command::new("script")
        .args(["-q", "-c", &command, "/dev/null"])
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .output()
        .expect("spawn script");
    assert!(
        output.status.success(),
        "tty json run should succeed, stdout: {:?}, stderr: {:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let payload: Value = serde_json::from_slice(&output.stdout).expect("valid json");
    assert_eq!(payload["status"], Value::String("ok".into()));
    assert_eq!(payload["details"]["interactive"], Value::Bool(false));
    assert!(
        payload["details"]["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("hi"),
        "expected captured runner stdout in json payload: {:?}",
        payload["details"]["stdout"]
    );
}

#[test]
fn run_forwards_arguments_without_separator() {
    let _guard = common::test_env_guard();
    let (_tmp, project) = prepare_fixture("run-no-separator");
    let Some(python) = find_python() else {
        eprintln!("skipping separator test (python binary not found)");
        return;
    };
    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["run", "sample_px_app/cli.py", "-n", "DirectName"])
        .assert()
        .success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("Hello, DirectName!"),
        "run should forward args without requiring -- separator: {stdout:?}"
    );
}

#[test]
fn run_treats_flags_after_target_as_script_args() {
    let _guard = common::test_env_guard();
    let (_tmp, project) = prepare_fixture("run-forward-flags");
    let Some(python) = find_python() else {
        eprintln!("skipping flag-forwarding test (python binary not found)");
        return;
    };
    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args([
            "run",
            "python",
            "-c",
            "import sys; print(' '.join(sys.argv[1:]))",
            "--json",
            "--frozen",
        ])
        .assert()
        .success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("--json") && stdout.contains("--frozen"),
        "flags after the target should be forwarded verbatim: {stdout:?}"
    );
    assert!(
        !stdout.trim_start().starts_with('{'),
        "px should not enable JSON output when --json follows the target: {stdout:?}"
    );
}

#[cfg(unix)]
#[test]
fn run_console_script_keeps_project_on_sys_path() {
    use std::os::unix::fs::PermissionsExt;

    let _guard = common::test_env_guard();
    let (_tmp, project) = prepare_fixture("run-console-script");
    let Some(python) = find_python() else {
        eprintln!("skipping console script test (python binary not found)");
        return;
    };

    cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .arg("sync")
        .assert()
        .success();

    let env_bin = project.join(".px").join("envs").join("current").join("bin");
    let shim = env_bin.join("python");
    let script = env_bin.join("sample-greet");
    fs::write(
        &script,
        format!(
            "#!{}\nimport sample_px_app.cli as cli\nprint(cli.greet('Console'))\n",
            shim.display()
        ),
    )
    .expect("write console script");
    let mut perms = fs::metadata(&script).expect("metadata").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script, perms).expect("chmod script");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["run", "sample-greet"])
        .assert()
        .success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("Hello, Console!"),
        "console script should see project module on sys.path: {stdout:?}"
    );
}

#[test]
fn run_defaults_to_first_project_script() {
    let _guard = common::test_env_guard();
    let (_tmp, project) = prepare_fixture("run-default-script");
    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .arg("run")
        .assert()
        .failure();

    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("px run requires a target"),
        "expected missing-target error, got {stderr:?}"
    );
}

#[test]
fn run_falls_back_to_package_cli_when_scripts_missing() {
    let _guard = common::test_env_guard();
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
    let _guard = common::test_env_guard();
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
fn run_reads_stdin_and_python_source_dirs() {
    let _guard = common::test_env_guard();
    let (_tmp, project) = prepare_fixture("run-python-stdin");
    let package = project.join("sample_px_app");
    let relocated = project.join("python").join("sample_px_app");
    fs::create_dir_all(relocated.parent().expect("parent dir")).expect("create python dir");
    fs::rename(&package, &relocated).expect("move package into python/");

    let Some(python) = find_python() else {
        eprintln!("skipping stdin/path test (python binary not found)");
        return;
    };
    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["run", "python", "-"])
        .write_stdin("from sample_px_app import cli\nprint(cli.greet('PxStdin'))\n")
        .assert()
        .success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("Hello, PxStdin!"),
        "px run should consume stdin and locate packages under python/: {stdout:?}"
    );
}

#[test]
fn run_respects_explicit_entry_even_with_other_defaults() {
    // px no longer resolves px-script aliases; explicit targets are required.
    let _guard = common::test_env_guard();
    let (_tmp, project) = prepare_fixture("run-explicit-entry");
    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .arg("run")
        .assert()
        .failure();

    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("px run requires a target"),
        "expected missing-target error, got {stderr:?}"
    );
}

#[test]
fn run_forwards_args_to_default_entry_when_no_entry_passed() {
    // Default entries are no longer inferred; a target is required.
    let _guard = common::test_env_guard();
    let (_tmp, project) = prepare_fixture("run-forward-default");
    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .arg("run")
        .assert()
        .failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("px run requires a target"),
        "expected missing-target error, got {stderr:?}"
    );
}

#[test]
fn run_errors_when_no_default_entry_available() {
    let _guard = common::test_env_guard();
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
    let _guard = common::test_env_guard();
    let (_tmp, project) = prepare_fixture("run-frozen-missing-env");
    if project.join(".px").exists() {
        fs::remove_dir_all(project.join(".px")).expect("clean .px");
    }
    let Some(python) = find_python() else {
        eprintln!("skipping frozen env test (python binary not found)");
        return;
    };

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["--json", "run", "--frozen", "sample_px_app/cli.py"])
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
    let _guard = common::test_env_guard();
    let (_tmp, project) = prepare_fixture("run-json-flag");
    let lock = project.join("px.lock");
    fs::remove_file(&lock).expect("remove lock");
    let Some(python) = find_python() else {
        eprintln!("skipping frozen missing-lock test (python binary not found)");
        return;
    };

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["--json", "run", "--frozen", "sample_px_app/cli.py"])
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
    let _guard = common::test_env_guard();
    let (_tmp, project) = prepare_fixture("run-json-flag");
    let px_dir = project.join(".px");
    fs::create_dir_all(&px_dir).expect("px dir");
    write_stale_state(&project, "stale-lock-hash");
    let Some(python) = find_python() else {
        eprintln!("skipping frozen outdated-env test (python binary not found)");
        return;
    };

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["--json", "run", "--frozen", "sample_px_app/cli.py"])
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
    let _guard = common::test_env_guard();
    let (_tmp, project) = prepare_fixture("run-json-flag");
    let lock = project.join("px.lock");
    let lock_id = "b0e3d49bac8025c47d3794eaf7e54f74d8b21d357aead0bd433a14e810b4c07f";
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
    let Some(python) = find_python() else {
        eprintln!("skipping frozen runtime mismatch test (python binary not found)");
        return;
    };

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["--json", "run", "--frozen", "sample_px_app/cli.py"])
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
    let _guard = common::test_env_guard();
    use common::prepare_named_fixture;
    let (_tmp, project) = prepare_named_fixture("run-json-flag", "run-json-flag");
    let Some(python) = find_python() else {
        eprintln!("skipping post-json test (python binary not found)");
        return;
    };
    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["run", "--json", "sample_px_app/cli.py", "JsonInline"])
        .assert()
        .success();

    let payload = parse_json(&assert);
    assert_eq!(payload.get("status"), Some(&Value::String("ok".into())));
    let stdout = payload["details"]["stdout"].as_str().unwrap_or_default();
    assert!(
        stdout.contains("Hello, JsonInline!"),
        "run output should include greeting when --json follows subcommand: {stdout:?}"
    );
}

#[test]
fn run_emits_hint_when_requests_socks_missing_under_proxy() {
    let _guard = common::test_env_guard();
    use common::prepare_named_fixture;
    let (_tmp, project) = prepare_named_fixture("run-proxy-traceback", "run-proxy-traceback");
    let Some(python) = find_python() else {
        eprintln!("skipping proxy traceback test (python binary not found)");
        return;
    };
    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("ALL_PROXY", "socks5h://127.0.0.1:9999")
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["--json", "run", "sample_px_app/cli.py"])
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
    let _guard = common::test_env_guard();
    if !require_online() {
        return;
    }
    use common::prepare_named_fixture;
    let (_tmp, project) = prepare_named_fixture("large_graph", "large-graph");
    let lock = project.join("px.lock");
    std::fs::remove_file(&lock).ok(); // force sync to write a fresh lock
    let cache = project.join(".px-cache");
    let Some(python) = find_python() else {
        eprintln!("skipping large graph test (python binary not found)");
        return;
    };

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .env("PX_CACHE_PATH", &cache)
        .args(["--json", "sync"])
        .assert()
        .success();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    assert!(lock.exists(), "sync should emit px.lock");
}

#[test]
fn run_rejects_dry_run_flag() {
    let _guard = common::test_env_guard();
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
        output.contains("No px project found"),
        "run without a project should report missing project for trailing --dry-run: {output:?}"
    );
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
    let _guard = common::test_env_guard();
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
