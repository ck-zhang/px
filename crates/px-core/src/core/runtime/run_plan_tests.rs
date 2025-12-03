#![deny(clippy::all, warnings)]

use std::fs;

use tempfile::tempdir;

use crate::core::runtime::run_plan::{plan_run_target, RunTargetPlan};
use crate::core::runtime::PythonContext;
use px_domain::PxOptions;

#[test]
fn prefers_console_script_from_site_bin() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let site_bin = temp.path().join("bin");
    fs::create_dir_all(&site_bin)?;
    let script = site_bin.join("demo");
    fs::write(&script, b"echo demo")?;

    let ctx = PythonContext {
        project_root: temp.path().to_path_buf(),
        project_name: "demo".into(),
        python: "/usr/bin/python".into(),
        pythonpath: String::new(),
        allowed_paths: vec![],
        site_bin: Some(site_bin.clone()),
        pep582_bin: Vec::new(),
        px_options: PxOptions::default(),
    };

    let plan = plan_run_target(&ctx, &temp.path().join("pyproject.toml"), "demo")?;
    let resolved = match plan {
        RunTargetPlan::Executable(path) => path,
        other => panic!("expected executable plan, got {other:?}"),
    };
    assert!(
        resolved.contains("bin/demo"),
        "expected site-bin script to be resolved, got {resolved}"
    );
    Ok(())
}

#[test]
fn ignores_px_script_entries() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let pyproject = temp.path().join("pyproject.toml");
    std::fs::write(
        &pyproject,
        r#"[tool.px.scripts]
demo = "demo.cli:main"
"#,
    )?;

    let ctx = PythonContext {
        project_root: temp.path().to_path_buf(),
        project_name: "demo".into(),
        python: "/usr/bin/python".into(),
        pythonpath: String::new(),
        allowed_paths: vec![],
        site_bin: None,
        pep582_bin: Vec::new(),
        px_options: PxOptions::default(),
    };

    let plan = plan_run_target(&ctx, &pyproject, "demo")?;
    match plan {
        RunTargetPlan::Executable(target) => assert_eq!(target, "demo"),
        other => panic!("expected passthrough executable, got {other:?}"),
    }

    Ok(())
}

#[test]
fn ignores_project_scripts_for_target_resolution() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let pyproject = temp.path().join("pyproject.toml");
    std::fs::write(
        &pyproject,
        r#"[project]
name = "demo"
version = "0.1.0"
scripts = { demo = "demo.cli:main" }
"#,
    )?;

    let ctx = PythonContext {
        project_root: temp.path().to_path_buf(),
        project_name: "demo".into(),
        python: "/usr/bin/python".into(),
        pythonpath: String::new(),
        allowed_paths: vec![],
        site_bin: None,
        pep582_bin: Vec::new(),
        px_options: PxOptions::default(),
    };

    let plan = plan_run_target(&ctx, &pyproject, "demo")?;
    match plan {
        RunTargetPlan::Executable(target) => assert_eq!(target, "demo"),
        other => panic!("expected passthrough executable, got {other:?}"),
    }

    Ok(())
}

#[test]
fn resolves_existing_project_script_under_root() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let script = temp.path().join("scripts").join("app.py");
    fs::create_dir_all(script.parent().expect("script parent"))?;
    fs::write(&script, b"print('demo')")?;

    let ctx = PythonContext {
        project_root: temp.path().to_path_buf(),
        project_name: "demo".into(),
        python: "/usr/bin/python".into(),
        pythonpath: String::new(),
        allowed_paths: vec![],
        site_bin: None,
        pep582_bin: Vec::new(),
        px_options: PxOptions::default(),
    };

    let plan = plan_run_target(&ctx, &temp.path().join("pyproject.toml"), "scripts/app.py")?;
    match plan {
        RunTargetPlan::Script(path) => {
            assert_eq!(
                path,
                script.canonicalize()?,
                "script plan should use canonical project path"
            );
        }
        other => panic!("expected script plan, got {other:?}"),
    }
    Ok(())
}

#[test]
fn does_not_guess_missing_python_script_targets() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let ctx = PythonContext {
        project_root: temp.path().to_path_buf(),
        project_name: "demo".into(),
        python: "/usr/bin/python".into(),
        pythonpath: String::new(),
        allowed_paths: vec![],
        site_bin: None,
        pep582_bin: Vec::new(),
        px_options: PxOptions::default(),
    };

    let plan = plan_run_target(&ctx, &temp.path().join("pyproject.toml"), "missing.py")?;
    match plan {
        RunTargetPlan::Executable(target) => {
            assert_eq!(
                target, "missing.py",
                "missing .py should be treated as executable"
            );
        }
        other => panic!("expected executable plan, got {other:?}"),
    }
    Ok(())
}

#[test]
fn python_alias_runs_as_plain_executable() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let ctx = PythonContext {
        project_root: temp.path().to_path_buf(),
        project_name: "demo".into(),
        python: "/usr/bin/python".into(),
        pythonpath: String::new(),
        allowed_paths: vec![],
        site_bin: None,
        pep582_bin: Vec::new(),
        px_options: PxOptions::default(),
    };

    let plan = plan_run_target(&ctx, &temp.path().join("pyproject.toml"), "python")?;
    match plan {
        RunTargetPlan::Executable(target) => assert_eq!(target, "python"),
        other => panic!("expected executable passthrough, got {other:?}"),
    }
    Ok(())
}
