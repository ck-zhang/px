use assert_cmd::cargo::cargo_bin_cmd;
use serde_json::Value;

#[test]
fn cache_path_prefixes_message() {
    let temp = tempfile::tempdir().expect("tempdir");
    let assert = cargo_bin_cmd!("px")
        .env("PX_CACHE_PATH", temp.path())
        .args(["debug", "cache", "path"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("stdout");
    assert!(
        stdout.starts_with("✔ px cache"),
        "unexpected prefix: {}",
        stdout
    );
}

#[test]
fn cache_prune_user_error_includes_hint() {
    let temp = tempfile::tempdir().expect("tempdir");
    let assert = cargo_bin_cmd!("px")
        .env("PX_CACHE_PATH", temp.path())
        .args(["debug", "cache", "prune"])
        .assert()
        .failure();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("stdout");
    assert!(stdout.contains("Fix:"), "expected fix section: {}", stdout);
}

#[test]
fn json_envelope_is_minimal() {
    let temp = tempfile::tempdir().expect("tempdir");
    let assert = cargo_bin_cmd!("px")
        .env("PX_CACHE_PATH", temp.path())
        .args(["--json", "debug", "cache", "stats"])
        .assert()
        .success();
    let payload: Value = serde_json::from_slice(&assert.get_output().stdout).expect("json");
    let obj = payload.as_object().expect("object");
    let mut keys = obj.keys().cloned().collect::<Vec<_>>();
    keys.sort();
    assert_eq!(keys, vec!["details", "message", "status"]);
    assert!(
        obj["message"]
            .as_str()
            .unwrap_or_default()
            .starts_with("px cache"),
        "message should be prefixed"
    );
}

#[test]
fn quiet_flag_suppresses_human_output() {
    let temp = tempfile::tempdir().expect("tempdir");
    let assert = cargo_bin_cmd!("px")
        .env("PX_CACHE_PATH", temp.path())
        .args(["-q", "debug", "cache", "path"])
        .assert()
        .success();
    assert!(assert.get_output().stdout.is_empty());
}
