use assert_cmd::cargo::cargo_bin_cmd;
use serde_json::json;
use std::fs;
use std::process::Command;
use tempfile::tempdir;

mod common;

use common::{parse_json, require_online};

#[test]
fn tool_install_rejects_requirement_like_name() {
    let tools_dir = tempdir().expect("tools dir");
    let store_dir = tempdir().expect("tool store");

    let assert = cargo_bin_cmd!("px")
        .env("PX_TOOLS_DIR", tools_dir.path())
        .env("PX_TOOL_STORE", store_dir.path())
        .args(["--json", "tool", "install", "ruff==0.14.6"])
        .assert()
        .failure();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    let hint = payload["details"]["hint"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        hint.contains("px tool install ruff ruff==0.14.6"),
        "expected hint to suggest name + spec split, got {hint:?}"
    );
}

#[test]
fn tool_install_without_runtime_does_not_scaffold() {
    let tools_dir = tempdir().expect("tools dir");
    let store_dir = tempdir().expect("tool store");
    let registry = tempdir().expect("registry").path().join("runtimes.json");

    let assert = cargo_bin_cmd!("px")
        .env("PX_RUNTIME_REGISTRY", &registry)
        .env("PX_TOOLS_DIR", tools_dir.path())
        .env("PX_TOOL_STORE", store_dir.path())
        .args(["--json", "tool", "install", "ruff"])
        .assert()
        .failure();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    assert!(
        !tools_dir.path().join("ruff").exists(),
        "tool directory should not be created when runtime is missing"
    );
}

#[test]
fn tool_run_requires_install_and_guides_user() {
    let tools_dir = tempdir().expect("tools dir");
    let store_dir = tempdir().expect("tool store");

    let assert = cargo_bin_cmd!("px")
        .env("PX_TOOLS_DIR", tools_dir.path())
        .env("PX_TOOL_STORE", store_dir.path())
        .args(["--json", "tool", "run", "ruff", "--", "--version"])
        .assert()
        .failure();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    let message = payload["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("not installed"),
        "expected missing install error, got {message:?}"
    );
    let hint = payload["details"]["hint"].as_str().unwrap_or_default();
    assert!(
        hint.contains("px tool install ruff"),
        "expected hint to suggest install, got {hint:?}"
    );
}

#[test]
fn tool_run_reports_corrupted_metadata() {
    let tools_dir = tempdir().expect("tools dir");
    let store_dir = tempdir().expect("tool store");
    let tool_root = tools_dir.path().join("ruff");
    std::fs::create_dir_all(&tool_root).expect("tool root");
    std::fs::write(tool_root.join("tool.json"), b"{not-json").expect("write corrupt metadata");

    let assert = cargo_bin_cmd!("px")
        .env("PX_TOOLS_DIR", tools_dir.path())
        .env("PX_TOOL_STORE", store_dir.path())
        .env("PX_RUNTIME_PYTHON", "python3")
        .env("PX_RUNTIME_HOST_ONLY", "1")
        .args(["--json", "tool", "run", "ruff"])
        .assert()
        .failure();

    let payload = parse_json(&assert);
    let msg = payload["message"].as_str().unwrap_or_default().to_string();
    if msg.contains("runtime unavailable") {
        return;
    }
    assert_eq!(payload["status"], "user-error");
    let message = payload["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("not installed"),
        "expected missing install message even with corrupt metadata, got {message:?}"
    );
    let error_detail = payload["details"]["error"].as_str().unwrap_or_default();
    assert!(
        error_detail.contains("invalid tool metadata"),
        "expected metadata parsing error surfaced, got {error_detail:?}"
    );
}

#[test]
fn tool_run_requires_lock_and_env() {
    let tools_dir = tempdir().expect("tools dir");
    let store_dir = tempdir().expect("tool store");
    let tool_root = tools_dir.path().join("ruff");
    std::fs::create_dir_all(&tool_root).expect("tool root");

    write_minimal_tool_manifest(&tool_root, "ruff");
    write_tool_metadata(&tool_root, "ruff");

    let assert = cargo_bin_cmd!("px")
        .env("PX_TOOLS_DIR", tools_dir.path())
        .env("PX_TOOL_STORE", store_dir.path())
        .args(["--json", "tool", "run", "ruff"])
        .assert()
        .failure();

    let payload = parse_json(&assert);
    let message = payload["message"].as_str().unwrap_or_default().to_string();
    if message.contains("runtime unavailable") {
        return;
    }
    assert_eq!(payload["status"], "user-error");
    assert!(
        message.contains("not ready"),
        "expected missing lock error, got {message:?}"
    );
    let hint = payload["details"]["hint"].as_str().unwrap_or_default();
    assert!(
        hint.to_ascii_lowercase().contains("px tool install ruff"),
        "hint should recommend reinstalling the tool, got {hint:?}"
    );
    assert_eq!(
        payload["details"]["reason"], "missing_lock",
        "reason should surface missing lock state"
    );
}

#[test]
fn tool_run_executes_happy_path_environment() {
    let tools_dir = tempdir().expect("tools dir");
    let store_dir = tempdir().expect("tool store");
    let tool_root = tools_dir.path().join("ruff");
    let site_dir = tool_root.join(".px").join("site");
    std::fs::create_dir_all(&site_dir).expect("site dir");

    // Seed a minimal importable module the tool entry points to.
    let module_dir = site_dir.join("tool_pkg");
    std::fs::create_dir_all(&module_dir).expect("module dir");
    std::fs::write(
        module_dir.join("echo.py"),
        "def main():\n    print('TOOL_OK')\n    return 0\n\nif __name__ == '__main__':\n    raise SystemExit(main())\n",
    )
    .expect("write echo module");
    std::fs::write(module_dir.join("__init__.py"), b"").expect("init file");

    let info = system_python_info();
    let lock_id = "test-lock-hash";

    write_minimal_tool_manifest(&tool_root, "ruff");
    write_tool_lockfile(&tool_root, "ruff", lock_id, ">=3.8");
    write_tool_state(&tool_root, lock_id, &site_dir, &info);
    write_tool_metadata_with_entry(&tool_root, "ruff", "tool_pkg.echo", &info);

    let assert = cargo_bin_cmd!("px")
        .env("PX_TOOLS_DIR", tools_dir.path())
        .env("PX_TOOL_STORE", store_dir.path())
        .env("PX_RUNTIME_PYTHON", &info.executable)
        .env("PX_RUNTIME_HOST_ONLY", "1")
        .args(["--json", "tool", "run", "ruff", "--", "hello"])
        .assert();

    let output = assert.get_output();
    let stdout_text = String::from_utf8_lossy(&output.stdout);
    let stderr_text = String::from_utf8_lossy(&output.stderr);
    if output.status.code() != Some(0)
        && (stdout_text.contains("runtime unavailable")
            || stderr_text.contains("runtime unavailable"))
    {
        return;
    }
    assert!(
        output.status.success(),
        "tool run failed: {stdout_text:?} {stderr_text:?}"
    );
    let payload: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse tool run json");
    assert_eq!(payload["status"], "ok");
    let message = payload["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("TOOL_OK"),
        "message should surface tool output, got {message:?}"
    );
    assert_eq!(
        payload["details"]["entry"], "tool_pkg.echo",
        "entry should reflect seeded module"
    );
    let stdout = payload["details"]["stdout"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        stdout.contains("TOOL_OK"),
        "tool run should execute seeded module, got {stdout:?}"
    );
}

#[test]
fn tool_run_without_args_honors_json_flag() {
    let tools_dir = tempdir().expect("tools dir");
    let store_dir = tempdir().expect("tool store");
    let tool_root = tools_dir.path().join("ruff");
    let site_dir = tool_root.join(".px").join("site");
    std::fs::create_dir_all(&site_dir).expect("site dir");

    // Seed a minimal importable module the tool entry points to.
    let module_dir = site_dir.join("tool_pkg");
    std::fs::create_dir_all(&module_dir).expect("module dir");
    std::fs::write(
        module_dir.join("echo.py"),
        "def main():\n    print('TOOL_OK')\n    return 0\n\nif __name__ == '__main__':\n    raise SystemExit(main())\n",
    )
    .expect("write echo module");
    std::fs::write(module_dir.join("__init__.py"), b"").expect("init file");

    let info = system_python_info();
    let lock_id = "test-lock-hash";

    write_minimal_tool_manifest(&tool_root, "ruff");
    write_tool_lockfile(&tool_root, "ruff", lock_id, ">=3.8");
    write_tool_state(&tool_root, lock_id, &site_dir, &info);
    write_tool_metadata_with_entry(&tool_root, "ruff", "tool_pkg.echo", &info);

    let assert = cargo_bin_cmd!("px")
        .env("PX_TOOLS_DIR", tools_dir.path())
        .env("PX_TOOL_STORE", store_dir.path())
        .env("PX_RUNTIME_PYTHON", &info.executable)
        .env("PX_RUNTIME_HOST_ONLY", "1")
        .args(["--json", "tool", "run", "ruff"])
        .assert();

    let output = assert.get_output();
    let stdout_text = String::from_utf8_lossy(&output.stdout);
    let stderr_text = String::from_utf8_lossy(&output.stderr);
    if output.status.code() != Some(0)
        && (stdout_text.contains("runtime unavailable")
            || stderr_text.contains("runtime unavailable"))
    {
        return;
    }
    assert!(
        output.status.success(),
        "tool run failed: {stdout_text:?} {stderr_text:?}"
    );

    let payload: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse tool run json");
    assert_eq!(payload["status"], "ok");
    let stdout = payload["details"]["stdout"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        stdout.contains("TOOL_OK"),
        "tool run should capture tool output under --json, got {stdout:?}"
    );
}

#[test]
fn tool_install_then_run_is_immediately_ready() {
    if !require_online() {
        return;
    }
    let info = system_python_info();

    let registry_dir = tempdir().expect("runtime registry dir");
    let registry = registry_dir.path().join("runtimes.json");
    let cache_dir = tempdir().expect("cache dir");
    let store_dir = cache_dir.path().join("store");
    let envs_dir = cache_dir.path().join("envs");
    fs::create_dir_all(&store_dir).expect("create store dir");
    fs::create_dir_all(&envs_dir).expect("create envs dir");
    let tools_dir = tempdir().expect("tools dir");
    let tool_store = tempdir().expect("tool store dir");

    cargo_bin_cmd!("px")
        .env("PX_RUNTIME_REGISTRY", &registry)
        .env("PX_CACHE_PATH", cache_dir.path())
        .env("PX_STORE_PATH", &store_dir)
        .env("PX_ENVS_PATH", &envs_dir)
        .env("PX_TOOLS_DIR", tools_dir.path())
        .env("PX_TOOL_STORE", tool_store.path())
        .args([
            "python",
            "install",
            &info.channel,
            "--path",
            &info.executable,
            "--default",
        ])
        .assert()
        .success();

    cargo_bin_cmd!("px")
        .env("PX_RUNTIME_REGISTRY", &registry)
        .env("PX_CACHE_PATH", cache_dir.path())
        .env("PX_STORE_PATH", &store_dir)
        .env("PX_ENVS_PATH", &envs_dir)
        .env("PX_TOOLS_DIR", tools_dir.path())
        .env("PX_TOOL_STORE", tool_store.path())
        .env_remove("PX_NO_ENSUREPIP")
        .args(["tool", "install", "ruff"])
        .assert()
        .success();

    let lock_contents = fs::read_to_string(tools_dir.path().join("ruff").join("px.lock"))
        .expect("read tool lock");
    let expected = lock_contents
        .lines()
        .find_map(|line| line.trim().strip_prefix("specifier = \"ruff=="))
        .and_then(|suffix| suffix.split('"').next())
        .unwrap_or_default()
        .to_string();
    assert!(
        !expected.is_empty(),
        "expected ruff to be pinned in tool lock"
    );

    let home_dir = tempdir().expect("home dir");
    let assert = cargo_bin_cmd!("px")
        .env("PX_RUNTIME_REGISTRY", &registry)
        .env("PX_CACHE_PATH", cache_dir.path())
        .env("PX_STORE_PATH", &store_dir)
        .env("PX_ENVS_PATH", &envs_dir)
        .env("PX_TOOLS_DIR", tools_dir.path())
        .env("PX_TOOL_STORE", tool_store.path())
        .env_remove("PX_RUNTIME_PYTHON")
        .env_remove("PX_NO_ENSUREPIP")
        .env("HOME", home_dir.path())
        .args(["--json", "tool", "run", "ruff", "--", "--version"])
        .assert()
        .success();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    let message = payload["message"].as_str().unwrap_or_default();
    assert!(
        message.contains(&expected),
        "expected tool run to execute pinned ruff=={expected}, got {message:?}"
    );

    let state_path = tools_dir.path().join("ruff").join(".px").join("state.json");
    let state: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&state_path).expect("read tool state"))
            .expect("parse tool state");
    let env_state = state
        .get("current_env")
        .and_then(|value| value.as_object())
        .expect("current_env object");
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
    match env_state.get("env_path") {
        None | Some(serde_json::Value::Null) => {}
        Some(serde_json::Value::String(value)) => {
            assert!(
                !value.trim().is_empty(),
                "tool state must not persist an empty-string env_path"
            );
        }
        Some(other) => panic!("unexpected env_path value: {other:?}"),
    }
    let profile_oid = env_state
        .get("profile_oid")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    assert!(
        !profile_oid.trim().is_empty(),
        "tool state should include a non-empty profile_oid"
    );
}

fn write_minimal_tool_manifest(root: &std::path::Path, name: &str) {
    let pyproject = root.join("pyproject.toml");
    let contents = format!(
        r#"[project]
name = "{name}"
version = "0.0.0"
requires-python = ">=3.8"
dependencies = []

[tool.px]
"#
    );
    std::fs::write(pyproject, contents).expect("write pyproject");
}

fn write_tool_metadata(root: &std::path::Path, name: &str) {
    let info = system_python_info();
    let metadata = json!({
        "name": name,
        "spec": name,
        "entry": name,
        "console_scripts": {},
        "runtime_version": info.channel,
        "runtime_full_version": info.version,
        "runtime_path": info.executable,
        "installed_spec": name,
        "created_at": "1970-01-01T00:00:00Z",
        "updated_at": "1970-01-01T00:00:00Z"
    });
    std::fs::write(root.join("tool.json"), metadata.to_string()).expect("write metadata");
}

fn write_tool_metadata_with_entry(
    root: &std::path::Path,
    name: &str,
    entry: &str,
    info: &PythonInfo,
) {
    let metadata = json!({
        "name": name,
        "spec": name,
        "entry": entry,
        "console_scripts": {},
        "runtime_version": info.channel,
        "runtime_full_version": info.version,
        "runtime_path": info.executable,
        "installed_spec": name,
        "created_at": "1970-01-01T00:00:00Z",
        "updated_at": "1970-01-01T00:00:00Z"
    });
    std::fs::write(root.join("tool.json"), metadata.to_string()).expect("write metadata");
}

fn write_tool_lockfile(root: &std::path::Path, name: &str, lock_id: &str, python_req: &str) {
    let contents = format!(
        r#"version = 1

[project]
name = "{name}"

[python]
requirement = "{python_req}"

[metadata]
mode = "p0-pinned"
lock_id = "{lock_id}"
"#
    );
    std::fs::write(root.join("px.lock"), contents).expect("write lockfile");
}

fn write_tool_state(
    root: &std::path::Path,
    lock_hash: &str,
    site_dir: &std::path::Path,
    info: &PythonInfo,
) {
    let state = json!({
        "current_env": {
            "id": "tool-env",
            "lock_hash": lock_hash,
            "platform": info.platform.clone(),
            "site_packages": site_dir.display().to_string(),
            "python": {
                "path": info.executable,
                "version": info.version
            }
        }
    });
    let px_dir = root.join(".px");
    std::fs::create_dir_all(&px_dir).expect("px dir");
    std::fs::write(px_dir.join("state.json"), state.to_string()).expect("write state");
}

struct PythonInfo {
    channel: String,
    version: String,
    executable: String,
    platform: String,
}

fn system_python_info() -> PythonInfo {
    let output = Command::new("python3")
        .arg("-c")
        .arg("import json,platform,sys,sysconfig;print(json.dumps({'v':platform.python_version(),'exe':sys.executable,'plat':sysconfig.get_platform().replace('-', '_')}))")
        .output()
        .expect("inspect python");
    let parsed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse python inspection");
    let version = parsed["v"].as_str().unwrap_or("3.11.0").to_string();
    let executable = parsed["exe"].as_str().unwrap_or("python3").to_string();
    let platform = parsed["plat"].as_str().unwrap_or("any").to_string();
    let mut parts = version.split('.').take(2).collect::<Vec<_>>();
    if parts.len() < 2 {
        parts = vec![&version, "0"];
    }
    let channel = format!("{}.{}", parts[0], parts[1]);
    PythonInfo {
        channel,
        version,
        executable,
        platform,
    }
}
