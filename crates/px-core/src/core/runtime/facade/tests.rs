use super::*;
use crate::api::{GlobalOptions, SystemEffects};
use crate::builder::builder_identity_for_runtime;
use crate::core::runtime::effects::Effects;
use crate::CommandContext;
use crate::InstallUserError;
use crate::{OwnerId, OwnerType};
use anyhow::{anyhow, Result};
use px_domain::api::{render_lockfile, ProjectSnapshot, PxOptions, ResolvedDependency};
use serde_json::{json, Value};
use std::env;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use tempfile::tempdir;
use zip::write::FileOptions;

use super::context::pep440_from_describe;
use super::env_materialize::{
    cached_project_wheel, compute_file_sha256, ensure_project_wheel_scripts,
    load_editable_project_metadata, materialize_wheel_scripts, normalize_project_name,
    persist_wheel_metadata, project_build_hash, project_site_env, project_wheel_cache_dir,
    uses_maturin_backend, uv_cli_candidates, write_project_metadata_stub, write_sitecustomize,
    SETUPTOOLS_SEED_VERSION, UV_SEED_VERSION,
};
use super::sandbox::apply_system_lib_compatibility;
use crate::core::sandbox::env_root_from_site_packages;

static PIP_ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[test]
fn base_env_sets_pyc_cache_prefix() -> Result<()> {
    env::remove_var("PYTHONPYCACHEPREFIX");
    let temp = tempdir()?;
    let prefix = temp.path().join("pyc-prefix");
    let ctx = PythonContext {
        project_root: temp.path().to_path_buf(),
        project_name: "demo".to_string(),
        python: "python".into(),
        pythonpath: temp.path().display().to_string(),
        allowed_paths: vec![temp.path().to_path_buf()],
        site_bin: None,
        pep582_bin: Vec::new(),
        pyc_cache_prefix: Some(prefix.clone()),
        px_options: PxOptions::default(),
    };
    let envs = ctx.base_env(&json!({}))?;
    assert!(
        envs.iter().all(|(key, _)| key != "PYTHONDONTWRITEBYTECODE"),
        "base env should not disable bytecode writes"
    );
    let value = envs
        .iter()
        .find(|(key, _)| key == "PYTHONPYCACHEPREFIX")
        .map(|(_, value)| value.clone());
    assert_eq!(value, Some(prefix.display().to_string()));
    assert!(
        prefix.exists(),
        "expected cache prefix {} to exist",
        prefix.display()
    );
    Ok(())
}

#[test]
fn system_lib_compat_caps_unpinned_gdal() -> Result<()> {
    let mut system_deps = crate::core::system_deps::SystemDeps::default();
    system_deps
        .apt_versions
        .insert("libgdal-dev".to_string(), "3.6.2+dfsg-1+b2".to_string());
    let reqs = vec!["gdal".to_string()];
    let out = apply_system_lib_compatibility(reqs, &system_deps)?;
    assert_eq!(out, vec!["gdal<=3.6.2".to_string()]);
    Ok(())
}

#[test]
fn system_lib_compat_preserves_lower_bounds() -> Result<()> {
    let mut system_deps = crate::core::system_deps::SystemDeps::default();
    system_deps
        .apt_versions
        .insert("libgdal-dev".to_string(), "3.6.2".to_string());
    let reqs = vec!["gdal>=3.0".to_string(), "psycopg2".to_string()];
    let out = apply_system_lib_compatibility(reqs, &system_deps)?;
    assert_eq!(
        out,
        vec!["gdal>=3.0,<=3.6.2".to_string(), "psycopg2".to_string()]
    );
    Ok(())
}

#[test]
fn system_lib_compat_errors_on_incompatible_pin() {
    let mut system_deps = crate::core::system_deps::SystemDeps::default();
    system_deps
        .apt_versions
        .insert("libgdal-dev".to_string(), "3.6.2".to_string());
    let reqs = vec!["gdal==3.8.0".to_string()];
    let err = apply_system_lib_compatibility(reqs, &system_deps).unwrap_err();
    let Some(install_err) = err.downcast_ref::<InstallUserError>() else {
        panic!("expected InstallUserError, got {err:?}");
    };
    let hint = install_err
        .details()
        .get("hint")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(hint.contains("base provides libgdal 3.6.2"));
    assert!(hint.contains("requested gdal==3.8.0"));
}

#[test]
fn materialize_scripts_from_dist_directory() -> Result<()> {
    let temp = tempdir()?;
    let artifact = temp.path().join("demo-0.1.0.dist");
    let dist_info = artifact.join("demo-0.1.0.dist-info");
    let data_scripts = artifact.join("demo-0.1.0.data").join("scripts");
    fs::create_dir_all(&dist_info)?;
    fs::create_dir_all(&data_scripts)?;
    fs::write(
        dist_info.join("entry_points.txt"),
        "[console_scripts]\nalpha = demo.cli:main\n[gui_scripts]\nbeta = demo.gui:run\n",
    )?;
    fs::write(data_scripts.join("copied.sh"), "echo copied\n")?;

    let bin_dir = temp.path().join("bin");
    materialize_wheel_scripts(&artifact, &bin_dir, Some(Path::new("/custom/python")))?;

    let alpha = fs::read_to_string(bin_dir.join("alpha"))?;
    assert!(
        alpha.starts_with("#!/custom/python"),
        "shebang honors python"
    );
    assert!(alpha.contains("demo.cli"));
    let beta = fs::read_to_string(bin_dir.join("beta"))?;
    assert!(beta.contains("demo.gui"));
    let copied = fs::read_to_string(bin_dir.join("copied.sh"))?;
    assert!(copied.contains("copied"));
    Ok(())
}

#[test]
fn materialize_scripts_from_wheel_file() -> Result<()> {
    let temp = tempdir()?;
    let wheel_path = temp.path().join("demo-0.2.0-py3-none-any.whl");
    let file = fs::File::create(&wheel_path)?;
    let mut zip = zip::ZipWriter::new(file);
    let opts = FileOptions::default();
    zip.start_file("demo-0.2.0.dist-info/entry_points.txt", opts)?;
    zip.write_all(b"[console_scripts]\ngamma = demo.core:run\n")?;
    zip.start_file("demo-0.2.0.data/scripts/helper.sh", opts)?;
    zip.write_all(b"echo helper\n")?;
    zip.finish()?;

    let bin_dir = temp.path().join("wheel-bin");
    materialize_wheel_scripts(&wheel_path, &bin_dir, None)?;

    let gamma = fs::read_to_string(bin_dir.join("gamma"))?;
    assert!(gamma.starts_with("#!/usr/bin/env python3"));
    assert!(gamma.contains("demo.core"));
    let helper = fs::read_to_string(bin_dir.join("helper.sh"))?;
    assert!(helper.contains("helper"));
    Ok(())
}

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

#[test]
fn refresh_project_site_bootstraps_pip() -> Result<()> {
    let _lock = PIP_ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
    if env::var("PX_ONLINE").unwrap_or_default() != "1" {
        eprintln!("skipping pip bootstrap test (PX_ONLINE!=1)");
        return Ok(());
    }
    let saved_env = vec![
        ("PX_CACHE_PATH", env::var("PX_CACHE_PATH").ok()),
        ("PX_STORE_PATH", env::var("PX_STORE_PATH").ok()),
        ("PX_ENVS_PATH", env::var("PX_ENVS_PATH").ok()),
    ];
    let temp_env = tempdir()?;
    let env_root = temp_env.path();
    env::set_var("PX_CACHE_PATH", env_root.join("cache"));
    env::set_var("PX_STORE_PATH", env_root.join("store"));
    env::set_var("PX_ENVS_PATH", env_root.join("envs"));

    let temp = tempdir()?;
    let project_root = temp.path();
    fs::write(
        project_root.join("pyproject.toml"),
        r#"[project]
name = "pip-bootstrap"
version = "0.0.0"
requires-python = ">=3.11"

[build-system]
requires = ["setuptools"]
build-backend = "setuptools.build_meta"
"#,
    )?;
    let snapshot = ProjectSnapshot::read_from(project_root)?;
    let lock = render_lockfile(&snapshot, &Vec::<ResolvedDependency>::new(), PX_VERSION)?;
    fs::write(project_root.join("px.lock"), lock)?;

    let global = GlobalOptions::default();
    let ctx = CommandContext::new(&global, Arc::new(SystemEffects::new()))?;
    refresh_project_site(&snapshot, &ctx)?;

    let state = load_project_state(ctx.fs(), project_root)?;
    let env = state.current_env.expect("env stored");
    let site_packages = PathBuf::from(&env.site_packages);
    let env_root = env
        .env_path
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| {
            site_packages
                .parent()
                .and_then(Path::parent)
                .and_then(Path::parent)
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| PathBuf::from(&env.site_packages));
    assert!(
        site_packages.join("pip").exists(),
        "pip package should be installed into the px site"
    );
    assert!(
        env_root.join("bin").join("pip").exists() || env_root.join("bin").join("pip3").exists(),
        "pip entrypoint should be generated under site/bin"
    );
    validate_cas_environment(&env).expect("pip bootstrap should keep CAS site in a clean state");

    for (key, value) in saved_env {
        match value {
            Some(prev) => env::set_var(key, prev),
            None => env::remove_var(key),
        }
    }

    Ok(())
}

#[test]
fn refresh_project_site_bootstraps_setuptools() -> Result<()> {
    let _lock = PIP_ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
    if env::var("PX_ONLINE").unwrap_or_default() != "1" {
        eprintln!("skipping setuptools bootstrap test (PX_ONLINE!=1)");
        return Ok(());
    }
    let saved_env = vec![
        ("PX_CACHE_PATH", env::var("PX_CACHE_PATH").ok()),
        ("PX_STORE_PATH", env::var("PX_STORE_PATH").ok()),
        ("PX_ENVS_PATH", env::var("PX_ENVS_PATH").ok()),
    ];
    let temp_env = tempdir()?;
    let env_root = temp_env.path();
    env::set_var("PX_CACHE_PATH", env_root.join("cache"));
    env::set_var("PX_STORE_PATH", env_root.join("store"));
    env::set_var("PX_ENVS_PATH", env_root.join("envs"));

    let temp = tempdir()?;
    let project_root = temp.path();
    fs::write(
        project_root.join("pyproject.toml"),
        r#"[project]
name = "seed-demo"
version = "0.0.0"
requires-python = ">=3.11"

[build-system]
requires = ["setuptools"]
build-backend = "setuptools.build_meta"
"#,
    )?;
    let snapshot = ProjectSnapshot::read_from(project_root)?;
    let lock = render_lockfile(&snapshot, &Vec::<ResolvedDependency>::new(), PX_VERSION)?;
    fs::write(project_root.join("px.lock"), lock)?;

    let global = GlobalOptions::default();
    let ctx = CommandContext::new(&global, Arc::new(SystemEffects::new()))?;
    refresh_project_site(&snapshot, &ctx)?;

    let state = load_project_state(ctx.fs(), project_root)?;
    let env = state.current_env.expect("env stored");
    let site_packages = PathBuf::from(&env.site_packages);
    let env_root = env
        .env_path
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| env_root_from_site_packages(&site_packages))
        .unwrap_or_else(|| site_packages.clone());
    let env_python = PathBuf::from(&env.python.path);
    let envs = project_site_env(&ctx, &snapshot, &env_root, &env_python)?;
    let script = "import importlib.metadata, pkg_resources, setuptools; print(importlib.metadata.version('setuptools'))";
    let output = ctx.python_runtime().run_command(
        env_python
            .to_str()
            .ok_or_else(|| anyhow!("invalid python path"))?,
        &["-c".to_string(), script.to_string()],
        &envs,
        project_root,
    )?;
    assert_eq!(
        output.code, 0,
        "setuptools import should succeed: {} {}",
        output.stdout, output.stderr
    );
    assert_eq!(
        output.stdout.trim(),
        SETUPTOOLS_SEED_VERSION,
        "expected seeded setuptools version"
    );
    validate_cas_environment(&env).expect("setuptools bootstrap should leave CAS site clean");

    for (key, value) in saved_env {
        match value {
            Some(prev) => env::set_var(key, prev),
            None => env::remove_var(key),
        }
    }

    Ok(())
}

#[test]
fn refresh_project_site_bootstraps_uv_when_uv_lock_present() -> Result<()> {
    let _lock = PIP_ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
    if env::var("PX_ONLINE").unwrap_or_default() != "1" {
        eprintln!("skipping uv bootstrap test (PX_ONLINE!=1)");
        return Ok(());
    }
    let saved_env = vec![
        ("PX_CACHE_PATH", env::var("PX_CACHE_PATH").ok()),
        ("PX_STORE_PATH", env::var("PX_STORE_PATH").ok()),
        ("PX_ENVS_PATH", env::var("PX_ENVS_PATH").ok()),
    ];
    let temp_env = tempdir()?;
    let env_root = temp_env.path();
    env::set_var("PX_CACHE_PATH", env_root.join("cache"));
    env::set_var("PX_STORE_PATH", env_root.join("store"));
    env::set_var("PX_ENVS_PATH", env_root.join("envs"));

    let temp = tempdir()?;
    let project_root = temp.path();
    fs::write(
        project_root.join("pyproject.toml"),
        r#"[project]
name = "uv-seed-demo"
version = "0.0.0"
requires-python = ">=3.11"

[build-system]
requires = ["setuptools"]
build-backend = "setuptools.build_meta"
"#,
    )?;
    fs::write(project_root.join("uv.lock"), "version = 1\n")?;
    let snapshot = ProjectSnapshot::read_from(project_root)?;
    let lock = render_lockfile(&snapshot, &Vec::<ResolvedDependency>::new(), PX_VERSION)?;
    fs::write(project_root.join("px.lock"), lock)?;

    let global = GlobalOptions::default();
    let ctx = CommandContext::new(&global, Arc::new(SystemEffects::new()))?;
    refresh_project_site(&snapshot, &ctx)?;

    let state = load_project_state(ctx.fs(), project_root)?;
    let env = state.current_env.expect("env stored");
    let site_packages = PathBuf::from(&env.site_packages);
    let env_root = env
        .env_path
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| env_root_from_site_packages(&site_packages))
        .unwrap_or_else(|| site_packages.clone());
    let env_python = PathBuf::from(&env.python.path);
    let envs = project_site_env(&ctx, &snapshot, &env_root, &env_python)?;
    let uv_path = uv_cli_candidates(&env_root)
        .into_iter()
        .find(|path| path.exists())
        .expect("uv cli available");
    let output = Command::new(&uv_path).arg("--version").output()?;
    assert!(
        output.status.success(),
        "uv cli should execute: stdout={} stderr={}",
        output.stdout.escape_ascii(),
        output.stderr.escape_ascii()
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(UV_SEED_VERSION),
        "uv version should match seed; stdout={stdout}"
    );
    let script = "import importlib.metadata as im; print(im.version('uv'))";
    let probe = ctx.python_runtime().run_command(
        env_python
            .to_str()
            .ok_or_else(|| anyhow!("invalid python path"))?,
        &["-c".to_string(), script.to_string()],
        &envs,
        project_root,
    )?;
    assert_eq!(
        probe.code, 0,
        "uv module should import: {} {}",
        probe.stdout, probe.stderr
    );
    assert_eq!(
        probe.stdout.trim(),
        UV_SEED_VERSION,
        "expected uv module version from seed"
    );
    validate_cas_environment(&env).expect("uv bootstrap should leave CAS site clean");

    for (key, value) in saved_env {
        match value {
            Some(prev) => env::set_var(key, prev),
            None => env::remove_var(key),
        }
    }

    Ok(())
}

#[test]
fn refresh_project_site_seeds_setuptools_with_no_ensurepip() -> Result<()> {
    let _lock = PIP_ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
    if env::var("PX_ONLINE").unwrap_or_default() != "1" {
        eprintln!("skipping setuptools bootstrap test (PX_ONLINE!=1)");
        return Ok(());
    }
    let saved_env = vec![
        ("PX_CACHE_PATH", env::var("PX_CACHE_PATH").ok()),
        ("PX_STORE_PATH", env::var("PX_STORE_PATH").ok()),
        ("PX_ENVS_PATH", env::var("PX_ENVS_PATH").ok()),
        ("PX_NO_ENSUREPIP", env::var("PX_NO_ENSUREPIP").ok()),
    ];
    let temp_env = tempdir()?;
    let env_root = temp_env.path();
    env::set_var("PX_CACHE_PATH", env_root.join("cache"));
    env::set_var("PX_STORE_PATH", env_root.join("store"));
    env::set_var("PX_ENVS_PATH", env_root.join("envs"));
    env::set_var("PX_NO_ENSUREPIP", "1");

    let temp = tempdir()?;
    let project_root = temp.path();
    fs::write(
        project_root.join("pyproject.toml"),
        r#"[project]
name = "seed-demo"
version = "0.0.0"
requires-python = ">=3.11"

[build-system]
requires = ["setuptools"]
build-backend = "setuptools.build_meta"
"#,
    )?;
    let snapshot = ProjectSnapshot::read_from(project_root)?;
    let lock = render_lockfile(&snapshot, &Vec::<ResolvedDependency>::new(), PX_VERSION)?;
    fs::write(project_root.join("px.lock"), lock)?;

    let global = GlobalOptions::default();
    let ctx = CommandContext::new(&global, Arc::new(SystemEffects::new()))?;
    refresh_project_site(&snapshot, &ctx)?;

    let state = load_project_state(ctx.fs(), project_root)?;
    let env = state.current_env.expect("env stored");
    let site_packages = PathBuf::from(&env.site_packages);
    let env_root = env
        .env_path
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| env_root_from_site_packages(&site_packages))
        .unwrap_or_else(|| site_packages.clone());
    let env_python = PathBuf::from(&env.python.path);
    let envs = project_site_env(&ctx, &snapshot, &env_root, &env_python)?;
    let script = "import importlib.metadata, pkg_resources, setuptools, pip; print(importlib.metadata.version('setuptools'))";
    let output = ctx.python_runtime().run_command(
        env_python
            .to_str()
            .ok_or_else(|| anyhow!("invalid python path"))?,
        &["-c".to_string(), script.to_string()],
        &envs,
        project_root,
    )?;
    assert_eq!(
        output.code, 0,
        "setuptools import should succeed even when PX_NO_ENSUREPIP=1: {} {}",
        output.stdout, output.stderr
    );
    assert_eq!(
        output.stdout.trim(),
        SETUPTOOLS_SEED_VERSION,
        "expected seeded setuptools version"
    );
    validate_cas_environment(&env).expect("bootstrap should leave CAS site clean");

    for (key, value) in saved_env {
        match value {
            Some(prev) => env::set_var(key, prev),
            None => env::remove_var(key),
        }
    }

    Ok(())
}

#[test]
fn refresh_project_site_preserves_editable_pip_entrypoints() -> Result<()> {
    let _lock = PIP_ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
    if env::var("PX_ONLINE").unwrap_or_default() != "1" {
        eprintln!("skipping editable pip preservation test (PX_ONLINE!=1)");
        return Ok(());
    }
    let saved_env = vec![
        ("PX_CACHE_PATH", env::var("PX_CACHE_PATH").ok()),
        ("PX_STORE_PATH", env::var("PX_STORE_PATH").ok()),
        ("PX_ENVS_PATH", env::var("PX_ENVS_PATH").ok()),
    ];
    let temp_env = tempdir()?;
    let env_root = temp_env.path();
    env::set_var("PX_CACHE_PATH", env_root.join("cache"));
    env::set_var("PX_STORE_PATH", env_root.join("store"));
    env::set_var("PX_ENVS_PATH", env_root.join("envs"));

    let temp = tempdir()?;
    let project_root = temp.path();
    fs::write(
        project_root.join("pyproject.toml"),
        r#"[project]
name = "pip"
version = "0.0.0"
requires-python = ">=3.11"
dependencies = []

[project.scripts]
pip = "pip._internal.cli.main:main"
pip3 = "pip._internal.cli.main:main"

[build-system]
requires = ["setuptools"]
build-backend = "setuptools.build_meta"

[tool.px]
"#,
    )?;
    let pkg_root = project_root
        .join("src")
        .join("pip")
        .join("_internal")
        .join("cli");
    fs::create_dir_all(&pkg_root)?;
    fs::write(pkg_root.join("__init__.py"), "")?;
    fs::write(pkg_root.join("main.py"), "def main():\n    return 0\n")?;
    let pkg_base = project_root.join("src").join("pip").join("_internal");
    fs::write(pkg_base.join("__init__.py"), "")?;
    fs::write(project_root.join("src").join("pip").join("__init__.py"), "")?;

    let snapshot = ProjectSnapshot::read_from(project_root)?;
    let lock = render_lockfile(&snapshot, &Vec::<ResolvedDependency>::new(), PX_VERSION)?;
    fs::write(project_root.join("px.lock"), lock)?;

    let global = GlobalOptions::default();
    let ctx = CommandContext::new(&global, Arc::new(SystemEffects::new()))?;
    refresh_project_site(&snapshot, &ctx)?;

    let state = load_project_state(ctx.fs(), project_root)?;
    let env = state.current_env.expect("env stored");
    let site_packages = PathBuf::from(&env.site_packages);
    let env_root = env
        .env_path
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| {
            site_packages
                .parent()
                .and_then(Path::parent)
                .and_then(Path::parent)
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| PathBuf::from(&env.site_packages));

    let pip_entry = env_root.join("bin").join("pip");
    let pip_contents = fs::read_to_string(&pip_entry)?;
    assert!(
        pip_contents.contains("pip._internal.cli.main"),
        "editable pip entrypoint should target project module"
    );
    assert!(
        !pip_contents.starts_with("#!/bin/sh"),
        "runtime ensurepip shim should not replace project pip script"
    );
    assert!(
        env_root
            .read_dir()?
            .flatten()
            .any(|entry| entry.path().join("PX-EDITABLE").exists()),
        "editable dist-info should be preserved for pip"
    );

    for (key, value) in saved_env {
        match value {
            Some(prev) => env::set_var(key, prev),
            None => env::remove_var(key),
        }
    }

    Ok(())
}

#[test]
fn validate_env_detects_untracked_site_packages_entries() -> Result<()> {
    let _lock = PIP_ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
    if env::var("PX_ONLINE").unwrap_or_default() != "1" {
        eprintln!("skipping validate env drift test (PX_ONLINE!=1)");
        return Ok(());
    }
    let saved_env = vec![
        ("PX_CACHE_PATH", env::var("PX_CACHE_PATH").ok()),
        ("PX_STORE_PATH", env::var("PX_STORE_PATH").ok()),
        ("PX_ENVS_PATH", env::var("PX_ENVS_PATH").ok()),
    ];
    let temp_env = tempdir()?;
    let env_root = temp_env.path();
    env::set_var("PX_CACHE_PATH", env_root.join("cache"));
    env::set_var("PX_STORE_PATH", env_root.join("store"));
    env::set_var("PX_ENVS_PATH", env_root.join("envs"));

    let temp = tempdir()?;
    let project_root = temp.path();
    fs::write(
        project_root.join("pyproject.toml"),
        r#"[project]
name = "drift-demo"
version = "0.0.0"
requires-python = ">=3.11"
dependencies = []

[build-system]
requires = ["setuptools"]
build-backend = "setuptools.build_meta"
"#,
    )?;
    let snapshot = ProjectSnapshot::read_from(project_root)?;
    let lock = render_lockfile(&snapshot, &Vec::<ResolvedDependency>::new(), PX_VERSION)?;
    fs::write(project_root.join("px.lock"), lock)?;

    let global = GlobalOptions::default();
    let ctx = CommandContext::new(&global, Arc::new(SystemEffects::new()))?;
    refresh_project_site(&snapshot, &ctx)?;

    let state = load_project_state(ctx.fs(), project_root)?;
    let env = state.current_env.expect("env stored");
    let site_packages = PathBuf::from(&env.site_packages);
    fs::create_dir_all(&site_packages)?;
    fs::write(site_packages.join("stray.py"), "print('oops')\n")?;

    let err = validate_cas_environment(&env).unwrap_err();
    let user = err
        .downcast::<InstallUserError>()
        .expect("user-facing error");
    assert_eq!(
        user.details
            .get("reason")
            .and_then(serde_json::Value::as_str),
        Some("env_outdated")
    );
    assert!(
        user.message.contains("site-packages"),
        "expected site-packages drift message, got {}",
        user.message
    );

    for (key, value) in saved_env {
        match value {
            Some(prev) => env::set_var(key, prev),
            None => env::remove_var(key),
        }
    }

    Ok(())
}

#[test]
fn site_dir_precedes_project_root_in_sys_path() -> Result<()> {
    let temp = tempdir()?;
    let project_root = temp.path();
    let site_dir = project_root.join("site");
    fs::create_dir_all(&site_dir)?;
    fs::write(site_dir.join("sitecustomize.py"), SITE_CUSTOMIZE)?;

    let dep_pkg = site_dir.join("deps");
    let dep_mod = dep_pkg.join("dep");
    fs::create_dir_all(&dep_mod)?;
    fs::write(dep_mod.join("__init__.py"), "VALUE = 'site'\n")?;
    fs::write(site_dir.join("px.pth"), format!("{}\n", dep_pkg.display()))?;

    // Namespace-like directory at the project root should not shadow site packages
    fs::create_dir_all(project_root.join("dep"))?;

    let effects = SystemEffects::new();
    let paths = build_pythonpath(effects.fs(), project_root, Some(site_dir.clone()))?;
    let allowed = env::join_paths(&paths.allowed_paths)
        .expect("allowed paths")
        .into_string()
        .expect("utf8 allowed paths");
    let allowed_env = allowed.clone();
    let python = match effects.python().detect_interpreter() {
        Ok(path) => path,
        Err(_) => return Ok(()),
    };

    let mut cmd = Command::new(&python);
    cmd.current_dir(project_root);
    cmd.env("PYTHONPATH", paths.pythonpath.clone());
    cmd.env("PX_ALLOWED_PATHS", allowed_env.clone());
    cmd.arg("-c").arg(
            "import importlib, json, os, sys; mod = importlib.import_module('dep'); \
             print(json.dumps({'file': getattr(mod, '__file__', ''), 'value': getattr(mod, 'VALUE', ''), 'prefix': sys.path[:3], 'env_py': os.environ.get('PYTHONPATH')}))",
        );
    let output = cmd.output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "python exited with {}: {}\n{}",
        output.status,
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );

    let payload: Value = serde_json::from_str(stdout.trim())?;
    let prefix: Vec<String> = payload
        .get("prefix")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(Value::as_str)
                .map(std::string::ToString::to_string)
                .collect()
        })
        .unwrap_or_default();
    let canonical_site = effects.fs().canonicalize(&site_dir)?;
    let canonical_site_str = canonical_site.display().to_string();
    let first_nonempty = if prefix.first().is_some_and(|entry| entry.is_empty()) {
        prefix.get(1).map(String::as_str)
    } else {
        prefix.first().map(String::as_str)
    };
    assert_eq!(first_nonempty, Some(canonical_site_str.as_str()));
    let value = payload
        .get("value")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert_eq!(value, "site");
    let env_py = payload
        .get("env_py")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert_eq!(env_py, allowed_env);
    let file = payload.get("file").and_then(Value::as_str).unwrap_or("");
    assert!(
        file.contains(dep_mod.to_string_lossy().as_ref()),
        "expected module to load from site packages, got {file}"
    );
    Ok(())
}

#[test]
fn build_pythonpath_refuses_legacy_site_fallback() -> Result<()> {
    let temp = tempdir()?;
    let project_root = temp.path();
    fs::create_dir_all(project_root.join(".px").join("site"))?;

    let err = match build_pythonpath(SystemEffects::new().fs(), project_root, None) {
        Ok(_) => panic!("missing CAS environment should be an error"),
        Err(err) => err,
    };
    let user = err
        .downcast::<InstallUserError>()
        .expect("expected user-facing error");
    assert_eq!(
        user.details
            .get("reason")
            .and_then(serde_json::Value::as_str),
        Some("missing_env"),
        "missing env should not fall back to .px/site"
    );
    Ok(())
}

#[test]
fn project_paths_precede_local_site_packages() -> Result<()> {
    let temp = tempdir()?;
    let project_root = temp.path();
    let site_dir = project_root.join("site");
    let site_packages = site_dir.join("site-packages");
    fs::create_dir_all(&site_packages)?;
    fs::write(site_dir.join("sitecustomize.py"), SITE_CUSTOMIZE)?;

    let site_pkg = site_packages.join("demo");
    fs::create_dir_all(&site_pkg)?;
    fs::write(site_pkg.join("__init__.py"), "VALUE = 'site'\n")?;

    let project_pkg = project_root.join("demo");
    fs::create_dir_all(&project_pkg)?;
    fs::write(project_pkg.join("__init__.py"), "VALUE = 'project'\n")?;

    let effects = SystemEffects::new();
    let paths = build_pythonpath(effects.fs(), project_root, Some(site_dir.clone()))?;
    let allowed_env = env::join_paths(&paths.allowed_paths)?
        .into_string()
        .expect("allowed paths");
    let python = match effects.python().detect_interpreter() {
        Ok(path) => path,
        Err(_) => return Ok(()),
    };

    let mut cmd = Command::new(&python);
    cmd.current_dir(project_root);
    cmd.env("PYTHONPATH", paths.pythonpath.clone());
    cmd.env("PX_ALLOWED_PATHS", allowed_env);
    cmd.env("PYTHONSAFEPATH", "1");
    cmd.arg("-c").arg(
            "import importlib, json, sys; mod = importlib.import_module('demo'); \
             print(json.dumps({'value': getattr(mod, 'VALUE', ''), 'file': getattr(mod, '__file__', ''), 'prefix': sys.path[:4]}))",
        );
    let output = cmd.output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "python exited with {}: {}\n{}",
        output.status,
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );

    let payload: Value = serde_json::from_str(stdout.trim())?;
    let value = payload
        .get("value")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert_eq!(value, "project");
    let file = payload.get("file").and_then(Value::as_str).unwrap_or("");
    assert!(
        file.contains(project_pkg.to_string_lossy().as_ref()),
        "expected project package, got {file}"
    );
    let prefix: Vec<PathBuf> = payload
        .get("prefix")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(Value::as_str)
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default();
    let proj_pos = prefix
        .iter()
        .position(|entry| fs::canonicalize(entry).ok() == Some(project_root.to_path_buf()));
    let site_pos = prefix
        .iter()
        .position(|entry| fs::canonicalize(entry).ok() == Some(site_packages.clone()));
    assert!(
        proj_pos < site_pos,
        "project path should precede site-packages in sys.path, got {:?}",
        prefix
    );
    Ok(())
}

#[test]
fn sitecustomize_filters_out_unallowed_paths() -> Result<()> {
    let temp = tempdir()?;
    let site_dir = temp.path().join("site");
    fs::create_dir_all(&site_dir)?;
    fs::write(site_dir.join("sitecustomize.py"), SITE_CUSTOMIZE)?;

    let effects = SystemEffects::new();
    let python = match effects.python().detect_interpreter() {
        Ok(path) => path,
        Err(_) => return Ok(()),
    };
    let version = Command::new(&python)
        .arg("-c")
        .arg("import sys; print(f\"{sys.version_info[0]}.{sys.version_info[1]}\")")
        .output()?;
    if !version.status.success() {
        return Ok(());
    }
    let version = String::from_utf8_lossy(&version.stdout).trim().to_string();

    let user_base = temp.path().join("userbase");
    let user_site = user_base
        .join("lib")
        .join(format!("python{version}"))
        .join("site-packages");
    fs::create_dir_all(&user_site)?;

    let mut cmd = Command::new(&python);
    cmd.current_dir(temp.path());
    cmd.env_clear();
    cmd.env("PYTHONPATH", site_dir.display().to_string());
    cmd.env("PX_ALLOWED_PATHS", site_dir.display().to_string());
    cmd.env("PYTHONUSERBASE", user_base.display().to_string());
    cmd.arg("-c")
        .arg("import json, sys; print(json.dumps(sys.path))");

    let output = cmd.output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "python exited with {}: {}\n{}",
        output.status,
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );

    let allowed_user_base = fs::canonicalize(&user_base)?;
    let paths: Vec<PathBuf> = serde_json::from_str(stdout.trim())?;
    assert!(
        paths
            .iter()
            .filter_map(|entry| fs::canonicalize(entry).ok())
            .all(|entry| !entry.starts_with(&allowed_user_base)),
        "user site paths should be filtered out of sys.path: {paths:?}"
    );
    Ok(())
}

#[test]
fn sitecustomize_uses_pythonpath_when_px_allowed_missing() -> Result<()> {
    let temp = tempdir()?;
    let site_dir = temp.path().join("site");
    fs::create_dir_all(&site_dir)?;
    fs::write(site_dir.join("sitecustomize.py"), SITE_CUSTOMIZE)?;

    let dep_dir = site_dir.join("deps");
    fs::create_dir_all(&dep_dir)?;
    fs::write(dep_dir.join("shim.py"), "VALUE = 'ok'\n")?;
    fs::write(site_dir.join("px.pth"), format!("{}\n", dep_dir.display()))?;

    let python = match SystemEffects::new().python().detect_interpreter() {
        Ok(path) => path,
        Err(_) => return Ok(()),
    };

    let mut cmd = Command::new(&python);
    cmd.current_dir(temp.path());
    cmd.env_clear();
    cmd.env("PYTHONPATH", site_dir.display().to_string());
    cmd.arg("-c")
        .arg("import json, sys, shim; print(json.dumps({'value': shim.VALUE, 'path': sys.path}))");
    let output = cmd.output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "python exited with {}: {}\n{}",
        output.status,
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );

    let payload: Value = serde_json::from_str(stdout.trim())?;
    let value = payload
        .get("value")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert_eq!(value, "ok");
    let paths: Vec<String> = payload
        .get("path")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(Value::as_str)
                .map(std::string::ToString::to_string)
                .collect()
        })
        .unwrap_or_default();
    let dep_canon = fs::canonicalize(&dep_dir)?;
    assert!(
        paths
            .iter()
            .any(|entry| fs::canonicalize(entry).ok() == Some(dep_canon.clone())),
        "px.pth entries should persist even when PX_ALLOWED_PATHS is unset; sys.path={paths:?}"
    );
    Ok(())
}

#[test]
fn sitecustomize_merges_pythonpath_when_px_allowed_set() -> Result<()> {
    let temp = tempdir()?;
    let site_dir = temp.path().join("site");
    fs::create_dir_all(&site_dir)?;
    fs::write(site_dir.join("sitecustomize.py"), SITE_CUSTOMIZE)?;

    let extra = temp.path().join("extra");
    fs::create_dir_all(&extra)?;
    fs::write(extra.join("shim.py"), "VALUE = 'ok'\n")?;

    let python = match SystemEffects::new().python().detect_interpreter() {
        Ok(path) => path,
        Err(_) => return Ok(()),
    };

    let mut cmd = Command::new(&python);
    cmd.current_dir(temp.path());
    cmd.env_clear();
    cmd.env("PX_ALLOWED_PATHS", site_dir.display().to_string());
    let pythonpath = env::join_paths([extra.clone(), site_dir.clone()])?;
    cmd.env("PYTHONPATH", pythonpath);
    cmd.arg("-c")
        .arg("import json, sys, shim; print(json.dumps({'value': shim.VALUE, 'path': sys.path}))");
    let output = cmd.output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "python exited with {}: {}\n{}",
        output.status,
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );

    let payload: Value = serde_json::from_str(stdout.trim())?;
    let value = payload
        .get("value")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert_eq!(value, "ok");
    let paths: Vec<String> = payload
        .get("path")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(Value::as_str)
                .map(std::string::ToString::to_string)
                .collect()
        })
        .unwrap_or_default();
    let extra_canon = fs::canonicalize(&extra)?;
    assert!(
        paths
            .iter()
            .any(|entry| fs::canonicalize(entry).ok() == Some(extra_canon.clone())),
        "extra PYTHONPATH entries should persist when PX_ALLOWED_PATHS is set; sys.path={paths:?}"
    );
    Ok(())
}

#[test]
fn sitecustomize_reinserts_cwd_when_script_dir_empty() -> Result<()> {
    let temp = tempdir()?;
    let site_dir = temp.path().join("site");
    fs::create_dir_all(&site_dir)?;
    fs::write(site_dir.join("sitecustomize.py"), SITE_CUSTOMIZE)?;

    let python = match SystemEffects::new().python().detect_interpreter() {
        Ok(path) => path,
        Err(_) => return Ok(()),
    };

    let mut cmd = Command::new(&python);
    cmd.current_dir(temp.path());
    cmd.env_clear();
    cmd.env("PYTHONPATH", site_dir.display().to_string());
    cmd.env("PX_ALLOWED_PATHS", site_dir.display().to_string());
    cmd.env("PYTHONSAFEPATH", "1");
    cmd.arg("-c").arg(
            "import sitecustomize, json, sys; print(json.dumps({'path': sys.path, 'site': getattr(sitecustomize, '__file__', '')}))",
        );
    let output = cmd.output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "python exited with {}: {}\n{}",
        output.status,
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );

    let payload: Value = serde_json::from_str(stdout.trim())?;
    let site_path = payload.get("site").and_then(Value::as_str).unwrap_or("");
    assert!(
        site_path.contains(site_dir.to_string_lossy().as_ref()),
        "sitecustomize should be loaded from the px site directory: {site_path}"
    );
    let paths: Vec<PathBuf> = payload
        .get("path")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(Value::as_str)
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default();
    let cwd = fs::canonicalize(temp.path())?;
    assert!(
        paths
            .iter()
            .any(|entry| fs::canonicalize(entry).ok() == Some(cwd.clone())),
        "current working directory should be retained in sys.path, got {paths:?}"
    );
    Ok(())
}

#[test]
fn editable_stub_exposes_project_version_metadata() -> Result<()> {
    let temp = tempdir()?;
    let project_root = temp.path();
    let pyproject = project_root.join("pyproject.toml");
    fs::write(
        &pyproject,
        r#"[project]
name = "demo-proj"
version = "1.2.3"
requires-python = ">=3.11"
dependencies = []

[build-system]
requires = ["setuptools"]
build-backend = "setuptools.build_meta"
"#,
    )?;
    let pkg_dir = project_root.join("src/demo_proj");
    fs::create_dir_all(&pkg_dir)?;
    fs::write(pkg_dir.join("__init__.py"), "__version__ = '1.2.3'\n")?;

    let snapshot = ProjectSnapshot::read_from(project_root)?;
    let site_dir = project_root.join(".px").join("env").join("site");
    let effects = SystemEffects::new();
    effects.fs().create_dir_all(&site_dir)?;
    write_sitecustomize(&site_dir, None, effects.fs())?;
    write_project_metadata_stub(&snapshot, &site_dir, effects.fs())?;

    let python = match effects.python().detect_interpreter() {
        Ok(path) => path,
        Err(_) => return Ok(()),
    };
    let allowed = env::join_paths([site_dir.clone(), project_root.join("src")])?;
    let allowed_str = allowed.to_string_lossy().into_owned();
    let mut cmd = Command::new(&python);
    cmd.current_dir(project_root);
    cmd.env("PYTHONPATH", allowed_str.clone());
    cmd.env("PX_ALLOWED_PATHS", allowed_str);
    cmd.arg("-c").arg(
            "import importlib.metadata, json; print(json.dumps({'version': importlib.metadata.version('demo-proj')}))",
        );
    let output = cmd.output()?;
    if !output.status.success() {
        return Ok(());
    }
    let payload: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(
        payload.get("version").and_then(Value::as_str),
        Some("1.2.3")
    );
    Ok(())
}

#[test]
fn editable_stub_writes_file_url_direct_url() -> Result<()> {
    let temp = tempdir()?;
    let project_root = temp.path();
    let pyproject = project_root.join("pyproject.toml");
    fs::write(
        &pyproject,
        r#"[project]
name = "demo-dir-url"
version = "0.1.0"
requires-python = ">=3.11"
dependencies = []

[build-system]
requires = ["setuptools"]
build-backend = "setuptools.build_meta"
"#,
    )?;
    let pkg_dir = project_root.join("demo_dir_url");
    fs::create_dir_all(&pkg_dir)?;
    fs::write(pkg_dir.join("__init__.py"), "")?;

    let snapshot = ProjectSnapshot::read_from(project_root)?;
    let site_dir = project_root.join(".px").join("env").join("site");
    let effects = SystemEffects::new();
    effects.fs().create_dir_all(&site_dir)?;
    write_project_metadata_stub(&snapshot, &site_dir, effects.fs())?;

    let contents =
        fs::read_to_string(site_dir.join("demo_dir_url-0.1.0.dist-info/direct_url.json"))?;
    let payload: Value = serde_json::from_str(&contents)?;
    let url = payload
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        url.starts_with("file://"),
        "direct_url.json should contain a file:// URL, got {url}"
    );
    Ok(())
}

#[test]
fn editable_stub_uses_source_version_when_manifest_missing() -> Result<()> {
    let temp = tempdir()?;
    let project_root = temp.path();
    let pyproject = project_root.join("pyproject.toml");
    fs::write(
        &pyproject,
        r#"[project]
name = "dynamic-demo"
dynamic = ["version"]
requires-python = ">=3.11"
dependencies = []

[build-system]
requires = ["setuptools"]
build-backend = "setuptools.build_meta"
"#,
    )?;
    let pkg_dir = project_root.join("src/dynamic_demo");
    fs::create_dir_all(&pkg_dir)?;
    fs::write(pkg_dir.join("__init__.py"), "__version__ = '9.9.9'\n")?;

    let snapshot = ProjectSnapshot::read_from(project_root)?;
    let site_dir = project_root.join(".px").join("env").join("site");
    let effects = SystemEffects::new();
    effects.fs().create_dir_all(&site_dir)?;
    write_sitecustomize(&site_dir, None, effects.fs())?;
    write_project_metadata_stub(&snapshot, &site_dir, effects.fs())?;

    let dist = site_dir
        .join("dynamic_demo-9.9.9.dist-info")
        .join("METADATA");
    let metadata = fs::read_to_string(&dist)?;
    assert!(
        metadata.contains("Version: 9.9.9"),
        "metadata should contain source-derived version"
    );
    Ok(())
}

#[test]
fn editable_stub_prefers_version_file_value() -> Result<()> {
    let temp = tempdir()?;
    let project_root = temp.path();
    let pyproject = project_root.join("pyproject.toml");
    fs::write(
        &pyproject,
        r#"[project]
name = "demo"
dynamic = ["version"]
requires-python = ">=3.11"
dependencies = []

[build-system]
requires = ["hatchling", "hatch-vcs"]
build-backend = "hatchling.build"

[tool.hatch.build.hooks.vcs]
version-file = "src/demo/version.py"
"#,
    )?;
    let pkg_dir = project_root.join("src/demo");
    fs::create_dir_all(&pkg_dir)?;
    fs::write(pkg_dir.join("__init__.py"), "")?;
    fs::write(
        pkg_dir.join("version.py"),
        "version = \"9.9.9\"\n__version__ = version\n",
    )?;

    let snapshot = ProjectSnapshot::read_from(project_root)?;
    let site_dir = project_root.join(".px").join("env").join("site");
    let effects = SystemEffects::new();
    effects.fs().create_dir_all(&site_dir)?;
    write_project_metadata_stub(&snapshot, &site_dir, effects.fs())?;

    let metadata = fs::read_to_string(site_dir.join("demo-9.9.9.dist-info").join("METADATA"))?;
    assert!(
        metadata.contains("Version: 9.9.9"),
        "metadata should use version from version-file stub"
    );
    Ok(())
}

#[test]
fn editable_stub_writes_console_scripts() -> Result<()> {
    let temp = tempdir()?;
    let project_root = temp.path();
    let pyproject = project_root.join("pyproject.toml");
    fs::write(
        &pyproject,
        r#"[project]
name = "demo"
dynamic = ["version"]
requires-python = ">=3.11"
dependencies = []

[project.scripts]
tox = "demo.run:main"

[build-system]
requires = ["hatchling"]
build-backend = "hatchling.build"
"#,
    )?;
    let pkg_dir = project_root.join("src/demo");
    fs::create_dir_all(&pkg_dir)?;
    fs::write(pkg_dir.join("__init__.py"), "__version__ = '1.0.0'\n")?;
    fs::write(pkg_dir.join("run.py"), "def main():\n    return 0\n")?;

    let snapshot = ProjectSnapshot::read_from(project_root)?;
    let site_dir = project_root.join(".px").join("env").join("site");
    let effects = SystemEffects::new();
    effects.fs().create_dir_all(&site_dir)?;
    write_project_metadata_stub(&snapshot, &site_dir, effects.fs())?;

    let script = site_dir.join("bin").join("tox");
    assert!(
        script.exists(),
        "console script should be generated for project entry points"
    );
    let contents = fs::read_to_string(script)?;
    assert!(
        contents.contains("demo.run"),
        "entrypoint should import target module"
    );
    Ok(())
}

#[test]
fn editable_stub_derives_hatch_vcs_version_without_version_file() -> Result<()> {
    if Command::new("git").arg("--version").status().is_err() {
        return Ok(());
    }
    let temp = tempdir()?;
    let project_root = temp.path();
    let pyproject = project_root.join("pyproject.toml");
    fs::write(
        &pyproject,
        r#"[project]
name = "demo"
dynamic = ["version"]
requires-python = ">=3.11"
dependencies = []

[build-system]
requires = ["hatchling", "hatch-vcs"]
build-backend = "hatchling.build"

[tool.hatch.version]
source = "vcs"
[tool.hatch.version.raw-options]
local_scheme = "no-local-version"
"#,
    )?;
    let pkg_dir = project_root.join("src/demo");
    fs::create_dir_all(&pkg_dir)?;
    fs::write(pkg_dir.join("__init__.py"), "")?;

    let git_status = Command::new("git")
        .args(["init", "-q"])
        .current_dir(project_root)
        .status()?;
    if !git_status.success() {
        return Ok(());
    }
    Command::new("git")
        .args(["add", "."])
        .current_dir(project_root)
        .status()?;
    Command::new("git")
        .args([
            "-c",
            "user.name=px",
            "-c",
            "user.email=px@example.com",
            "commit",
            "-m",
            "init",
        ])
        .current_dir(project_root)
        .status()?;

    let snapshot = ProjectSnapshot::read_from(project_root)?;
    let site_dir = project_root.join(".px").join("env").join("site");
    let effects = SystemEffects::new();
    effects.fs().create_dir_all(&site_dir)?;
    write_project_metadata_stub(&snapshot, &site_dir, effects.fs())?;

    let metadata = fs::read_to_string(site_dir.join("demo-0.0.0.dist-info").join("METADATA"))?;
    assert!(
        metadata.contains("Version: 0.0.0"),
        "hatch-vcs projects without version-file should derive a numeric version"
    );
    Ok(())
}

#[test]
fn ensure_version_file_populates_missing_file_from_git() -> Result<()> {
    if Command::new("git").arg("--version").status().is_err() {
        return Ok(());
    }
    let temp = tempdir()?;
    let manifest = temp.path().join("pyproject.toml");
    fs::write(
        &manifest,
        r#"[project]
name = "demo"
version = "0.1.0"
requires-python = ">=3.11"

[tool.hatch.build.hooks.vcs]
version-file = "demo/_version.py"
"#,
    )?;
    let demo_dir = temp.path().join("demo");
    fs::create_dir_all(&demo_dir)?;
    fs::write(demo_dir.join("__init__.py"), "")?;

    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("skipping version file test (git not available)");
        return Ok(());
    }

    assert!(
        Command::new("git")
            .arg("init")
            .current_dir(temp.path())
            .output()?
            .status
            .success(),
        "git init failed"
    );
    assert!(
        Command::new("git")
            .args(["config", "user.email", "ci@example.com"])
            .current_dir(temp.path())
            .output()?
            .status
            .success(),
        "git config email failed"
    );
    assert!(
        Command::new("git")
            .args(["config", "user.name", "CI"])
            .current_dir(temp.path())
            .output()?
            .status
            .success(),
        "git config name failed"
    );
    assert!(
        Command::new("git")
            .args(["add", "."])
            .current_dir(temp.path())
            .output()?
            .status
            .success(),
        "git add failed"
    );
    assert!(
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(temp.path())
            .output()?
            .status
            .success(),
        "git commit failed"
    );

    ensure_version_file(&manifest)?;
    let contents = fs::read_to_string(temp.path().join("demo/_version.py"))?;
    assert!(
        contents.contains("version = \"0.0.0+g"),
        "version file should be derived from git rev"
    );
    assert!(
        contents.contains("__version__ = version"),
        "git stub should alias __version__ to version"
    );
    Ok(())
}

#[test]
fn ensure_version_file_respects_hatch_git_describe_command() -> Result<()> {
    if Command::new("git").arg("--version").status().is_err() {
        return Ok(());
    }
    let temp = tempdir()?;
    let manifest = temp.path().join("pyproject.toml");
    fs::write(
        &manifest,
        r#"[project]
name = "demo"
version = "0.1.0"
requires-python = ">=3.11"

[tool.hatch.version.raw-options]
git_describe_command = ["git", "describe", "--tags", "--dirty", "--long", "--match", "demo-v*"]

[tool.hatch.build.hooks.vcs]
version-file = "demo/_version.py"
"#,
    )?;
    let demo_dir = temp.path().join("demo");
    fs::create_dir_all(&demo_dir)?;
    fs::write(demo_dir.join("__init__.py"), "")?;

    assert!(
        Command::new("git")
            .arg("init")
            .current_dir(temp.path())
            .output()?
            .status
            .success(),
        "git init failed"
    );
    assert!(
        Command::new("git")
            .args(["config", "user.email", "ci@example.com"])
            .current_dir(temp.path())
            .output()?
            .status
            .success(),
        "git config email failed"
    );
    assert!(
        Command::new("git")
            .args(["config", "user.name", "CI"])
            .current_dir(temp.path())
            .output()?
            .status
            .success(),
        "git config name failed"
    );
    assert!(
        Command::new("git")
            .args(["add", "."])
            .current_dir(temp.path())
            .output()?
            .status
            .success(),
        "git add failed"
    );
    assert!(
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(temp.path())
            .output()?
            .status
            .success(),
        "git commit failed"
    );
    assert!(
        Command::new("git")
            .args(["tag", "demo-v1.0.0"])
            .current_dir(temp.path())
            .output()?
            .status
            .success(),
        "tag demo-v1.0.0 failed"
    );

    fs::write(demo_dir.join("__init__.py"), "__version__ = '0.1.1'\n")?;
    assert!(
        Command::new("git")
            .args(["add", "."])
            .current_dir(temp.path())
            .output()?
            .status
            .success(),
        "git add second commit failed"
    );
    assert!(
        Command::new("git")
            .args(["commit", "-m", "second"])
            .current_dir(temp.path())
            .output()?
            .status
            .success(),
        "git commit second failed"
    );
    assert!(
        Command::new("git")
            .args(["tag", "other-v9.9.9"])
            .current_dir(temp.path())
            .output()?
            .status
            .success(),
        "tag other-v9.9.9 failed"
    );

    ensure_version_file(&manifest)?;
    let contents = fs::read_to_string(temp.path().join("demo/_version.py"))?;
    assert!(
        contents.contains("demo-v1.0.0"),
        "custom git_describe_command should prefer matching tags"
    );
    assert!(
        !contents.contains("other-v9.9.9"),
        "custom describe command should ignore non-matching tags"
    );
    Ok(())
}

#[test]
fn ensure_version_file_drops_local_suffix_for_hatch() -> Result<()> {
    if Command::new("git").arg("--version").status().is_err() {
        return Ok(());
    }
    let temp = tempdir()?;
    let manifest = temp.path().join("pyproject.toml");
    fs::write(
        &manifest,
        r#"[project]
name = "demo"
version = "0.1.0"
requires-python = ">=3.11"

[tool.hatch.version.raw-options]
local_scheme = "no-local-version"

[tool.hatch.build.hooks.vcs]
version-file = "demo/_version.py"
"#,
    )?;
    let demo_dir = temp.path().join("demo");
    fs::create_dir_all(&demo_dir)?;
    fs::write(demo_dir.join("__init__.py"), "")?;

    assert!(
        Command::new("git")
            .arg("init")
            .current_dir(temp.path())
            .output()?
            .status
            .success(),
        "git init failed"
    );
    assert!(
        Command::new("git")
            .args(["config", "user.email", "ci@example.com"])
            .current_dir(temp.path())
            .output()?
            .status
            .success(),
        "git config email failed"
    );
    assert!(
        Command::new("git")
            .args(["config", "user.name", "CI"])
            .current_dir(temp.path())
            .output()?
            .status
            .success(),
        "git config name failed"
    );
    assert!(
        Command::new("git")
            .args(["add", "."])
            .current_dir(temp.path())
            .output()?
            .status
            .success(),
        "git add failed"
    );
    assert!(
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(temp.path())
            .output()?
            .status
            .success(),
        "git commit failed"
    );

    ensure_version_file(&manifest)?;
    let contents = fs::read_to_string(temp.path().join("demo/_version.py"))?;
    assert!(
        contents.contains("version = \"0.0.0\""),
        "no-local-version should strip the git hash suffix when no tags exist"
    );
    assert!(
        !contents.contains('+'),
        "no-local-version should omit local version segments"
    );
    Ok(())
}

#[test]
fn ensure_version_file_falls_back_without_git_metadata() -> Result<()> {
    let temp = tempdir()?;
    let manifest = temp.path().join("pyproject.toml");
    fs::write(
        &manifest,
        r#"[project]
name = "demo"
version = "0.1.0"
requires-python = ">=3.11"

[tool.hatch.build.hooks.vcs]
version-file = "demo/_version.py"
"#,
    )?;
    let demo_dir = temp.path().join("demo");
    fs::create_dir_all(&demo_dir)?;
    fs::write(demo_dir.join("__init__.py"), "")?;

    ensure_version_file(&manifest)?;
    let contents = fs::read_to_string(temp.path().join("demo/_version.py"))?;
    assert!(
        contents.contains("version = \"0.0.0+unknown\""),
        "fallback version should be written when git metadata is missing"
    );
    assert!(
        contents.contains("__version__ = version"),
        "fallback stub should alias __version__ to version"
    );
    Ok(())
}

#[test]
fn ensure_version_file_honors_no_local_without_git_metadata() -> Result<()> {
    let temp = tempdir()?;
    let manifest = temp.path().join("pyproject.toml");
    fs::write(
        &manifest,
        r#"[project]
name = "demo"
version = "0.1.0"
requires-python = ">=3.11"

[tool.hatch.version.raw-options]
local_scheme = "no-local-version"

[tool.hatch.build.hooks.vcs]
version-file = "demo/_version.py"
"#,
    )?;
    let demo_dir = temp.path().join("demo");
    fs::create_dir_all(&demo_dir)?;
    fs::write(demo_dir.join("__init__.py"), "")?;

    ensure_version_file(&manifest)?;
    let contents = fs::read_to_string(temp.path().join("demo/_version.py"))?;
    assert!(
        contents.contains("version = \"0.0.0\""),
        "no-local-version should drop local suffix when git metadata is unavailable"
    );
    assert!(
        !contents.contains('+'),
        "no-local-version fallback should not include local components"
    );
    Ok(())
}

#[test]
fn ensure_version_file_upgrades_hatch_stub_missing_alias() -> Result<()> {
    let temp = tempdir()?;
    let manifest = temp.path().join("pyproject.toml");
    fs::write(
        &manifest,
        r#"[project]
name = "demo"
version = "0.1.0"
requires-python = ">=3.11"

[tool.hatch.build.hooks.vcs]
version-file = "demo/_version.py"
"#,
    )?;
    let demo_dir = temp.path().join("demo");
    fs::create_dir_all(&demo_dir)?;
    fs::write(demo_dir.join("__init__.py"), "")?;
    fs::write(demo_dir.join("_version.py"), "__version__ = \"1.2.3\"\n")?;

    ensure_version_file(&manifest)?;
    let contents = fs::read_to_string(demo_dir.join("_version.py"))?;
    assert!(
        contents.contains("version = \"0.0.0+unknown\""),
        "hatch stub should rewrite missing alias with derived version"
    );
    assert!(
        contents.contains("__version__ = version"),
        "hatch stub should alias __version__ to version"
    );
    assert!(
        contents.contains("__all__ = [\"__version__\", \"version\"]"),
        "hatch stub should export both aliases"
    );
    Ok(())
}

#[test]
fn ensure_version_file_supports_setuptools_scm_write_to() -> Result<()> {
    let temp = tempdir()?;
    let manifest = temp.path().join("pyproject.toml");
    fs::write(
        &manifest,
        r#"[project]
name = "demo"
version = "0.1.0"
requires-python = ">=3.11"

[tool.setuptools_scm]
write_to = "demo/_version.py"
"#,
    )?;
    let demo_dir = temp.path().join("demo");
    fs::create_dir_all(&demo_dir)?;
    fs::write(demo_dir.join("__init__.py"), "")?;

    ensure_version_file(&manifest)?;
    let contents = fs::read_to_string(demo_dir.join("_version.py"))?;
    assert!(
        contents.contains("version = \"0.0.0+unknown\""),
        "setuptools_scm stub should include derived version"
    );
    assert!(
        contents.contains("__version__ = version"),
        "setuptools_scm stub should alias __version__"
    );
    assert!(
        contents.contains("version_tuple = tuple(_v.release)"),
        "setuptools_scm stub should export version_tuple from parsed release"
    );
    Ok(())
}

#[test]
fn ensure_version_file_supports_pdm_write_to() -> Result<()> {
    if Command::new("git").arg("--version").status().is_err() {
        return Ok(());
    }
    let temp = tempdir()?;
    let manifest = temp.path().join("pyproject.toml");
    fs::write(
        &manifest,
        r#"[project]
name = "pdm-demo"
dynamic = ["version"]
requires-python = ">=3.11"

[tool.pdm.version]
write_to = "pdm/VERSION"

[build-system]
requires = ["setuptools"]
build-backend = "setuptools.build_meta"
"#,
    )?;
    let pkg_dir = temp.path().join("pdm");
    fs::create_dir_all(&pkg_dir)?;
    fs::write(pkg_dir.join("__init__.py"), "")?;

    assert!(
        Command::new("git")
            .arg("init")
            .current_dir(temp.path())
            .output()?
            .status
            .success(),
        "git init failed"
    );
    assert!(
        Command::new("git")
            .args(["config", "user.email", "ci@example.com"])
            .current_dir(temp.path())
            .output()?
            .status
            .success(),
        "git config email failed"
    );
    assert!(
        Command::new("git")
            .args(["config", "user.name", "CI"])
            .current_dir(temp.path())
            .output()?
            .status
            .success(),
        "git config name failed"
    );
    assert!(
        Command::new("git")
            .args(["add", "."])
            .current_dir(temp.path())
            .output()?
            .status
            .success(),
        "git add failed"
    );
    assert!(
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(temp.path())
            .output()?
            .status
            .success(),
        "git commit failed"
    );

    ensure_version_file(&manifest)?;
    let contents = fs::read_to_string(pkg_dir.join("VERSION"))?;
    assert!(
        contents.trim().starts_with("0.0.0+g"),
        "pdm VERSION file should be derived from git rev"
    );

    let metadata = load_editable_project_metadata(&manifest, SystemEffects::new().fs()).unwrap();
    assert!(
        metadata.version.starts_with("0.0.0+g"),
        "editable metadata should use pdm VERSION contents"
    );
    Ok(())
}

#[test]
fn ensure_version_file_writes_inline_version_stub() -> Result<()> {
    let temp = tempdir()?;
    let manifest = temp.path().join("pyproject.toml");
    fs::write(
        &manifest,
        r#"[project]
name = "demo-pkg"
version = "1.2.3.dev0"
requires-python = ">=3.11"

[build-system]
requires = ["setuptools"]
build-backend = "setuptools.build_meta"
"#,
    )?;
    let pkg_dir = temp.path().join("demo_pkg");
    fs::create_dir_all(&pkg_dir)?;
    fs::write(pkg_dir.join("__init__.py"), "")?;
    fs::write(pkg_dir.join("version.pyi"), "version: str\n")?;

    ensure_version_file(&manifest)?;
    let contents = fs::read_to_string(pkg_dir.join("version.py"))?;
    assert!(
        contents.contains("version = \"1.2.3.dev0\""),
        "stub should use manifest version"
    );
    assert!(
        contents.contains("release = False"),
        "dev versions should mark release as False"
    );
    assert!(
        contents.contains("short_version = version.split(\"+\")[0]"),
        "stub should set short_version"
    );
    Ok(())
}

#[test]
fn infers_version_from_versioneer_module() -> Result<()> {
    let temp = tempdir()?;
    let manifest = temp.path().join("pyproject.toml");
    fs::write(
        &manifest,
        r#"[project]
name = "demo-ver"
dynamic = ["version"]
requires-python = ">=3.11"
dependencies = []

[build-system]
requires = ["setuptools"]
build-backend = "setuptools.build_meta"
"#,
    )?;
    let pkg_dir = temp.path().join("demo_ver");
    fs::create_dir_all(&pkg_dir)?;
    fs::write(
            pkg_dir.join("__init__.py"),
            "from ._version import get_versions\nv = get_versions()\n__version__ = v.get('closest-tag', v['version'])\n",
        )?;
    fs::write(
        pkg_dir.join("_version.py"),
        "def get_versions():\n    return {'version': '1.2.3+dev', 'closest-tag': 'v1.2.3'}\n",
    )?;

    let metadata = load_editable_project_metadata(&manifest, SystemEffects::new().fs()).unwrap();
    assert_eq!(metadata.version, "1.2.3+dev");
    Ok(())
}

#[test]
fn pep440_from_describe_formats_dirty_and_tagged() {
    let version = pep440_from_describe("v1.2.3-4-gabc123").unwrap();
    assert_eq!(version, "1.2.3+4.gabc123");
    let dirty = pep440_from_describe("v0.1.0-0-gdeadbeef-dirty").unwrap();
    assert_eq!(dirty, "0.1.0+0.gdeadbeef.dirty");
}

#[test]
fn pep440_from_describe_handles_tags_with_hyphens() {
    let version = pep440_from_describe("v1.2.3-beta.1-0-gabc123").unwrap();
    assert_eq!(version, "1.2.3-beta.1+0.gabc123");
}
