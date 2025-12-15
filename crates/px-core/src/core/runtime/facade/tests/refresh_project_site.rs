use super::super::env_materialize::{
    project_site_env, uv_cli_candidates, SETUPTOOLS_SEED_VERSION, UV_SEED_VERSION,
};
use super::super::*;
use crate::api::{GlobalOptions, SystemEffects};
use crate::core::sandbox::env_root_from_site_packages;
use crate::CommandContext;
use crate::InstallUserError;
use anyhow::{anyhow, Result};
use px_domain::api::{render_lockfile, ProjectSnapshot, ResolvedDependency};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use tempfile::tempdir;

static PIP_ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

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
