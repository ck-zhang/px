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

    run_install(&project);

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
    let empty = doc
        .get("dependencies")
        .and_then(Item::as_array_of_tables)
        .map(|tables| tables.is_empty())
        .unwrap_or(true);
    assert!(empty, "dependencies should be empty for the fixture");
}

#[test]
fn install_frozen_passes_when_lock_matches() {
    let (_tmp, project) = prepare_fixture("install-frozen");
    run_install(&project);

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["--json", "install", "--frozen"])
        .assert()
        .success();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
}

#[test]
fn install_frozen_fails_on_manifest_drift() {
    let (_tmp, project) = prepare_fixture("install-frozen-drift");
    run_install(&project);
    add_dependency(&project, "requests==2.32.3");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["--json", "install", "--frozen"])
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
    let (_tmp, project) = prepare_fixture("tidy-drift");
    run_install(&project);
    bump_python_requirement(&project, ">=3.13");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["--json", "tidy"])
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

    run_install(&project);

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["--json", "tidy"])
        .assert()
        .success();
    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
}

#[test]
fn lock_diff_is_clean_after_install() {
    let (_tmp, project) = prepare_fixture("lock-diff-clean");
    run_install(&project);

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["--json", "lock", "diff"])
        .assert()
        .success();
    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    assert_eq!(payload["details"]["status"], "clean");
}

#[test]
fn lock_diff_detects_python_mismatch() {
    let (_tmp, project) = prepare_fixture("lock-diff-drift");
    run_install(&project);
    bump_python_requirement(&project, ">=3.13");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["--json", "lock", "diff"])
        .assert()
        .failure();
    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    assert_eq!(payload["details"]["status"], "drift");
    assert!(payload["details"]["python_mismatch"].is_object());
}

#[test]
fn lock_diff_reports_missing_lock() {
    let (_tmp, project) = prepare_fixture("lock-diff-missing");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["--json", "lock", "diff"])
        .assert()
        .failure();
    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    assert_eq!(payload["details"]["status"], "missing_lock");
}

#[test]
fn lock_upgrade_writes_v2_graph() {
    if !require_online() {
        return;
    }
    let (_tmp, project) = prepare_fixture("lock-upgrade-v2");
    add_dependency(&project, "packaging==24.1");
    run_install(&project);

    cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["lock", "upgrade"])
        .assert()
        .success();

    let lock_path = project.join("px.lock");
    let contents = fs::read_to_string(&lock_path).expect("read lockfile");
    let doc: DocumentMut = contents.parse().expect("valid lockfile");
    assert_eq!(doc["version"].as_integer(), Some(2));
    let nodes = doc["graph"]["nodes"]
        .as_array_of_tables()
        .expect("graph nodes");
    assert!(!nodes.is_empty(), "graph nodes should be preserved");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["--json", "lock", "diff"])
        .assert()
        .success();
    let payload = parse_json(&assert);
    assert_eq!(payload["details"]["status"], "clean");
}

#[test]
fn lock_diff_detects_graph_mutation() {
    if !require_online() {
        return;
    }
    let (_tmp, project) = prepare_fixture("lock-upgrade-drift");
    add_dependency(&project, "packaging==24.1");
    run_install(&project);

    cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["lock", "upgrade"])
        .assert()
        .success();

    let lock_path = project.join("px.lock");
    let contents = fs::read_to_string(&lock_path).expect("read lockfile");
    let mut doc: DocumentMut = contents.parse().expect("valid lockfile");
    if let Some(nodes) = doc["graph"]["nodes"].as_array_of_tables_mut() {
        if let Some(first) = nodes.iter_mut().next() {
            first.insert("version", Item::Value(TomlValue::from("0.0.0")));
        }
    }
    fs::write(&lock_path, doc.to_string()).expect("write mutated lockfile");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["--json", "lock", "diff"])
        .assert()
        .failure();
    let payload = parse_json(&assert);
    assert_eq!(payload["details"]["status"], "drift");
}

#[test]
fn install_frozen_accepts_v2_lock() {
    if !require_online() {
        return;
    }
    let (_tmp, project) = prepare_fixture("lock-upgrade-frozen");
    add_dependency(&project, "packaging==24.1");
    run_install(&project);

    cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["lock", "upgrade"])
        .assert()
        .success();

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["--json", "install", "--frozen"])
        .assert()
        .success();
    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
}

fn run_install(project: &Path) {
    cargo_bin_cmd!("px")
        .current_dir(project)
        .arg("install")
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
