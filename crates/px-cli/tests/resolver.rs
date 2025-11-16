use std::{
    fs,
    path::{Path, PathBuf},
};

use assert_cmd::cargo::cargo_bin_cmd;
use tempfile::TempDir;
use toml_edit::DocumentMut;

fn require_online() -> bool {
    match std::env::var("PX_ONLINE").ok().as_deref() {
        Some("1") => true,
        _ => {
            eprintln!("skipping resolver tests (PX_ONLINE!=1)");
            false
        }
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
        .args(["install"])
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
        .args(["install", "--frozen"])
        .assert()
        .success();
}

#[test]
fn resolver_disabled_still_errors_for_ranges() {
    if !require_online() {
        return;
    }

    let temp = tempfile::tempdir().expect("tempdir");
    let project = init_project(&temp, "resolver_disabled");
    add_dependency(&project, "packaging>=24,<25");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_ONLINE", "1")
        .env("PX_RESOLVER", "0")
        .args(["install"])
        .assert()
        .failure();
    let output = assert.get_output();
    let mut buffer = String::new();
    if !output.stdout.is_empty() {
        buffer.push_str(&String::from_utf8_lossy(&output.stdout));
    }
    if !output.stderr.is_empty() {
        buffer.push_str(&String::from_utf8_lossy(&output.stderr));
    }
    assert!(
        buffer.contains("requires `name==version`"),
        "expected pinning error, got {buffer}"
    );
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
        .args(["install"])
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
        .args(["install", "--frozen"])
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
        .args(["install"])
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
fn resolver_disabled_rejects_extras() {
    if !require_online() {
        return;
    }

    let temp = tempfile::tempdir().expect("tempdir");
    let project = init_project(&temp, "resolver_disabled_extras");
    let spec = r#"requests[socks]>=2.32 ; python_version >= "3.10""#;
    add_dependency(&project, spec);

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_ONLINE", "1")
        .env("PX_RESOLVER", "0")
        .args(["install"])
        .assert()
        .failure();
    let output = assert.get_output();
    let mut buffer = String::new();
    if !output.stdout.is_empty() {
        buffer.push_str(&String::from_utf8_lossy(&output.stdout));
    }
    if !output.stderr.is_empty() {
        buffer.push_str(&String::from_utf8_lossy(&output.stderr));
    }
    assert!(
        buffer.contains("extras are not supported")
            || buffer.contains("environment markers are not supported"),
        "expected extras/marker error, got {buffer}"
    );
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
