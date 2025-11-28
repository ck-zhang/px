use std::env;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::Result;
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
    pub pytest_args: Vec<String>,
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

/// Runs the project's tests using either pytest or px's fallback runner.
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

    if let Some(ws_ctx) = match prepare_workspace_run_context(ctx, strict) {
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

    if let Some(ws_ctx) = match prepare_workspace_run_context(ctx, strict) {
        Ok(result) => result,
        Err(outcome) => return Ok(outcome),
    } {
        let command_args = json!({ "pytest_args": request.pytest_args });
        let (mut envs, _) = build_env_with_preflight(ctx, &ws_ctx.py_ctx, &command_args)?;
        envs.push(("PX_TEST_RUNNER".into(), "pytest".into()));

        if ctx.config().test.fallback_builtin {
            let mut outcome = run_builtin_tests("test", ctx, &ws_ctx.py_ctx, envs)?;
            attach_autosync_details(&mut outcome, ws_ctx.sync_report);
            return Ok(outcome);
        }

        let pytest_cmd = build_pytest_command(&ws_ctx.py_ctx.project_root, &request.pytest_args);

        let output = ctx.python_runtime().run_command(
            &ws_ctx.py_ctx.python,
            &pytest_cmd,
            &envs,
            &ws_ctx.py_ctx.project_root,
        )?;
        if output.code == 0 {
            let mut outcome = outcome_from_output("test", "pytest", &output, "px test", None);
            attach_autosync_details(&mut outcome, ws_ctx.sync_report);
            return Ok(outcome);
        }

        if missing_pytest(&output.stderr) {
            let mut outcome = if ctx.config().test.fallback_builtin {
                run_builtin_tests("test", ctx, &ws_ctx.py_ctx, envs)?
            } else {
                ExecutionOutcome::user_error(
                    "pytest is not available in the project environment",
                    json!({
                        "stdout": output.stdout,
                        "stderr": output.stderr,
                        "hint": "Add pytest to your project (for example `px add --dev pytest` or enable your test dependency group), then rerun `px test`.",
                        "reason": "missing_pytest",
                    }),
                )
            };
            attach_autosync_details(&mut outcome, ws_ctx.sync_report);
            return Ok(outcome);
        }

        let mut outcome = outcome_from_output("test", "pytest", &output, "px test", None);
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
    let command_args = json!({ "pytest_args": request.pytest_args });
    let (mut envs, _preflight) = build_env_with_preflight(ctx, &py_ctx, &command_args)?;
    envs.push(("PX_TEST_RUNNER".into(), "pytest".into()));

    if ctx.config().test.fallback_builtin {
        let mut outcome = run_builtin_tests("test", ctx, &py_ctx, envs)?;
        attach_autosync_details(&mut outcome, sync_report);
        return Ok(outcome);
    }

    let pytest_cmd = build_pytest_command(&py_ctx.project_root, &request.pytest_args);

    let output = ctx.python_runtime().run_command(
        &py_ctx.python,
        &pytest_cmd,
        &envs,
        &py_ctx.project_root,
    )?;
    if output.code == 0 {
        let mut outcome = outcome_from_output("test", "pytest", &output, "px test", None);
        attach_autosync_details(&mut outcome, sync_report);
        return Ok(outcome);
    }

    if missing_pytest(&output.stderr) {
        let mut outcome = if ctx.config().test.fallback_builtin {
            run_builtin_tests("test", ctx, &py_ctx, envs)?
        } else {
            ExecutionOutcome::user_error(
                "pytest is not available in the project environment",
                json!({
                    "stdout": output.stdout,
                    "stderr": output.stderr,
                    "hint": "Add pytest to your project (for example `px add --dev pytest` or enable your test dependency group), then rerun `px test`.",
                    "reason": "missing_pytest",
                }),
            )
        };
        attach_autosync_details(&mut outcome, sync_report);
        return Ok(outcome);
    }

    let mut outcome = ExecutionOutcome::failure(
        format!("px test failed (exit {})", output.code),
        json!({
            "stdout": output.stdout,
            "stderr": output.stderr,
            "code": output.code,
        }),
    );
    attach_autosync_details(&mut outcome, sync_report);
    Ok(outcome)
}

fn run_builtin_tests(
    command_name: &str,
    core_ctx: &CommandContext,
    ctx: &PythonContext,
    mut envs: Vec<(String, String)>,
) -> Result<ExecutionOutcome> {
    envs.push(("PX_TEST_RUNNER".into(), "builtin".into()));
    let script = "from sample_px_app import cli\nassert cli.greet() == 'Hello, World!'\nprint('px fallback test passed')";
    let args = vec!["-c".to_string(), script.to_string()];
    let output =
        core_ctx
            .python_runtime()
            .run_command(&ctx.python, &args, &envs, &ctx.project_root)?;
    Ok(outcome_from_output(
        command_name,
        "builtin",
        &output,
        "px test",
        None,
    ))
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

fn build_pytest_command(project_root: &Path, extra_args: &[String]) -> Vec<String> {
    let mut pytest_cmd = vec!["-m".to_string(), "pytest".to_string()];
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
}
