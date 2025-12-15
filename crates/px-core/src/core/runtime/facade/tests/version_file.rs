use super::super::context::pep440_from_describe;
use super::super::env_materialize::load_editable_project_metadata;
use super::super::*;
use crate::api::SystemEffects;
use crate::core::runtime::effects::Effects;
use anyhow::Result;
use std::fs;
use std::process::Command;
use tempfile::tempdir;

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
