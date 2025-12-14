use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use tempfile::tempdir;

use super::builder::{build_with_container_builder, builder_container_mounts};
use super::*;

fn restore_env(key: &str, original: Option<String>) {
    match original {
        Some(value) => env::set_var(key, value),
        None => env::remove_var(key),
    }
}

#[test]
fn build_options_hash_reflects_env_changes() -> Result<()> {
    let key = "CFLAGS";
    let original = env::var(key).ok();
    env::set_var(key, "value-1");
    let temp_python = env::temp_dir().join("python");
    let python = temp_python.display().to_string();

    let first = compute_build_options_hash(&python, BuildMethod::PipWheel)?;
    env::set_var(key, "value-2");
    let second = compute_build_options_hash(&python, BuildMethod::PipWheel)?;

    restore_env(key, original);
    assert_ne!(first, second, "hash should change when build env changes");
    Ok(())
}

#[test]
fn build_options_hash_reflects_rust_env() -> Result<()> {
    let key = "RUSTFLAGS";
    let original = env::var(key).ok();
    env::set_var(key, "value-1");
    let temp_python = env::temp_dir().join("python");
    let python = temp_python.display().to_string();

    let first = compute_build_options_hash(&python, BuildMethod::PipWheel)?;
    env::set_var(key, "value-2");
    let second = compute_build_options_hash(&python, BuildMethod::PipWheel)?;

    restore_env(key, original);
    assert_ne!(
        first, second,
        "hash should change when rust build env changes"
    );
    Ok(())
}

#[test]
fn build_options_hash_varies_by_method() -> Result<()> {
    let temp_python = env::temp_dir().join("python");
    let python = temp_python.display().to_string();
    let pip = compute_build_options_hash(&python, BuildMethod::PipWheel)?;
    let build = compute_build_options_hash(&python, BuildMethod::PythonBuild)?;
    assert_ne!(pip, build, "build method should influence options hash");
    Ok(())
}

#[test]
fn builder_container_mounts_do_not_include_apt_cache() {
    let builder_mount = PathBuf::from("/tmp/builder");
    let build_mount = PathBuf::from("/tmp/build");
    let mounts = builder_container_mounts(&builder_mount, &build_mount);
    assert_eq!(
        mounts,
        vec![
            format!("{}:/work:rw,Z", build_mount.display()),
            format!("{}:/builder:rw,Z", builder_mount.display())
        ]
    );
    assert!(
        mounts.iter().all(|mount| {
            !mount.contains("apt-cache")
                && !mount.contains("/var/cache/apt")
                && !mount.contains("/var/lib/apt")
        }),
        "builder container should not mount host apt caches"
    );
}

#[test]
fn builder_container_required_when_backend_missing() {
    let temp = tempdir().unwrap();
    let sdist = temp.path().join("demo-0.1.0.tar.gz");
    fs::write(&sdist, b"demo").expect("write sdist");
    let dist_dir = temp.path().join("dist");
    fs::create_dir_all(&dist_dir).expect("dist dir");
    let request = super::super::SdistRequest {
        normalized_name: "demo",
        version: "0.1.0",
        filename: "demo-0.1.0.tar.gz",
        url: "https://example.com/demo-0.1.0.tar.gz",
        sha256: None,
        python_path: "python",
        builder_id: "demo-builder",
        builder_root: Some(temp.path().to_path_buf()),
    };
    let previous = env::var("PX_SANDBOX_BACKEND").ok();
    env::set_var("PX_SANDBOX_BACKEND", "/definitely/missing/backend");
    let result = build_with_container_builder(&request, &sdist, &dist_dir, temp.path());
    match previous {
        Some(val) => env::set_var("PX_SANDBOX_BACKEND", val),
        None => env::remove_var("PX_SANDBOX_BACKEND"),
    }
    assert!(result.is_err(), "builder should require container backend");
}

fn discover_project_dir(root: &Path) -> Result<PathBuf> {
    if is_project_dir(root) {
        return Ok(root.to_path_buf());
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let path = entry.path();
            if is_project_dir(&path) {
                return Ok(path);
            }
        }
    }
    Err(anyhow!("unable to find project dir in {}", root.display()))
}

fn is_project_dir(path: &Path) -> bool {
    path.join("pyproject.toml").exists()
        || path.join("setup.py").exists()
        || path.join("setup.cfg").exists()
}

#[test]
fn detects_project_at_root_with_pyproject() -> Result<()> {
    let dir = tempdir()?;
    let pyproject = dir.path().join("pyproject.toml");
    fs::write(&pyproject, b"[project]\nname = \"demo\"")?;

    let detected = discover_project_dir(dir.path())?;

    assert_eq!(detected, dir.path());
    Ok(())
}

#[test]
fn detects_project_in_subdir_with_setup_py() -> Result<()> {
    let dir = tempdir()?;
    let nested = dir.path().join("pkg");
    fs::create_dir_all(&nested)?;
    fs::write(
        nested.join("setup.py"),
        b"from setuptools import setup\nsetup()",
    )?;

    let detected = discover_project_dir(dir.path())?;

    assert_eq!(detected, nested);
    Ok(())
}

#[test]
fn errors_when_project_files_missing() {
    let dir = tempdir().unwrap();
    let result = discover_project_dir(dir.path());
    assert!(result.is_err());
}
