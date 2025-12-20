use super::*;
use std::io::IsTerminal;

use super::builtin::run_builtin_tests;
use super::env::{append_allowed_paths, append_pythonpath, run_python_command};
use super::outcome::{mark_reporter_rendered, missing_pytest_outcome, test_failure, test_success};
use crate::{
    build_pythonpath,
    progress::ProgressSuspendGuard,
    tools::{load_installed_tool, tool_install, ToolInstallRequest},
    CommandStatus, InstallUserError,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::core::runtime::run) enum TestReporter {
    Px,
    Pytest,
}

const DEFAULT_PYTEST_REQUIREMENT: &str = "pytest";
const PYTEST_CHECK_SCRIPT: &str =
    "import importlib.util, sys; sys.exit(0 if importlib.util.find_spec('pytest') else 1)";

#[allow(clippy::too_many_arguments)]
pub(super) fn run_pytest_runner(
    ctx: &CommandContext,
    runner: &dyn CommandRunner,
    py_ctx: &PythonContext,
    envs: EnvPairs,
    test_args: &[String],
    stream_runner: bool,
    allow_builtin_fallback: bool,
    workdir: &Path,
) -> Result<ExecutionOutcome> {
    let reporter = test_reporter_from_env();
    let (mut envs, pytest_cmd) = build_pytest_invocation(ctx, py_ctx, envs, test_args, reporter)?;
    if let Err(outcome) = ensure_pytest_available(ctx, py_ctx, &mut envs) {
        if ctx.config().test.fallback_builtin || allow_builtin_fallback {
            return run_builtin_tests(ctx, runner, py_ctx, envs, stream_runner, workdir);
        }
        return Ok(outcome);
    }
    let output = run_python_command(runner, py_ctx, &pytest_cmd, &envs, stream_runner, workdir)?;
    if output.code == 0 {
        let mut outcome = test_success("pytest", output, stream_runner, test_args);
        if let TestReporter::Px = reporter {
            mark_reporter_rendered(&mut outcome);
        }
        return Ok(outcome);
    }
    if missing_pytest(&output.stderr) {
        if ctx.config().test.fallback_builtin || allow_builtin_fallback {
            return run_builtin_tests(ctx, runner, py_ctx, envs, stream_runner, workdir);
        }
        return Ok(missing_pytest_outcome(output, test_args));
    }
    let mut outcome = test_failure("pytest", output, stream_runner, test_args);
    if let TestReporter::Px = reporter {
        mark_reporter_rendered(&mut outcome);
    }
    Ok(outcome)
}

fn ensure_pytest_available(
    ctx: &CommandContext,
    py_ctx: &PythonContext,
    envs: &mut EnvPairs,
) -> Result<(), ExecutionOutcome> {
    match pytest_available(ctx, py_ctx, envs)? {
        true => Ok(()),
        false => {
            let tool_root = ensure_pytest_tool(ctx, py_ctx)?;
            append_tool_env_paths(ctx, envs, &tool_root)?;
            Ok(())
        }
    }
}

fn pytest_available(
    ctx: &CommandContext,
    py_ctx: &PythonContext,
    envs: &[(String, String)],
) -> Result<bool, ExecutionOutcome> {
    let args = vec!["-c".to_string(), PYTEST_CHECK_SCRIPT.to_string()];
    let output = ctx
        .python_runtime()
        .run_command(&py_ctx.python, &args, envs, &py_ctx.project_root)
        .map_err(|err| {
            ExecutionOutcome::failure(
                "px test: failed to probe pytest availability",
                json!({
                    "error": err.to_string(),
                }),
            )
        })?;
    Ok(output.code == 0)
}

fn ensure_pytest_tool(
    ctx: &CommandContext,
    py_ctx: &PythonContext,
) -> Result<PathBuf, ExecutionOutcome> {
    if let Ok(tool) = load_installed_tool("pytest") {
        if tool_env_has_pytest(ctx, py_ctx, &tool.root) {
            return Ok(tool.root);
        }
    }
    if should_announce_tool_install(ctx) {
        eprintln!("px test: installing {DEFAULT_PYTEST_REQUIREMENT}");
    }
    let request = ToolInstallRequest {
        name: "pytest".to_string(),
        spec: Some(DEFAULT_PYTEST_REQUIREMENT.to_string()),
        python: Some(py_ctx.python.clone()),
        entry: Some("pytest".to_string()),
    };
    let _suspend = ProgressSuspendGuard::new();
    let outcome = match tool_install(ctx, &request) {
        Ok(outcome) => outcome,
        Err(err) => match err.downcast::<InstallUserError>() {
            Ok(user) => {
                return Err(ExecutionOutcome::user_error(user.message, user.details));
            }
            Err(other) => {
                return Err(ExecutionOutcome::failure(
                    "px test: failed to install pytest tool",
                    json!({
                        "error": other.to_string(),
                        "spec": DEFAULT_PYTEST_REQUIREMENT,
                    }),
                ));
            }
        },
    };
    if outcome.status != CommandStatus::Ok {
        return Err(outcome);
    }
    let tool = load_installed_tool("pytest").map_err(|err| {
        ExecutionOutcome::failure(
            "px test: failed to load pytest tool after install",
            json!({
                "error": err.to_string(),
                "spec": DEFAULT_PYTEST_REQUIREMENT,
            }),
        )
    })?;
    Ok(tool.root)
}

fn tool_env_has_pytest(ctx: &CommandContext, py_ctx: &PythonContext, tool_root: &Path) -> bool {
    let mut envs = Vec::new();
    if append_tool_env_paths(ctx, &mut envs, tool_root).is_err() {
        return false;
    }
    pytest_available(ctx, py_ctx, &envs).unwrap_or(false)
}

fn append_tool_env_paths(
    ctx: &CommandContext,
    envs: &mut EnvPairs,
    tool_root: &Path,
) -> Result<(), ExecutionOutcome> {
    let paths = build_pythonpath(ctx.fs(), tool_root, None).map_err(|err| {
        ExecutionOutcome::failure(
            "px test: failed to load tool environment paths",
            json!({
                "tool_root": tool_root.display().to_string(),
                "error": err.to_string(),
            }),
        )
    })?;
    for path in paths.allowed_paths {
        append_allowed_paths(envs, &path);
        append_pythonpath(envs, &path);
    }
    Ok(())
}

fn should_announce_tool_install(ctx: &CommandContext) -> bool {
    !ctx.global.json && !ctx.global.quiet && std::io::stderr().is_terminal()
}

pub(in crate::core::runtime::run) fn build_pytest_invocation(
    ctx: &CommandContext,
    py_ctx: &PythonContext,
    mut envs: EnvPairs,
    test_args: &[String],
    reporter: TestReporter,
) -> Result<(EnvPairs, Vec<String>)> {
    super::super::set_env_pair(&mut envs, "PYTHONNOUSERSITE", "1".into());

    let mut defaults = default_pytest_flags(reporter);
    if py_ctx.project_root != py_ctx.state_root {
        let cache_dir = py_ctx.state_root.join(".px").join("pytest-cache");
        append_allowed_paths(&mut envs, &cache_dir);
        defaults.extend_from_slice(&["--cache-dir".to_string(), cache_dir.display().to_string()]);
    }
    if let TestReporter::Px = reporter {
        let plugin_path = ensure_px_pytest_plugin(ctx, py_ctx)?;
        let plugin_dir = plugin_path
            .parent()
            .unwrap_or(py_ctx.project_root.as_path());
        append_pythonpath(&mut envs, plugin_dir);
        append_allowed_paths(&mut envs, plugin_dir);
        defaults.extend_from_slice(&["-p".to_string(), "px_pytest_plugin".to_string()]);
    }
    let pytest_cmd = build_pytest_command_with_defaults(&py_ctx.project_root, test_args, &defaults);
    Ok((envs, pytest_cmd))
}

pub(in crate::core::runtime::run) fn default_pytest_flags(reporter: TestReporter) -> Vec<String> {
    let mut flags = vec![
        "--color=yes".to_string(),
        "--tb=short".to_string(),
        "--ignore=.px".to_string(),
    ];
    if matches!(reporter, TestReporter::Px | TestReporter::Pytest) {
        flags.push("-q".to_string());
    }
    flags
}

fn test_reporter_from_env() -> TestReporter {
    match std::env::var("PX_TEST_REPORTER")
        .ok()
        .as_deref()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("pytest") => TestReporter::Pytest,
        Some("px") | None => TestReporter::Px,
        _ => TestReporter::Px,
    }
}

fn ensure_px_pytest_plugin(ctx: &CommandContext, py_ctx: &PythonContext) -> Result<PathBuf> {
    let plugin_dir = py_ctx.state_root.join(".px").join("plugins");
    ctx.fs()
        .create_dir_all(&plugin_dir)
        .context("creating px plugin dir")?;
    let plugin_path = plugin_dir.join("px_pytest_plugin.py");
    ctx.fs()
        .write(&plugin_path, PX_PYTEST_PLUGIN.as_bytes())
        .context("writing pytest reporter plugin")?;
    Ok(plugin_path)
}

pub(in crate::core::runtime::run) fn missing_pytest(stderr: &str) -> bool {
    let lowered = stderr.to_ascii_lowercase();
    if !lowered.contains("no module named") {
        return false;
    }
    lowered.contains("no module named 'pytest'")
        || lowered.contains("no module named \"pytest\"")
        || lowered
            .split_once("no module named")
            .map(|(_, rest)| rest.trim_start().starts_with("pytest"))
            .unwrap_or(false)
}

#[cfg(test)]
pub(in crate::core::runtime::run) fn build_pytest_command(
    project_root: &Path,
    extra_args: &[String],
) -> Vec<String> {
    build_pytest_command_with_defaults(project_root, extra_args, &[])
}

fn build_pytest_command_with_defaults(
    project_root: &Path,
    extra_args: &[String],
    defaults: &[String],
) -> Vec<String> {
    let mut pytest_cmd = vec!["-m".to_string(), "pytest".to_string()];
    pytest_cmd.extend(defaults.iter().cloned());
    if extra_args.is_empty() {
        for candidate in ["tests", "test"] {
            if project_root.join(candidate).exists() {
                pytest_cmd.push(candidate.to_string());
                break;
            }
        }
    }
    pytest_cmd.extend(extra_args.iter().cloned());
    pytest_cmd
}

const PX_PYTEST_PLUGIN: &str = include_str!("px_pytest_plugin.py");
