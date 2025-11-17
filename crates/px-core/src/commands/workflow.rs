use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{json, Value};
use toml_edit::{DocumentMut, Item, Table};

use crate::{
    attach_autosync_details, outcome_from_output, project_table, python_context_with_mode,
    CommandContext, EnvGuard, ExecutionOutcome, PythonContext,
};

#[derive(Clone, Debug)]
pub struct WorkflowTestRequest {
    pub pytest_args: Vec<String>,
    pub frozen: bool,
}

#[derive(Clone, Debug)]
pub struct WorkflowRunRequest {
    pub entry: Option<String>,
    pub target: Option<String>,
    pub args: Vec<String>,
    pub frozen: bool,
}

pub fn workflow_test(
    ctx: &CommandContext,
    request: WorkflowTestRequest,
) -> Result<ExecutionOutcome> {
    workflow_test_outcome(ctx, &request)
}

pub fn workflow_run(ctx: &CommandContext, request: WorkflowRunRequest) -> Result<ExecutionOutcome> {
    workflow_run_outcome(ctx, &request)
}

fn workflow_run_outcome(
    ctx: &CommandContext,
    request: &WorkflowRunRequest,
) -> Result<ExecutionOutcome> {
    let strict = request.frozen || ctx.env_flag_enabled("CI");
    let guard = if strict {
        EnvGuard::Strict
    } else {
        EnvGuard::AutoSync
    };
    let (py_ctx, sync_report) = match python_context_with_mode(ctx, guard) {
        Ok(result) => result,
        Err(outcome) => return Ok(outcome),
    };
    let extra_args = request.args.clone();
    let command_args = json!({
        "entry": request.entry,
        "target": request.target,
        "args": extra_args,
    });
    if let Some(entry) = request.entry.as_deref() {
        if let Some(target) = detect_passthrough_target(entry, &py_ctx) {
            let mut outcome =
                run_passthrough(ctx, &py_ctx, target, extra_args.clone(), &command_args)?;
            attach_autosync_details(&mut outcome, sync_report);
            return Ok(outcome);
        }
    }

    let resolved = match request.entry.clone() {
        Some(entry) => ResolvedEntry::explicit(entry),
        None => {
            let manifest = py_ctx.project_root.join("pyproject.toml");
            if !manifest.exists() {
                return Ok(DefaultEntryIssue::MissingManifest(manifest).into_outcome(&py_ctx));
            }
            match infer_default_entry(&manifest)? {
                Some(entry) => entry,
                None => {
                    return Ok(DefaultEntryIssue::NoScripts(manifest).into_outcome(&py_ctx));
                }
            }
        }
    };
    let mut outcome = run_module_entry(ctx, &py_ctx, resolved, extra_args, &command_args)?;
    attach_autosync_details(&mut outcome, sync_report);
    Ok(outcome)
}

fn workflow_test_outcome(
    ctx: &CommandContext,
    request: &WorkflowTestRequest,
) -> Result<ExecutionOutcome> {
    let strict = request.frozen || ctx.env_flag_enabled("CI");
    let guard = if strict {
        EnvGuard::Strict
    } else {
        EnvGuard::AutoSync
    };
    let (py_ctx, sync_report) = match python_context_with_mode(ctx, guard) {
        Ok(result) => result,
        Err(outcome) => return Ok(outcome),
    };
    let command_args = json!({ "pytest_args": request.pytest_args });
    let mut envs = py_ctx.base_env(&command_args)?;
    envs.push(("PX_TEST_RUNNER".into(), "pytest".into()));

    if ctx.config().test.fallback_builtin {
        let mut outcome = run_builtin_tests("test", ctx, py_ctx, envs)?;
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
        let mut outcome = outcome_from_output("test", "pytest", output, "px test", None);
        attach_autosync_details(&mut outcome, sync_report);
        return Ok(outcome);
    }

    if missing_pytest(&output.stderr) {
        let mut outcome = run_builtin_tests("test", ctx, py_ctx, envs)?;
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
    source: EntrySource,
}

impl ResolvedEntry {
    fn explicit(entry: String) -> Self {
        Self {
            entry,
            source: EntrySource::Explicit,
        }
    }
}

#[derive(Debug, Clone)]
enum EntrySource {
    Explicit,
    ProjectScript { script: String },
    PackageCli { package: String },
}

impl EntrySource {
    fn label(&self) -> &'static str {
        match self {
            EntrySource::Explicit => "explicit",
            EntrySource::ProjectScript { .. } => "project-scripts",
            EntrySource::PackageCli { .. } => "package-cli",
        }
    }

    fn script_name(&self) -> Option<&str> {
        match self {
            EntrySource::ProjectScript { script } => Some(script.as_str()),
            _ => None,
        }
    }

    fn is_inferred(&self) -> bool {
        !matches!(self, EntrySource::Explicit)
    }
}

#[derive(Debug)]
enum DefaultEntryIssue {
    MissingManifest(PathBuf),
    NoScripts(PathBuf),
}

impl DefaultEntryIssue {
    fn into_outcome(self, ctx: &PythonContext) -> ExecutionOutcome {
        match self {
            DefaultEntryIssue::MissingManifest(path) => ExecutionOutcome::user_error(
                format!("pyproject.toml not found in {}", ctx.project_root.display()),
                json!({
                    "hint": "run `px migrate --apply` or pass ENTRY explicitly",
                    "project_root": ctx.project_root.display().to_string(),
                    "manifest": path.display().to_string(),
                }),
            ),
            DefaultEntryIssue::NoScripts(path) => ExecutionOutcome::user_error(
                "no default entry found; add [project.scripts] or pass ENTRY",
                json!({
                    "hint": "add [project.scripts] to pyproject.toml or run `px run <module>`",
                    "manifest": path.display().to_string(),
                }),
            ),
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
    ctx: PythonContext,
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
        output,
        "px test",
        None,
    ))
}

fn run_module_entry(
    core_ctx: &CommandContext,
    py_ctx: &PythonContext,
    resolved: ResolvedEntry,
    extra_args: Vec<String>,
    command_args: &Value,
) -> Result<ExecutionOutcome> {
    let ResolvedEntry { entry, source } = resolved;
    let mut python_args = vec!["-m".to_string(), entry.clone()];
    python_args.extend(extra_args.iter().cloned());

    let mut envs = py_ctx.base_env(command_args)?;
    envs.push(("PX_RUN_ENTRY".into(), entry.clone()));

    let output = core_ctx.python_runtime().run_command(
        &py_ctx.python,
        &python_args,
        &envs,
        &py_ctx.project_root,
    )?;
    let mut details = json!({
        "mode": "module",
        "entry": entry.clone(),
        "args": extra_args,
        "source": source.label(),
    });
    if let Some(script) = source.script_name() {
        details["script"] = Value::String(script.to_string());
    }
    if source.is_inferred() {
        details["defaulted"] = Value::Bool(true);
    }
    if let EntrySource::PackageCli { package } = &source {
        details["package"] = Value::String(package.clone());
    }

    Ok(outcome_from_output(
        "run",
        &entry,
        output,
        "px run",
        Some(details),
    ))
}

fn run_passthrough(
    core_ctx: &CommandContext,
    py_ctx: &PythonContext,
    target: PassthroughTarget,
    extra_args: Vec<String>,
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
            args.extend(extra_args.clone());
            args
        }
        _ => extra_args.clone(),
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
        output,
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

fn python_script_target(entry: &str, root: &Path) -> Option<(String, String)> {
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
        .map(|ext| ext.eq_ignore_ascii_case("py") || ext.eq_ignore_ascii_case("pyw"))
        .unwrap_or(false)
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

fn infer_default_entry(manifest: &Path) -> Result<Option<ResolvedEntry>> {
    let contents = fs::read_to_string(manifest)?;
    let doc: DocumentMut = contents.parse()?;
    let project = project_table(&doc)?;

    if let Some((script, module)) = first_script_entry(project) {
        return Ok(Some(ResolvedEntry {
            entry: module,
            source: EntrySource::ProjectScript { script },
        }));
    }

    if let Some(name) = project.get("name").and_then(Item::as_str) {
        if !name.trim().is_empty() {
            let module = package_module_name(name);
            return Ok(Some(ResolvedEntry {
                entry: format!("{module}.cli"),
                source: EntrySource::PackageCli {
                    package: name.to_string(),
                },
            }));
        }
    }

    Ok(None)
}

fn first_script_entry(project: &Table) -> Option<(String, String)> {
    let scripts = project.get("scripts")?.as_table()?;
    for (name, item) in scripts.iter() {
        if let Some(value) = item.as_str() {
            if let Some(module) = parse_script_value(value) {
                return Some((name.to_string(), module));
            }
        }
    }
    None
}

fn parse_script_value(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let module = trimmed
        .split(|c| c == ':' || c == ' ')
        .next()
        .map(|part| part.trim())
        .unwrap_or("");
    if module.is_empty() {
        None
    } else {
        Some(module.to_string())
    }
}

fn package_module_name(name: &str) -> String {
    name.replace('-', "_").replace(' ', "_")
}

fn missing_pytest(stderr: &str) -> bool {
    stderr.contains("No module named") && stderr.contains("pytest")
}
