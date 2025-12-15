use super::super::env_materialize::{
    cached_project_wheel, compute_file_sha256, ensure_project_wheel_scripts,
    normalize_project_name, persist_wheel_metadata, project_build_hash, project_wheel_cache_dir,
    uses_maturin_backend,
};
use super::super::*;
use crate::api::SystemEffects;
use crate::core::runtime::builder::builder_identity_for_runtime;
use crate::core::runtime::effects::Effects;
use crate::{OwnerId, OwnerType};
use anyhow::Result;
use px_domain::api::ProjectSnapshot;
use std::env;
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use tempfile::tempdir;
use zip::write::FileOptions;

#[test]
fn detects_maturin_backend() -> Result<()> {
    let temp = tempdir()?;
    let manifest = temp.path().join("pyproject.toml");
    fs::write(
        &manifest,
        r#"[project]
name = "demo"
version = "0.1.0"

[build-system]
requires = ["maturin>=1.0"]
build-backend = "maturin"
"#,
    )?;
    assert!(uses_maturin_backend(&manifest)?);

    fs::write(
        &manifest,
        r#"[project]
name = "demo"
version = "0.1.0"

[build-system]
requires = ["setuptools"]
build-backend = "setuptools.build_meta"

[tool.maturin]
features = []
"#,
    )?;
    assert!(uses_maturin_backend(&manifest)?);
    Ok(())
}

#[test]
fn project_wheel_cache_dir_varies_with_build_env() -> Result<()> {
    let key = "RUSTFLAGS";
    let original = env::var(key).ok();
    env::set_var(key, "value-a");

    let temp = tempdir()?;
    let project_root = temp.path().join("maturin-fingerprint");
    fs::create_dir_all(&project_root)?;
    fs::write(
        project_root.join("pyproject.toml"),
        r#"[project]
name = "fingerprint"
version = "0.1.0"

[build-system]
requires = ["maturin>=1.0"]
build-backend = "maturin"
"#,
    )?;

    let snapshot = ProjectSnapshot::read_from(&project_root)?;
    let runtime = RuntimeMetadata {
        path: "/usr/bin/python".into(),
        version: "3.12.0".into(),
        platform: "test-platform".into(),
    };
    let builder = builder_identity_for_runtime(&runtime)?;
    let cache_root = temp.path().join("cache");

    let first_hash = project_build_hash(
        &runtime,
        &snapshot,
        Path::new("/usr/bin/python"),
        false,
        &builder.builder_id,
    )?;
    env::set_var(key, "value-b");
    let second_hash = project_build_hash(
        &runtime,
        &snapshot,
        Path::new("/usr/bin/python"),
        false,
        &builder.builder_id,
    )?;
    match original {
        Some(value) => env::set_var(key, value),
        None => env::remove_var(key),
    }

    assert_ne!(
        first_hash, second_hash,
        "build hash should reflect build env"
    );

    let first_dir = project_wheel_cache_dir(
        &cache_root,
        &snapshot,
        &runtime,
        Path::new("/usr/bin/python"),
        false,
        &first_hash,
    );
    let second_dir = project_wheel_cache_dir(
        &cache_root,
        &snapshot,
        &runtime,
        Path::new("/usr/bin/python"),
        false,
        &second_hash,
    );
    assert_ne!(
        first_dir, second_dir,
        "cache dir should vary when build hash changes"
    );
    Ok(())
}

#[test]
fn reuses_cached_wheel_and_installs_scripts() -> Result<()> {
    let effects = SystemEffects::new();
    let python = match effects.python().detect_interpreter() {
        Ok(path) => path,
        Err(_) => return Ok(()),
    };

    let temp = tempdir()?;
    let project_root = temp.path().join("cached");
    fs::create_dir_all(&project_root)?;
    fs::write(
        project_root.join("pyproject.toml"),
        r#"[project]
name = "demo-wheel"
version = "0.1.0"
requires-python = ">=3.11"

[build-system]
requires = ["maturin>=1.0"]
build-backend = "maturin"
"#,
    )?;

    let snapshot = ProjectSnapshot::read_from(&project_root)?;
    let runtime = RuntimeMetadata {
        path: python.clone(),
        version: "3.12.0".into(),
        platform: "test-platform".into(),
    };
    let builder = builder_identity_for_runtime(&runtime)?;
    let build_hash = project_build_hash(
        &runtime,
        &snapshot,
        Path::new(&python),
        false,
        &builder.builder_id,
    )?;
    let cache_root = temp.path().join("cache");
    let cache_dir = project_wheel_cache_dir(
        &cache_root,
        &snapshot,
        &runtime,
        Path::new(&python),
        false,
        &build_hash,
    );
    let ensure_dir = project_wheel_cache_dir(
        &cache_root,
        &snapshot,
        &runtime,
        Path::new(&runtime.path),
        false,
        &build_hash,
    );
    assert_eq!(cache_dir, ensure_dir, "cache dir should be stable");
    fs::create_dir_all(&cache_dir)?;
    let wheel_path = cache_dir.join("demo_wheel-0.1.0-py3-none-any.whl");
    let file = File::create(&wheel_path)?;
    let mut zip = zip::ZipWriter::new(file);
    let opts = FileOptions::default();
    zip.start_file("demo_wheel-0.1.0.dist-info/METADATA", opts)?;
    zip.write_all(b"Name: demo-wheel\nVersion: 0.1.0\n")?;
    zip.start_file("demo_wheel-0.1.0.dist-info/entry_points.txt", opts)?;
    zip.write_all(b"[console_scripts]\ndemo-wheel = demo.cli:main\n")?;
    zip.finish()?;
    let sha256 = compute_file_sha256(&wheel_path)?;
    persist_wheel_metadata(&cache_dir, &wheel_path, &sha256, "demo-wheel", "0.1.0")?;
    assert!(
        cached_project_wheel(&cache_dir)?.is_some(),
        "cached wheel should be detected"
    );

    let env_root = project_root.join(".px").join("env");
    fs::create_dir_all(env_root.join("bin"))?;
    let owner = OwnerId {
        owner_type: OwnerType::ProjectEnv,
        owner_id: "test-owner".into(),
    };
    ensure_project_wheel_scripts(&cache_root, &snapshot, &env_root, &runtime, &owner, None)?;

    let script = env_root.join("bin").join("demo-wheel");
    assert!(
        script.exists(),
        "console script should be materialized from cached wheel"
    );
    let contents = fs::read_to_string(script)?;
    assert!(
        contents.contains("demo.cli"),
        "script content should reflect entry point"
    );
    assert!(cache_dir.join("wheel.json").exists());
    Ok(())
}

#[test]
fn installs_scripts_from_cached_maturin_wheel() -> Result<()> {
    use std::io::Write as _;

    let temp = tempdir()?;
    let project_root = temp.path().join("maturin-demo");
    fs::create_dir_all(&project_root)?;
    fs::write(
        project_root.join("pyproject.toml"),
        r#"[project]
name = "maturin-demo"
version = "0.1.0"
requires-python = ">=3.11"

[build-system]
requires = ["maturin>=1.0"]
build-backend = "maturin"
"#,
    )?;

    let snapshot = ProjectSnapshot::read_from(&project_root)?;
    let runtime = RuntimeMetadata {
        path: "/usr/bin/python".into(),
        version: "3.12.0".into(),
        platform: "test-platform".into(),
    };
    let builder = builder_identity_for_runtime(&runtime)?;
    let build_hash = project_build_hash(
        &runtime,
        &snapshot,
        Path::new("/usr/bin/python"),
        false,
        &builder.builder_id,
    )?;
    let cache_root = temp.path().join("cache");
    let wheel_dir = project_wheel_cache_dir(
        &cache_root,
        &snapshot,
        &runtime,
        Path::new("/usr/bin/python"),
        false,
        &build_hash,
    );
    fs::create_dir_all(&wheel_dir)?;
    let wheel_path = wheel_dir.join("maturin_demo-0.1.0-py3-none-any.whl");
    let file = File::create(&wheel_path)?;
    let mut zip = zip::ZipWriter::new(file);
    let opts = FileOptions::default();
    zip.start_file("maturin_demo-0.1.0.dist-info/METADATA", opts)?;
    zip.write_all(b"Name: maturin-demo\nVersion: 0.1.0\n")?;
    zip.start_file("maturin_demo-0.1.0.data/scripts/maturin-demo", opts)?;
    zip.write_all(b"echo built\n")?;
    zip.finish()?;
    let sha256 = compute_file_sha256(&wheel_path)?;
    persist_wheel_metadata(&wheel_dir, &wheel_path, &sha256, "maturin-demo", "0.1.0")?;

    let env_root = project_root.join(".px").join("env");
    fs::create_dir_all(env_root.join("bin"))?;

    let owner = OwnerId {
        owner_type: OwnerType::ProjectEnv,
        owner_id: "test-owner".into(),
    };
    ensure_project_wheel_scripts(&cache_root, &snapshot, &env_root, &runtime, &owner, None)?;

    let script = env_root.join("bin").join("maturin-demo");
    assert!(
        script.exists(),
        "script should be installed from cached wheel"
    );
    let contents = fs::read_to_string(&script)?;
    assert!(
        contents.contains("built"),
        "wheel script should be copied into env bin"
    );
    Ok(())
}

#[test]
fn reuses_cached_maturin_wheel_when_build_env_changes() -> Result<()> {
    use std::io::Write as _;

    let saved_rustflags = env::var("RUSTFLAGS").ok();
    env::set_var("RUSTFLAGS", "first-hash");

    let temp = tempdir()?;
    let project_root = temp.path().join("maturin-change");
    fs::create_dir_all(&project_root)?;
    fs::write(
        project_root.join("pyproject.toml"),
        r#"[project]
name = "maturin-change"
version = "0.1.0"
requires-python = ">=3.11"

[build-system]
requires = ["maturin>=1.0"]
build-backend = "maturin"
"#,
    )?;

    let snapshot = ProjectSnapshot::read_from(&project_root)?;
    let runtime = RuntimeMetadata {
        path: "/usr/bin/python".into(),
        version: "3.12.0".into(),
        platform: "test-platform".into(),
    };
    let builder = builder_identity_for_runtime(&runtime)?;
    let cache_root = temp.path().join("cache");
    let first_hash = project_build_hash(
        &runtime,
        &snapshot,
        Path::new(&runtime.path),
        false,
        &builder.builder_id,
    )?;
    let first_dir = project_wheel_cache_dir(
        &cache_root,
        &snapshot,
        &runtime,
        Path::new(&runtime.path),
        false,
        &first_hash,
    );
    fs::create_dir_all(&first_dir)?;
    let wheel_path = first_dir.join("maturin_change-0.1.0-py3-none-any.whl");
    let file = File::create(&wheel_path)?;
    let mut zip = zip::ZipWriter::new(file);
    let opts = FileOptions::default();
    zip.start_file("maturin_change-0.1.0.dist-info/METADATA", opts)?;
    zip.write_all(b"Name: maturin-change\nVersion: 0.1.0\n")?;
    zip.start_file("maturin_change-0.1.0.dist-info/entry_points.txt", opts)?;
    zip.write_all(b"[console_scripts]\nmaturin-change = demo.cli:main\n")?;
    zip.finish()?;
    let sha256 = compute_file_sha256(&wheel_path)?;
    persist_wheel_metadata(&first_dir, &wheel_path, &sha256, "maturin-change", "0.1.0")?;

    env::set_var("RUSTFLAGS", "second-hash");
    let second_hash = project_build_hash(
        &runtime,
        &snapshot,
        Path::new(&runtime.path),
        false,
        &builder.builder_id,
    )?;
    assert_ne!(
        first_hash, second_hash,
        "build hash should vary with build env"
    );

    let env_root = project_root.join(".px").join("env");
    fs::create_dir_all(env_root.join("bin"))?;
    let owner = OwnerId {
        owner_type: OwnerType::ProjectEnv,
        owner_id: "test-owner".into(),
    };
    ensure_project_wheel_scripts(&cache_root, &snapshot, &env_root, &runtime, &owner, None)?;

    let script = env_root.join("bin").join("maturin-change");
    assert!(
        script.exists(),
        "script should be installed from cached wheel even when build env changes"
    );
    let cache_root = cache_root
        .join("project-wheels")
        .join(normalize_project_name(&snapshot.name));
    let mut copied = false;
    if let Ok(entries) = fs::read_dir(&cache_root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path == first_dir || !path.is_dir() {
                continue;
            }
            if path.join("wheel.json").exists() {
                let candidate = path.join("maturin_change-0.1.0-py3-none-any.whl");
                if candidate.exists() && compute_file_sha256(&candidate)? == sha256 {
                    copied = true;
                    break;
                }
            }
        }
    }
    assert!(
        copied,
        "cached wheel should be copied into the current build cache directory"
    );

    match saved_rustflags {
        Some(prev) => env::set_var("RUSTFLAGS", prev),
        None => env::remove_var("RUSTFLAGS"),
    }

    Ok(())
}
