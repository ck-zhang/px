use std::{
    fs,
    path::{Path, PathBuf},
};

use assert_cmd::cargo::cargo_bin_cmd;
use tempfile::TempDir;

mod common;

use common::{artifact_from_lock, parse_json};

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
        .args(["install"])
        .assert()
        .success();

    let artifact_path = artifact_from_lock(&project, "packaging");
    fs::remove_file(&artifact_path).expect("remove artifact");
    assert!(!artifact_path.exists());

    cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_ONLINE", "1")
        .env("PX_CACHE_PATH", &cache_root)
        .args(["store", "prefetch"])
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
        .args(["install"])
        .assert()
        .success();

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_CACHE_PATH", &cache_root)
        .args(["--json", "store", "prefetch", "--dry-run"])
        .assert()
        .success();
    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    assert_eq!(payload["details"]["summary"]["requested"], 1);
    assert_eq!(payload["details"]["summary"]["fetched"], 0);
}

fn init_project(temp: &TempDir, name: &str) -> PathBuf {
    cargo_bin_cmd!("px")
        .current_dir(temp.path())
        .args(["project", "init", "--package", name])
        .assert()
        .success();
    temp.path().to_path_buf()
}

fn add_dependency(project: &Path, spec: &str) {
    cargo_bin_cmd!("px")
        .current_dir(project)
        .args(["project", "add", spec])
        .assert()
        .success();
}
