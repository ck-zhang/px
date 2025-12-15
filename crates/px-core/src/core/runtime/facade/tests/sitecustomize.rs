use super::super::*;
use crate::api::SystemEffects;
use crate::core::runtime::effects::Effects;
use anyhow::Result;
use serde_json::Value;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use tempfile::tempdir;

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
