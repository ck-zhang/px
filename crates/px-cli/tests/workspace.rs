use std::fs;

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
        json["details"]["state"].as_str(),
        Some("needs-lock"),
        "empty workspace should report missing lock, not crash"
    );
}
