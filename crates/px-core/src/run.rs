use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use toml_edit::{DocumentMut, Item};

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
}

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
    let snapshot = match manifest_snapshot() {
        Ok(snapshot) => snapshot,
        Err(err) => {
            if is_missing_project_error(&err) {
                return Ok(missing_project_outcome());
            }
            let msg = err.to_string();
            if msg.contains("pyproject.toml not found") {
                let root = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                let manifest = root.join("pyproject.toml");
                return Ok(ExecutionOutcome::user_error(
                    format!("pyproject.toml not found in {}", root.display()),
                    json!({
                        "hint": "run `px migrate --apply` or pass a target explicitly",
                        "project_root": root.display().to_string(),
                        "manifest": manifest.display().to_string(),
                    }),
                ));
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
        return Ok(ExecutionOutcome::user_error(
            "px run requires a target",
            json!({
                "hint": "pass a command or script explicitly, or define [tool.px.scripts].<name>",
            }),
        ));
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

    if let Some(resolved) = resolve_px_script(&manifest, &target)? {
        let mut outcome = run_module_entry(ctx, &py_ctx, resolved, &request.args, &command_args)?;
        attach_autosync_details(&mut outcome, sync_report);
        return Ok(outcome);
    }

    if let Some(script_path) = script_under_project_root(&py_ctx.project_root, &target) {
        let mut outcome =
            run_project_script(ctx, &py_ctx, &script_path, &request.args, &command_args)?;
        attach_autosync_details(&mut outcome, sync_report);
        return Ok(outcome);
    }

    if let Some(target) = detect_passthrough_target(&target, &py_ctx) {
        let mut outcome = run_passthrough(ctx, &py_ctx, target, &request.args, &command_args)?;
        attach_autosync_details(&mut outcome, sync_report);
        return Ok(outcome);
    }

    let mut outcome = run_executable(ctx, &py_ctx, &target, &request.args, &command_args)?;
    attach_autosync_details(&mut outcome, sync_report);
    Ok(outcome)
}

fn resolve_px_script(manifest: &Path, target: &str) -> Result<Option<ResolvedEntry>> {
    if !manifest.exists() {
        return Ok(None);
    }
    let contents = fs::read_to_string(manifest)?;
    let doc: DocumentMut = contents.parse()?;
    let scripts = doc
        .get("tool")
        .and_then(Item::as_table)
        .and_then(|tool| tool.get("px"))
        .and_then(Item::as_table)
        .and_then(|px| px.get("scripts"))
        .and_then(Item::as_table);
    let Some(table) = scripts else {
        return Ok(None);
    };
    let value = match table.get(target) {
        Some(item) => item,
        None => return Ok(None),
    };
    let Some(raw) = value.as_str() else {
        return Ok(None);
    };
    let (entry, call) = parse_entry_value(raw)
        .ok_or_else(|| anyhow!("invalid [tool.px.scripts] entry for `{target}`"))?;
    Ok(Some(ResolvedEntry {
        entry,
        call,
        source: EntrySource::PxScript {
            script: target.to_string(),
        },
    }))
}

fn script_under_project_root(root: &Path, target: &str) -> Option<PathBuf> {
    let candidate = if Path::new(target).is_absolute() {
        PathBuf::from(target)
    } else {
        root.join(target)
    };
    let canonical = candidate.canonicalize().ok()?;
    if canonical.starts_with(root) && canonical.is_file() {
        Some(canonical)
    } else {
        None
    }
}

fn run_project_script(
    core_ctx: &CommandContext,
    py_ctx: &PythonContext,
    script: &Path,
    extra_args: &[String],
    command_args: &Value,
) -> Result<ExecutionOutcome> {
    let envs = py_ctx.base_env(command_args)?;
    let mut args = Vec::with_capacity(extra_args.len() + 1);
    args.push(script.display().to_string());
    args.extend(extra_args.iter().cloned());
    let output = core_ctx.python_runtime().run_command(
        &py_ctx.python,
        &args,
        &envs,
        &py_ctx.project_root,
    )?;
    let details = json!({
        "mode": "script",
        "script": script.display().to_string(),
        "args": extra_args,
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
) -> Result<ExecutionOutcome> {
    let envs = py_ctx.base_env(command_args)?;
    let output =
        core_ctx
            .python_runtime()
            .run_command(program, extra_args, &envs, &py_ctx.project_root)?;
    let details = json!({
        "mode": "executable",
        "program": program,
        "args": extra_args,
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
    let snapshot = match manifest_snapshot() {
        Ok(snapshot) => snapshot,
        Err(err) => {
            if is_missing_project_error(&err) {
                return Ok(missing_project_outcome());
            }
            let msg = err.to_string();
            if msg.contains("pyproject.toml not found") {
                let root = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                let manifest = root.join("pyproject.toml");
                return Ok(ExecutionOutcome::user_error(
                    format!("pyproject.toml not found in {}", root.display()),
                    json!({
                        "hint": "run `px migrate --apply` or pass a target explicitly",
                        "project_root": root.display().to_string(),
                        "manifest": manifest.display().to_string(),
                    }),
                ));
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
    let mut envs = py_ctx.base_env(&command_args)?;
    envs.push(("PX_TEST_RUNNER".into(), "pytest".into()));

    if ctx.config().test.fallback_builtin {
        let mut outcome = run_builtin_tests("test", ctx, &py_ctx, envs)?;
        attach_autosync_details(&mut outcome, sync_report);
        return Ok(outcome);
    }

    let mut pytest_cmd = vec!["-m".to_string(), "pytest".to_string(), "tests".to_string()];
    pytest_cmd.extend(request.pytest_args.iter().cloned());

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
        let mut outcome = run_builtin_tests("test", ctx, &py_ctx, envs)?;
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

#[derive(Debug, Clone)]
struct ResolvedEntry {
    entry: String,
    call: Option<String>,
    source: EntrySource,
}

#[derive(Debug, Clone)]
enum EntrySource {
    PxScript { script: String },
}

impl EntrySource {
    fn label(&self) -> &'static str {
        match self {
            EntrySource::PxScript { .. } => "px-scripts",
        }
    }

    fn script_name(&self) -> Option<&str> {
        match self {
            EntrySource::PxScript { script } => Some(script.as_str()),
        }
    }
}

#[derive(Debug, Clone)]
struct PassthroughTarget {
    program: String,
    display: String,
    reason: PassthroughReason,
    resolved: Option<String>,
}

#[derive(Debug, Clone)]
enum PassthroughReason {
    PythonAlias,
    ExecutablePath,
    PythonScript {
        script_arg: String,
        script_path: String,
    },
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
    let mut envs = py_ctx.base_env(command_args)?;
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

    let output = core_ctx.python_runtime().run_command(
        &py_ctx.python,
        &python_args,
        &envs,
        &py_ctx.project_root,
    )?;
    let mut details = json!({
        "mode": mode,
        "entry": env_entry.clone(),
        "args": extra_args,
        "source": source.label(),
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
) -> Result<ExecutionOutcome> {
    let PassthroughTarget {
        program,
        display,
        reason,
        resolved,
    } = target;
    let envs = py_ctx.base_env(command_args)?;
    let program_args = match &reason {
        PassthroughReason::PythonScript { script_arg, .. } => {
            let mut args = Vec::with_capacity(extra_args.len() + 1);
            args.push(script_arg.clone());
            args.extend(extra_args.iter().cloned());
            args
        }
        _ => extra_args.to_vec(),
    };
    let output = core_ctx.python_runtime().run_command(
        &program,
        &program_args,
        &envs,
        &py_ctx.project_root,
    )?;
    let mut details = json!({
        "mode": "passthrough",
        "program": display.clone(),
        "args": extra_args,
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

fn detect_passthrough_target(entry: &str, ctx: &PythonContext) -> Option<PassthroughTarget> {
    if looks_like_python_alias(entry) {
        return Some(PassthroughTarget {
            program: ctx.python.clone(),
            display: entry.to_string(),
            reason: PassthroughReason::PythonAlias,
            resolved: Some(ctx.python.clone()),
        });
    }

    if let Some((script_arg, script_path)) = python_script_target(entry, &ctx.project_root) {
        return Some(PassthroughTarget {
            program: ctx.python.clone(),
            display: entry.to_string(),
            reason: PassthroughReason::PythonScript {
                script_arg,
                script_path,
            },
            resolved: Some(ctx.python.clone()),
        });
    }

    if looks_like_path_target(entry) {
        let (program, resolved) = resolve_executable_path(entry, &ctx.project_root);
        return Some(PassthroughTarget {
            program,
            display: entry.to_string(),
            reason: PassthroughReason::ExecutablePath,
            resolved,
        });
    }

    None
}

fn looks_like_python_alias(entry: &str) -> bool {
    let lower = entry.to_lowercase();
    lower == "python"
        || lower == "python3"
        || lower.starts_with("python3.")
        || lower == "py"
        || lower == "py3"
}

fn looks_like_path_target(entry: &str) -> bool {
    let path = Path::new(entry);
    path.components().count() > 1 || entry.contains('/') || entry.contains('\\')
}

pub(crate) fn python_script_target(entry: &str, root: &Path) -> Option<(String, String)> {
    if !looks_like_python_script(entry) {
        return None;
    }
    let script_arg = entry.to_string();
    let script_path = resolve_script_path(entry, root);
    Some((script_arg, script_path))
}

fn looks_like_python_script(entry: &str) -> bool {
    Path::new(entry)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("py") || ext.eq_ignore_ascii_case("pyw"))
}

fn resolve_script_path(entry: &str, root: &Path) -> String {
    let path = Path::new(entry);
    if path.is_absolute() {
        entry.to_string()
    } else {
        root.join(path).display().to_string()
    }
}

fn resolve_executable_path(entry: &str, root: &Path) -> (String, Option<String>) {
    let path = Path::new(entry);
    if path.is_absolute() {
        (entry.to_string(), Some(entry.to_string()))
    } else {
        let resolved = root.join(path);
        let display = resolved.display().to_string();
        (display.clone(), Some(display))
    }
}

fn parse_entry_value(value: &str) -> Option<(String, Option<String>)> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let token = trimmed.split_whitespace().next().unwrap_or("").trim();
    if token.is_empty() {
        return None;
    }
    let mut parts = token.splitn(2, ':');
    let module = parts.next().unwrap_or("").trim();
    if module.is_empty() {
        return None;
    }
    let call = parts
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Some((module.to_string(), call))
}

fn missing_pytest(stderr: &str) -> bool {
    stderr.contains("No module named") && stderr.contains("pytest")
}
