use std::fs::{self, OpenOptions};

use assert_cmd::cargo::cargo_bin_cmd;
use fs4::FileExt;

mod common;

use common::{parse_json, prepare_fixture, test_env_guard};

#[test]
fn add_fails_fast_when_project_lock_held() {
    let _guard = test_env_guard();
    let (_tmp, root) = prepare_fixture("project-lock-held");

    let pyproject_path = root.join("pyproject.toml");
    let lock_path = root.join("px.lock");
    let pyproject_before = fs::read_to_string(&pyproject_path).expect("read pyproject");
    let lock_before = fs::read_to_string(&lock_path).expect("read lock");

    fs::create_dir_all(root.join(".px")).expect("create .px");
    let lock_file_path = root.join(".px").join("project.lock");
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_file_path)
        .expect("open project lock");
    lock_file.lock_exclusive().expect("lock project");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&root)
        .args(["--json", "add", "--dry-run", "requests"])
        .assert()
        .failure();
    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    assert_eq!(payload["details"]["reason"], "project_locked");
    let hint = payload["details"]["hint"].as_str().unwrap_or_default();
    assert!(
        hint.to_ascii_lowercase().contains("wait"),
        "hint should suggest waiting, got {hint:?}"
    );

    let pyproject_after = fs::read_to_string(&pyproject_path).expect("read pyproject");
    let lock_after = fs::read_to_string(&lock_path).expect("read lock");
    assert_eq!(pyproject_after, pyproject_before);
    assert_eq!(lock_after, lock_before);
}
