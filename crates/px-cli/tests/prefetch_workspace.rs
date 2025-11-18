use std::{fs, path::Path};

use assert_cmd::cargo::cargo_bin_cmd;

mod common;

use common::{artifact_from_lock, parse_json, prepare_workspace_fixture};

const MEMBERS: [&str; 2] = ["member_alpha", "member_beta"];

fn require_online() -> bool {
    match std::env::var("PX_ONLINE").ok().as_deref() {
        Some("1") => true,
        _ => {
            eprintln!("skipping workspace prefetch tests (PX_ONLINE!=1)");
            false
        }
    }
}

#[test]
fn workspace_prefetch_rehydrates_members() {
    if !require_online() {
        return;
    }

    let (_tmp, root) = prepare_workspace_fixture("prefetch-ws");
    let cache_root = root.join(".px-cache");
    fs::create_dir_all(&cache_root).expect("cache dir");

    hydrate_workspace_members(&root, &cache_root);

    let missing_artifact = artifact_from_lock(&root.join(MEMBERS[0]), "packaging");
    fs::remove_file(&missing_artifact).expect("remove artifact");
    assert!(!missing_artifact.exists());

    let assert = cargo_bin_cmd!("px")
        .current_dir(&root)
        .env("PX_ONLINE", "1")
        .env("PX_CACHE_PATH", &cache_root)
        .args(["debug", "cache", "prefetch", "--workspace"])
        .assert()
        .success();

    assert!(missing_artifact.exists());

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("stdout");
    assert!(stdout.contains("member-alpha"));
    assert!(stdout.contains("member-beta"));
}

#[test]
fn workspace_prefetch_dry_run_reports_totals() {
    if !require_online() {
        return;
    }

    let (_tmp, root) = prepare_workspace_fixture("prefetch-ws-json");
    let cache_root = root.join(".px-cache-json");
    fs::create_dir_all(&cache_root).expect("cache dir");

    hydrate_workspace_members(&root, &cache_root);

    let assert = cargo_bin_cmd!("px")
        .current_dir(&root)
        .env_remove("PX_ONLINE")
        .env("PX_CACHE_PATH", &cache_root)
        .args(["--json", "debug", "cache", "prefetch", "--workspace", "--dry-run"])
        .assert()
        .success();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    let workspace = &payload["details"]["workspace"];
    let members = workspace["members"].as_array().expect("members array");
    assert_eq!(members.len(), MEMBERS.len());
    for member in members {
        assert_eq!(member["summary"]["requested"], 1);
        assert_eq!(member["summary"]["fetched"], 0);
    }
    assert_eq!(workspace["totals"]["requested"], 2);
    assert_eq!(workspace["totals"]["hit"], 2);
    assert_eq!(workspace["totals"]["fetched"], 0);
}

fn hydrate_workspace_members(root: &Path, cache_root: &Path) {
    for member in MEMBERS {
        let member_dir = root.join(member);
        cargo_bin_cmd!("px")
            .current_dir(&member_dir)
            .env("PX_ONLINE", "1")
            .args(["add", "packaging==24.1"])
            .assert()
            .success();

        cargo_bin_cmd!("px")
            .current_dir(&member_dir)
            .env("PX_ONLINE", "1")
            .env("PX_CACHE_PATH", cache_root)
            .args(["sync"])
            .assert()
            .success();
    }
}
