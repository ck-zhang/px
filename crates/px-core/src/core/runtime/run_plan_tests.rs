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
fn resolves_project_scripts_without_installing_package() -> anyhow::Result<()> {
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
        RunTargetPlan::PxScript(resolved) => {
            assert_eq!(resolved.entry, "demo.cli");
            assert_eq!(resolved.call.as_deref(), Some("main"));
        }
        other => panic!("expected px script plan from project scripts, got {other:?}"),
    }

    Ok(())
}
