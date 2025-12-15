use super::super::*;
use crate::api::SystemEffects;
use crate::core::runtime::effects::Effects;
use crate::InstallUserError;
use anyhow::Result;
use serde_json::Value;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use tempfile::tempdir;

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
