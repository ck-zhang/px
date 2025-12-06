use std::{fs, process::Command};

use assert_cmd::cargo::cargo_bin_cmd;
use toml_edit::{DocumentMut, Item};

mod common;

use common::{parse_json, prepare_named_fixture};

#[test]
fn workspace_sync_writes_workspace_metadata() {
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
fn workspace_add_rolls_back_on_failure() {
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
    let _guard = common::test_env_guard();
    let (_temp, root) = prepare_named_fixture("workspace_basic", "workspace_run_at_ref");

    let member = root.join("apps/a");
    let workspace_py = member.join("app.py");
    fs::write(
        &workspace_py,
        "import b\nprint('A sees', b.VALUE)\n",
    )
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
