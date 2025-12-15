use super::super::env_materialize::{write_project_metadata_stub, write_sitecustomize};
use crate::api::SystemEffects;
use crate::core::runtime::effects::Effects;
use anyhow::Result;
use px_domain::api::ProjectSnapshot;
use serde_json::Value;
use std::env;
use std::fs;
use std::process::Command;
use tempfile::tempdir;

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
