use assert_cmd::cargo::cargo_bin_cmd;
use serde_json::Value;

mod common;

use common::{init_empty_project, parse_json, project_identity};

#[test]
fn output_build_produces_wheel_and_sdist() {
    let (_tmp, project) = init_empty_project("output-build");
    let (name, normalized, version) = project_identity(&project);
    let dist_dir = project.join("dist-artifacts");
    let dist_arg = dist_dir.to_string_lossy().to_string();

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["--json", "build", "both", "--out", &dist_arg])
        .assert()
        .success();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    let artifacts = payload["details"]["artifacts"]
        .as_array()
        .expect("artifacts array");
    assert_eq!(artifacts.len(), 2, "expected wheel + sdist entries");
    let paths: Vec<String> = artifacts
        .iter()
        .filter_map(|entry| {
            entry
                .as_object()
                .and_then(|map| map.get("path"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    let expected_files = [
        format!("dist-artifacts/{name}-{version}.tar.gz"),
        format!("dist-artifacts/{normalized}-{version}-py3-none-any.whl"),
    ];
    for rel in &expected_files {
        assert!(
            paths.iter().any(|entry| entry == rel),
            "artifacts should include {rel}, got {paths:?}"
        );
        assert!(
            project.join(rel).exists(),
            "built file {rel} should exist on disk"
        );
    }
}

#[test]
fn publish_dry_run_reports_registry_and_artifacts() {
    let (_tmp, project) = init_empty_project("output-publish-dry-run");
    cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["build"])
        .assert()
        .success();

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .args([
            "--json",
            "publish",
            "--dry-run",
            "--registry",
            "testpypi",
            "--token-env",
            "PX_FAKE_TOKEN",
        ])
        .assert()
        .success();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    assert_eq!(payload["details"]["registry"], "testpypi");
    assert_eq!(payload["details"]["dry_run"], Value::Bool(true));
    let artifacts = payload["details"]["artifacts"]
        .as_array()
        .expect("artifacts array");
    assert!(
        !artifacts.is_empty(),
        "dry-run publish should report existing artifacts"
    );
}

#[test]
fn publish_requires_token_when_online() {
    let (_tmp, project) = init_empty_project("output-publish-token");
    cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["build"])
        .assert()
        .success();

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_ONLINE", "1")
        .args(["--json", "publish"])
        .assert()
        .failure();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    let message = payload["message"].as_str().expect("message string");
    assert!(
        message.contains("PX_PUBLISH_TOKEN must be set"),
        "expected missing token error, got {message:?}"
    );
    let hint = payload["details"]["hint"].as_str().expect("hint field");
    assert!(
        hint.contains("PX_PUBLISH_TOKEN"),
        "hint should mention token variable: {hint:?}"
    );
}

#[test]
fn publish_errors_when_dist_missing() {
    let (_tmp, project) = init_empty_project("output-publish-missing-dist");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["--json", "publish"])
        .assert()
        .failure();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    let message = payload["message"].as_str().expect("message string");
    assert!(
        message.contains("no artifacts"),
        "expected publish to fail when dist/ is empty: {message:?}"
    );
    assert_eq!(payload["details"]["dist_dir"], "dist");
}

#[test]
fn build_dry_run_reports_empty_artifacts() {
    let (_tmp, project) = init_empty_project("output-build-dry-run");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["--json", "build", "--dry-run"])
        .assert()
        .success();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    assert_eq!(payload["details"]["dry_run"], Value::Bool(true));
    let artifacts = payload["details"]["artifacts"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        artifacts.is_empty(),
        "dry-run build should not report artifacts: {artifacts:?}"
    );
    assert!(
        !project.join("dist").exists(),
        "dry-run build should not create dist directory"
    );
}
