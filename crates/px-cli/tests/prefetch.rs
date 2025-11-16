use std::{
    fs,
    path::{Path, PathBuf},
};

use assert_cmd::cargo::cargo_bin_cmd;
use tempfile::TempDir;

mod common;

use common::{artifact_from_lock, parse_json, prepare_fixture};

fn require_online() -> bool {
    match std::env::var("PX_ONLINE").ok().as_deref() {
        Some("1") => true,
        _ => {
            eprintln!("skipping prefetch tests (PX_ONLINE!=1)");
            false
        }
    }
}

#[test]
fn prefetch_hydrates_missing_artifact() {
    if !require_online() {
        return;
    }

    let temp = tempfile::tempdir().expect("tempdir");
    let project = init_project(&temp, "prefetch_demo");
    add_dependency(&project, "packaging==24.1");
    let cache_root = temp.path().join("cache");

    cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_ONLINE", "1")
        .env("PX_CACHE_PATH", &cache_root)
        .args(["sync"])
        .assert()
        .success();

    let artifact_path = artifact_from_lock(&project, "packaging");
    fs::remove_file(&artifact_path).expect("remove artifact");
    assert!(!artifact_path.exists());

    cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_ONLINE", "1")
        .env("PX_CACHE_PATH", &cache_root)
        .args(["debug", "cache", "prefetch"])
        .assert()
        .success();

    assert!(artifact_path.exists());
}

#[test]
fn prefetch_dry_run_reports_counts() {
    if !require_online() {
        return;
    }

    let temp = tempfile::tempdir().expect("tempdir");
    let project = init_project(&temp, "prefetch_dry_run");
    add_dependency(&project, "packaging==24.1");
    let cache_root = temp.path().join("cache");

    cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_ONLINE", "1")
        .env("PX_CACHE_PATH", &cache_root)
        .args(["sync"])
        .assert()
        .success();

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_CACHE_PATH", &cache_root)
        .args(["--json", "debug", "cache", "prefetch", "--dry-run"])
        .assert()
        .success();
    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    assert_eq!(payload["details"]["summary"]["requested"], 1);
    assert_eq!(payload["details"]["summary"]["fetched"], 0);
}

#[test]
fn store_prefetch_requires_px_online_for_downloads() {
    let (_tmp, project) = prepare_fixture("prefetch-gating");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_ONLINE", "0")
        .args(["debug", "cache", "prefetch"])
        .assert()
        .failure();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("px cache prefetch: PX_ONLINE=1 required for downloads"),
        "gated run should mention PX_ONLINE requirement: {stdout:?}"
    );
    assert!(
        stdout.contains("Hint: export PX_ONLINE=1"),
        "gated run should emit hint with remediation: {stdout:?}"
    );
}

#[test]
fn store_prefetch_dry_run_emits_status_and_json_flag() {
    if !require_online() {
        return;
    }

    let (_tmp, project) = prepare_fixture("prefetch-dry-run-human");
    run_online_install(&project);

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_ONLINE", "0")
        .args(["debug", "cache", "prefetch", "--dry-run"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("px cache prefetch: dry-run"),
        "dry-run human output should include dry-run summary: {stdout:?}"
    );
    assert!(
        !stdout.contains("Hint:"),
        "dry-run should not emit hint when successful: {stdout:?}"
    );

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_ONLINE", "0")
        .args(["--json", "debug", "cache", "prefetch", "--dry-run"])
        .assert()
        .success();
    let payload = parse_json(&assert);
    assert_eq!(payload["details"]["status"], "dry-run");
}

fn init_project(temp: &TempDir, name: &str) -> PathBuf {
    cargo_bin_cmd!("px")
        .current_dir(temp.path())
        .args(["init", "--package", name])
        .assert()
        .success();
    temp.path().to_path_buf()
}

fn add_dependency(project: &Path, spec: &str) {
    cargo_bin_cmd!("px")
        .current_dir(project)
        .args(["add", spec])
        .assert()
        .success();
}

fn run_online_install(project: &Path) {
    cargo_bin_cmd!("px")
        .current_dir(project)
        .env("PX_ONLINE", "1")
        .arg("sync")
        .assert()
        .success();
}
