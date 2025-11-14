use std::{fs, path::Path};

use assert_cmd::cargo::cargo_bin_cmd;
use tempfile::TempDir;
use toml_edit::DocumentMut;

#[test]
fn project_init_creates_scaffold_and_runs() {
    let temp = tempfile::tempdir().expect("tempdir");
    scaffold_demo(&temp, "demo_pkg");

    let project_dir = temp.path();
    assert!(project_dir.join("pyproject.toml").exists());
    assert!(project_dir.join("demo_pkg").join("cli.py").exists());
    assert!(project_dir.join("tests").join("test_cli.py").exists());

    let assert = cargo_bin_cmd!("px")
        .current_dir(project_dir)
        .args(["run", "demo_pkg.cli"])
        .assert()
        .success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(stdout.contains("Hello, World!"));
}

#[test]
fn project_add_inserts_dependency() {
    let temp = tempfile::tempdir().expect("tempdir");
    scaffold_demo(&temp, "demo_add");
    let project_dir = temp.path();

    cargo_bin_cmd!("px")
        .current_dir(project_dir)
        .args(["project", "add", "requests==2.32.3"])
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
        .args(["project", "add", "foo==1.0"])
        .assert()
        .success();

    cargo_bin_cmd!("px")
        .current_dir(project_dir)
        .args(["project", "remove", "foo"])
        .assert()
        .success();

    let deps = read_dependencies(project_dir.join("pyproject.toml"));
    assert!(deps.iter().all(|dep| !dep.starts_with("foo")));
}

fn scaffold_demo(temp: &TempDir, package: &str) {
    cargo_bin_cmd!("px")
        .current_dir(temp.path())
        .args(["project", "init", "--package", package])
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
