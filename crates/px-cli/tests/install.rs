use std::{env, fs, path::PathBuf};

use assert_cmd::cargo::cargo_bin_cmd;
use toml_edit::DocumentMut;

fn require_online() -> bool {
    match env::var("PX_ONLINE").ok().as_deref() {
        Some("1") => true,
        _ => {
            eprintln!("skipping online test (set PX_ONLINE=1)");
            false
        }
    }
}

#[test]
fn install_pinned_fetches_artifact() {
    if !require_online() {
        return;
    }

    let temp = tempfile::tempdir().expect("tempdir");
    let project_root = temp.path().to_path_buf();
    let cache_dir = temp.path().join("cache");

    cargo_bin_cmd!("px")
        .current_dir(&project_root)
        .args(["project", "init", "--package", "demo_pin"])
        .assert()
        .success();

    cargo_bin_cmd!("px")
        .current_dir(&project_root)
        .args(["project", "add", "packaging==24.1"])
        .assert()
        .success();

    cargo_bin_cmd!("px")
        .current_dir(&project_root)
        .env("PX_ONLINE", "1")
        .env("PX_CACHE_PATH", &cache_dir)
        .args(["install"])
        .assert()
        .success();

    let lock_path = project_root.join("px.lock");
    assert!(lock_path.exists(), "px.lock should be created");
    let lock_doc: DocumentMut = fs::read_to_string(&lock_path)
        .expect("lock readable")
        .parse()
        .expect("valid lock toml");

    assert_eq!(lock_doc["version"].as_integer(), Some(1));
    assert_eq!(lock_doc["metadata"]["mode"].as_str(), Some("p0-pinned"));

    let deps = lock_doc["dependencies"]
        .as_array_of_tables()
        .expect("dependencies array");
    assert_eq!(deps.len(), 1);
    let dep = deps.iter().next().expect("one dependency");
    assert_eq!(dep["name"].as_str(), Some("packaging"));
    assert_eq!(dep["specifier"].as_str(), Some("packaging==24.1"));

    let artifact = dep["artifact"].as_table().expect("artifact table");
    let cached_path = artifact["cached_path"].as_str().expect("cached path");
    assert!(PathBuf::from(cached_path).exists());
    assert!(artifact["sha256"].as_str().is_some());
    assert!(artifact["filename"].as_str().unwrap().ends_with(".whl"));

    cargo_bin_cmd!("px")
        .current_dir(&project_root)
        .env("PX_ONLINE", "1")
        .env("PX_CACHE_PATH", &cache_dir)
        .args(["install", "--frozen"])
        .assert()
        .success();
}

#[test]
fn install_rejects_unpinned_specs() {
    if !require_online() {
        return;
    }

    let temp = tempfile::tempdir().expect("tempdir");
    let project_root = temp.path().to_path_buf();

    cargo_bin_cmd!("px")
        .current_dir(&project_root)
        .args(["project", "init", "--package", "unpinned"])
        .assert()
        .success();

    cargo_bin_cmd!("px")
        .current_dir(&project_root)
        .args(["project", "add", "packaging>=24"])
        .assert()
        .success();

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project_root)
        .env("PX_ONLINE", "1")
        .args(["install"])
        .assert()
        .failure();
    let output = assert.get_output();
    let message = if output.stderr.is_empty() {
        String::from_utf8_lossy(&output.stdout)
    } else {
        String::from_utf8_lossy(&output.stderr)
    };
    assert!(
        message.contains("requires `name==version`"),
        "output should mention pinning requirement: {message}"
    );
}
