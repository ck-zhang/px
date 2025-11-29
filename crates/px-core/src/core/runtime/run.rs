use std::env;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tracing::debug;

use crate::run_plan::{
    plan_run_target, PassthroughReason, PassthroughTarget, ResolvedEntry, RunTargetPlan,
};
use crate::tooling::{missing_pyproject_outcome, run_target_required_outcome};
use crate::workspace::prepare_workspace_run_context;
use crate::{
    attach_autosync_details, is_missing_project_error, manifest_snapshot, missing_project_outcome,
    outcome_from_output, python_context_with_mode, state_guard::guard_for_execution,
    CommandContext, ExecutionOutcome, PythonContext,
};

#[derive(Clone, Debug)]
pub struct TestRequest {
    pub args: Vec<String>,
    pub frozen: bool,
}

#[derive(Clone, Debug)]
pub struct RunRequest {
    pub entry: Option<String>,
    pub target: Option<String>,
    pub args: Vec<String>,
    pub frozen: bool,
    pub interactive: Option<bool>,
}

type EnvPairs = Vec<(String, String)>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TestReporter {
    Px,
    Pytest,
}

#[derive(Clone, Debug)]
enum TestRunner {
    Pytest,
    Builtin,
    Script(PathBuf),
}

/// Runs the project's tests using a project-provided runner or pytest, with an
/// optional px fallback runner.
///
/// # Errors
/// Returns an error if the Python environment cannot be prepared or test execution fails.
pub fn test_project(ctx: &CommandContext, request: &TestRequest) -> Result<ExecutionOutcome> {
    test_project_outcome(ctx, request)
}

/// Executes a configured px run entry or script.
///
/// # Errors
/// Returns an error if the Python environment cannot be prepared or the entry fails to run.
pub fn run_project(ctx: &CommandContext, request: &RunRequest) -> Result<ExecutionOutcome> {
    run_project_outcome(ctx, request)
}

fn run_project_outcome(ctx: &CommandContext, request: &RunRequest) -> Result<ExecutionOutcome> {
    let strict = request.frozen || ctx.env_flag_enabled("CI");

    if let Some(ws_ctx) = match prepare_workspace_run_context(ctx, strict, "run") {
        Ok(result) => result,
        Err(outcome) => return Ok(outcome),
    } {
        let target = request
            .entry
            .clone()
            .or_else(|| request.target.clone())
            .unwrap_or_default();
        if target.trim().is_empty() {
            return Ok(run_target_required_outcome());
        }
        let command_args = json!({
            "target": &target,
            "args": &request.args,
        });
        let interactive = request.interactive.unwrap_or_else(|| {
            !strict && std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
        });
        let plan = plan_run_target(&ws_ctx.py_ctx, &ws_ctx.manifest_path, &target)?;
        let mut outcome = match plan {
            RunTargetPlan::PxScript(resolved) => run_module_entry(
                ctx,
                &ws_ctx.py_ctx,
                resolved,
                &request.args,
                &command_args,
                interactive,
            )?,
            RunTargetPlan::Script(path) => run_project_script(
                ctx,
                &ws_ctx.py_ctx,
                &path,
                &request.args,
                &command_args,
                interactive,
            )?,
            RunTargetPlan::Passthrough(target) => run_passthrough(
                ctx,
                &ws_ctx.py_ctx,
                target,
                &request.args,
                &command_args,
                interactive,
            )?,
            RunTargetPlan::Executable(program) => run_executable(
                ctx,
                &ws_ctx.py_ctx,
                &program,
                &request.args,
                &command_args,
                interactive,
            )?,
        };
        attach_autosync_details(&mut outcome, ws_ctx.sync_report);
        return Ok(outcome);
    }

    let snapshot = match manifest_snapshot() {
        Ok(snapshot) => snapshot,
        Err(err) => {
            if is_missing_project_error(&err) {
                return Ok(missing_project_outcome());
            }
            let msg = err.to_string();
            if msg.contains("pyproject.toml not found") {
                let root = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                return Ok(missing_pyproject_outcome("run", &root));
            }
            return Err(err);
        }
    };
    let target = request
        .entry
        .clone()
        .or_else(|| request.target.clone())
        .unwrap_or_default();
    if target.trim().is_empty() {
        return Ok(run_target_required_outcome());
    }
    let state_report = match crate::state_guard::state_or_violation(ctx, &snapshot, "run") {
        Ok(report) => report,
        Err(outcome) => return Ok(outcome),
    };
    let guard = match guard_for_execution(strict, &snapshot, &state_report, "run") {
        Ok(guard) => guard,
        Err(outcome) => return Ok(outcome),
    };
    let (py_ctx, sync_report) = match python_context_with_mode(ctx, guard) {
        Ok(result) => result,
        Err(outcome) => return Ok(outcome),
    };
    let command_args = json!({
        "target": &target,
        "args": &request.args,
    });
    let manifest = py_ctx.project_root.join("pyproject.toml");

    let interactive = request.interactive.unwrap_or_else(|| {
        !strict && std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
    });
    let plan = plan_run_target(&py_ctx, &manifest, &target)?;
    let mut outcome = match plan {
        RunTargetPlan::PxScript(resolved) => run_module_entry(
            ctx,
            &py_ctx,
            resolved,
            &request.args,
            &command_args,
            interactive,
        )?,
        RunTargetPlan::Script(path) => run_project_script(
            ctx,
            &py_ctx,
            &path,
            &request.args,
            &command_args,
            interactive,
        )?,
        RunTargetPlan::Passthrough(target) => run_passthrough(
            ctx,
            &py_ctx,
            target,
            &request.args,
            &command_args,
            interactive,
        )?,
        RunTargetPlan::Executable(program) => run_executable(
            ctx,
            &py_ctx,
            &program,
            &request.args,
            &command_args,
            interactive,
        )?,
    };
    attach_autosync_details(&mut outcome, sync_report);
    Ok(outcome)
}

fn run_pytest_runner(
    ctx: &CommandContext,
    py_ctx: &PythonContext,
    envs: EnvPairs,
    test_args: &[String],
    stream_runner: bool,
) -> Result<ExecutionOutcome> {
    let reporter = test_reporter_from_env();
    let (envs, pytest_cmd) = build_pytest_invocation(ctx, py_ctx, envs, test_args, reporter)?;
    let output = run_python_command(ctx, py_ctx, &pytest_cmd, &envs, stream_runner)?;
    if output.code == 0 {
        let mut outcome = test_success("pytest", output, stream_runner, test_args);
        if let TestReporter::Px = reporter {
            mark_reporter_rendered(&mut outcome);
        }
        return Ok(outcome);
    }
    if missing_pytest(&output.stderr) {
        if ctx.config().test.fallback_builtin {
            return run_builtin_tests(ctx, py_ctx, envs, stream_runner);
        }
        return Ok(missing_pytest_outcome(output, test_args));
    }
    let mut outcome = test_failure("pytest", output, stream_runner, test_args);
    if let TestReporter::Px = reporter {
        mark_reporter_rendered(&mut outcome);
    }
    Ok(outcome)
}

fn build_pytest_invocation(
    ctx: &CommandContext,
    py_ctx: &PythonContext,
    mut envs: EnvPairs,
    test_args: &[String],
    reporter: TestReporter,
) -> Result<(EnvPairs, Vec<String>)> {
    let mut defaults = default_pytest_flags(reporter);
    if let TestReporter::Px = reporter {
        let plugin_path = ensure_px_pytest_plugin(ctx, py_ctx)?;
        append_pythonpath(
            &mut envs,
            plugin_path
                .parent()
                .unwrap_or(py_ctx.project_root.as_path()),
        );
        defaults.extend_from_slice(&["-p".to_string(), "px_pytest_plugin".to_string()]);
    }
    let pytest_cmd = build_pytest_command_with_defaults(&py_ctx.project_root, test_args, &defaults);
    Ok((envs, pytest_cmd))
}

fn default_pytest_flags(reporter: TestReporter) -> Vec<String> {
    let mut flags = vec!["--color=yes".to_string(), "--tb=short".to_string()];
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

fn append_pythonpath(envs: &mut EnvPairs, plugin_dir: &Path) {
    let plugin_entry = plugin_dir.display().to_string();
    if let Some((_, value)) = envs.iter_mut().find(|(key, _)| key == "PYTHONPATH") {
        let mut parts: Vec<_> = env::split_paths(value).collect();
        if !parts.iter().any(|p| p == plugin_dir) {
            parts.insert(0, plugin_dir.to_path_buf());
            if let Ok(joined) = env::join_paths(parts) {
                if let Ok(strval) = joined.into_string() {
                    *value = strval;
                }
            }
        }
    } else {
        envs.push(("PYTHONPATH".into(), plugin_entry));
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

fn run_python_command(
    ctx: &CommandContext,
    py_ctx: &PythonContext,
    args: &[String],
    envs: &[(String, String)],
    stream_runner: bool,
) -> Result<crate::RunOutput> {
    if stream_runner {
        ctx.python_runtime()
            .run_command_streaming(&py_ctx.python, args, envs, &py_ctx.project_root)
    } else {
        ctx.python_runtime()
            .run_command(&py_ctx.python, args, envs, &py_ctx.project_root)
    }
}

fn run_project_script(
    core_ctx: &CommandContext,
    py_ctx: &PythonContext,
    script: &Path,
    extra_args: &[String],
    command_args: &Value,
    interactive: bool,
) -> Result<ExecutionOutcome> {
    let (envs, _) = build_env_with_preflight(core_ctx, py_ctx, command_args)?;
    let mut args = Vec::with_capacity(extra_args.len() + 1);
    args.push(script.display().to_string());
    args.extend(extra_args.iter().cloned());
    let runtime = core_ctx.python_runtime();
    let output = if interactive {
        runtime.run_command_passthrough(&py_ctx.python, &args, &envs, &py_ctx.project_root)?
    } else {
        runtime.run_command(&py_ctx.python, &args, &envs, &py_ctx.project_root)?
    };
    let details = json!({
        "mode": "script",
        "script": script.display().to_string(),
        "args": extra_args,
        "interactive": interactive,
    });
    Ok(outcome_from_output(
        "run",
        &script.display().to_string(),
        &output,
        "px run",
        Some(details),
    ))
}

fn run_executable(
    core_ctx: &CommandContext,
    py_ctx: &PythonContext,
    program: &str,
    extra_args: &[String],
    command_args: &Value,
    interactive: bool,
) -> Result<ExecutionOutcome> {
    let (envs, _) = build_env_with_preflight(core_ctx, py_ctx, command_args)?;
    let runtime = core_ctx.python_runtime();
    let output = if interactive {
        runtime.run_command_passthrough(program, extra_args, &envs, &py_ctx.project_root)?
    } else {
        runtime.run_command(program, extra_args, &envs, &py_ctx.project_root)?
    };
    let details = json!({
        "mode": "executable",
        "program": program,
        "args": extra_args,
        "interactive": interactive,
    });
    Ok(outcome_from_output(
        "run",
        program,
        &output,
        "px run",
        Some(details),
    ))
}

fn test_project_outcome(ctx: &CommandContext, request: &TestRequest) -> Result<ExecutionOutcome> {
    let strict = request.frozen || ctx.env_flag_enabled("CI");

    if let Some(ws_ctx) = match prepare_workspace_run_context(ctx, strict, "test") {
        Ok(result) => result,
        Err(outcome) => return Ok(outcome),
    } {
        return run_tests_for_context(ctx, &ws_ctx.py_ctx, request, ws_ctx.sync_report);
    }

    let snapshot = match manifest_snapshot() {
        Ok(snapshot) => snapshot,
        Err(err) => {
            if is_missing_project_error(&err) {
                return Ok(missing_project_outcome());
            }
            let msg = err.to_string();
            if msg.contains("pyproject.toml not found") {
                let root = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                return Ok(missing_pyproject_outcome("test", &root));
            }
            return Err(err);
        }
    };
    let state_report = match crate::state_guard::state_or_violation(ctx, &snapshot, "test") {
        Ok(report) => report,
        Err(outcome) => return Ok(outcome),
    };
    let guard = match guard_for_execution(strict, &snapshot, &state_report, "test") {
        Ok(guard) => guard,
        Err(outcome) => return Ok(outcome),
    };
    let (py_ctx, sync_report) = match python_context_with_mode(ctx, guard) {
        Ok(result) => result,
        Err(outcome) => return Ok(outcome),
    };
    run_tests_for_context(ctx, &py_ctx, request, sync_report)
}

fn select_test_runner(ctx: &CommandContext, py_ctx: &PythonContext) -> TestRunner {
    if ctx.config().test.fallback_builtin {
        return TestRunner::Builtin;
    }
    if let Some(script) = find_runtests_script(&py_ctx.project_root) {
        return TestRunner::Script(script);
    }
    TestRunner::Pytest
}

fn find_runtests_script(project_root: &Path) -> Option<PathBuf> {
    ["tests/runtests.py", "runtests.py"]
        .iter()
        .map(|rel| project_root.join(rel))
        .find(|candidate| candidate.is_file())
}

fn run_tests_for_context(
    ctx: &CommandContext,
    py_ctx: &PythonContext,
    request: &TestRequest,
    sync_report: Option<crate::EnvironmentSyncReport>,
) -> Result<ExecutionOutcome> {
    let command_args = json!({ "test_args": request.args });
    let (mut envs, _preflight) = build_env_with_preflight(ctx, py_ctx, &command_args)?;
    let stream_runner = !ctx.global.json;

    let mut outcome = match select_test_runner(ctx, py_ctx) {
        TestRunner::Builtin => run_builtin_tests(ctx, py_ctx, envs, stream_runner)?,
        TestRunner::Script(script) => {
            run_script_runner(ctx, py_ctx, envs, &script, &request.args, stream_runner)?
        }
        TestRunner::Pytest => {
            envs.push(("PX_TEST_RUNNER".into(), "pytest".into()));
            run_pytest_runner(ctx, py_ctx, envs, &request.args, stream_runner)?
        }
    };
    attach_autosync_details(&mut outcome, sync_report);
    Ok(outcome)
}

fn run_script_runner(
    ctx: &CommandContext,
    py_ctx: &PythonContext,
    mut envs: EnvPairs,
    script: &Path,
    args: &[String],
    stream_runner: bool,
) -> Result<ExecutionOutcome> {
    let runner_label = script
        .strip_prefix(&py_ctx.project_root)
        .unwrap_or(script)
        .display()
        .to_string();
    envs.push(("PX_TEST_RUNNER".into(), runner_label.clone()));
    let mut cmd_args = vec![script.display().to_string()];
    cmd_args.extend_from_slice(args);
    let output = run_python_command(ctx, py_ctx, &cmd_args, &envs, stream_runner)?;
    if output.code == 0 {
        Ok(test_success(&runner_label, output, stream_runner, args))
    } else {
        Ok(test_failure(&runner_label, output, stream_runner, args))
    }
}

fn run_builtin_tests(
    core_ctx: &CommandContext,
    ctx: &PythonContext,
    mut envs: Vec<(String, String)>,
    stream_runner: bool,
) -> Result<ExecutionOutcome> {
    envs.push(("PX_TEST_RUNNER".into(), "builtin".into()));
    let script = "from sample_px_app import cli\nassert cli.greet() == 'Hello, World!'\nprint('px fallback test passed')";
    let args = vec!["-c".to_string(), script.to_string()];
    let output = run_python_command(core_ctx, ctx, &args, &envs, stream_runner)?;
    let runner_args: Vec<String> = Vec::new();
    Ok(test_success("builtin", output, stream_runner, &runner_args))
}

fn test_success(
    runner: &str,
    output: crate::RunOutput,
    stream_runner: bool,
    args: &[String],
) -> ExecutionOutcome {
    ExecutionOutcome::success(
        format!("{runner} ok"),
        test_details(runner, output, stream_runner, args, None),
    )
}

fn test_failure(
    runner: &str,
    output: crate::RunOutput,
    stream_runner: bool,
    args: &[String],
) -> ExecutionOutcome {
    let code = output.code;
    let mut details = test_details(runner, output, stream_runner, args, Some("tests_failed"));
    if let Value::Object(map) = &mut details {
        map.insert("suppress_cli_frame".into(), Value::Bool(true));
    }
    ExecutionOutcome::failure(format!("{runner} failed (exit {code})"), details)
}

fn missing_pytest_outcome(output: crate::RunOutput, args: &[String]) -> ExecutionOutcome {
    ExecutionOutcome::user_error(
        "pytest is not available in the project environment",
        json!({
            "stdout": output.stdout,
            "stderr": output.stderr,
            "hint": "Add pytest to your project with `px add pytest`, then rerun `px test`.",
            "reason": "missing_pytest",
            "code": crate::diag_commands::TEST,
            "runner": "pytest",
            "args": args,
        }),
    )
}

fn test_details(
    runner: &str,
    output: crate::RunOutput,
    stream_runner: bool,
    args: &[String],
    reason: Option<&str>,
) -> serde_json::Value {
    let mut details = json!({
        "runner": runner,
        "stdout": output.stdout,
        "stderr": output.stderr,
        "code": output.code,
        "args": args,
        "streamed": stream_runner,
    });
    if let Some(reason) = reason {
        if let Some(map) = details.as_object_mut() {
            map.insert("reason".to_string(), json!(reason));
        }
    }
    details
}

fn mark_reporter_rendered(outcome: &mut ExecutionOutcome) {
    match &mut outcome.details {
        Value::Object(map) => {
            map.insert("reporter_rendered".into(), Value::Bool(true));
        }
        Value::Null => {
            outcome.details = json!({ "reporter_rendered": true });
        }
        other => {
            let prev = other.take();
            outcome.details = json!({ "value": prev, "reporter_rendered": true });
        }
    }
}

fn run_module_entry(
    core_ctx: &CommandContext,
    py_ctx: &PythonContext,
    resolved: ResolvedEntry,
    extra_args: &[String],
    command_args: &Value,
    interactive: bool,
) -> Result<ExecutionOutcome> {
    let ResolvedEntry {
        entry,
        call,
        source,
    } = resolved;
    let mut mode = "module";
    let mut python_args;
    let mut env_entry = entry.clone();
    let argv0 = source
        .script_name()
        .map(std::string::ToString::to_string)
        .unwrap_or_else(|| entry.clone());
    let (mut envs, _) = build_env_with_preflight(core_ctx, py_ctx, command_args)?;
    if let Some(call_name) = call.as_ref() {
        mode = "console-script";
        env_entry = format!("{entry}:{call_name}");
        python_args = vec![
            "-c".to_string(),
            console_entry_payload(&entry, call_name, &argv0),
        ];
        python_args.extend(extra_args.iter().cloned());
    } else {
        python_args = vec!["-m".to_string(), entry.clone()];
        python_args.extend(extra_args.iter().cloned());
    }
    envs.push(("PX_RUN_ENTRY".into(), env_entry.clone()));

    let runtime = core_ctx.python_runtime();
    let output = if interactive {
        runtime.run_command_passthrough(
            &py_ctx.python,
            &python_args,
            &envs,
            &py_ctx.project_root,
        )?
    } else {
        runtime.run_command(&py_ctx.python, &python_args, &envs, &py_ctx.project_root)?
    };
    let mut details = json!({
        "mode": mode,
        "entry": env_entry.clone(),
        "args": extra_args,
        "source": source.label(),
        "interactive": interactive,
    });
    if let Some(script) = source.script_name() {
        details["script"] = Value::String(script.to_string());
    }
    if let Some(call_name) = call {
        details["call"] = Value::String(call_name);
    }

    Ok(outcome_from_output(
        "run",
        &env_entry,
        &output,
        "px run",
        Some(details),
    ))
}

fn console_entry_payload(module: &str, call: &str, argv0: &str) -> String {
    let access = call
        .split('.')
        .filter_map(|part| {
            let trimmed = part.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        })
        .fold(String::new(), |mut acc, part| {
            acc.push('.');
            acc.push_str(&part);
            acc
        });
    format!(
        "import importlib, sys; sys.argv[0] = {argv0:?}; mod = importlib.import_module({module:?}); target = mod{access}; sys.exit(target())"
    )
}

fn run_passthrough(
    core_ctx: &CommandContext,
    py_ctx: &PythonContext,
    target: PassthroughTarget,
    extra_args: &[String],
    command_args: &Value,
    interactive: bool,
) -> Result<ExecutionOutcome> {
    let PassthroughTarget {
        program,
        display,
        reason,
        resolved,
    } = target;
    let (envs, _) = build_env_with_preflight(core_ctx, py_ctx, command_args)?;
    let program_args = match &reason {
        PassthroughReason::PythonScript { script_arg, .. } => {
            let mut args = Vec::with_capacity(extra_args.len() + 1);
            args.push(script_arg.clone());
            args.extend(extra_args.iter().cloned());
            args
        }
        _ => extra_args.to_vec(),
    };
    let needs_stdin = match &reason {
        PassthroughReason::PythonScript { script_arg, .. } => script_arg == "-",
        PassthroughReason::PythonAlias => program_args.first().map(String::as_str) == Some("-"),
        _ => false,
    };
    let mut interactive = interactive;
    if needs_stdin {
        // `python -` needs stdin; treat it as interactive even when not attached to a TTY.
        interactive = true;
    }
    let runtime = core_ctx.python_runtime();
    let output = if needs_stdin {
        runtime.run_command_with_stdin(
            &program,
            &program_args,
            &envs,
            &py_ctx.project_root,
            true,
        )?
    } else if interactive {
        runtime.run_command_passthrough(&program, &program_args, &envs, &py_ctx.project_root)?
    } else {
        runtime.run_command(&program, &program_args, &envs, &py_ctx.project_root)?
    };
    let mut details = json!({
        "mode": "passthrough",
        "program": display.clone(),
        "args": extra_args,
        "interactive": interactive,
    });
    if let Some(resolved_path) = resolved {
        details["resolved_program"] = Value::String(resolved_path);
    }
    match reason {
        PassthroughReason::PythonAlias => {
            details["uses_px_python"] = Value::Bool(true);
        }
        PassthroughReason::ExecutablePath => {}
        PassthroughReason::PythonScript { script_path, .. } => {
            details["uses_px_python"] = Value::Bool(true);
            details["script"] = Value::String(script_path);
        }
    }

    Ok(outcome_from_output(
        "run",
        &display,
        &output,
        "px run",
        Some(details),
    ))
}

fn missing_pytest(stderr: &str) -> bool {
    stderr.contains("No module named") && stderr.contains("pytest")
}

#[cfg(test)]
fn build_pytest_command(project_root: &Path, extra_args: &[String]) -> Vec<String> {
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

fn build_env_with_preflight(
    ctx: &CommandContext,
    py_ctx: &PythonContext,
    command_args: &Value,
) -> Result<(EnvPairs, Option<bool>)> {
    let mut envs = py_ctx.base_env(command_args)?;
    let preflight = preflight_plugins(ctx, py_ctx, &envs)?;
    if let Some(ok) = preflight {
        envs.push((
            "PX_PLUGIN_PREFLIGHT".into(),
            if ok { "1".into() } else { "0".into() },
        ));
    }
    Ok((envs, preflight))
}

fn preflight_plugins(
    ctx: &CommandContext,
    py_ctx: &PythonContext,
    envs: &[(String, String)],
) -> Result<Option<bool>> {
    if py_ctx.px_options.plugin_imports.is_empty() {
        return Ok(None);
    }
    let imports = py_ctx
        .px_options
        .plugin_imports
        .iter()
        .map(|name| format!("{name:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let script = format!(
        "import importlib.util, sys\nmissing=[name for name in [{imports}] if importlib.util.find_spec(name) is None]\nsys.exit(1 if missing else 0)"
    );
    let args = vec!["-c".to_string(), script];
    match ctx
        .python_runtime()
        .run_command(&py_ctx.python, &args, envs, &py_ctx.project_root)
    {
        Ok(output) => Ok(Some(output.code == 0)),
        Err(err) => {
            debug!(error = ?err, "plugin preflight failed");
            Ok(Some(false))
        }
    }
}

const PX_PYTEST_PLUGIN: &str = r#"import sys
import time
import pytest
from _pytest._io.terminalwriter import TerminalWriter


class PxTerminalReporter:
    def __init__(self, config):
        self.config = config
        self._tw = TerminalWriter(file=sys.stdout)
        self._tw.hasmarkup = True
        self.session_start = time.time()
        self.collection_start = None
        self.collection_duration = 0.0
        self.collected = 0
        self.files = []
        self._current_file = None
        self.failures = []
        self.stats = {"passed": 0, "failed": 0, "skipped": 0, "error": 0, "xfailed": 0, "xpassed": 0}
        self.exitstatus = 0

    def pytest_sessionstart(self, session):
        import platform

        py_ver = platform.python_version()
        root = str(self.config.rootpath)
        cfg = self.config.inifile or "auto-detected"
        self._tw.line(f"px test  •  Python {py_ver}  •  pytest {pytest.__version__}", cyan=True, bold=True)
        self._tw.line(f"root:   {root}")
        self._tw.line(f"config: {cfg}")
        self.collection_start = time.time()

    def pytest_collection_finish(self, session):
        self.collected = len(session.items)
        files = {str(item.fspath) for item in session.items}
        self.files = sorted(files)
        self.collection_duration = time.time() - (self.collection_start or self.session_start)
        label = "tests" if self.collected != 1 else "test"
        file_label = "files" if len(self.files) != 1 else "file"
        self._tw.line(f"collected {self.collected} {label} from {len(self.files)} {file_label} in {self.collection_duration:.2f}s")
        self._tw.line("")

    def pytest_runtest_logreport(self, report):
        if report.when not in ("setup", "call", "teardown"):
            return
        status = None
        if report.passed and report.when == "call":
            status = "passed"
            self.stats["passed"] += 1
        elif report.skipped:
            status = "skipped"
            self.stats["skipped"] += 1
        elif report.failed:
            status = "failed" if report.when == "call" else "error"
            self.stats[status] += 1

        if status:
            file_path = str(report.location[0])
            name = report.location[2]
            duration = getattr(report, "duration", 0.0)
            self._print_test_result(file_path, name, status, duration)

        if report.failed:
            self.failures.append(report)

    def pytest_sessionfinish(self, session, exitstatus):
        self.exitstatus = exitstatus
        if self.failures:
            self._render_failures()
        self._render_summary(exitstatus)

    # --- rendering helpers ---
    def _print_test_result(self, file_path, name, status, duration):
        if self._current_file != file_path:
            self._current_file = file_path
            self._tw.line("")
            self._tw.line(file_path)
        icon, color = self._status_icon(status)
        dur = f"{duration:.2f}s"
        line = f"  {icon} {name}  {dur}"
        self._tw.line(line, **color)

    def _render_failures(self):
        self._tw.line(f"FAILURES ({len(self.failures)})", red=True, bold=True)
        self._tw.line("-" * 11)
        for idx, report in enumerate(self.failures, start=1):
            self._render_single_failure(idx, report)

    def _render_single_failure(self, idx, report):
        path, lineno = self._failure_lineno(report)
        self._tw.line("")
        self._tw.line(f"{idx}) {report.nodeid}", bold=True)
        self._tw.line("")
        message = self._failure_message(report)
        if message:
            self._tw.line(f"   {message}", red=True)
            self._tw.line("")
        snippet = self._load_snippet(path, lineno)
        if snippet:
            file_line = f"   {path}:{lineno}"
            self._tw.line(file_line)
            for i, text in snippet:
                pointer = "→" if i == lineno else " "
                self._tw.line(f"  {pointer}{i:>4}  {text}")
            self._tw.line("")
        explanation = self._assertion_explanation(report)
        if explanation:
            self._tw.line("   Explanation:")
            for line in explanation:
                self._tw.line(f"     {line}")

    def _render_summary(self, exitstatus):
        total = sum(self.stats.values())
        duration = time.time() - self.session_start
        status_label = "✓ PASSED" if exitstatus == 0 else "✗ FAILED"
        status_color = {"green": exitstatus == 0, "red": exitstatus != 0, "bold": True}
        self._tw.line("")
        self._tw.line(f"RESULT   {status_label} (exit code {exitstatus})", **status_color)
        self._tw.line(f"TOTAL    {total} tests in {duration:.2f}s")
        self._tw.line(f"PASSED   {self.stats['passed']}")
        self._tw.line(f"FAILED   {self.stats['failed']}")
        self._tw.line(f"SKIPPED  {self.stats['skipped']}")
        self._tw.line(f"ERRORS   {self.stats['error']}")

    # --- utility helpers ---
    def _status_icon(self, status):
        if status in ("passed", "xpassed"):
            return "✓", {"green": True}
        if status in ("skipped", "xfailed"):
            return "∙", {"yellow": True}
        return "✗", {"red": True, "bold": True}

    def _failure_message(self, report):
        longrepr = getattr(report, "longrepr", None)
        if hasattr(longrepr, "reprcrash") and longrepr.reprcrash:
            return longrepr.reprcrash.message
        if hasattr(report, "longreprtext"):
            return report.longreprtext.splitlines()[0]
        return str(longrepr) if longrepr else "test failed"

    def _load_snippet(self, path, lineno, context=2):
        path = str(path)
        try:
            with open(path, "r", encoding="utf-8") as f:
                lines = f.readlines()
        except OSError:
            return None
        start = max(0, lineno - context - 1)
        end = min(len(lines), lineno + context)
        snippet = []
        for idx in range(start, end):
            text = lines[idx].rstrip("\n")
            snippet.append((idx + 1, text))
        return snippet

    def _failure_lineno(self, report):
        longrepr = getattr(report, "longrepr", None)
        if hasattr(longrepr, "reprcrash") and longrepr.reprcrash:
            return str(longrepr.reprcrash.path), longrepr.reprcrash.lineno
        path, lineno, _ = report.location
        return str(path), lineno + 1

    def _assertion_explanation(self, report):
        longrepr = getattr(report, "longrepr", None)
        summary = None
        if hasattr(longrepr, "reprcrash") and longrepr.reprcrash:
            summary = longrepr.reprcrash.message or ""
        if summary:
            lowered = summary.lower()
            if "did not raise" in lowered:
                expected = summary.split("DID NOT RAISE")[-1].strip()
                expected = expected or "expected exception"
                summary = f"Expected {expected} to be raised, but none was."
            elif "assert" in lowered and "==" in summary:
                parts = summary.split("==", 1)
                left = parts[0].replace("AssertionError:", "").replace("assert", "", 1).strip()
                right = parts[1].strip()
                summary = f"Expected: {right}"
                if left:
                    summary += f"\n     Actual:   {left}"
            else:
                summary = summary.replace("AssertionError:", "").strip()
        if not summary:
            return None
        parts = summary.split("\n")
        return [part for part in parts if part.strip()]


def pytest_configure(config):
    config.option.color = "yes"
    pm = config.pluginmanager
    reporter = PxTerminalReporter(config)
    default = pm.getplugin("terminalreporter")
    if default:
        pm.unregister(default)
        pm.register(reporter, "terminalreporter")
        config._px_reporter_registered = True
    else:
        config._px_reporter_registered = False
    config._px_reporter = reporter


def pytest_sessionstart(session):
    config = session.config
    reporter = getattr(config, "_px_reporter", None)
    if reporter is None:
        return
    if not getattr(config, "_px_reporter_registered", False):
        pm = config.pluginmanager
        default = pm.getplugin("terminalreporter")
        if default and default is not reporter:
            pm.unregister(default)
        pm.register(reporter, "terminalreporter")
        config._px_reporter_registered = True
    reporter.pytest_sessionstart(session)
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GlobalOptions, SystemEffects};
    use px_domain::PxOptions;
    use serde_json::json;
    use std::fs;
    use std::sync::Arc;
    use tempfile::tempdir;

    fn ctx_with_defaults() -> CommandContext<'static> {
        static GLOBAL: GlobalOptions = GlobalOptions {
            quiet: false,
            verbose: 0,
            trace: false,
            json: false,
            config: None,
        };
        CommandContext::new(&GLOBAL, Arc::new(SystemEffects::new())).expect("ctx")
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
            python,
            pythonpath: String::new(),
            allowed_paths: vec![temp.path().to_path_buf()],
            site_bin: None,
            pep582_bin: Vec::new(),
            px_options: PxOptions {
                manage_command: Some("self".into()),
                plugin_imports: vec!["json".into()],
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
            python,
            pythonpath: String::new(),
            allowed_paths: vec![temp.path().to_path_buf()],
            site_bin: None,
            pep582_bin: Vec::new(),
            px_options: PxOptions {
                manage_command: None,
                plugin_imports: vec!["px_missing_plugin_mod".into()],
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
        assert_eq!(flags, vec!["--color=yes", "--tb=short", "-q"]);
    }

    #[test]
    fn default_pytest_flags_pytest_reporter_matches() {
        let flags = default_pytest_flags(TestReporter::Pytest);
        assert_eq!(flags, vec!["--color=yes", "--tb=short", "-q"]);
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
}
