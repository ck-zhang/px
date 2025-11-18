#![allow(dead_code)]

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use assert_cmd::{assert::Assert, cargo::cargo_bin_cmd};
use serde_json::Value;
use std::env;
use tempfile::TempDir;
use toml_edit::DocumentMut;

pub fn prepare_fixture(prefix: &str) -> (TempDir, PathBuf) {
    let temp = tempfile::Builder::new()
        .prefix(prefix)
        .tempdir()
        .expect("tempdir");
    let dst = temp.path().join("sample_px_app");
    copy_dir_all(&fixture_source(), &dst).expect("copy fixture");
    (temp, dst)
}

pub fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .to_path_buf()
}

pub fn fixture_source() -> PathBuf {
    workspace_root().join("fixtures").join("sample_px_app")
}

pub fn workspace_fixture_source() -> PathBuf {
    workspace_root().join("fixtures").join("workspace_dual")
}

pub fn traceback_fixture_source() -> PathBuf {
    workspace_root().join("fixtures").join("traceback_lab")
}

pub fn prepare_workspace_fixture(prefix: &str) -> (TempDir, PathBuf) {
    let temp = tempfile::Builder::new()
        .prefix(prefix)
        .tempdir()
        .expect("tempdir");
    let dst = temp.path().join("workspace_dual");
    copy_dir_all(&workspace_fixture_source(), &dst).expect("copy workspace");
    (temp, dst)
}

pub fn prepare_traceback_fixture(prefix: &str) -> (TempDir, PathBuf) {
    let temp = tempfile::Builder::new()
        .prefix(prefix)
        .tempdir()
        .expect("tempdir");
    let dst = temp.path().join("traceback_lab");
    copy_dir_all(&traceback_fixture_source(), &dst).expect("copy traceback fixture");
    (temp, dst)
}

fn copy_dir_all(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_all(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

pub fn parse_json(assert: &Assert) -> Value {
    serde_json::from_slice(&assert.get_output().stdout).expect("valid json")
}

pub fn init_empty_project(prefix: &str) -> (TempDir, PathBuf) {
    let temp = tempfile::Builder::new()
        .prefix(prefix)
        .tempdir()
        .expect("tempdir");
    cargo_bin_cmd!("px")
        .current_dir(temp.path())
        .arg("init")
        .assert()
        .success();
    let root = temp.path().to_path_buf();
    (temp, root)
}

pub fn project_identity(root: &Path) -> (String, String, String) {
    let pyproject = root.join("pyproject.toml");
    let doc: DocumentMut = fs::read_to_string(&pyproject)
        .expect("read pyproject")
        .parse()
        .expect("parse pyproject");
    let name = doc["project"]["name"]
        .as_str()
        .expect("project name")
        .to_string();
    let version = doc["project"]["version"]
        .as_str()
        .expect("project version")
        .to_string();
    let normalized = name.replace('-', "_");
    (name, normalized, version)
}

pub fn require_online() -> bool {
    match env::var("PX_ONLINE").ok().as_deref() {
        Some("1") => true,
        _ => {
            eprintln!("skipping test that needs PX_ONLINE=1");
            false
        }
    }
}

pub fn artifact_from_lock(project_root: &Path, name: &str) -> PathBuf {
    let lock = project_root.join("px.lock");
    let contents = fs::read_to_string(&lock).expect("read lock");
    let doc: DocumentMut = contents.parse().expect("valid lock");
    let deps = doc["dependencies"]
        .as_array_of_tables()
        .expect("deps table");
    let entry = deps
        .iter()
        .find(|table| table.get("name").and_then(toml_edit::Item::as_str) == Some(name))
        .expect("dependency entry");
    let artifact = entry["artifact"].as_table().expect("artifact table");
    let path = artifact
        .get("cached_path")
        .and_then(toml_edit::Item::as_str)
        .expect("cached path");
    PathBuf::from(path)
}
