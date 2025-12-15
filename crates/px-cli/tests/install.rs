use std::{
    fs,
    path::{Path, PathBuf},
};

use assert_cmd::cargo::cargo_bin_cmd;
use toml_edit::DocumentMut;

mod common;

fn write_pyproject(dir: &Path, deps: &[&str]) {
    let mut deps_block = String::new();
    for dep in deps {
        deps_block.push_str("  \"");
        deps_block.push_str(dep);
        deps_block.push_str("\",\n");
    }
    let contents = format!(
        "[project]\nname = \"install-fixture\"\nversion = \"0.1.0\"\n\
requires-python = \">=3.9\"\ndependencies = [\n{deps_block}]\n\
\n[tool.px]\n\
\n[build-system]\nrequires = [\"setuptools>=70\", \"wheel\"]\n\
build-backend = \"setuptools.build_meta\"\n"
    );
    fs::write(dir.join("pyproject.toml"), contents).expect("write pyproject");
}

#[test]
fn install_pinned_fetches_artifact() {
    if !common::require_online() {
        return;
    }
    let _guard = common::test_env_guard();
    common::reset_test_store_env();
    common::ensure_test_store_env();

    let Some(python) = common::find_python() else {
        eprintln!("skipping install test (python binary not found)");
        return;
    };

    let temp = tempfile::tempdir().expect("tempdir");
    let project_root = temp.path().to_path_buf();

    cargo_bin_cmd!("px")
        .current_dir(&project_root)
        .env("PX_RUNTIME_PYTHON", &python)
        .env("PX_ONLINE", "1")
        .args(["init", "--package", "demo_pin"])
        .assert()
        .success();

    cargo_bin_cmd!("px")
        .current_dir(&project_root)
        .env("PX_RUNTIME_PYTHON", &python)
        .env("PX_ONLINE", "1")
        .args(["add", "packaging==24.1"])
        .assert()
        .success();

    cargo_bin_cmd!("px")
        .current_dir(&project_root)
        .env("PX_ONLINE", "1")
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["sync"])
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
    assert!(std::path::Path::new(artifact["filename"].as_str().unwrap())
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("whl")));

    cargo_bin_cmd!("px")
        .current_dir(&project_root)
        .env("PX_ONLINE", "1")
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["sync", "--frozen"])
        .assert()
        .success();
}

#[test]
fn install_autopins_unpinned_specs() {
    if !common::require_online() {
        return;
    }
    let _guard = common::test_env_guard();
    common::reset_test_store_env();
    common::ensure_test_store_env();

    let Some(python) = common::find_python() else {
        eprintln!("skipping install test (python binary not found)");
        return;
    };

    let temp = tempfile::tempdir().expect("tempdir");
    let project_root = temp.path().to_path_buf();

    cargo_bin_cmd!("px")
        .current_dir(&project_root)
        .env("PX_RUNTIME_PYTHON", &python)
        .env("PX_ONLINE", "1")
        .args(["init", "--package", "unpinned"])
        .assert()
        .success();

    cargo_bin_cmd!("px")
        .current_dir(&project_root)
        .env("PX_RUNTIME_PYTHON", &python)
        .env("PX_ONLINE", "1")
        .args(["add", "packaging>=24"])
        .assert()
        .success();

    cargo_bin_cmd!("px")
        .current_dir(&project_root)
        .env("PX_ONLINE", "1")
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["sync"])
        .assert()
        .success();

    let manifest = fs::read_to_string(project_root.join("pyproject.toml")).expect("read pyproject");
    assert!(
        manifest.contains("packaging=="),
        "expected `px add` to autopin the dependency, got manifest:\n{manifest}"
    );
}

#[test]
fn install_skips_nonmatching_marker_specs() {
    if !common::require_online() {
        return;
    }
    let _guard = common::test_env_guard();
    common::reset_test_store_env();
    common::ensure_test_store_env();

    let Some(python) = common::find_python() else {
        eprintln!("skipping install test (python binary not found)");
        return;
    };

    let temp = tempfile::tempdir().expect("tempdir");
    write_pyproject(temp.path(), &["tomli>=1.1.0; python_version < '3.11'"]);

    cargo_bin_cmd!("px")
        .current_dir(temp.path())
        .env("PX_ONLINE", "1")
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["sync"])
        .assert()
        .success();

    let lock_path = temp.path().join("px.lock");
    assert!(lock_path.exists(), "px.lock should be created");
    let lock_doc: DocumentMut = fs::read_to_string(&lock_path)
        .expect("lock readable")
        .parse()
        .expect("valid lock toml");
    let deps_empty = lock_doc
        .get("dependencies")
        .and_then(|item| item.as_array_of_tables())
        .is_none_or(toml_edit::ArrayOfTables::is_empty);
    assert!(deps_empty, "non-matching marker should be skipped");
}

#[test]
fn install_accepts_pinned_marker_spec() {
    if !common::require_online() {
        return;
    }
    let _guard = common::test_env_guard();
    common::reset_test_store_env();
    common::ensure_test_store_env();

    let Some(python) = common::find_python() else {
        eprintln!("skipping install test (python binary not found)");
        return;
    };

    let temp = tempfile::tempdir().expect("tempdir");
    write_pyproject(
        temp.path(),
        &["typing_extensions==4.12.0; python_version >= '3.11'"],
    );

    cargo_bin_cmd!("px")
        .current_dir(temp.path())
        .env("PX_ONLINE", "1")
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["sync"])
        .assert()
        .success();

    let lock_path = temp.path().join("px.lock");
    assert!(lock_path.exists(), "px.lock should exist");
    let lock_doc: DocumentMut = fs::read_to_string(&lock_path)
        .expect("lock readable")
        .parse()
        .expect("valid lock toml");
    let deps = lock_doc["dependencies"]
        .as_array_of_tables()
        .expect("dependencies array");
    assert_eq!(deps.len(), 1, "expected single dependency entry");
    let dep = deps.iter().next().unwrap();
    assert_eq!(dep["name"].as_str(), Some("typing-extensions"));
    assert_eq!(
        dep["marker"].as_str(),
        Some("python_full_version >= '3.11'")
    );
}

#[test]
fn install_autopins_loose_marker_specs() {
    if !common::require_online() {
        return;
    }
    let _guard = common::test_env_guard();
    common::reset_test_store_env();
    common::ensure_test_store_env();

    let Some(python) = common::find_python() else {
        eprintln!("skipping install test (python binary not found)");
        return;
    };

    let temp = tempfile::tempdir().expect("tempdir");
    write_pyproject(
        temp.path(),
        &["rich==13.7.1", "tomli>=1.1.0; python_version >= '3.11'"],
    );

    cargo_bin_cmd!("px")
        .current_dir(temp.path())
        .env("PX_ONLINE", "1")
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["sync"])
        .assert()
        .success();

    let lock_path = temp.path().join("px.lock");
    assert!(lock_path.exists(), "lockfile should be written");
}
