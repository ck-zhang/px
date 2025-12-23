use tempfile::TempDir;

use assert_cmd::cargo::cargo_bin_cmd;

mod common;

use common::{require_online, test_env_guard};

fn px_cmd() -> assert_cmd::Command {
    common::ensure_test_store_env();
    cargo_bin_cmd!("px")
}

fn scaffold_demo(temp: &TempDir, package: &str) {
    px_cmd()
        .current_dir(temp.path())
        .args(["init", "--package", package])
        .assert()
        .success();
}

#[test]
fn project_add_dry_run_prints_change_preview() {
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    scaffold_demo(&temp, "demo_add_dry_run_preview");
    let project_dir = temp.path();

    let assert = px_cmd()
        .current_dir(project_dir)
        .args(["add", "requests==2.32.3", "--dry-run"])
        .assert()
        .success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("stdout utf8");
    assert!(
        stdout.contains("px add: pyproject.toml: + requests==2.32.3"),
        "expected pyproject diff preview, got {stdout:?}"
    );
    assert!(
        stdout.contains("px add: px.lock:") && stdout.contains("would"),
        "expected lock preview line, got {stdout:?}"
    );
    assert!(
        stdout.contains("px add: env: would rebuild"),
        "expected env preview line, got {stdout:?}"
    );
    assert!(
        stdout.contains("px add: tools: would rebuild"),
        "expected tools preview line, got {stdout:?}"
    );
}

#[test]
fn project_update_dry_run_prints_change_preview() {
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    scaffold_demo(&temp, "demo_update_dry_run_preview");
    let project_dir = temp.path();

    px_cmd()
        .current_dir(project_dir)
        .args(["add", "requests==2.32.3"])
        .assert()
        .success();

    let assert = px_cmd()
        .current_dir(project_dir)
        .args(["update", "--dry-run"])
        .assert()
        .success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("stdout utf8");
    assert!(
        stdout.contains("px update: px.lock:"),
        "expected lock preview line, got {stdout:?}"
    );
    assert!(
        stdout.contains("px update: env: would rebuild"),
        "expected env preview line, got {stdout:?}"
    );
    assert!(
        stdout.contains("px update: tools: would rebuild"),
        "expected tools preview line, got {stdout:?}"
    );
}
