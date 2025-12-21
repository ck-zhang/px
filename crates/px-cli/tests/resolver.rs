use std::{
    fs,
    path::{Path, PathBuf},
};

use assert_cmd::cargo::cargo_bin_cmd;
use serde_json::Value;
use tempfile::TempDir;
use toml_edit::DocumentMut;

mod common;

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

    px_cmd(&project).args(["sync"]).assert().success();

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

    px_cmd(&project)
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
    px_cmd(&project).args(["sync"]).assert().success();

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
        specifier.contains("python_full_version") || specifier.contains("python_version"),
        "specifier missing marker: {specifier}"
    );

    px_cmd(&project)
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
    add_dependency(&project, "packaging");
    px_cmd(&project).args(["sync"]).assert().success();

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
        pinned.starts_with("packaging=="),
        "expected packaging to be pinned, got {pinned}"
    );
}

#[test]
fn resolver_reports_conflicts_with_backtracking() {
    if !require_online() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let project = init_project(&temp, "resolver_conflict");
    // Inject a known conflict directly to exercise `px sync`'s backtracking error
    // reporting without depending on `px add` behavior.
    let pyproject = project.join("pyproject.toml");
    let mut doc: DocumentMut = fs::read_to_string(&pyproject)
        .expect("read pyproject")
        .parse()
        .expect("pyproject toml");
    let deps = doc["project"]["dependencies"]
        .as_array_mut()
        .expect("dependencies array");
    deps.push("urllib3>=2.2");
    deps.push("botocore==1.31.0"); // botocore pins urllib3<1.27
    fs::write(&pyproject, doc.to_string()).expect("write pyproject");

    let assert = px_cmd(&project).args(["--json", "sync"]).assert().failure();
    let payload: Value = serde_json::from_slice(&assert.get_output().stdout).expect("json payload");
    assert_eq!(payload["status"], "user-error");
    let message = payload["message"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    assert!(
        message.contains("resolve")
            || message.contains("resolution")
            || message.contains("conflict"),
        "should report resolver conflict, got {message:?}"
    );
}

#[test]
fn resolver_rejects_direct_url_wheels() {
    if !require_online() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let project = init_project(&temp, "resolver_direct_url");
    let url = "packaging @ https://files.pythonhosted.org/packages/08/aa/cc0199a5f0ad350994d660967a8efb233fe0416e4639146c089643407ce6/packaging-24.1-py3-none-any.whl";
    let assert = px_cmd(&project).args(["--json", "add", url]).assert().failure();

    let payload: Value = serde_json::from_slice(&assert.get_output().stdout).expect("json payload");
    assert_eq!(payload["status"], "user-error");
    assert!(
        payload["message"]
            .as_str()
            .unwrap_or_default()
            .to_ascii_lowercase()
            .contains("unsupported dependency source"),
        "expected unsupported dependency source error, got {:?}",
        payload["message"]
    );
    let hint = payload["details"]["hint"].as_str().unwrap_or_default();
    assert!(
        hint.contains("registry-based"),
        "expected hint to suggest supported sources, got {hint:?}"
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

    px_cmd(&project)
        .env("PIP_INDEX_URL", "https://pypi.org/pypi")
        .args(["sync"])
        .assert()
        .success();
}

fn init_project(temp: &TempDir, name: &str) -> PathBuf {
    px_cmd(temp.path())
        .args(["init", "--package", name])
        .assert()
        .success();
    temp.path().to_path_buf()
}

fn add_dependency(project: &Path, spec: &str) {
    px_cmd(project).args(["add", spec]).assert().success();
}

fn px_cmd(project: &Path) -> assert_cmd::Command {
    let python = common::find_python().unwrap_or_else(|| "python3".to_string());
    let cache = project.join(".px-cache");
    let mut cmd = cargo_bin_cmd!("px");
    cmd.current_dir(project)
        .env("PX_ONLINE", "1")
        .env("PX_CACHE_PATH", &cache)
        .env("PX_NO_ENSUREPIP", "1")
        .env("PX_RUNTIME_HOST_ONLY", "1")
        .env("PX_RUNTIME_PYTHON", python)
        .env("PX_SYSTEM_DEPS_MODE", "offline");
    cmd
}
