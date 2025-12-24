use std::fs;
use std::process::Command;

use assert_cmd::cargo::cargo_bin_cmd;
use serde_json::Value;

mod common;

use common::{
    fake_sandbox_backend, find_python, parse_json, prepare_fixture, prepare_traceback_fixture,
    require_online, test_env_guard,
};

#[test]
fn fmt_bypasses_project_lock_env_gating() {
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
    let (_tmp, project) = prepare_fixture("fmt-bypass");
    let cache = project.join(".px-cache");
    let store = cache.join("store");
    let envs = cache.join("envs");
    let tools = cache.join("tools");
    fs::create_dir_all(&envs).expect("create envs dir");
    fs::create_dir_all(&tools).expect("create tools dir");
    let Some(python) = find_python() else {
        eprintln!("skipping fmt bypass test (python binary not found)");
        return;
    };
    let Some((python_path, channel)) = detect_host_python(&python) else {
        eprintln!("skipping fmt bypass test (unable to inspect python interpreter)");
        return;
    };
    let lock = project.join("px.lock");
    fs::remove_file(&lock).expect("remove px.lock");
    fs::remove_dir_all(project.join(".px")).ok();
    let pyproject = project.join("pyproject.toml");
    let mut pyproject_contents = fs::read_to_string(&pyproject).expect("read pyproject");
    pyproject_contents.push_str(
        r#"
[tool.px.fmt]
module = "black"
args = ["sample_px_app"]
"#,
    );
    fs::write(&pyproject, pyproject_contents).expect("write pyproject");

    // Tools require a px-registered runtime (not just PX_RUNTIME_PYTHON). Register the
    // host interpreter into the per-test runtime registry so tool installation is hermetic.
    let registry = cache.join("runtimes.json");
    cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_REGISTRY", &registry)
        .args(["python", "install", &channel, "--path", &python_path])
        .assert()
        .success();

    cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .env("PX_RUNTIME_REGISTRY", &registry)
        .env("PX_CACHE_PATH", &cache)
        .env("PX_STORE_PATH", &store)
        .env("PX_ENVS_PATH", &envs)
        .env("PX_TOOLS_DIR", &tools)
        .env("PX_RUNTIME_HOST_ONLY", "1")
        .env("PX_NO_ENSUREPIP", "1")
        .env("PX_SYSTEM_DEPS_MODE", "offline")
        .args(["tool", "install", "black", "black==25.12.0"])
        .assert()
        .success();

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .env("PX_RUNTIME_REGISTRY", &registry)
        .env("PX_CACHE_PATH", &cache)
        .env("PX_STORE_PATH", &store)
        .env("PX_ENVS_PATH", &envs)
        .env("PX_TOOLS_DIR", &tools)
        .env("PX_RUNTIME_HOST_ONLY", "1")
        .env("PX_NO_ENSUREPIP", "1")
        .env("PX_SYSTEM_DEPS_MODE", "offline")
        .args(["--json", "fmt"])
        .assert()
        .success();

    let payload = parse_json(&assert);
    assert_eq!(
        payload["status"], "ok",
        "fmt should succeed without px.lock"
    );
}

const INSPECT_SCRIPT: &str =
    "import json, platform, sys; print(json.dumps({'version': platform.python_version(), 'executable': sys.executable}))";

fn detect_host_python(python: &str) -> Option<(String, String)> {
    let output = Command::new(python)
        .arg("-c")
        .arg(INSPECT_SCRIPT)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let payload: Value = serde_json::from_slice(&output.stdout).ok()?;
    let executable = payload.get("executable")?.as_str()?.to_string();
    let version = payload.get("version")?.as_str()?.to_string();
    let parts: Vec<_> = version.split('.').collect();
    let major = parts.first().copied().unwrap_or("0");
    let minor = parts.get(1).copied().unwrap_or("0");
    let channel = format!("{major}.{minor}");
    Some((executable, channel))
}

#[test]
fn frozen_test_refuses_autosync_for_missing_env() {
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
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
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
    let (_tmp, project) = prepare_fixture("dev-missing-env");
    fs::remove_dir_all(project.join(".px")).ok();
    // Create a simple, guaranteed-core pytest to avoid environment-specific stdlib modules.
    let tests_dir = project.join("tests");
    fs::create_dir_all(&tests_dir).expect("create tests dir");
    fs::write(
        tests_dir.join("test_smoke.py"),
        "def test_smoke():\n    import json\n    assert json.loads('123') == 123\n",
    )
    .expect("write smoke test");
    let Some(python) = find_python() else {
        eprintln!("skipping dev env repair test (python binary not found)");
        return;
    };

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
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

    // Verify the repaired environment is usable by running a guaranteed-core module.
    let state_path = project.join(".px").join("state.json");
    let state_contents = fs::read_to_string(&state_path).expect("read state.json");
    let state: Value = serde_json::from_str(&state_contents).expect("parse state.json");
    let python = state["current_env"]["python"]["path"]
        .as_str()
        .expect("state python path");
    let output = std::process::Command::new(python)
        .arg("-c")
        .arg("import json; print(123)")
        .output()
        .expect("run env python");
    assert!(
        output.status.success(),
        "env python should run successfully: status {:?}, stdout {}, stderr {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "123",
        "env python should execute core import correctly"
    );
}

#[test]
fn run_recreates_project_state_after_deletion() {
    let _guard = test_env_guard();
    let (_tmp, project) = prepare_fixture("run-recreate-state");
    let Some(python) = find_python() else {
        eprintln!("skipping state recreate test (python binary not found)");
        return;
    };

    cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["run", "python", "-c", "print('FIRST')"])
        .assert()
        .success();

    let state_path = project.join(".px").join("state.json");
    assert!(state_path.exists(), "expected initial state.json to exist");

    fs::remove_dir_all(project.join(".px")).ok();

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["--json", "status"])
        .assert()
        .failure();
    let payload = parse_json(&assert);
    assert_eq!(
        payload["project"]["state"], "NeedsEnv",
        "expected status to report missing env after deleting .px"
    );

    cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["run", "python", "-c", "print('SECOND')"])
        .assert()
        .success();
    assert!(
        state_path.exists(),
        "px run should recreate .px/state.json after deletion"
    );

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["--json", "status"])
        .assert()
        .success();
    let payload = parse_json(&assert);
    let state = payload["project"]["state"].as_str().unwrap_or_default();
    assert!(
        state == "Consistent" || state == "InitializedEmpty",
        "expected status to be healthy after self-heal, got {state:?}"
    );
    assert_eq!(payload["env"]["status"], "clean");
}

#[cfg(not(windows))]
#[test]
fn sandbox_run_requires_consistent_env() {
    let _guard = test_env_guard();
    let Some(python) = find_python() else {
        eprintln!("skipping sandbox env repair test (python not found)");
        return;
    };
    let (tmp, project) = prepare_traceback_fixture("sandbox-run-env");
    let store_dir = common::sandbox_store_dir("sandbox-store");
    let store = store_dir.path().to_path_buf();
    let (backend, log) = fake_sandbox_backend(tmp.path()).expect("sandbox backend");
    fs::remove_dir_all(project.join(".px")).ok();

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_SANDBOX_BACKEND", &backend)
        .env("PX_SANDBOX_STORE", &store)
        .env("PX_FAKE_SANDBOX_LOG", &log)
        .env("PX_FAKE_SANDBOX_INSPECT_EXIT", "1")
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["--json", "run", "--sandbox", "python", "-c", "print('hi')"])
        .assert()
        .success();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    assert!(
        project.join(".px").exists(),
        "sandbox should have bootstrapped environment"
    );
}

#[test]
fn test_refuses_lock_bootstrap_without_tty() {
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
    let (_tmp, project) = prepare_fixture("test-missing-lock");
    let lock = project.join("px.lock");
    if lock.exists() {
        fs::remove_file(&lock).expect("remove px.lock");
    }
    fs::remove_dir_all(project.join(".px")).ok();
    let Some(python) = find_python() else {
        eprintln!("skipping lock bootstrap test (python binary not found)");
        return;
    };

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .env("PX_TEST_FALLBACK_STD", "1")
        .args(["--json", "test"])
        .assert()
        .failure();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    assert_eq!(payload["details"]["reason"], "missing_lock");
    assert!(
        payload["details"]["hint"]
            .as_str()
            .unwrap_or_default()
            .to_ascii_lowercase()
            .contains("px sync"),
        "expected hint to recommend px sync, got {:?}",
        payload["details"]["hint"]
    );
}

#[test]
fn run_refuses_lock_bootstrap_without_tty() {
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
    let (_tmp, project) = prepare_fixture("run-missing-lock");
    let lock = project.join("px.lock");
    if lock.exists() {
        fs::remove_file(&lock).expect("remove px.lock");
    }
    fs::remove_dir_all(project.join(".px")).ok();
    let Some(python) = find_python() else {
        eprintln!("skipping run autosync test (python binary not found)");
        return;
    };

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["--json", "run", "python", "--", "-c", "print('ok')"])
        .assert()
        .failure();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    assert_eq!(payload["details"]["reason"], "missing_lock");
    assert!(
        payload["details"]["hint"]
            .as_str()
            .unwrap_or_default()
            .to_ascii_lowercase()
            .contains("px sync"),
        "expected hint to recommend px sync, got {:?}",
        payload["details"]["hint"]
    );
}

#[cfg(unix)]
#[test]
fn test_bootstraps_lock_before_running_tests_under_tty() {
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
    let (_tmp, project) = prepare_fixture("test-missing-lock-tty");
    let lock = project.join("px.lock");
    if lock.exists() {
        fs::remove_file(&lock).expect("remove px.lock");
    }
    fs::remove_dir_all(project.join(".px")).ok();
    let Some(python) = find_python() else {
        eprintln!("skipping test tty autosync test (python binary not found)");
        return;
    };

    let script_ok = Command::new("script")
        .args(["-q", "-e", "-c", "true", "/dev/null"])
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if !script_ok {
        eprintln!("skipping test tty autosync test (script command unavailable)");
        return;
    }

    let px = assert_cmd::cargo::cargo_bin!("px");
    let command = format!("{} --json test", px.display());
    let output = Command::new("script")
        .args(["-q", "-e", "-c", &command, "/dev/null"])
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .env("PX_TEST_FALLBACK_STD", "1")
        .output()
        .expect("spawn script");

    assert!(
        output.status.success(),
        "tty autosync test should succeed, stdout: {:?}, stderr: {:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let payload: Value = serde_json::from_slice(&output.stdout).expect("valid json");
    assert_eq!(payload["status"], Value::String("ok".into()));
    let autosync = payload["details"]
        .get("autosync")
        .and_then(serde_json::Value::as_object)
        .expect("autosync details present");
    assert_eq!(
        autosync.get("action").and_then(serde_json::Value::as_str),
        Some("lock-bootstrap"),
        "expected missing lock to trigger bootstrap"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("px.lock missing; syncing"),
        "expected lock bootstrap message on stderr, got {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(lock.exists(), "px.lock should be created during autosync");
}

#[cfg(unix)]
#[test]
fn test_autosyncs_manifest_drift_under_tty() {
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
    let (_tmp, project) = prepare_fixture("test-drift-tty");
    let Some(python) = find_python() else {
        eprintln!("skipping test tty drift autosync test (python binary not found)");
        return;
    };

    let script_ok = Command::new("script")
        .args(["-q", "-e", "-c", "true", "/dev/null"])
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if !script_ok {
        eprintln!("skipping test tty drift autosync test (script command unavailable)");
        return;
    }

    cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["sync"])
        .assert()
        .success();

    let pyproject = project.join("pyproject.toml");
    let mut doc: toml_edit::DocumentMut = fs::read_to_string(&pyproject)
        .expect("read pyproject")
        .parse()
        .expect("parse pyproject");
    doc["project"]["dependencies"]
        .as_array_mut()
        .expect("dependencies array")
        .push("requests==2.32.3");
    fs::write(&pyproject, doc.to_string()).expect("write pyproject");

    let px = assert_cmd::cargo::cargo_bin!("px");
    let command = format!("{} --json test", px.display());
    let output = Command::new("script")
        .args(["-q", "-e", "-c", &command, "/dev/null"])
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .env("PX_TEST_FALLBACK_STD", "1")
        .output()
        .expect("spawn script");

    assert!(
        output.status.success(),
        "tty autosync test should succeed, stdout: {:?}, stderr: {:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let payload: Value = serde_json::from_slice(&output.stdout).expect("valid json");
    assert_eq!(payload["status"], Value::String("ok".into()));
    let autosync = payload["details"]
        .get("autosync")
        .and_then(serde_json::Value::as_object)
        .expect("autosync details present");
    assert_eq!(
        autosync.get("action").and_then(serde_json::Value::as_str),
        Some("lock-sync"),
        "expected manifest drift to trigger lock-sync autosync"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Manifest changed; syncing"),
        "expected drift message on stderr, got {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn run_bootstraps_lock_before_execution_under_tty() {
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
    let (_tmp, project) = prepare_fixture("run-missing-lock-tty");
    let lock = project.join("px.lock");
    if lock.exists() {
        fs::remove_file(&lock).expect("remove px.lock");
    }
    fs::remove_dir_all(project.join(".px")).ok();
    let Some(python) = find_python() else {
        eprintln!("skipping run tty autosync test (python binary not found)");
        return;
    };

    let script_ok = Command::new("script")
        .args(["-q", "-e", "-c", "true", "/dev/null"])
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if !script_ok {
        eprintln!("skipping run tty autosync test (script command unavailable)");
        return;
    }

    let px = assert_cmd::cargo::cargo_bin!("px");
    let command = format!("{} --json run python -- -c \"print('ok')\"", px.display());
    let output = Command::new("script")
        .args(["-q", "-e", "-c", &command, "/dev/null"])
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .output()
        .expect("spawn script");

    assert!(
        output.status.success(),
        "tty autosync run should succeed, stdout: {:?}, stderr: {:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let payload: Value = serde_json::from_slice(&output.stdout).expect("valid json");
    assert_eq!(payload["status"], Value::String("ok".into()));
    let autosync = payload["details"]
        .get("autosync")
        .and_then(serde_json::Value::as_object)
        .expect("autosync details present");
    assert_eq!(
        autosync.get("action").and_then(serde_json::Value::as_str),
        Some("lock-bootstrap"),
        "expected missing lock to trigger bootstrap"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("px.lock missing; syncing"),
        "expected lock bootstrap message on stderr, got {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(lock.exists(), "px.lock should be created during autosync");
}

#[cfg(unix)]
#[test]
fn run_autosyncs_manifest_drift_under_tty() {
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
    let (_tmp, project) = prepare_fixture("run-drift-tty");
    let Some(python) = find_python() else {
        eprintln!("skipping run tty drift autosync test (python binary not found)");
        return;
    };

    let script_ok = Command::new("script")
        .args(["-q", "-e", "-c", "true", "/dev/null"])
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if !script_ok {
        eprintln!("skipping run tty drift autosync test (script command unavailable)");
        return;
    }

    cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["sync"])
        .assert()
        .success();

    let pyproject = project.join("pyproject.toml");
    let mut doc: toml_edit::DocumentMut = fs::read_to_string(&pyproject)
        .expect("read pyproject")
        .parse()
        .expect("parse pyproject");
    doc["project"]["dependencies"]
        .as_array_mut()
        .expect("dependencies array")
        .push("requests==2.32.3");
    fs::write(&pyproject, doc.to_string()).expect("write pyproject");

    let px = assert_cmd::cargo::cargo_bin!("px");
    let command = format!("{} --json run python -- -c \"print('ok')\"", px.display());
    let output = Command::new("script")
        .args(["-q", "-e", "-c", &command, "/dev/null"])
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .output()
        .expect("spawn script");

    assert!(
        output.status.success(),
        "tty autosync run should succeed, stdout: {:?}, stderr: {:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let payload: Value = serde_json::from_slice(&output.stdout).expect("valid json");
    assert_eq!(payload["status"], Value::String("ok".into()));
    let autosync = payload["details"]
        .get("autosync")
        .and_then(serde_json::Value::as_object)
        .expect("autosync details present");
    assert_eq!(
        autosync.get("action").and_then(serde_json::Value::as_str),
        Some("lock-sync"),
        "expected manifest drift to trigger lock-sync autosync"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Manifest changed; syncing"),
        "expected drift message on stderr, got {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn run_refuses_manifest_drift_without_tty() {
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
    let (_tmp, project) = prepare_fixture("run-drift-non-tty");
    let Some(python) = find_python() else {
        eprintln!("skipping run drift test (python binary not found)");
        return;
    };

    cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["sync"])
        .assert()
        .success();

    let pyproject = project.join("pyproject.toml");
    let mut doc: toml_edit::DocumentMut = fs::read_to_string(&pyproject)
        .expect("read pyproject")
        .parse()
        .expect("parse pyproject");
    doc["project"]["dependencies"]
        .as_array_mut()
        .expect("dependencies array")
        .push("requests==2.32.3");
    fs::write(&pyproject, doc.to_string()).expect("write pyproject");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["--json", "run", "python", "--", "-c", "print('ok')"])
        .assert()
        .failure();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    assert_eq!(payload["details"]["reason"], "lock_drift");
    assert!(
        payload["details"]["hint"]
            .as_str()
            .unwrap_or_default()
            .to_ascii_lowercase()
            .contains("px sync"),
        "expected hint to recommend px sync, got {:?}",
        payload["details"]["hint"]
    );
}

#[cfg(unix)]
#[test]
fn run_refuses_manifest_drift_in_ci_even_under_tty() {
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
    let (_tmp, project) = prepare_fixture("run-drift-ci-tty");
    let Some(python) = find_python() else {
        eprintln!("skipping run drift ci test (python binary not found)");
        return;
    };

    let script_ok = Command::new("script")
        .args(["-q", "-e", "-c", "true", "/dev/null"])
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if !script_ok {
        eprintln!("skipping run drift ci test (script command unavailable)");
        return;
    }

    cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["sync"])
        .assert()
        .success();

    let pyproject = project.join("pyproject.toml");
    let mut doc: toml_edit::DocumentMut = fs::read_to_string(&pyproject)
        .expect("read pyproject")
        .parse()
        .expect("parse pyproject");
    doc["project"]["dependencies"]
        .as_array_mut()
        .expect("dependencies array")
        .push("requests==2.32.3");
    fs::write(&pyproject, doc.to_string()).expect("write pyproject");

    let px = assert_cmd::cargo::cargo_bin!("px");
    let command = format!("{} --json run python -- -c \"print('ok')\"", px.display());
    let output = Command::new("script")
        .args(["-q", "-e", "-c", &command, "/dev/null"])
        .current_dir(&project)
        .env("CI", "1")
        .env("PX_RUNTIME_PYTHON", &python)
        .output()
        .expect("spawn script");

    assert!(
        !output.status.success(),
        "ci run should refuse drift, stdout: {:?}, stderr: {:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let payload: Value = serde_json::from_slice(&output.stdout).expect("valid json");
    assert_eq!(payload["status"], Value::String("user-error".into()));
    assert_eq!(payload["details"]["reason"], Value::String("lock_drift".into()));
}

#[test]
fn frozen_sync_reports_lock_drift() {
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
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
