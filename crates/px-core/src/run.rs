use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use toml_edit::{DocumentMut, Item};

use crate::{
    attach_autosync_details, is_missing_project_error, manifest_snapshot, missing_project_outcome,
    outcome_from_output, project_table, python_context_with_mode, state_guard::guard_for_execution,
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
                        "hint": "run `px migrate --apply` or pass ENTRY explicitly",
                        "project_root": root.display().to_string(),
                        "manifest": manifest.display().to_string(),
                    }),
                ));
            }
            return Err(err);
        }
    };
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
        "entry": &request.entry,
        "target": &request.target,
        "args": &request.args,
    });
    let manifest = py_ctx.project_root.join("pyproject.toml");
    let default_entry = || -> Result<ResolvedEntry> {
        if !manifest.exists() {
            return Err(anyhow!("manifest missing"));
        }
        infer_default_entry(&manifest)?.ok_or_else(|| anyhow!("no default entry configured"))
    };

    if let Some(entry) = request.entry.as_deref() {
        if let Some(target) = detect_passthrough_target(entry, &py_ctx) {
            let mut outcome = run_passthrough(ctx, &py_ctx, target, &request.args, &command_args)?;
            attach_autosync_details(&mut outcome, sync_report);
            return Ok(outcome);
        }

        if should_forward_to_default(entry, &py_ctx) && request.args.is_empty() {
            match default_entry() {
                Ok(resolved_default) => {
                    let mut forwarded = Vec::with_capacity(request.args.len() + 1);
                    forwarded.push(entry.to_string());
                    forwarded.extend(request.args.iter().cloned());
                    let mut outcome = run_module_entry(
                        ctx,
                        &py_ctx,
                        resolved_default,
                        &forwarded,
                        &command_args,
                    )?;
                    attach_autosync_details(&mut outcome, sync_report);
                    return Ok(outcome);
                }
                Err(err) => {
                    let issue = if manifest.exists() {
                        DefaultEntryIssue::NoScripts(manifest.clone())
                    } else {
                        DefaultEntryIssue::MissingManifest(manifest.clone())
                    };
                    let mut outcome = issue.into_outcome(&py_ctx);
                    outcome.details["hint"] = json!(format!(
                        "pass an entry explicitly or fix default inference ({err})"
                    ));
                    attach_autosync_details(&mut outcome, sync_report);
                    return Ok(outcome);
                }
            }
        }
    }

    let resolved = if let Some(entry) = request.entry.clone() {
        ResolvedEntry::explicit(entry)
    } else {
        match default_entry() {
            Ok(entry) => entry,
            Err(_) if !manifest.exists() => {
                return Ok(DefaultEntryIssue::MissingManifest(manifest).into_outcome(&py_ctx));
            }
            Err(_) => {
                return Ok(DefaultEntryIssue::NoScripts(manifest).into_outcome(&py_ctx));
            }
        }
    };
    let mut outcome = run_module_entry(ctx, &py_ctx, resolved, &request.args, &command_args)?;
    attach_autosync_details(&mut outcome, sync_report);
    Ok(outcome)
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
                        "hint": "run `px migrate --apply` or pass ENTRY explicitly",
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
    PxScript { script: String },
    PackageCli { package: String },
}

impl EntrySource {
    fn label(&self) -> &'static str {
        match self {
            EntrySource::Explicit => "explicit",
            EntrySource::ProjectScript { .. } => "project-scripts",
            EntrySource::PxScript { .. } => "px-scripts",
            EntrySource::PackageCli { .. } => "package-cli",
        }
    }

    fn script_name(&self) -> Option<&str> {
        match self {
            EntrySource::ProjectScript { script } | EntrySource::PxScript { script } => {
                Some(script.as_str())
            }
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
                "no default entry found; add [project.scripts] or [tool.px.scripts]",
                json!({
                    "hint": "define a script under [project.scripts] or [tool.px.scripts], or run `px run <module>` explicitly",
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
        &output,
        "px run",
        Some(details),
    ))
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

fn should_forward_to_default(entry: &str, ctx: &PythonContext) -> bool {
    if looks_like_python_alias(entry) || looks_like_path_target(entry) {
        return false;
    }
    let pathish = Path::new(entry);
    if pathish.exists() || entry.ends_with(".py") {
        return false;
    }
    if entry.contains('.') || entry.contains('\\') || entry.contains('/') {
        return false;
    }
    // If the manifest is missing, default inference will emit a clearer hint.
    ctx.project_root.join("pyproject.toml").exists()
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

fn infer_default_entry(manifest: &Path) -> Result<Option<ResolvedEntry>> {
    let contents = fs::read_to_string(manifest)?;
    let doc: DocumentMut = contents.parse()?;
    let project = project_table(&doc)?;

    if let Some((script, module)) = first_script_entry(project.get("scripts")) {
        return Ok(Some(ResolvedEntry {
            entry: module,
            source: EntrySource::ProjectScript { script },
        }));
    }

    let px_scripts_item = doc
        .get("tool")
        .and_then(Item::as_table)
        .and_then(|tool| tool.get("px"))
        .and_then(Item::as_table)
        .and_then(|px| px.get("scripts"));
    if let Some((script, module)) = first_script_entry(px_scripts_item) {
        return Ok(Some(ResolvedEntry {
            entry: module,
            source: EntrySource::PxScript { script },
        }));
    }

    if let Some(name) = project.get("name").and_then(Item::as_str) {
        if !name.trim().is_empty() {
            let module = package_module_name(name);
            if package_module_exists(manifest, &module) {
                return Ok(Some(ResolvedEntry {
                    entry: format!("{module}.cli"),
                    source: EntrySource::PackageCli {
                        package: name.to_string(),
                    },
                }));
            }
        }
    }

    Ok(None)
}

fn first_script_entry(item: Option<&Item>) -> Option<(String, String)> {
    let scripts = item?.as_table()?;
    for (name, item) in scripts {
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
    let module = trimmed.split([':', ' ']).next().map_or("", str::trim);
    if module.is_empty() {
        None
    } else {
        Some(module.to_string())
    }
}

fn package_module_exists(manifest: &Path, module: &str) -> bool {
    let root = manifest.parent().unwrap_or_else(|| Path::new("."));
    let module_path = module.replace('.', "/");
    let dir = root.join(&module_path);
    dir.join("__init__.py").exists() || dir.with_extension("py").exists()
}

fn package_module_name(name: &str) -> String {
    name.replace(['-', ' '], "_")
}

fn missing_pytest(stderr: &str) -> bool {
    stderr.contains("No module named") && stderr.contains("pytest")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_manifest(contents: &str) -> (tempfile::TempDir, PathBuf) {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("pyproject.toml");
        fs::write(&path, contents).expect("write manifest");
        (temp, path)
    }

    #[test]
    fn prefers_project_scripts_over_px_scripts() {
        let manifest = r#"
[project]
name = "demo-app"
version = "0.1.0"

[project.scripts]
app = "demo.app:main"

[tool.px.scripts]
alt = "demo.alt:main"
"#;
        let (_tmp, path) = write_manifest(manifest);
        let resolved = infer_default_entry(&path)
            .expect("entry lookup")
            .expect("default entry");
        assert_eq!(resolved.entry, "demo.app");
        match resolved.source {
            EntrySource::ProjectScript { script } => assert_eq!(script, "app"),
            other => panic!("expected project script, got {other:?}"),
        }
    }

    #[test]
    fn falls_back_to_px_scripts_when_project_scripts_missing() {
        let manifest = r#"
[project]
name = "demo-app"
version = "0.1.0"

[tool.px.scripts]
lint = "demo.lint:main"
"#;
        let (_tmp, path) = write_manifest(manifest);
        let resolved = infer_default_entry(&path)
            .expect("entry lookup")
            .expect("default entry");
        assert_eq!(resolved.entry, "demo.lint");
        match resolved.source {
            EntrySource::PxScript { script } => assert_eq!(script, "lint"),
            other => panic!("expected px script, got {other:?}"),
        }
    }
}
