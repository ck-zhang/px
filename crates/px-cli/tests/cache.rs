use std::{fs, path::Path};

use assert_cmd::cargo::cargo_bin_cmd;

mod common;

use common::parse_json;

#[test]
fn cache_stats_reports_entries_and_size() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = temp.path().join("store");
    fs::create_dir_all(store.join("nested")).expect("dirs");
    write_bytes(&store.join("a.bin"), 4);
    write_bytes(&store.join("nested").join("b.bin"), 6);

    let assert = cargo_bin_cmd!("px")
        .env("PX_CACHE_PATH", &store)
        .args(["--json", "cache", "stats"])
        .assert()
        .success();
    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    let details = payload["details"].as_object().expect("details object");
    assert_eq!(details["cache_exists"], true);
    assert_eq!(details["total_entries"], 2);
    assert_eq!(details["total_size_bytes"], 10);
}

#[test]
fn cache_prune_respects_dry_run_and_all() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = temp.path().join("store");
    populate_cache(&store);

    let assert = cargo_bin_cmd!("px")
        .env("PX_CACHE_PATH", &store)
        .args(["--json", "cache", "prune", "--all", "--dry-run"])
        .assert()
        .success();
    let payload = parse_json(&assert);
    assert_eq!(payload["details"]["status"], "dry-run");
    assert!(store.join("nested").join("b.bin").exists());

    let assert = cargo_bin_cmd!("px")
        .env("PX_CACHE_PATH", &store)
        .args(["--json", "cache", "prune", "--all"])
        .assert()
        .success();
    let payload = parse_json(&assert);
    assert_eq!(payload["details"]["status"], "success");
    assert!(!store.join("nested").join("b.bin").exists());
}

#[test]
fn cache_path_human_output_is_prefixed() {
    let temp = tempfile::tempdir().expect("tempdir");
    let cache_dir = temp.path().join("cache");
    fs::create_dir_all(&cache_dir).expect("dirs");

    let assert = cargo_bin_cmd!("px")
        .env("PX_CACHE_PATH", &cache_dir)
        .args(["cache", "path"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("px infra cache: path"),
        "cache path output should include prefixed summary: {stdout:?}"
    );
}

#[test]
fn cache_prune_dry_run_human_message_is_concise() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = temp.path().join("store");
    populate_cache(&store);

    let assert = cargo_bin_cmd!("px")
        .env("PX_CACHE_PATH", &store)
        .args(["cache", "prune", "--all", "--dry-run"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("px infra cache: would remove"),
        "dry-run output should mention would-remove summary: {stdout:?}"
    );
    assert!(
        !stdout.contains("Hint:"),
        "dry-run success should not emit Hint: {stdout:?}"
    );
}

fn populate_cache(store: &Path) {
    fs::create_dir_all(store.join("nested")).expect("dirs");
    write_bytes(&store.join("a.bin"), 4);
    write_bytes(&store.join("nested").join("b.bin"), 6);
}

fn write_bytes(path: &Path, size: usize) {
    let data = vec![b'X'; size];
    fs::write(path, data).expect("write file");
}
