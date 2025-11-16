use std::{fs, path::Path};

use assert_cmd::cargo::cargo_bin_cmd;
use serde_json::Value;
use tempfile::TempDir;
use toml_edit::DocumentMut;

mod common;

use common::parse_json;

#[test]
fn project_init_creates_minimal_shape() {
    let temp = tempfile::tempdir().expect("tempdir");
    let project_dir = temp.path().join("demo_shape");
    fs::create_dir_all(&project_dir).expect("create project dir");

    cargo_bin_cmd!("px")
        .current_dir(&project_dir)
        .arg("init")
        .assert()
        .success();

    let pyproject = project_dir.join("pyproject.toml");
    assert!(pyproject.exists(), "pyproject should be created");
    assert!(
        !project_dir.join("px.lock").exists(),
        "px init must not create px.lock"
    );
    assert!(
        !project_dir.join("demo_shape").exists(),
        "px init must not scaffold package code"
    );
    assert!(
        !project_dir.join("dist").exists(),
        "px init must not create dist/"
    );

    let px_dir = project_dir.join(".px");
    assert!(px_dir.is_dir(), ".px directory should exist after init");
    assert!(px_dir.join("envs").is_dir(), ".px/envs should exist");
    assert!(px_dir.join("logs").is_dir(), ".px/logs should exist");
    assert!(
        px_dir.join("state.json").exists(),
        ".px/state.json should exist"
    );

    let contents = fs::read_to_string(&pyproject).expect("read pyproject");
    let doc: DocumentMut = contents.parse().expect("valid pyproject");
    let project = doc["project"].as_table().expect("project table");
    let deps = project
        .get("dependencies")
        .and_then(|item| item.as_array())
        .map(|array| array.len())
        .unwrap_or(0);
    assert_eq!(deps, 0, "dependencies should start empty");
    assert!(
        doc.get("tool")
            .and_then(|tool| tool.as_table())
            .and_then(|table| table.get("px"))
            .is_some(),
        "pyproject should include [tool.px]"
    );
}

#[test]
fn project_init_infers_package_name_from_directory() {
    let temp = tempfile::tempdir().expect("tempdir");
    let project_dir = temp.path().join("Fancy-App");
    fs::create_dir_all(&project_dir).expect("create project dir");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project_dir)
        .args(["init"])
        .assert()
        .success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("px init: initialized project fancy_app"),
        "expected concise init message, got {stdout:?}"
    );

    let name = read_project_name(project_dir.join("pyproject.toml"));
    assert_eq!(name, "fancy_app");
}

#[test]
fn project_init_refuses_when_pyproject_exists() {
    let temp = tempfile::tempdir().expect("tempdir");
    scaffold_demo(&temp, "demo_pkg");
    let project_dir = temp.path();

    let assert = cargo_bin_cmd!("px")
        .current_dir(project_dir)
        .args(["init"])
        .assert()
        .failure();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("px init: project already initialized"),
        "expected polite refusal, got {stdout:?}"
    );
    assert!(stdout.contains("Hint:"), "expected a single hint line");
}

#[test]
fn project_init_json_reports_details() {
    let temp = tempfile::tempdir().expect("tempdir");
    let project_dir = temp.path().join("json-demo");
    fs::create_dir_all(&project_dir).expect("create project dir");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project_dir)
        .args(["--json", "init"])
        .assert()
        .success();

    let payload: Value = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    assert_eq!(payload["details"]["package"], "json_demo");
    let created = payload["details"]["files_created"]
        .as_array()
        .expect("files array")
        .iter()
        .filter_map(|entry| entry.as_str())
        .collect::<Vec<_>>();
    assert!(
        created.contains(&"pyproject.toml"),
        "pyproject should be recorded in files_created: {created:?}"
    );
    assert!(
        created.iter().any(|entry| entry.starts_with(".px")),
        "files_created should include .px paths: {created:?}"
    );
    assert!(
        created.iter().any(|entry| entry == &".px/state.json"),
        "state tracking should be recorded"
    );
}

#[test]
fn project_add_inserts_dependency() {
    let temp = tempfile::tempdir().expect("tempdir");
    scaffold_demo(&temp, "demo_add");
    let project_dir = temp.path();

    cargo_bin_cmd!("px")
        .current_dir(project_dir)
        .args(["add", "requests==2.32.3"])
        .assert()
        .success();

    let deps = read_dependencies(project_dir.join("pyproject.toml"));
    assert!(deps.iter().any(|dep| dep == "requests==2.32.3"));
}

#[test]
fn project_remove_deletes_dependency() {
    let temp = tempfile::tempdir().expect("tempdir");
    scaffold_demo(&temp, "demo_remove");
    let project_dir = temp.path();

    cargo_bin_cmd!("px")
        .current_dir(project_dir)
        .args(["add", "foo==1.0"])
        .assert()
        .success();

    cargo_bin_cmd!("px")
        .current_dir(project_dir)
        .args(["remove", "foo"])
        .assert()
        .success();

    let deps = read_dependencies(project_dir.join("pyproject.toml"));
    assert!(deps.iter().all(|dep| !dep.starts_with("foo")));
}

#[test]
fn px_commands_require_project_root() {
    let temp = tempfile::tempdir().expect("tempdir");
    let assert = cargo_bin_cmd!("px")
        .current_dir(temp.path())
        .args(["add", "requests==2.32.3"])
        .assert()
        .failure();

    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("No px project found. Run `px init` in your project directory first."),
        "expected root error, got stderr: {stderr:?}"
    );
}

#[test]
fn px_commands_walk_up_to_project_root() {
    let temp = tempfile::tempdir().expect("tempdir");
    let project_dir = temp.path().join("root-app");
    fs::create_dir_all(project_dir.join("nested").join("deep")).expect("create dirs");

    cargo_bin_cmd!("px")
        .current_dir(&project_dir)
        .args(["init", "--package", "root_app"])
        .assert()
        .success();

    let nested = project_dir.join("nested").join("deep");
    cargo_bin_cmd!("px")
        .current_dir(&nested)
        .args(["add", "requests==2.32.3"])
        .assert()
        .success();

    let deps = read_dependencies(project_dir.join("pyproject.toml"));
    assert!(
        deps.iter().any(|dep| dep == "requests==2.32.3"),
        "dependency should be added from nested directory: {deps:?}"
    );
}

fn scaffold_demo(temp: &TempDir, package: &str) {
    cargo_bin_cmd!("px")
        .current_dir(temp.path())
        .args(["init", "--package", package])
        .assert()
        .success();
}

fn read_dependencies(path: impl AsRef<Path>) -> Vec<String> {
    let contents = fs::read_to_string(path).expect("pyproject readable");
    let doc: DocumentMut = contents.parse().expect("valid toml");
    doc["project"]["dependencies"]
        .as_array()
        .map(|array| {
            array
                .iter()
                .filter_map(|val| val.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn read_project_name(path: impl AsRef<Path>) -> String {
    let contents = fs::read_to_string(path).expect("pyproject readable");
    let doc: DocumentMut = contents.parse().expect("valid toml");
    doc["project"]["name"].as_str().unwrap().to_string()
}
