use std::{
    env, fs,
    path::Path,
    process::{Command, Stdio},
};

use assert_cmd::cargo::cargo_bin_cmd;
use toml_edit::{DocumentMut, Item};

mod common;

use common::{artifact_from_lock, parse_json, prepare_fixture};

fn require_online() -> bool {
    match env::var("PX_ONLINE").ok().as_deref() {
        Some("1") => true,
        _ => {
            eprintln!("skipping sdist tests (PX_ONLINE!=1)");
            false
        }
    }
}

#[test]
fn force_sdist_build_writes_cache_and_lock() {
    if !require_online() || !require_python_build() {
        return;
    }

    let (_tmp, project) = prepare_fixture("sdist-force");
    add_dependency(&project, "packaging==24.1");
    let cache = tempfile::tempdir().expect("cache dir");

    run_force_sdist_install(&project, cache.path());

    let artifact_path = artifact_from_lock(&project, "packaging");
    assert!(
        artifact_path.exists(),
        "cached artifact should exist at {:?}",
        artifact_path
    );

    let lock_contents = fs::read_to_string(project.join("px.lock")).expect("read lock");
    let doc: DocumentMut = lock_contents.parse().expect("valid lock");
    let deps = doc["dependencies"]
        .as_array_of_tables()
        .expect("dependencies array");
    let entry = deps
        .iter()
        .find(|table| table.get("name").and_then(Item::as_str) == Some("packaging"))
        .expect("packaging entry");
    let artifact = entry["artifact"].as_table().expect("artifact table");

    let check_field = |key: &str| {
        let value = artifact.get(key).and_then(Item::as_str).unwrap_or_default();
        assert!(
            !value.is_empty(),
            "artifact.{key} should be populated (found `{value}`)"
        );
    };
    check_field("python_tag");
    check_field("abi_tag");
    check_field("platform_tag");
    let filename = artifact
        .get("filename")
        .and_then(Item::as_str)
        .unwrap_or_default();
    assert!(filename.ends_with(".whl"));
}

#[test]
fn frozen_install_verifies_built_wheels() {
    if !require_online() || !require_python_build() {
        return;
    }

    let (_tmp, project) = prepare_fixture("sdist-frozen");
    add_dependency(&project, "packaging==24.1");
    let cache = tempfile::tempdir().expect("cache dir");

    run_force_sdist_install(&project, cache.path());

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_ONLINE", "1")
        .env("PX_FORCE_SDIST", "1")
        .env("PX_CACHE_PATH", cache.path())
        .args(["--json", "install", "--frozen"])
        .assert()
        .success();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
}

fn run_force_sdist_install(project: &Path, cache: &Path) {
    cargo_bin_cmd!("px")
        .current_dir(project)
        .env("PX_ONLINE", "1")
        .env("PX_FORCE_SDIST", "1")
        .env("PX_CACHE_PATH", cache)
        .args(["install"])
        .assert()
        .success();
}

fn add_dependency(project: &Path, spec: &str) {
    cargo_bin_cmd!("px")
        .current_dir(project)
        .args(["add", spec])
        .assert()
        .success();
}

fn require_python_build() -> bool {
    let candidates = [
        env::var("PYTHON"),
        Ok("python3".to_string()),
        Ok("python".to_string()),
    ];
    for candidate in candidates.into_iter().flatten() {
        let status = Command::new(&candidate)
            .args(["-m", "build", "--version"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        match status {
            Ok(result) if result.success() => return true,
            Ok(_) | Err(_) => continue,
        }
    }
    eprintln!("skipping sdist tests (python -m build not installed)");
    false
}
