use std::{
    fs,
    path::{Path, PathBuf},
};

use assert_cmd::cargo::cargo_bin_cmd;
use serde_json::Value;
use tempfile::TempDir;
use toml_edit::DocumentMut;

fn require_online() -> bool {
    if let Some("1") = std::env::var("PX_ONLINE").ok().as_deref() {
        true
    } else {
        eprintln!("skipping resolver tests (PX_ONLINE!=1)");
        false
    }
}

#[test]
fn resolver_pins_range_when_enabled() {
    if !require_online() {
        return;
    }

    let temp = tempfile::tempdir().expect("tempdir");
    let project = init_project(&temp, "resolver_demo");
    add_dependency(&project, "packaging>=24,<25");

    cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_ONLINE", "1")
        .env("PX_CACHE_PATH", project.join(".px-cache"))
        .args(["sync"])
        .assert()
        .success();

    let lock = project.join("px.lock");
    assert!(lock.exists(), "px.lock should be created");
    let doc: DocumentMut = fs::read_to_string(&lock)
        .expect("read lock")
        .parse()
        .expect("lock toml");
    assert_eq!(doc["version"].as_integer(), Some(1));
    let deps = doc["dependencies"]
        .as_array_of_tables()
        .expect("deps table");
    let dep = deps
        .iter()
        .find(|table| table.get("name").and_then(toml_edit::Item::as_str) == Some("packaging"))
        .expect("packaging entry");
    let pinned = dep["specifier"].as_str().expect("specifier");
    assert!(
        pinned.starts_with("packaging==24."),
        "expected packaging to be pinned, got {pinned}"
    );

    cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_ONLINE", "1")
        .env("PX_CACHE_PATH", project.join(".px-cache"))
        .args(["sync", "--frozen"])
        .assert()
        .success();
}

#[test]
fn resolver_handles_extras_and_markers() {
    if !require_online() {
        return;
    }

    let temp = tempfile::tempdir().expect("tempdir");
    let project = init_project(&temp, "resolver_extras_markers");
    let spec = r#"requests[socks]>=2.32 ; python_version >= "3.10""#;
    add_dependency(&project, spec);
    let cache = project.join(".px-cache");

    cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_ONLINE", "1")
        .env("PX_CACHE_PATH", &cache)
        .args(["sync"])
        .assert()
        .success();

    let lock = project.join("px.lock");
    let doc: DocumentMut = fs::read_to_string(&lock)
        .expect("read lock")
        .parse()
        .expect("lock toml");
    let deps = doc["dependencies"]
        .as_array_of_tables()
        .expect("deps table");
    let dep = deps
        .iter()
        .find(|table| table.get("name").and_then(toml_edit::Item::as_str) == Some("requests"))
        .expect("requests entry");
    let specifier = dep["specifier"].as_str().expect("specifier");
    assert!(
        specifier.contains("[socks]=="),
        "specifier missing extras: {specifier}"
    );
    assert!(
        specifier.contains("python_version"),
        "specifier missing marker: {specifier}"
    );

    cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_ONLINE", "1")
        .env("PX_CACHE_PATH", &cache)
        .args(["sync", "--frozen"])
        .assert()
        .success();
}

#[test]
fn resolver_pins_unversioned_spec() {
    if !require_online() {
        return;
    }

    let temp = tempfile::tempdir().expect("tempdir");
    let project = init_project(&temp, "resolver_unversioned");
    add_dependency(&project, "numpy");
    let cache = project.join(".px-cache");

    cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_ONLINE", "1")
        .env("PX_CACHE_PATH", &cache)
        .args(["sync"])
        .assert()
        .success();

    let pyproject = project.join("pyproject.toml");
    let doc: DocumentMut = fs::read_to_string(&pyproject)
        .expect("read pyproject")
        .parse()
        .expect("pyproject toml");
    let deps = doc["project"]["dependencies"]
        .as_array()
        .expect("dependencies array");
    let pinned = deps
        .iter()
        .find_map(|item| item.as_str())
        .expect("dependency entry");
    assert!(
        pinned.starts_with("numpy=="),
        "expected numpy to be pinned, got {pinned}"
    );
}

#[test]
fn resolver_reports_conflicts_with_backtracking() {
    if !require_online() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let project = init_project(&temp, "resolver_conflict");
    add_dependency(&project, "urllib3>=2.2");
    add_dependency(&project, "botocore==1.31.0"); // pins urllib3<1.27

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_ONLINE", "1")
        .args(["--json", "sync"])
        .assert()
        .failure();
    let payload: Value = serde_json::from_slice(&assert.get_output().stdout).expect("json payload");
    assert_eq!(payload["status"], "user-error");
    let message = payload["message"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    assert!(
        message.contains("resolve") || message.contains("conflict"),
        "should report resolver conflict, got {message:?}"
    );
}

#[test]
fn resolver_supports_direct_url_wheels() {
    if !require_online() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let project = init_project(&temp, "resolver_direct_url");
    let url = "packaging @ https://files.pythonhosted.org/packages/08/aa/cc0199a5f0ad350994d660967a8efb233fe0416e4639146c089643407ce6/packaging-24.1-py3-none-any.whl";
    add_dependency(&project, url);

    cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_ONLINE", "1")
        .args(["--json", "sync"])
        .assert()
        .success();

    let lock = project.join("px.lock");
    let doc: DocumentMut = fs::read_to_string(&lock)
        .expect("read lock")
        .parse()
        .expect("lock toml");
    let deps = doc["dependencies"]
        .as_array_of_tables()
        .expect("deps table");
    let dep = deps
        .iter()
        .find(|table| table.get("name").and_then(toml_edit::Item::as_str) == Some("packaging"))
        .expect("packaging entry");
    let specifier = dep["specifier"].as_str().expect("specifier");
    assert!(
        specifier.contains("packaging==24.1"),
        "expected packaging pin from direct URL, got {specifier}"
    );
}

#[test]
fn resolver_respects_primary_index_env() {
    if !require_online() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let project = init_project(&temp, "resolver_custom_index");
    add_dependency(&project, "packaging");

    cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_ONLINE", "1")
        .env("PIP_INDEX_URL", "https://pypi.org/pypi")
        .env("PX_CACHE_PATH", project.join(".px-cache"))
        .args(["sync"])
        .assert()
        .success();
}

fn init_project(temp: &TempDir, name: &str) -> PathBuf {
    cargo_bin_cmd!("px")
        .current_dir(temp.path())
        .args(["init", "--package", name])
        .assert()
        .success();
    temp.path().to_path_buf()
}

fn add_dependency(project: &Path, spec: &str) {
    cargo_bin_cmd!("px")
        .current_dir(project)
        .args(["add", spec])
        .assert()
        .success();
}
