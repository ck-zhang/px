use super::*;

// Mapping note: `ephemeral.rs` implements `px run/test --ephemeral` by building a
// cache-rooted snapshot and running from the user's directory without writing
// `.px/` or `px.lock` into the working tree.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use toml_edit::{Array, DocumentMut, Item, Table, Value as TomlValue};

use crate::core::runtime::facade::{ensure_environment_with_guard, load_project_state};
use crate::core::runtime::runtime_manager;
use crate::core::runtime::EnvGuard;
use crate::core::sandbox::env_root_from_site_packages;
use crate::python_sys::detect_interpreter_tags;
use crate::EnvironmentSyncReport;

const EPHEMERAL_PROJECT_NAME: &str = "px-ephemeral";
const DEFAULT_EPHEMERAL_REQUIRES_PYTHON: &str = ">=3.8";

#[derive(Clone, Debug)]
enum EphemeralInput {
    InlineScript {
        requires_python: String,
        deps: Vec<String>,
    },
    Pyproject {
        requires_python: String,
        deps: Vec<String>,
    },
    Requirements {
        deps: Vec<String>,
    },
    Empty,
}

#[derive(Clone, Debug, Serialize)]
struct EphemeralKeyPayload<'a> {
    kind: &'a str,
    requires_python: &'a str,
    deps: &'a [String],
    runtime: &'a str,
    platform: &'a str,
    indexes: &'a [String],
    force_sdist: bool,
}

pub(super) fn run_ephemeral_outcome(
    ctx: &CommandContext,
    request: &RunRequest,
    target: &str,
    interactive: bool,
    _strict: bool,
) -> Result<ExecutionOutcome> {
    if request.at.is_some() {
        return Ok(ExecutionOutcome::user_error(
            "px run --ephemeral does not support --at",
            json!({
                "code": "PX903",
                "reason": "ephemeral_at_ref_unsupported",
                "hint": "Drop --at or run in an adopted px project directory.",
            }),
        ));
    }
    if target.trim().is_empty() {
        return Ok(run_target_required_outcome());
    }

    let invocation_root = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let input = match detect_ephemeral_input(&invocation_root, Some(target)) {
        Ok(input) => input,
        Err(outcome) => return Ok(outcome),
    };

    let pinned_required = request.frozen || ctx.env_flag_enabled("CI");
    if pinned_required {
        if let Err(outcome) = enforce_pinned_inputs("run", &invocation_root, &input, request.frozen)
        {
            return Ok(outcome);
        }
    }

    let (snapshot, runtime, sync_report) =
        match prepare_ephemeral_snapshot(ctx, &invocation_root, &input, request.frozen) {
            Ok(result) => result,
            Err(outcome) => return Ok(outcome),
        };

    let workdir = invocation_workdir(&invocation_root);
    let host_runner = HostCommandRunner::new(ctx);

    let mut cas_native_fallback: Option<CasNativeFallback> = None;
    if !request.sandbox {
        match prepare_cas_native_run_context(ctx, &snapshot, &invocation_root) {
            Ok(native_ctx) => {
                let mut command_args = json!({
                    "target": target,
                    "args": &request.args,
                });
                DependencyContext::from_sources(&snapshot.requirements, Some(&snapshot.lock_path))
                    .inject(&mut command_args);
                let plan = plan_run_target(
                    &native_ctx.py_ctx,
                    &snapshot.manifest_path,
                    target,
                    &workdir,
                )?;
                let outcome = match plan {
                    RunTargetPlan::Script(path) => run_project_script_cas_native(
                        ctx,
                        &host_runner,
                        &native_ctx,
                        &path,
                        &request.args,
                        &command_args,
                        &workdir,
                        interactive,
                    )?,
                    RunTargetPlan::Executable(program) => run_executable_cas_native(
                        ctx,
                        &host_runner,
                        &native_ctx,
                        &program,
                        &request.args,
                        &command_args,
                        &workdir,
                        interactive,
                    )?,
                };
                if let Some(reason) = cas_native_fallback_reason(&outcome) {
                    if is_integrity_failure(&outcome) {
                        return Ok(outcome);
                    }
                    cas_native_fallback = Some(CasNativeFallback {
                        reason,
                        summary: cas_native_fallback_summary(&outcome),
                    });
                } else {
                    let mut outcome = outcome;
                    attach_autosync_details(&mut outcome, sync_report);
                    return Ok(outcome);
                }
            }
            Err(outcome) => {
                let Some(reason) = cas_native_fallback_reason(&outcome) else {
                    return Ok(outcome);
                };
                if is_integrity_failure(&outcome) {
                    return Ok(outcome);
                }
                cas_native_fallback = Some(CasNativeFallback {
                    reason,
                    summary: cas_native_fallback_summary(&outcome),
                });
            }
        }
    }

    let py_ctx = match ephemeral_python_context(ctx, &snapshot, &runtime, &invocation_root) {
        Ok(py_ctx) => py_ctx,
        Err(outcome) => return Ok(outcome),
    };

    let mut sandbox: Option<SandboxRunContext> = None;
    if request.sandbox {
        let sbx = match prepare_project_sandbox(ctx, &snapshot) {
            Ok(sbx) => sbx,
            Err(outcome) => return Ok(outcome),
        };
        sandbox = Some(sbx);
    }

    let mut outcome = if let Some(ref mut sbx) = sandbox {
        let sandbox_runner = match sandbox_runner_for_context(&py_ctx, sbx, &workdir) {
            Ok(runner) => runner,
            Err(outcome) => return Ok(outcome),
        };
        run_ephemeral_materialized(
            ctx,
            request,
            target,
            &py_ctx,
            &sandbox_runner,
            &snapshot,
            &workdir,
            interactive,
            true,
        )?
    } else {
        run_ephemeral_materialized(
            ctx,
            request,
            target,
            &py_ctx,
            &host_runner,
            &snapshot,
            &workdir,
            interactive,
            false,
        )?
    };

    attach_autosync_details(&mut outcome, sync_report);
    if let Some(ref fallback) = cas_native_fallback {
        attach_cas_native_fallback(&mut outcome, fallback);
    }
    if let Some(ref sbx) = sandbox {
        attach_sandbox_details(&mut outcome, sbx);
    }
    Ok(outcome)
}

pub(super) fn test_ephemeral_outcome(
    ctx: &CommandContext,
    request: &TestRequest,
    _strict: bool,
) -> Result<ExecutionOutcome> {
    if request.at.is_some() {
        return Ok(ExecutionOutcome::user_error(
            "px test --ephemeral does not support --at",
            json!({
                "code": "PX903",
                "reason": "ephemeral_at_ref_unsupported",
                "hint": "Drop --at or run in an adopted px project directory.",
            }),
        ));
    }

    let invocation_root = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let input = match detect_ephemeral_input(&invocation_root, None) {
        Ok(input) => input,
        Err(outcome) => return Ok(outcome),
    };

    let pinned_required = request.frozen || ctx.env_flag_enabled("CI");
    if pinned_required {
        if let Err(outcome) =
            enforce_pinned_inputs("test", &invocation_root, &input, request.frozen)
        {
            return Ok(outcome);
        }
    }

    let (snapshot, runtime, sync_report) =
        match prepare_ephemeral_snapshot(ctx, &invocation_root, &input, request.frozen) {
            Ok(result) => result,
            Err(outcome) => return Ok(outcome),
        };

    let workdir = invocation_workdir(&invocation_root);
    let host_runner = HostCommandRunner::new(ctx);

    let mut cas_native_fallback: Option<CasNativeFallback> = None;
    if !request.sandbox {
        match prepare_cas_native_run_context(ctx, &snapshot, &invocation_root) {
            Ok(native_ctx) => {
                let outcome = super::test_exec::run_tests_for_context_cas_native(
                    ctx,
                    &host_runner,
                    &native_ctx,
                    request,
                    sync_report.clone(),
                    &workdir,
                )?;
                return Ok(outcome);
            }
            Err(outcome) => {
                let Some(reason) = cas_native_fallback_reason(&outcome) else {
                    return Ok(outcome);
                };
                if is_integrity_failure(&outcome) {
                    return Ok(outcome);
                }
                cas_native_fallback = Some(CasNativeFallback {
                    reason,
                    summary: cas_native_fallback_summary(&outcome),
                });
            }
        }
    }

    let py_ctx = match ephemeral_python_context(ctx, &snapshot, &runtime, &invocation_root) {
        Ok(py_ctx) => py_ctx,
        Err(outcome) => return Ok(outcome),
    };

    let mut sandbox: Option<SandboxRunContext> = None;
    if request.sandbox {
        let sbx = match prepare_project_sandbox(ctx, &snapshot) {
            Ok(sbx) => sbx,
            Err(outcome) => return Ok(outcome),
        };
        sandbox = Some(sbx);
    }

    let mut outcome = if let Some(ref mut sbx) = sandbox {
        let sandbox_runner = match sandbox_runner_for_context(&py_ctx, sbx, &workdir) {
            Ok(runner) => runner,
            Err(outcome) => return Ok(outcome),
        };
        run_tests_for_context(
            ctx,
            &sandbox_runner,
            &py_ctx,
            request,
            sync_report,
            &workdir,
        )?
    } else {
        run_tests_for_context(ctx, &host_runner, &py_ctx, request, sync_report, &workdir)?
    };

    if let Some(ref fallback) = cas_native_fallback {
        attach_cas_native_fallback(&mut outcome, fallback);
    }
    if let Some(ref sbx) = sandbox {
        attach_sandbox_details(&mut outcome, sbx);
    }
    Ok(outcome)
}

#[allow(clippy::too_many_arguments)]
fn run_ephemeral_materialized(
    ctx: &CommandContext,
    request: &RunRequest,
    target: &str,
    py_ctx: &PythonContext,
    runner: &dyn CommandRunner,
    snapshot: &ManifestSnapshot,
    workdir: &Path,
    interactive: bool,
    sandboxed: bool,
) -> Result<ExecutionOutcome> {
    let deps = DependencyContext::from_sources(&snapshot.requirements, Some(&snapshot.lock_path));
    let mut command_args = json!({
        "target": target,
        "args": &request.args,
    });
    deps.inject(&mut command_args);
    let plan = plan_run_target(py_ctx, &snapshot.manifest_path, target, workdir)?;
    match plan {
        RunTargetPlan::Script(path) => run_project_script(
            ctx,
            runner,
            py_ctx,
            &path,
            &request.args,
            &command_args,
            workdir,
            interactive,
            if sandboxed { "python" } else { &py_ctx.python },
        ),
        RunTargetPlan::Executable(program) => run_executable(
            ctx,
            runner,
            py_ctx,
            &program,
            &request.args,
            &command_args,
            workdir,
            interactive,
        ),
    }
}

fn detect_ephemeral_input(
    invocation_root: &Path,
    run_target: Option<&str>,
) -> Result<EphemeralInput, ExecutionOutcome> {
    if let Some(target) = run_target {
        if let Some(inline) = detect_inline_script(target)? {
            let mut deps = inline.dependencies().to_vec();
            deps.sort();
            deps.dedup();
            return Ok(EphemeralInput::InlineScript {
                requires_python: inline.requires_python().to_string(),
                deps,
            });
        }
    }

    let pyproject_path = invocation_root.join("pyproject.toml");
    if pyproject_path.exists() {
        let contents = fs::read_to_string(&pyproject_path).map_err(|err| {
            ExecutionOutcome::failure(
                "failed to read pyproject.toml for ephemeral run",
                json!({
                    "error": err.to_string(),
                    "pyproject": pyproject_path.display().to_string(),
                }),
            )
        })?;
        let snapshot = px_domain::api::ProjectSnapshot::from_contents(
            invocation_root,
            &pyproject_path,
            &contents,
        )
        .map_err(|err| {
            ExecutionOutcome::failure(
                "failed to parse pyproject.toml for ephemeral run",
                json!({
                    "error": err.to_string(),
                    "pyproject": pyproject_path.display().to_string(),
                }),
            )
        })?;
        return Ok(EphemeralInput::Pyproject {
            requires_python: snapshot.python_requirement,
            deps: snapshot.requirements,
        });
    }

    let requirements_path = invocation_root.join("requirements.txt");
    if requirements_path.exists() {
        let deps = read_requirements_for_ephemeral(&requirements_path)?;
        return Ok(EphemeralInput::Requirements { deps });
    }

    Ok(EphemeralInput::Empty)
}

fn enforce_pinned_inputs(
    command: &str,
    invocation_root: &Path,
    input: &EphemeralInput,
    frozen: bool,
) -> Result<(), ExecutionOutcome> {
    let empty: &[String] = &[];
    let (requires_python, deps) = match input {
        EphemeralInput::InlineScript {
            requires_python,
            deps,
        } => (Some(requires_python.as_str()), deps.as_slice()),
        EphemeralInput::Pyproject {
            requires_python,
            deps,
        } => (Some(requires_python.as_str()), deps.as_slice()),
        EphemeralInput::Requirements { deps } => (None, deps.as_slice()),
        EphemeralInput::Empty => (None, empty),
    };
    let mut unpinned = Vec::new();
    for spec in deps {
        if px_domain::api::spec_requires_pin(spec) {
            unpinned.push(spec.clone());
        }
    }
    if unpinned.is_empty() {
        return Ok(());
    }
    let why = if frozen {
        "ephemeral runs in --frozen mode require fully pinned dependencies"
    } else {
        "ephemeral runs in CI require fully pinned dependencies"
    };
    let mut details = json!({
        "reason": "ephemeral_unpinned_inputs",
        "command": command,
        "unpinned": unpinned,
        "hint": "Pin dependencies as `name==version` (or adopt with `px migrate --apply` to generate px.lock).",
        "cwd": invocation_root.display().to_string(),
    });
    if let Some(req) = requires_python {
        if let Some(map) = details.as_object_mut() {
            map.insert("requires_python".to_string(), json!(req));
        }
    }
    Err(ExecutionOutcome::user_error(why, details))
}

fn prepare_ephemeral_snapshot(
    ctx: &CommandContext,
    invocation_root: &Path,
    input: &EphemeralInput,
    frozen: bool,
) -> Result<
    (
        ManifestSnapshot,
        runtime_manager::RuntimeSelection,
        Option<EnvironmentSyncReport>,
    ),
    ExecutionOutcome,
> {
    let (requires_python, deps) = match input {
        EphemeralInput::InlineScript {
            requires_python,
            deps,
        } => (requires_python.clone(), deps.clone()),
        EphemeralInput::Pyproject {
            requires_python,
            deps,
        } => (requires_python.clone(), deps.clone()),
        EphemeralInput::Requirements { deps } => {
            (DEFAULT_EPHEMERAL_REQUIRES_PYTHON.to_string(), deps.clone())
        }
        EphemeralInput::Empty => (DEFAULT_EPHEMERAL_REQUIRES_PYTHON.to_string(), Vec::new()),
    };

    let manifest_doc = build_ephemeral_manifest_doc(&requires_python, &deps);
    let manifest_contents = manifest_doc.to_string();
    let temp_snapshot = px_domain::api::ProjectSnapshot::from_contents(
        invocation_root,
        invocation_root.join("pyproject.toml"),
        &manifest_contents,
    )
    .map_err(|err| {
        ExecutionOutcome::failure(
            "failed to assemble ephemeral project snapshot",
            json!({ "error": err.to_string() }),
        )
    })?;

    let runtime = prepare_project_runtime(&temp_snapshot).map_err(|err| {
        install_error_outcome(err, "python runtime unavailable for ephemeral run")
    })?;
    let tags = detect_interpreter_tags(&runtime.record.path).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to detect python interpreter tags for ephemeral run",
            json!({ "error": err.to_string() }),
        )
    })?;
    let platform = tags
        .platform
        .first()
        .cloned()
        .unwrap_or_else(|| "any".to_string());
    let indexes = resolver_indexes();
    let kind = match input {
        EphemeralInput::InlineScript { .. } => "pep723",
        EphemeralInput::Pyproject { .. } => "pyproject",
        EphemeralInput::Requirements { .. } => "requirements",
        EphemeralInput::Empty => "empty",
    };
    let key = ephemeral_cache_key(EphemeralKeyPayload {
        kind,
        requires_python: &requires_python,
        deps: &deps,
        runtime: &runtime.record.full_version,
        platform: &platform,
        indexes: &indexes,
        force_sdist: ctx.config().resolver.force_sdist,
    });
    let root = ctx.cache().path.join("ephemeral").join(&key);
    fs::create_dir_all(&root).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to create ephemeral cache directory",
            json!({
                "error": err.to_string(),
                "path": root.display().to_string(),
            }),
        )
    })?;
    let manifest_path = root.join("pyproject.toml");
    write_if_missing(&manifest_path, &manifest_contents).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to write ephemeral pyproject.toml",
            json!({
                "error": err.to_string(),
                "manifest": manifest_path.display().to_string(),
            }),
        )
    })?;
    let snapshot = px_domain::api::ProjectSnapshot::read_from(&root).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to load ephemeral project snapshot",
            json!({
                "error": err.to_string(),
                "root": root.display().to_string(),
            }),
        )
    })?;

    let guard = if frozen {
        EnvGuard::Strict
    } else {
        EnvGuard::AutoSync
    };
    let sync_report = ensure_environment_with_guard(ctx, &snapshot, guard).map_err(|err| {
        install_error_outcome(err, "failed to prepare ephemeral python environment")
    })?;
    Ok((snapshot, runtime, sync_report))
}

fn ephemeral_python_context(
    ctx: &CommandContext,
    snapshot: &ManifestSnapshot,
    runtime: &runtime_manager::RuntimeSelection,
    execution_root: &Path,
) -> Result<PythonContext, ExecutionOutcome> {
    let state = load_project_state(ctx.fs(), &snapshot.root).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to read ephemeral project state",
            json!({ "error": err.to_string() }),
        )
    })?;
    let env_state = state.current_env.as_ref().ok_or_else(|| {
        ExecutionOutcome::user_error(
            "ephemeral environment is missing",
            json!({
                "reason": "missing_env",
                "hint": "rerun without --frozen (or enable PX_ONLINE=1) to populate the cache",
            }),
        )
    })?;

    let env_root = env_state
        .env_path
        .as_ref()
        .map(|path| PathBuf::from(path.trim()))
        .filter(|path| !path.as_os_str().is_empty())
        .or_else(|| {
            let site = PathBuf::from(env_state.site_packages.trim());
            env_root_from_site_packages(&site)
        })
        .ok_or_else(|| {
            ExecutionOutcome::failure(
                "ephemeral environment missing env root",
                json!({ "reason": "missing_env_root" }),
            )
        })?;

    let paths = build_pythonpath(ctx.fs(), execution_root, Some(env_root)).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to assemble PYTHONPATH for ephemeral run",
            json!({ "error": err.to_string() }),
        )
    })?;
    let mut allowed_paths = paths.allowed_paths;
    if snapshot.root != execution_root && !allowed_paths.iter().any(|p| p == &snapshot.root) {
        allowed_paths.push(snapshot.root.clone());
    }
    let python = select_python_from_site(
        &paths.site_bin,
        &runtime.record.path,
        &runtime.record.full_version,
    );

    let profile_oid = env_state
        .profile_oid
        .clone()
        .or_else(|| Some(env_state.id.clone()));
    let pyc_cache_prefix = if env::var_os("PYTHONPYCACHEPREFIX").is_some() {
        None
    } else if let Some(oid) = profile_oid.as_deref() {
        match crate::store::ensure_pyc_cache_prefix(&ctx.cache().path, oid) {
            Ok(prefix) => Some(prefix),
            Err(err) => {
                let prefix = crate::store::pyc_cache_prefix(&ctx.cache().path, oid);
                return Err(ExecutionOutcome::user_error(
                    "python bytecode cache directory is not writable",
                    json!({
                        "reason": "pyc_cache_unwritable",
                        "cache_dir": prefix.display().to_string(),
                        "error": err.to_string(),
                        "hint": "ensure the directory is writable or set PX_CACHE_PATH to a writable location",
                    }),
                ));
            }
        }
    } else {
        None
    };

    Ok(PythonContext {
        project_root: execution_root.to_path_buf(),
        state_root: snapshot.root.clone(),
        project_name: snapshot.name.clone(),
        python,
        pythonpath: paths.pythonpath,
        allowed_paths,
        site_bin: paths.site_bin,
        pep582_bin: paths.pep582_bin,
        pyc_cache_prefix,
        px_options: snapshot.px_options.clone(),
    })
}

fn build_ephemeral_manifest_doc(requires_python: &str, dependencies: &[String]) -> DocumentMut {
    let mut doc = DocumentMut::new();
    let mut project = Table::new();
    project.insert("name", Item::Value(TomlValue::from(EPHEMERAL_PROJECT_NAME)));
    project.insert("version", Item::Value(TomlValue::from("0.0.0")));
    project.insert(
        "requires-python",
        Item::Value(TomlValue::from(requires_python)),
    );
    let mut deps = Array::new();
    for dep in dependencies {
        deps.push(dep.as_str());
    }
    project.insert("dependencies", Item::Value(TomlValue::Array(deps)));
    doc.insert("project", Item::Table(project));
    let mut tool = Table::new();
    tool.insert("px", Item::Table(Table::new()));
    doc.insert("tool", Item::Table(tool));
    doc
}

fn read_requirements_for_ephemeral(path: &Path) -> Result<Vec<String>, ExecutionOutcome> {
    let mut visited = std::collections::HashSet::new();
    let mut deps = Vec::new();
    collect_requirements_recursive(path, &mut visited, &mut deps)?;
    deps.sort();
    deps.dedup();
    Ok(deps)
}

fn collect_requirements_recursive(
    path: &Path,
    visited: &mut std::collections::HashSet<PathBuf>,
    deps: &mut Vec<String>,
) -> Result<(), ExecutionOutcome> {
    let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(canonical.clone()) {
        return Ok(());
    }

    let contents = fs::read_to_string(&canonical).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to read requirements file for ephemeral run",
            json!({
                "error": err.to_string(),
                "requirements": canonical.display().to_string(),
            }),
        )
    })?;
    let base_dir = canonical.parent().unwrap_or_else(|| Path::new("."));

    let mut pending = String::new();
    let mut pending_start_line: usize = 0;

    for (idx, raw_line) in contents.lines().enumerate() {
        let mut line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if let Some(hash_idx) = line.find('#') {
            if line[..hash_idx]
                .chars()
                .last()
                .is_some_and(|ch| ch.is_whitespace())
            {
                line = line[..hash_idx].trim_end();
            }
        }

        let continues = line.ends_with('\\');
        if continues {
            line = line
                .strip_suffix('\\')
                .unwrap_or(line)
                .trim_end_matches([' ', '\t']);
        }

        if pending.is_empty() {
            pending_start_line = idx + 1;
        } else {
            pending.push(' ');
        }
        pending.push_str(line);

        if continues {
            continue;
        }

        if !pending.trim().is_empty() {
            parse_ephemeral_requirement_line(
                pending.trim(),
                pending_start_line,
                &canonical,
                base_dir,
                visited,
                deps,
            )?;
        }
        pending.clear();
        pending_start_line = 0;
    }

    if !pending.trim().is_empty() {
        parse_ephemeral_requirement_line(
            pending.trim(),
            pending_start_line,
            &canonical,
            base_dir,
            visited,
            deps,
        )?;
    }

    Ok(())
}

fn strip_pip_hash_tokens(line: &str) -> String {
    let mut out = String::new();
    let mut tokens = line.split_whitespace().peekable();

    while let Some(token) = tokens.next() {
        let lower = token.to_ascii_lowercase();
        if lower == "--hash" {
            let _ = tokens.next();
            continue;
        }
        if lower.starts_with("--hash=") {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(token);
    }

    out
}

fn looks_like_windows_abs_path(line: &str) -> bool {
    let bytes = line.as_bytes();
    if bytes.len() < 3 {
        return false;
    }
    bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && matches!(bytes[2], b'/' | b'\\')
}

fn parse_ephemeral_requirement_line(
    line: &str,
    line_no: usize,
    requirements: &Path,
    base_dir: &Path,
    visited: &mut std::collections::HashSet<PathBuf>,
    deps: &mut Vec<String>,
) -> Result<(), ExecutionOutcome> {
    let stripped = strip_pip_hash_tokens(line);
    let line = stripped.trim();
    if line.is_empty() {
        return Ok(());
    }

    if let Some(rest) = line
        .strip_prefix("-r")
        .or_else(|| line.strip_prefix("--requirement"))
    {
        let target = rest.trim_start_matches([' ', '=']).trim();
        if target.is_empty() {
            return Ok(());
        }
        let include = if Path::new(target).is_absolute() {
            PathBuf::from(target)
        } else {
            base_dir.join(target)
        };
        collect_requirements_recursive(&include, visited, deps)?;
        return Ok(());
    }

    if line.starts_with("-e") || line.starts_with("--editable") {
        return Err(ExecutionOutcome::user_error(
            "ephemeral requirements.txt does not support editable installs",
            json!({
                "reason": "ephemeral_requirements_editable_unsupported",
                "requirements": requirements.display().to_string(),
                "line": line_no,
                "hint": "Use pinned, non-editable requirements (no -e) or adopt the project with `px migrate --apply`.",
            }),
        ));
    }

    if line.starts_with('-') {
        return Err(ExecutionOutcome::user_error(
            "ephemeral requirements.txt does not support pip options",
            json!({
                "reason": "ephemeral_requirements_option_unsupported",
                "requirements": requirements.display().to_string(),
                "line": line_no,
                "hint": "Move pip options (like --index-url/--find-links/--constraint) to your environment or adopt the project with `px migrate --apply`.",
            }),
        ));
    }

    if line == "."
        || line.starts_with("./")
        || line.starts_with("../")
        || line.starts_with('/')
        || line.starts_with("\\\\")
        || looks_like_windows_abs_path(line)
        || line.to_ascii_lowercase().starts_with("file:")
    {
        return Err(ExecutionOutcome::user_error(
            "ephemeral requirements.txt does not support local path dependencies",
            json!({
                "reason": "ephemeral_requirements_local_path_unsupported",
                "requirements": requirements.display().to_string(),
                "line": line_no,
                "hint": "Use published, pinned requirements, or adopt the project with `px migrate --apply`.",
            }),
        ));
    }

    deps.push(line.to_string());
    Ok(())
}

fn write_if_missing(path: &Path, contents: &str) -> std::io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    fs::write(path, contents)?;
    Ok(())
}

fn ephemeral_cache_key(payload: EphemeralKeyPayload<'_>) -> String {
    let bytes = serde_json::to_vec(&payload).unwrap_or_default();
    hex::encode(Sha256::digest(&bytes))
}

fn resolver_indexes() -> Vec<String> {
    let mut indexes = Vec::new();
    if let Ok(primary) = env::var("PX_INDEX_URL")
        .or_else(|_| env::var("PIP_INDEX_URL"))
        .map(|value| value.trim().to_string())
    {
        if !primary.is_empty() {
            indexes.push(normalize_index_url(&primary));
        }
    }
    if let Ok(extra) = env::var("PIP_EXTRA_INDEX_URL") {
        for entry in extra.split_whitespace() {
            let trimmed = entry.trim();
            if !trimmed.is_empty() {
                indexes.push(normalize_index_url(trimmed));
            }
        }
    }
    if indexes.is_empty() {
        indexes.push("https://pypi.org/simple".to_string());
    }
    indexes
}

fn normalize_index_url(raw: &str) -> String {
    let mut url = raw.trim_end_matches('/').to_string();
    if url.ends_with("/simple") {
        return url;
    }
    if let Some(stripped) = url.strip_suffix("/pypi") {
        url = stripped.to_string();
    } else if let Some(stripped) = url.strip_suffix("/json") {
        url = stripped.to_string();
    }
    url.push_str("/simple");
    url
}
