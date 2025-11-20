use assert_cmd::cargo::cargo_bin_cmd;
use tempfile::tempdir;

mod common;

use common::parse_json;

#[test]
fn tool_install_rejects_requirement_like_name() {
    let tools_dir = tempdir().expect("tools dir");
    let store_dir = tempdir().expect("tool store");

    let assert = cargo_bin_cmd!("px")
        .env("PX_TOOLS_DIR", tools_dir.path())
        .env("PX_TOOL_STORE", store_dir.path())
        .args(["--json", "tool", "install", "ruff==0.6.9"])
        .assert()
        .failure();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    let hint = payload["details"]["hint"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        hint.contains("px tool install ruff ruff==0.6.9"),
        "expected hint to suggest name + spec split, got {hint:?}"
    );
}
