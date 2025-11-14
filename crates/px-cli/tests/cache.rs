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

fn populate_cache(store: &Path) {
    fs::create_dir_all(store.join("nested")).expect("dirs");
    write_bytes(&store.join("a.bin"), 4);
    write_bytes(&store.join("nested").join("b.bin"), 6);
}

fn write_bytes(path: &Path, size: usize) {
    let data = vec![b'X'; size];
    fs::write(path, data).expect("write file");
}
