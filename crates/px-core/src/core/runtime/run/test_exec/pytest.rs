use super::*;

use super::builtin::run_builtin_tests;
use super::env::{append_allowed_paths, append_pythonpath, run_python_command};
use super::outcome::{mark_reporter_rendered, missing_pytest_outcome, test_failure, test_success};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::core::runtime::run) enum TestReporter {
    Px,
    Pytest,
}

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
    let (envs, pytest_cmd) = build_pytest_invocation(ctx, py_ctx, envs, test_args, reporter)?;
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

pub(in crate::core::runtime::run) fn build_pytest_invocation(
    ctx: &CommandContext,
    py_ctx: &PythonContext,
    mut envs: EnvPairs,
    test_args: &[String],
    reporter: TestReporter,
) -> Result<(EnvPairs, Vec<String>)> {
    let mut defaults = default_pytest_flags(reporter);
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
    let plugin_dir = py_ctx.project_root.join(".px").join("plugins");
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
