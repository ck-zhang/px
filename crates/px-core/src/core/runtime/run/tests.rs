use super::ref_tree::{
    copy_tree, is_lfs_pointer, list_submodules, restore_lfs_pointers, EnvVarGuard,
};
#[cfg(unix)]
use super::sandbox::map_program_for_container;
use super::test_exec::{
    build_pytest_command, build_pytest_invocation, default_pytest_flags, find_runtests_script,
    merged_pythonpath, missing_pytest, TestReporter,
};
use super::*;
use crate::{
    api::{GlobalOptions, SystemEffects},
    CommandStatus,
};
use px_domain::api::PxOptions;
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use tempfile::tempdir;

fn ctx_with_defaults() -> CommandContext<'static> {
    static GLOBAL: GlobalOptions = GlobalOptions {
        quiet: false,
        verbose: 0,
        trace: false,
        debug: false,
        json: false,
    };
    CommandContext::new(&GLOBAL, Arc::new(SystemEffects::new())).expect("ctx")
}

#[test]
fn run_reference_parsing_normalizes_gh_locator_case_and_git_suffix() {
    let sha = "0123456789abcdef0123456789abcdef01234567";
    let target = format!("gh:Foo/Bar.Git@{sha}:scripts/hello.py");
    let parsed = parse_run_reference_target(&target)
        .expect("parse")
        .expect("run reference");
    let RunReferenceTarget::Script {
        locator,
        git_ref,
        script_path,
    } = parsed
    else {
        panic!("expected Script run-by-reference target");
    };
    assert_eq!(
        locator, "git+https://github.com/foo/bar.git",
        "GitHub shorthand should normalize case and .git suffix"
    );
    assert_eq!(git_ref.as_deref(), Some(sha));
    assert_eq!(script_path, PathBuf::from("scripts/hello.py"));
}

#[cfg(unix)]
#[test]
fn maps_program_path_into_container_roots() {
    let mapped_env = map_program_for_container(
        "/home/user/.px/envs/demo/bin/pythonproject",
        Path::new("/home/user/project"),
        Path::new("/app"),
        Path::new("/home/user/.px/envs/demo"),
        Path::new("/px/env"),
    );
    assert_eq!(mapped_env, "/px/env/bin/pythonproject");

    let mapped_project = map_program_for_container(
        "/home/user/project/scripts/run.py",
        Path::new("/home/user/project"),
        Path::new("/app"),
        Path::new("/home/user/.px/envs/demo"),
        Path::new("/px/env"),
    );
    assert_eq!(mapped_project, "/app/scripts/run.py");

    let passthrough = map_program_for_container(
        "pythonproject",
        Path::new("/home/user/project"),
        Path::new("/app"),
        Path::new("/home/user/.px/envs/demo"),
        Path::new("/px/env"),
    );
    assert_eq!(passthrough, "pythonproject");
}

#[test]
fn detects_mutating_pip_invocation_for_install() {
    let temp = tempdir().expect("tempdir");
    let py_ctx = PythonContext {
        project_root: temp.path().to_path_buf(),
        state_root: temp.path().to_path_buf(),
        project_name: "demo".into(),
        python: "/usr/bin/python".into(),
        pythonpath: String::new(),
        allowed_paths: vec![temp.path().to_path_buf()],
        site_bin: None,
        pep582_bin: Vec::new(),
        pyc_cache_prefix: None,
        px_options: PxOptions::default(),
    };

    let args = vec!["install".to_string(), "demo".to_string()];
    let subcommand = mutating_pip_invocation("pip", &args, &py_ctx);
    assert_eq!(subcommand.as_deref(), Some("install"));
}

#[test]
fn detects_mutating_python_dash_m_pip_invocation() {
    let temp = tempdir().expect("tempdir");
    let py_ctx = PythonContext {
        project_root: temp.path().to_path_buf(),
        state_root: temp.path().to_path_buf(),
        project_name: "demo".into(),
        python: "/usr/bin/python".into(),
        pythonpath: String::new(),
        allowed_paths: vec![temp.path().to_path_buf()],
        site_bin: None,
        pep582_bin: Vec::new(),
        pyc_cache_prefix: None,
        px_options: PxOptions::default(),
    };

    let args = vec![
        "-m".to_string(),
        "pip".to_string(),
        "uninstall".to_string(),
        "demo".to_string(),
    ];
    let subcommand = mutating_pip_invocation(&py_ctx.python, &args, &py_ctx);
    assert_eq!(subcommand.as_deref(), Some("uninstall"));
}

#[test]
fn read_only_pip_commands_are_allowed() {
    let temp = tempdir().expect("tempdir");
    let py_ctx = PythonContext {
        project_root: temp.path().to_path_buf(),
        state_root: temp.path().to_path_buf(),
        project_name: "demo".into(),
        python: "/usr/bin/python".into(),
        pythonpath: String::new(),
        allowed_paths: vec![temp.path().to_path_buf()],
        site_bin: None,
        pep582_bin: Vec::new(),
        pyc_cache_prefix: None,
        px_options: PxOptions::default(),
    };

    let list_args = vec!["list".to_string()];
    assert!(mutating_pip_invocation("pip3", &list_args, &py_ctx).is_none());

    let help_args = vec!["help".to_string(), "install".to_string()];
    assert!(mutating_pip_invocation("pip", &help_args, &py_ctx).is_none());

    let version_args = vec!["--version".to_string()];
    assert!(mutating_pip_invocation("pip", &version_args, &py_ctx).is_none());
}

#[test]
fn detects_lfs_pointer_format() {
    let pointer = b"version https://git-lfs.github.com/spec/v1\n\
oid sha256:1234567890abcdef\nsize 12\n";
    assert!(is_lfs_pointer(pointer));
    assert!(!is_lfs_pointer(b"plain file"));
}

#[test]
fn copy_tree_skips_git_metadata() {
    let src = tempdir().expect("src");
    let dest = tempdir().expect("dest");
    let normal = src.path().join("data.txt");
    fs::write(&normal, b"hello").expect("write");
    let git_dir = src.path().join(".git");
    fs::create_dir_all(&git_dir).expect("git dir");
    fs::write(git_dir.join("config"), b"ignore").expect("git config");

    copy_tree(src.path(), dest.path()).expect("copy");
    assert!(dest.path().join("data.txt").is_file());
    assert!(!dest.path().join(".git").exists());
}

#[test]
fn list_submodules_reports_commits() -> Result<()> {
    let workspace = tempdir()?;
    let root = workspace.path();

    // Create a tiny submodule repo.
    let subrepo = root.join("subrepo");
    fs::create_dir_all(&subrepo)?;
    git(&subrepo, &["init"])?;
    fs::write(subrepo.join("data.txt"), "demo")?;
    git(&subrepo, &["add", "data.txt"])?;
    git(&subrepo, &["commit", "-m", "init"])?;
    let sub_head = git(&subrepo, &["rev-parse", "HEAD"])?;
    let sub_head = sub_head.trim().to_string();

    // Main repo with submodule.
    git(root, &["init"])?;
    fs::write(root.join("pyproject.toml"), "")?;
    fs::write(root.join("px.lock"), "")?;
    git(
        root,
        &["submodule", "add", subrepo.to_str().unwrap(), "libs/data"],
    )?;
    git(root, &["commit", "-am", "add submodule"])?;

    let subs = list_submodules(root, "HEAD").expect("list submodules");
    assert_eq!(subs.len(), 1);
    assert_eq!(subs[0].0, PathBuf::from("libs/data"));
    assert_eq!(subs[0].1, sub_head);
    Ok(())
}

#[test]
fn restore_lfs_pointers_smudges_with_git_lfs() -> Result<()> {
    let workspace = tempdir()?;
    let root = workspace.path();

    git(root, &["init"])?;
    fs::write(root.join("pyproject.toml"), "")?;
    fs::write(root.join("px.lock"), "")?;
    fs::create_dir_all(root.join("assets"))?;
    let pointer = root.join("assets").join("file.bin");
    fs::write(
        &pointer,
        "version https://git-lfs.github.com/spec/v1\n\
oid sha256:deadbeef\nsize 4\n",
    )?;
    git(root, &["add", "."])?;
    git(root, &["commit", "-m", "add lfs pointer"])?;

    // Fake git-lfs subcommand on PATH.
    let fake_bin = workspace.path().join("bin");
    fs::create_dir_all(&fake_bin)?;
    let fake = if cfg!(windows) {
        fake_bin.join("git-lfs.cmd")
    } else {
        fake_bin.join("git-lfs")
    };
    let body = if cfg!(windows) {
        "@echo off\r\nif \"%1\"==\"smudge\" (\r\n  more >nul\r\n  echo SMUDGED\r\n  exit /B 0\r\n)\r\nexit /B 1\r\n"
            .to_string()
    } else {
        "#!/bin/sh\nif [ \"$1\" = \"smudge\" ]; then cat >/dev/null; echo \"SMUDGED\"; else exit 1; fi\n"
            .to_string()
    };
    fs::write(&fake, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&fake)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&fake, perms)?;
    }
    let existing = std::env::var_os("PATH").unwrap_or_default();
    let mut entries = vec![fake_bin];
    entries.extend(std::env::split_paths(&existing));
    let value = std::env::join_paths(entries)
        .unwrap_or(existing)
        .into_string()
        .unwrap_or_else(|value| value.to_string_lossy().to_string());
    let _path_guard = EnvVarGuard::set("PATH", value);

    restore_lfs_pointers(root, "HEAD", root).expect("smudge lfs pointers");
    let contents = fs::read_to_string(&pointer)?;
    assert_eq!(contents.trim(), "SMUDGED");

    Ok(())
}

#[test]
fn run_executable_blocks_mutating_pip_commands() -> Result<()> {
    let ctx = ctx_with_defaults();
    let temp = tempdir()?;
    let py_ctx = PythonContext {
        project_root: temp.path().to_path_buf(),
        state_root: temp.path().to_path_buf(),
        project_name: "demo".into(),
        python: "/usr/bin/python".into(),
        pythonpath: String::new(),
        allowed_paths: vec![temp.path().to_path_buf()],
        site_bin: None,
        pep582_bin: Vec::new(),
        pyc_cache_prefix: None,
        px_options: PxOptions::default(),
    };

    let args = vec!["install".to_string(), "demo".to_string()];
    let runner = HostCommandRunner::new(&ctx);
    let outcome = run_executable(
        &ctx,
        &runner,
        &py_ctx,
        "pip",
        &args,
        &json!({}),
        &py_ctx.project_root,
        false,
    )?;

    assert_eq!(outcome.status, CommandStatus::UserError);
    assert_eq!(
        outcome
            .details
            .get("reason")
            .and_then(|value| value.as_str()),
        Some("pip_mutation_forbidden")
    );
    assert_eq!(
        outcome
            .details
            .get("subcommand")
            .and_then(|value| value.as_str()),
        Some("install")
    );
    assert_eq!(
        outcome
            .details
            .get("program")
            .and_then(|value| value.as_str()),
        Some("pip")
    );
    Ok(())
}

#[test]
fn run_executable_uses_workdir() -> Result<()> {
    let ctx = ctx_with_defaults();
    let temp = tempdir()?;
    let workdir = temp.path().join("nested");
    fs::create_dir_all(&workdir)?;
    let python = match ctx.python_runtime().detect_interpreter() {
        Ok(path) => path,
        Err(_) => return Ok(()),
    };
    let py_ctx = PythonContext {
        project_root: temp.path().to_path_buf(),
        state_root: temp.path().to_path_buf(),
        project_name: "demo".into(),
        python: python.clone(),
        pythonpath: String::new(),
        allowed_paths: vec![temp.path().to_path_buf()],
        site_bin: None,
        pep582_bin: Vec::new(),
        pyc_cache_prefix: None,
        px_options: PxOptions::default(),
    };

    let runner = HostCommandRunner::new(&ctx);
    let args = vec![
        "-c".to_string(),
        "import os; print(os.getcwd())".to_string(),
    ];
    let outcome = run_executable(
        &ctx,
        &runner,
        &py_ctx,
        &python,
        &args,
        &json!({}),
        &workdir,
        false,
    )?;

    let stdout = outcome
        .details
        .get("stdout")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .trim();
    let out_path = PathBuf::from(stdout);
    let out_canon = out_path.canonicalize().unwrap_or(out_path);
    let workdir_canon = workdir.canonicalize().unwrap_or(workdir);
    assert_eq!(out_canon, workdir_canon);
    Ok(())
}

#[test]
fn build_env_marks_available_plugins() -> Result<()> {
    let ctx = ctx_with_defaults();
    let python = match ctx.python_runtime().detect_interpreter() {
        Ok(path) => path,
        Err(_) => return Ok(()),
    };
    let temp = tempdir()?;
    let py_ctx = PythonContext {
        project_root: temp.path().to_path_buf(),
        state_root: temp.path().to_path_buf(),
        project_name: "demo".into(),
        python,
        pythonpath: String::new(),
        allowed_paths: vec![temp.path().to_path_buf()],
        site_bin: None,
        pep582_bin: Vec::new(),
        pyc_cache_prefix: None,
        px_options: PxOptions {
            manage_command: Some("self".into()),
            plugin_imports: vec!["json".into()],
            env_vars: BTreeMap::new(),
            pin_manifest: false,
        },
    };

    let (envs, preflight) = build_env_with_preflight(&ctx, &py_ctx, &json!({}))?;
    assert_eq!(preflight, Some(true));
    assert!(envs
        .iter()
        .any(|(key, value)| key == "PYAPP_COMMAND_NAME" && value == "self"));
    assert!(envs
        .iter()
        .any(|(key, value)| key == "PX_PLUGIN_PREFLIGHT" && value == "1"));
    Ok(())
}

#[test]
fn build_env_marks_missing_plugins() -> Result<()> {
    let ctx = ctx_with_defaults();
    let python = match ctx.python_runtime().detect_interpreter() {
        Ok(path) => path,
        Err(_) => return Ok(()),
    };
    let temp = tempdir()?;
    let py_ctx = PythonContext {
        project_root: temp.path().to_path_buf(),
        state_root: temp.path().to_path_buf(),
        project_name: "demo".into(),
        python,
        pythonpath: String::new(),
        allowed_paths: vec![temp.path().to_path_buf()],
        site_bin: None,
        pep582_bin: Vec::new(),
        pyc_cache_prefix: None,
        px_options: PxOptions {
            manage_command: None,
            plugin_imports: vec!["px_missing_plugin_mod".into()],
            env_vars: BTreeMap::new(),
            pin_manifest: false,
        },
    };

    let (envs, preflight) = build_env_with_preflight(&ctx, &py_ctx, &json!({}))?;
    assert_eq!(preflight, Some(false));
    assert!(envs
        .iter()
        .any(|(key, value)| key == "PX_PLUGIN_PREFLIGHT" && value == "0"));
    Ok(())
}

#[test]
fn pytest_plugin_path_is_on_env_vars() -> Result<()> {
    let ctx = ctx_with_defaults();
    let python = match ctx.python_runtime().detect_interpreter() {
        Ok(path) => path,
        Err(_) => return Ok(()),
    };
    let temp = tempdir()?;
    let py_ctx = PythonContext {
        project_root: temp.path().to_path_buf(),
        state_root: temp.path().to_path_buf(),
        project_name: "demo".into(),
        python,
        pythonpath: temp.path().display().to_string(),
        allowed_paths: vec![temp.path().to_path_buf()],
        site_bin: None,
        pep582_bin: Vec::new(),
        pyc_cache_prefix: None,
        px_options: PxOptions::default(),
    };
    let envs = py_ctx.base_env(&json!({}))?;
    let (envs, _cmd) = build_pytest_invocation(&ctx, &py_ctx, envs, &[], TestReporter::Px, false)?;
    let plugin_dir = temp.path().join(".px").join("plugins");
    let pythonpath = envs
        .iter()
        .find(|(k, _)| k == "PYTHONPATH")
        .map(|(_, v)| v)
        .cloned()
        .unwrap_or_default();
    let python_no_user_site = envs
        .iter()
        .find(|(k, _)| k == "PYTHONNOUSERSITE")
        .map(|(_, v)| v.as_str())
        .unwrap_or_default()
        .to_string();
    let allowed = envs
        .iter()
        .find(|(k, _)| k == "PX_ALLOWED_PATHS")
        .map(|(_, v)| v)
        .cloned()
        .unwrap_or_default();
    let py_entries: Vec<_> = std::env::split_paths(&pythonpath).collect();
    let allowed_entries: Vec<_> = std::env::split_paths(&allowed).collect();
    assert!(
        py_entries.iter().any(|entry| entry == &plugin_dir),
        "PYTHONPATH should include the px pytest plugin dir"
    );
    assert!(
        allowed_entries.iter().any(|entry| entry == &plugin_dir),
        "PX_ALLOWED_PATHS should include the px pytest plugin dir"
    );
    assert_eq!(
        python_no_user_site, "1",
        "px test should disable user site-packages by default"
    );
    Ok(())
}

#[test]
fn missing_pytest_detection_targets_pytest_module_only() {
    assert!(missing_pytest(
        "ModuleNotFoundError: No module named 'pytest'\n"
    ));
    assert!(missing_pytest("ImportError: No module named pytest"));
    assert!(!missing_pytest(
        "ModuleNotFoundError: No module named 'px_pytest_plugin'\n"
    ));
}

#[test]
fn pytest_command_prefers_tests_dir() -> Result<()> {
    let temp = tempdir()?;
    fs::create_dir_all(temp.path().join("tests"))?;

    let cmd = build_pytest_command(temp.path(), &[]);
    assert_eq!(cmd, vec!["-m", "pytest", "tests"]);
    Ok(())
}

#[test]
fn default_pytest_flags_keep_warnings_enabled() {
    let flags = default_pytest_flags(TestReporter::Px);
    assert_eq!(
        flags,
        vec!["--color=yes", "--tb=short", "--ignore=.px", "-q"]
    );
}

#[test]
fn default_pytest_flags_pytest_reporter_matches() {
    let flags = default_pytest_flags(TestReporter::Pytest);
    assert_eq!(
        flags,
        vec!["--color=yes", "--tb=short", "--ignore=.px", "-q"]
    );
}

#[test]
fn pytest_command_falls_back_to_test_dir() -> Result<()> {
    let temp = tempdir()?;
    fs::create_dir_all(temp.path().join("test"))?;

    let cmd = build_pytest_command(temp.path(), &[]);
    assert_eq!(cmd, vec!["-m", "pytest", "test"]);
    Ok(())
}

#[test]
fn pytest_command_respects_user_args() {
    let temp = tempdir().expect("tempdir");
    fs::create_dir_all(temp.path().join("test")).expect("create test dir");

    let cmd = build_pytest_command(
        temp.path(),
        &["-k".to_string(), "unit".to_string(), "extra".to_string()],
    );
    assert_eq!(cmd, vec!["-m", "pytest", "-k", "unit", "extra"]);
}

#[test]
fn prefers_tests_runtests_script() -> Result<()> {
    let temp = tempdir()?;
    let root = temp.path();
    fs::write(root.join("runtests.py"), "print('root')")?;
    fs::create_dir_all(root.join("tests"))?;
    fs::write(root.join("tests/runtests.py"), "print('tests')")?;

    let detected = find_runtests_script(root).expect("script detected");
    assert_eq!(
        detected,
        root.join("tests").join("runtests.py"),
        "tests/runtests.py should be preferred over root runtests.py"
    );
    Ok(())
}

#[test]
fn merged_pythonpath_keeps_extra_entries() {
    let allowed = std::env::join_paths([Path::new("a"), Path::new("b")])
        .expect("allowed paths")
        .into_string()
        .unwrap_or_else(|value| value.to_string_lossy().to_string());
    let pythonpath = std::env::join_paths([Path::new("extra"), Path::new("b")])
        .expect("pythonpath")
        .into_string()
        .unwrap_or_else(|value| value.to_string_lossy().to_string());
    let envs = vec![
        ("PX_ALLOWED_PATHS".into(), allowed),
        ("PYTHONPATH".into(), pythonpath),
    ];
    let merged = merged_pythonpath(&envs).expect("merged path");
    let entries: Vec<_> = std::env::split_paths(&merged).collect();
    assert_eq!(
        entries,
        vec![
            PathBuf::from("a"),
            PathBuf::from("b"),
            PathBuf::from("extra")
        ]
    );
}

fn git(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git").args(args).current_dir(cwd).output()?;
    if !output.status.success() {
        bail!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}
