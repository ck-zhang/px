use std::fs;

use assert_cmd::cargo::cargo_bin_cmd;
use serde_json::Value;

mod common;

use common::{parse_json, prepare_workspace_fixture};

#[test]
fn workspace_list_reports_members() {
    let (_tmp, root) = prepare_workspace_fixture("ws-list");
    let assert = cargo_bin_cmd!("px")
        .current_dir(&root)
        .args(["--json", "workspace", "list"])
        .assert()
        .success();
    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    let members = payload["details"]["workspace"]["members"]
        .as_array()
        .expect("members array");
    assert_eq!(members.len(), 2);
    let names: Vec<_> = members.iter().filter_map(|m| m["name"].as_str()).collect();
    assert!(names.contains(&"member-alpha"));
    assert!(names.contains(&"member-beta"));
}

#[test]
fn workspace_verify_detects_and_clears_drift() {
    let (_tmp, root) = prepare_workspace_fixture("ws-verify");
    let beta = root.join("member_beta");
    fs::remove_file(beta.join("px.lock")).expect("remove px.lock");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&root)
        .args(["--json", "workspace", "verify"])
        .assert()
        .failure();
    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    let members = payload["details"]["workspace"]["members"]
        .as_array()
        .unwrap();
    assert!(members
        .iter()
        .any(|member| member["status"] == "missing-lock"));

    cargo_bin_cmd!("px")
        .current_dir(&beta)
        .arg("install")
        .assert()
        .success();

    let assert = cargo_bin_cmd!("px")
        .current_dir(&root)
        .args(["--json", "workspace", "verify"])
        .assert()
        .success();
    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    let members = payload["details"]["workspace"]["members"]
        .as_array()
        .unwrap();
    assert!(members.iter().all(|member| member["status"] == "ok"));
}

#[test]
fn workspace_verify_human_messages_reflect_state() {
    let (_tmp, root) = prepare_workspace_fixture("ws-verify-human");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&root)
        .args(["workspace", "verify"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("px workspace verify: all members clean"),
        "clean verify output should mention all members clean: {stdout:?}"
    );
    assert!(
        !stdout.contains("Hint:"),
        "clean verify should not emit a hint: {stdout:?}"
    );

    let beta = root.join("member_beta");
    fs::remove_file(beta.join("px.lock")).expect("remove px.lock");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&root)
        .args(["workspace", "verify"])
        .assert()
        .failure();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("px workspace verify: drift in member-beta"),
        "drift output should mention member-beta drift: {stdout:?}"
    );
    assert!(
        stdout.contains("Hint: run `px workspace install`"),
        "drift output should emit remediation hint: {stdout:?}"
    );
}

#[test]
fn workspace_install_restores_missing_locks() {
    let (_tmp, root) = prepare_workspace_fixture("ws-install-members");
    let alpha_lock = root.join("member_alpha/px.lock");
    let beta_lock = root.join("member_beta/px.lock");
    fs::remove_file(&alpha_lock).expect("remove alpha lock");
    fs::remove_file(&beta_lock).expect("remove beta lock");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&root)
        .args(["--json", "workspace", "install"])
        .assert()
        .success();
    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    let counts = &payload["details"]["workspace"]["counts"];
    assert_eq!(counts["ok"], 2);
    assert!(alpha_lock.exists());
    assert!(beta_lock.exists());
}

#[test]
fn workspace_tidy_reports_and_clears_drift() {
    let (_tmp, root) = prepare_workspace_fixture("ws-tidy-flow");
    let beta_lock = root.join("member_beta/px.lock");
    fs::remove_file(&beta_lock).expect("remove beta lock");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&root)
        .args(["--json", "workspace", "tidy"])
        .assert()
        .failure();
    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    let beta = member_entry(&payload, "member-beta");
    assert_eq!(beta["status"], "missing-lock");

    cargo_bin_cmd!("px")
        .current_dir(&root)
        .args(["workspace", "install"])
        .assert()
        .success();

    let assert = cargo_bin_cmd!("px")
        .current_dir(&root)
        .args(["--json", "workspace", "tidy"])
        .assert()
        .success();
    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    let beta = member_entry(&payload, "member-beta");
    assert_eq!(beta["status"], "tidied");
}

#[test]
fn workspace_install_frozen_requires_clean_state() {
    let (_tmp, root) = prepare_workspace_fixture("ws-install-frozen");
    let beta_lock = root.join("member_beta/px.lock");
    fs::remove_file(&beta_lock).expect("remove beta lock");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&root)
        .args(["--json", "workspace", "install", "--frozen"])
        .assert()
        .failure();
    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    assert_eq!(
        member_entry(&payload, "member-beta")["status"],
        "missing-lock"
    );

    cargo_bin_cmd!("px")
        .current_dir(&root)
        .args(["workspace", "install"])
        .assert()
        .success();

    let assert = cargo_bin_cmd!("px")
        .current_dir(&root)
        .args(["--json", "workspace", "install", "--frozen"])
        .assert()
        .success();
    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    assert_eq!(member_entry(&payload, "member-beta")["status"], "verified");
}

fn member_entry<'a>(payload: &'a Value, name: &str) -> &'a Value {
    payload["details"]["workspace"]["members"]
        .as_array()
        .and_then(|items| items.iter().find(|member| member["name"] == name))
        .expect("member entry")
}
