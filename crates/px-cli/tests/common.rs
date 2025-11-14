#![allow(dead_code)]

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use assert_cmd::assert::Assert;
use serde_json::Value;
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

pub fn prepare_workspace_fixture(prefix: &str) -> (TempDir, PathBuf) {
    let temp = tempfile::Builder::new()
        .prefix(prefix)
        .tempdir()
        .expect("tempdir");
    let dst = temp.path().join("workspace_dual");
    copy_dir_all(&workspace_fixture_source(), &dst).expect("copy workspace");
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
