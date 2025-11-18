use std::{env, fs, path::Path};

use assert_cmd::cargo::cargo_bin_cmd;
use toml_edit::{Array, DocumentMut, Item, Value as TomlValue};

mod common;

use common::{parse_json, prepare_fixture};

fn require_online() -> bool {
    match env::var("PX_ONLINE").ok().as_deref() {
        Some("1") => true,
        _ => {
            eprintln!("skipping lock upgrade tests (PX_ONLINE!=1)");
            false
        }
    }
}

#[test]
fn install_writes_lockfile_for_fixture() {
    let (_tmp, project) = prepare_fixture("install-lock");
    let lock_path = project.join("px.lock");

    run_sync(&project);

    assert!(lock_path.exists(), "px.lock should be created");
    let contents = fs::read_to_string(&lock_path).expect("read lockfile");
    let doc: DocumentMut = contents.parse().expect("valid lockfile");
    assert_eq!(doc["version"].as_integer(), Some(1));
    let metadata = doc["metadata"]
        .as_table()
        .expect("metadata table should exist");
    assert_eq!(
        metadata
            .get("mode")
            .and_then(Item::as_value)
            .and_then(TomlValue::as_str),
        Some("p0-pinned"),
        "cache mode should record pinned installs"
    );
    let deps = doc
        .get("dependencies")
        .and_then(Item::as_array_of_tables)
        .expect("lock should record dependencies");
    assert!(
        deps.iter().any(|table| {
            table
                .get("specifier")
                .and_then(Item::as_value)
                .and_then(TomlValue::as_str)
                == Some("rich==13.7.1")
        }),
        "rich pin should be captured in px.lock"
    );
}

#[test]
fn install_frozen_passes_when_lock_matches() {
    let (_tmp, project) = prepare_fixture("install-frozen");
    run_sync(&project);

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["--json", "sync", "--frozen"])
        .assert()
        .success();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
}

#[test]
fn install_frozen_fails_on_manifest_drift() {
    let (_tmp, project) = prepare_fixture("install-frozen-drift");
    run_sync(&project);
    add_dependency(&project, "requests==2.32.3");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["--json", "sync", "--frozen"])
        .assert()
        .failure();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    assert!(
        payload["details"]["drift"]
            .as_array()
            .map(|a| !a.is_empty())
            .unwrap_or(false),
        "expected drift entries when pyproject changes"
    );
}

#[test]
fn tidy_reports_drift_until_lock_regenerated() {
    if !require_online() {
        return;
    }
    let (_tmp, project) = prepare_fixture("tidy-drift");
    run_sync(&project);
    bump_python_requirement(&project, ">=3.13");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["--json", "debug", "tidy"])
        .assert()
        .failure();
    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    assert!(
        payload["details"]["drift"]
            .as_array()
            .map(|a| !a.is_empty())
            .unwrap_or(false),
        "tidy should report drift"
    );

    run_sync(&project);

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["--json", "debug", "tidy"])
        .assert()
        .success();
    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
}

fn run_sync(project: &Path) {
    cargo_bin_cmd!("px")
        .current_dir(project)
        .arg("sync")
        .assert()
        .success();
}

fn add_dependency(project: &Path, spec: &str) {
    let path = project.join("pyproject.toml");
    let contents = fs::read_to_string(&path).expect("read pyproject");
    let mut doc: DocumentMut = contents.parse().expect("valid pyproject");
    if !doc["project"]["dependencies"].is_array() {
        doc["project"]["dependencies"] = Item::Value(TomlValue::Array(Array::new()));
    }
    doc["project"]["dependencies"]
        .as_array_mut()
        .expect("dependencies array")
        .push_formatted(TomlValue::from(spec.to_string()));
    fs::write(&path, doc.to_string()).expect("write pyproject");
}

fn bump_python_requirement(project: &Path, requirement: &str) {
    let path = project.join("pyproject.toml");
    let contents = fs::read_to_string(&path).expect("read pyproject");
    let mut doc: DocumentMut = contents.parse().expect("valid pyproject");
    doc["project"]["requires-python"] = Item::Value(TomlValue::from(requirement.to_string()));
    fs::write(&path, doc.to_string()).expect("write pyproject");
}
