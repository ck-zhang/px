use std::{fs, process::Command};

use assert_cmd::cargo::cargo_bin_cmd;
use serde_json::Value;
use toml_edit::{DocumentMut, Item};

mod common;

use common::{
    detect_host_python, fake_sandbox_backend, find_python, parse_json, prepare_named_fixture,
    require_online, test_env_guard,
};

#[test]
fn workspace_sync_writes_workspace_metadata() {
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
    let (_temp, root) = prepare_named_fixture("workspace_basic", "workspace_meta");

    cargo_bin_cmd!("px")
        .current_dir(&root)
        .args(["sync", "--json"])
        .assert()
        .success();

    let lock_path = root.join("px.workspace.lock");
    assert!(lock_path.exists(), "workspace lockfile should be created");
    let doc: DocumentMut = fs::read_to_string(&lock_path)
        .expect("read lock")
        .parse()
        .expect("parse lock");
    let workspace = doc["workspace"]
        .as_table()
        .expect("workspace table in lock");
    let members = workspace
        .get("members")
        .and_then(Item::as_array_of_tables)
        .expect("workspace members");
    assert_eq!(members.len(), 2, "expected two workspace members recorded");
    let paths: Vec<_> = members
        .iter()
        .filter_map(|table| table.get("path").and_then(Item::as_str))
        .collect();
    assert!(
        paths.contains(&"apps/a") && paths.contains(&"libs/b"),
        "workspace members paths should be recorded"
    );
}

#[test]
fn workspace_sync_makes_status_env_clean() {
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
    let (_temp, root) = prepare_named_fixture("workspace_basic", "workspace_sync_status_clean");

    cargo_bin_cmd!("px")
        .current_dir(&root)
        .arg("sync")
        .assert()
        .success();

    let status = cargo_bin_cmd!("px")
        .current_dir(&root)
        .args(["status", "--json"])
        .assert()
        .success();
    let payload = parse_json(&status);

    assert_eq!(payload["workspace"]["env_clean"], Value::Bool(true));
    assert_ne!(
        payload["workspace"]["state"],
        Value::String("WNeedsEnv".into())
    );
    assert_eq!(payload["env"]["status"], Value::String("clean".into()));
}

#[test]
#[cfg(not(windows))]
fn workspace_status_does_not_depend_on_path_python() {
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
    let Some(python) = find_python() else {
        eprintln!("skipping workspace status runtime test (python not found)");
        return;
    };
    let Some((python_exe, channel)) = detect_host_python(&python) else {
        eprintln!("skipping workspace status runtime test (unable to inspect python)");
        return;
    };
    let (_temp, root) = prepare_named_fixture("workspace_basic", "workspace_status_path_runtime");

    let registry_dir = tempfile::tempdir().expect("registry dir");
    let registry = registry_dir.path().join("runtimes.json");
    cargo_bin_cmd!("px")
        .current_dir(&root)
        .env("PX_RUNTIME_REGISTRY", &registry)
        .args([
            "python",
            "install",
            &channel,
            "--path",
            &python_exe,
            "--default",
        ])
        .assert()
        .success();

    cargo_bin_cmd!("px")
        .current_dir(&root)
        .env("PX_RUNTIME_REGISTRY", &registry)
        .env_remove("PX_RUNTIME_PYTHON")
        .arg("sync")
        .assert()
        .success();

    // Poison PATH python to ensure status uses the selected px runtime, not PATH discovery.
    let bin_dir = tempfile::tempdir().expect("bin dir");
    let fake_python = bin_dir.path().join("python3");
    std::fs::write(
        &fake_python,
        r#"#!/bin/sh
if [ "$1" = "-c" ]; then
  script="$2"
  case "$script" in
    *platform.python_version* )
      echo '{"version":"0.0.0"}'
      exit 0
      ;;
  esac
  case "$script" in
    *sysconfig.get_platform* )
      echo '{"python":["cp00"],"abi":["none"],"platform":["any"],"tags":[]}'
      exit 0
      ;;
  esac
  echo '{}'
  exit 0
fi
echo 'Python 0.0.0' >&2
exit 0
"#,
    )
    .expect("write fake python");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&fake_python)
            .expect("fake python metadata")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_python, perms).expect("set fake python perms");
    }

    let path = std::env::var("PATH").unwrap_or_default();
    let poisoned_path = format!("{}:{}", bin_dir.path().display(), path);

    let status = cargo_bin_cmd!("px")
        .current_dir(&root)
        .env("PX_RUNTIME_REGISTRY", &registry)
        .env_remove("PX_RUNTIME_PYTHON")
        .env("PATH", poisoned_path)
        .args(["status", "--json"])
        .assert()
        .success();
    let payload = parse_json(&status);

    assert_eq!(payload["workspace"]["env_clean"], serde_json::Value::Bool(true));
    assert_eq!(payload["workspace"]["env_issue"], serde_json::Value::Null);
}

#[test]
fn workspace_add_rolls_back_on_failure() {
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
    let (_temp, root) = prepare_named_fixture("workspace_basic", "workspace_add_fail");
    let member = root.join("apps/a");

    cargo_bin_cmd!("px")
        .current_dir(&member)
        .args(["add", "bogus-package-px-does-not-exist==1.0.0"])
        .assert()
        .failure();

    let manifest = member.join("pyproject.toml");
    let doc: DocumentMut = fs::read_to_string(&manifest)
        .expect("read manifest")
        .parse()
        .expect("parse manifest");
    let deps = doc["project"]["dependencies"]
        .as_array()
        .expect("dependencies array");
    assert!(
        deps.is_empty(),
        "failed add must not persist dependency edits"
    );
    assert!(
        !root.join("px.workspace.lock").exists(),
        "lockfile should not be created on failed add"
    );
}

#[test]
fn workspace_status_handles_empty_members() {
    let _guard = test_env_guard();
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let manifest = r#"[project]
name = "empty-ws"
version = "0.0.0"
requires-python = ">=3.11"

[tool.px.workspace]
members = []
"#;
    fs::write(root.join("pyproject.toml"), manifest).expect("write pyproject");

    let assert = cargo_bin_cmd!("px")
        .current_dir(root)
        .args(["status", "--json"])
        .assert()
        .failure();
    let json = parse_json(&assert);
    assert_eq!(
        json["workspace"]["state"].as_str(),
        Some("WNeedsLock"),
        "empty workspace should report missing lock, not crash"
    );
}

#[test]
fn workspace_test_routes_through_workspace_state() {
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let workspace_manifest = r#"[project]
name = "ws"
version = "0.0.0"
requires-python = ">=3.11"

[tool.px.workspace]
members = ["apps/app"]
"#;
    fs::write(root.join("pyproject.toml"), workspace_manifest).expect("write workspace manifest");
    let app_root = root.join("apps").join("app");
    fs::create_dir_all(&app_root).expect("create app root");
    fs::write(
        app_root.join("pyproject.toml"),
        r#"[project]
name = "member"
version = "0.1.0"
requires-python = ">=3.11"
dependencies = []

[tool.px]
"#,
    )
    .expect("write member manifest");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&app_root)
        .args(["--json", "test"])
        .assert()
        .failure();
    let payload = parse_json(&assert);
    assert_eq!(payload["details"]["reason"], "missing_lock");
    let hint = payload["details"]["hint"].as_str().unwrap_or_default();
    assert!(
        hint.contains("px test"),
        "workspace missing lock hint should mention the invoking command: {hint:?}"
    );
    let lockfile = payload["details"]["lockfile"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        lockfile.contains("px.workspace.lock"),
        "error should point at workspace lockfile, got {lockfile:?}"
    );
}

#[test]
fn workspace_run_at_ref_uses_workspace_identity() {
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
    let (_temp, root) = prepare_named_fixture("workspace_basic", "workspace_run_at_ref");

    let member = root.join("apps/a");
    let workspace_py = member.join("app.py");
    fs::write(&workspace_py, "import b\nprint('A sees', b.VALUE)\n")
        .expect("write workspace script");
    let lib_pkg = root.join("libs/b/src/b");
    fs::create_dir_all(&lib_pkg).expect("lib package dirs");
    fs::write(lib_pkg.join("__init__.py"), "VALUE = 'ok'\n").expect("write lib package");

    cargo_bin_cmd!("px")
        .current_dir(&root)
        .arg("sync")
        .assert()
        .success();

    assert!(
        Command::new("git")
            .arg("init")
            .current_dir(&root)
            .status()
            .expect("git init")
            .success(),
        "git init should succeed"
    );
    assert!(
        Command::new("git")
            .args(["add", "."])
            .current_dir(&root)
            .status()
            .expect("git add")
            .success(),
        "git add should succeed"
    );
    assert!(
        Command::new("git")
            .args([
                "-c",
                "user.name=px-tests",
                "-c",
                "user.email=px-tests@example.com",
                "commit",
                "-m",
                "initial workspace"
            ])
            .current_dir(&root)
            .status()
            .expect("git commit")
            .success(),
        "git commit should succeed"
    );

    let assert = cargo_bin_cmd!("px")
        .current_dir(&member)
        .args(["run", "--at", "HEAD", "app.py"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    assert!(
        stdout.contains("A sees ok"),
        "px run --at should use workspace lock/env without drift: {stdout}"
    );
}

#[test]
#[cfg(not(windows))]
fn sandbox_workspace_requires_consistent_env() {
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
    let Some(python) = find_python() else {
        eprintln!("skipping workspace sandbox env repair test (python not found)");
        return;
    };
    let (tmp, root) = prepare_named_fixture("workspace_basic", "workspace_sandbox_env");
    let (backend, log) = fake_sandbox_backend(tmp.path()).expect("backend script");
    let store_dir = common::sandbox_store_dir("sandbox-store");
    let store = store_dir.path().to_path_buf();
    let member = root.join("apps").join("a");
    fs::create_dir_all(&member).expect("create member root");
    fs::write(
        member.join("pyproject.toml"),
        r#"[project]
name = "member"
version = "0.1.0"
requires-python = ">=3.11"
dependencies = []

[tool.px]
"#,
    )
    .expect("write member manifest");
    let tests_dir = member.join("tests");
    fs::create_dir_all(&tests_dir).expect("create tests dir");
    fs::write(
        tests_dir.join("runtests.py"),
        "print('sandbox workspace tests'); import sys; sys.exit(0)\n",
    )
    .expect("write runtests script");
    let lib_root = root.join("libs").join("b");
    fs::create_dir_all(lib_root.join("src").join("b")).expect("create lib package");
    fs::write(
        lib_root.join("pyproject.toml"),
        r#"[project]
name = "lib-b"
version = "0.1.0"
requires-python = ">=3.11"
dependencies = []

[tool.px]
"#,
    )
    .expect("write lib manifest");

    cargo_bin_cmd!("px")
        .current_dir(&root)
        .args(["sync"])
        .assert()
        .success();

    fs::remove_dir_all(root.join(".px")).ok();

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
    assert!(
        root.join(".px").exists(),
        "sandbox workspace run should recreate workspace environment"
    );
}
