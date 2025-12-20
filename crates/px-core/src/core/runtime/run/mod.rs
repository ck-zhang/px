use std::collections::{BTreeMap, HashSet};
use std::env;
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

#[cfg(windows)]
use std::io::Read;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tempfile::TempDir;
use tracing::debug;

use super::cas_env::{
    ensure_profile_env, ensure_profile_manifest, materialize_pkg_archive, project_env_owner_id,
    workspace_env_owner_id,
};
use super::run_plan::{plan_run_target, RunTargetPlan};
use super::script::{detect_inline_script, run_inline_script};
use crate::core::runtime::artifacts::dependency_name;
use crate::core::runtime::facade::{
    build_pythonpath, compute_lock_hash_bytes, detect_runtime_metadata, marker_env_for_snapshot,
    prepare_project_runtime, select_python_from_site, ManifestSnapshot,
};
use crate::core::sandbox::{discover_site_packages, run_pxapp_bundle};
use crate::project::evaluate_project_state;
use crate::tooling::{missing_pyproject_outcome, run_target_required_outcome};
use crate::workspace::{
    discover_workspace_scope, prepare_workspace_run_context, WorkspaceScope, WorkspaceStateKind,
};
use crate::{
    attach_autosync_details, is_missing_project_error, manifest_snapshot, missing_project_outcome,
    outcome_from_output, python_context_with_mode, state_guard::guard_for_execution,
    CommandContext, ExecutionOutcome, OwnerId, OwnerType, PythonContext,
};
use px_domain::api::{detect_lock_drift, load_lockfile_optional, verify_locked_artifacts};

mod cas_native;
mod commit;
mod driver;
mod ephemeral;
mod errors;
mod ref_tree;
mod reference;
mod request;
mod runners;
mod sandbox;
mod test_exec;
mod workdir;

use cas_native::{
    is_python_alias_target, load_or_build_console_script_index, prepare_cas_native_run_context,
    prepare_cas_native_workspace_run_context, CasNativeFallback, CasNativeFallbackReason,
    CasNativeRunContext, ConsoleScriptCandidate, ProcessPlan, CONSOLE_SCRIPT_DISPATCH,
};

use commit::{run_project_at_ref, run_tests_at_ref};
use test_exec::{run_tests_for_context, test_project_outcome};

use reference::run_reference_target;
pub(crate) use reference::{parse_run_reference_target, RunReferenceTarget};
pub(crate) use runners::{CommandRunner, HostCommandRunner};
use sandbox::{
    attach_sandbox_details, prepare_commit_sandbox, prepare_project_sandbox,
    prepare_workspace_sandbox, sandbox_workspace_env_inconsistent,
};
pub(crate) use sandbox::{sandbox_runner_for_context, SandboxRunContext};

pub use driver::{run_project, test_project};
pub(crate) use ref_tree::{git_repo_root, materialize_ref_tree, validate_lock_for_ref};
pub use request::{RunRequest, TestRequest};
use workdir::{invocation_workdir, map_workdir};

type EnvPairs = Vec<(String, String)>;

pub(crate) use errors::install_error_outcome;
use errors::{
    attach_cas_native_fallback, cas_native_fallback_reason, cas_native_fallback_summary,
    error_details_with_code, is_integrity_failure,
};
use ref_tree::{commit_stdlib_guard, manifest_has_px};

#[derive(Clone, Default)]
struct DependencyContext {
    manifest: HashSet<String>,
    locked: HashSet<String>,
}

impl DependencyContext {
    fn from_sources(manifest_specs: &[String], lock_path: Option<&Path>) -> Self {
        let mut manifest = HashSet::new();
        for spec in manifest_specs {
            let name = dependency_name(spec);
            if !name.is_empty() {
                manifest.insert(name);
            }
        }

        let mut locked = HashSet::new();
        if let Some(path) = lock_path {
            if let Ok(Some(lock)) = load_lockfile_optional(path) {
                for spec in lock.dependencies {
                    let name = dependency_name(&spec);
                    if !name.is_empty() {
                        locked.insert(name);
                    }
                }
                for dep in lock.resolved {
                    let name = dep.name.trim();
                    if !name.is_empty() {
                        locked.insert(name.to_lowercase());
                    }
                }
            }
        }

        Self { manifest, locked }
    }

    fn inject(&self, args: &mut Value) {
        if let Value::Object(map) = args {
            if !self.manifest.is_empty() {
                map.insert(
                    "manifest_deps".into(),
                    serde_json::to_value(sorted_list(&self.manifest)).unwrap_or(Value::Null),
                );
            }
            if !self.locked.is_empty() {
                map.insert(
                    "locked_deps".into(),
                    serde_json::to_value(sorted_list(&self.locked)).unwrap_or(Value::Null),
                );
            }
        }
    }
}

fn sorted_list(values: &HashSet<String>) -> Vec<String> {
    let mut items: Vec<String> = values.iter().cloned().collect();
    items.sort();
    items
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_project_script(
    core_ctx: &CommandContext,
    runner: &dyn CommandRunner,
    py_ctx: &PythonContext,
    script: &Path,
    extra_args: &[String],
    command_args: &Value,
    workdir: &Path,
    interactive: bool,
    program: &str,
) -> Result<ExecutionOutcome> {
    let (envs, _) = build_env_with_preflight(core_ctx, py_ctx, command_args)?;
    let mut args = Vec::with_capacity(extra_args.len() + 1);
    args.push(script.display().to_string());
    args.extend(extra_args.iter().cloned());
    let output = if interactive {
        match runner.run_command_passthrough(program, &args, &envs, workdir) {
            Ok(output) => output,
            Err(err) => return Ok(command_start_error_outcome("run", program, &args, err)),
        }
    } else {
        match runner.run_command(program, &args, &envs, workdir) {
            Ok(output) => output,
            Err(err) => return Ok(command_start_error_outcome("run", program, &args, err)),
        }
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

fn json_env_value(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn set_env_pair(envs: &mut EnvPairs, key: &str, value: String) {
    if let Some((_, existing)) = envs.iter_mut().find(|(k, _)| k == key) {
        *existing = value;
    } else {
        envs.push((key.to_string(), value));
    }
}

fn command_start_error_outcome(
    command: &str,
    program: &str,
    args: &[String],
    err: anyhow::Error,
) -> ExecutionOutcome {
    let kind = err
        .chain()
        .find_map(|cause| cause.downcast_ref::<std::io::Error>().map(|io| io.kind()));
    let (reason, hint) = match kind {
        Some(std::io::ErrorKind::NotFound) => (
            "command_not_found",
            "Check the command name; if it comes from a dependency, add that dependency and run `px sync`.",
        ),
        Some(std::io::ErrorKind::PermissionDenied) => (
            "command_not_executable",
            "Check executable permissions and try again.",
        ),
        _ => (
            "command_failed_to_start",
            "Re-run with `--debug` to see more detail, or verify the command can be executed on this machine.",
        ),
    };
    ExecutionOutcome::user_error(
        format!("failed to start {program}"),
        json!({
            "code": crate::diag_commands::RUN,
            "reason": reason,
            "command": command,
            "program": program,
            "args": args,
            "error": err.to_string(),
            "hint": hint,
        }),
    )
}

fn apply_profile_env_vars(envs: &mut EnvPairs, vars: &BTreeMap<String, Value>) {
    for (key, value) in vars {
        set_env_pair(envs, key, json_env_value(value));
    }
}

fn apply_runtime_python_home(envs: &mut EnvPairs, runtime: &Path) {
    let Some(runtime_root) = crate::core::fs::python_install_root(runtime) else {
        return;
    };
    set_env_pair(envs, "PYTHONHOME", runtime_root.display().to_string());
}

fn python_args_disable_site(args: &[String]) -> bool {
    let mut idx = 0usize;
    while idx < args.len() {
        let arg = &args[idx];
        if arg == "--" {
            break;
        }
        if arg == "-S" {
            return true;
        }
        if arg == "-c" || arg == "-m" {
            break;
        }
        if arg == "-W" || arg == "-X" {
            idx = idx.saturating_add(2);
            continue;
        }
        if arg == "-" || !arg.starts_with('-') {
            break;
        }
        idx = idx.saturating_add(1);
    }
    false
}

fn apply_cas_native_sys_path_for_no_site(envs: &mut EnvPairs, sys_path_entries: &[PathBuf]) {
    if sys_path_entries.is_empty() {
        return;
    }

    let existing = envs
        .iter()
        .find(|(key, _)| key == "PYTHONPATH")
        .map(|(_, value)| value.clone())
        .unwrap_or_default();
    let mut merged = Vec::new();
    let mut seen = HashSet::new();
    for entry in env::split_paths(&existing) {
        if seen.insert(entry.clone()) {
            merged.push(entry);
        }
    }
    for entry in sys_path_entries {
        if seen.insert(entry.clone()) {
            merged.push(entry.clone());
        }
    }
    let Ok(joined) = env::join_paths(merged.iter().map(|path| path.as_os_str())) else {
        return;
    };
    let Ok(pythonpath) = joined.into_string() else {
        return;
    };
    set_env_pair(envs, "PYTHONPATH", pythonpath);
}

fn program_on_path(program: &str, envs: &EnvPairs) -> bool {
    let value = envs
        .iter()
        .find(|(key, _)| key == "PATH")
        .map(|(_, value)| value.clone())
        .or_else(|| env::var("PATH").ok())
        .unwrap_or_default();
    for entry in env::split_paths(&value) {
        if entry.join(program).is_file() {
            return true;
        }
    }
    false
}

#[cfg(windows)]
fn resolve_program_on_path(program: &str, envs: &EnvPairs) -> Option<PathBuf> {
    let value = envs
        .iter()
        .find(|(key, _)| key == "PATH")
        .map(|(_, value)| value.clone())
        .or_else(|| env::var("PATH").ok())
        .unwrap_or_default();
    for entry in env::split_paths(&value) {
        let candidate = entry.join(program);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn execute_plan(
    runner: &dyn CommandRunner,
    plan: &ProcessPlan,
    interactive: bool,
    inherit_stdin: bool,
) -> Result<crate::RunOutput> {
    let Some((program, args)) = plan.argv.split_first() else {
        bail!("execution plan missing argv[0]");
    };
    if inherit_stdin {
        runner.run_command_with_stdin(program, args, &plan.envs, &plan.cwd, true)
    } else if interactive {
        runner.run_command_passthrough(program, args, &plan.envs, &plan.cwd)
    } else {
        runner.run_command(program, args, &plan.envs, &plan.cwd)
    }
}

#[allow(clippy::too_many_arguments)]
fn run_project_script_cas_native(
    core_ctx: &CommandContext,
    runner: &dyn CommandRunner,
    native: &CasNativeRunContext,
    script: &Path,
    extra_args: &[String],
    command_args: &Value,
    workdir: &Path,
    interactive: bool,
) -> Result<ExecutionOutcome> {
    let (mut envs, _) = build_env_with_preflight(core_ctx, &native.py_ctx, command_args)?;
    apply_runtime_python_home(&mut envs, &native.runtime_path);
    apply_profile_env_vars(&mut envs, &native.env_vars);
    let mut argv = Vec::with_capacity(extra_args.len() + 2);
    argv.push(native.py_ctx.python.clone());
    argv.push(script.display().to_string());
    argv.extend(extra_args.iter().cloned());
    let plan = ProcessPlan {
        runtime_path: native.runtime_path.clone(),
        sys_path_entries: native.sys_path_entries.clone(),
        cwd: workdir.to_path_buf(),
        envs,
        argv,
    };
    let output = execute_plan(runner, &plan, interactive, false)?;
    let details = json!({
        "mode": "script",
        "script": script.display().to_string(),
        "args": extra_args,
        "interactive": interactive,
        "execution": {
            "runtime": plan.runtime_path.display().to_string(),
            "profile_oid": native.profile_oid.clone(),
            "sys_path": plan
                .sys_path_entries
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>(),
        }
    });
    Ok(outcome_from_output(
        "run",
        &script.display().to_string(),
        &output,
        "px run",
        Some(details),
    ))
}

fn resolve_console_script_candidate(
    ctx: &CommandContext,
    native: &CasNativeRunContext,
    script: &str,
) -> Result<Option<ConsoleScriptCandidate>, ExecutionOutcome> {
    let index = load_or_build_console_script_index(&ctx.cache().path, native).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to index console scripts for native execution",
            json!({
                "reason": "cas_native_console_script_index_failed",
                "error": err.to_string(),
                "profile_oid": native.profile_oid.clone(),
            }),
        )
    })?;
    let Some(candidates) = index.scripts.get(script) else {
        return Ok(None);
    };
    if candidates.len() == 1 {
        return Ok(Some(
            candidates.first().expect("candidates non-empty").clone(),
        ));
    }
    let rendered = candidates
        .iter()
        .map(|candidate| {
            json!({
                "distribution": &candidate.dist,
                "version": candidate.dist_version.as_deref(),
                "entry_point": &candidate.entry_point,
            })
        })
        .collect::<Vec<_>>();
    Err(ExecutionOutcome::user_error(
        format!("console script `{script}` is provided by multiple distributions"),
        json!({
            "reason": "ambiguous_console_script",
            "script": script,
            "candidates": rendered,
            "hint": "Remove one of the distributions providing this script, or run a module directly via `px run python -m <module>`.",
        }),
    ))
}

#[allow(clippy::too_many_arguments)]
fn run_console_script_cas_native(
    core_ctx: &CommandContext,
    runner: &dyn CommandRunner,
    native: &CasNativeRunContext,
    script: &str,
    candidate: &ConsoleScriptCandidate,
    extra_args: &[String],
    command_args: &Value,
    workdir: &Path,
    interactive: bool,
) -> Result<ExecutionOutcome> {
    let (mut envs, _) = build_env_with_preflight(core_ctx, &native.py_ctx, command_args)?;
    apply_runtime_python_home(&mut envs, &native.runtime_path);
    apply_profile_env_vars(&mut envs, &native.env_vars);
    let mut argv = Vec::with_capacity(extra_args.len() + 5);
    argv.push(native.py_ctx.python.clone());
    argv.push("-c".to_string());
    argv.push(CONSOLE_SCRIPT_DISPATCH.to_string());
    argv.push(script.to_string());
    argv.push(candidate.dist.clone());
    argv.extend(extra_args.iter().cloned());
    let plan = ProcessPlan {
        runtime_path: native.runtime_path.clone(),
        sys_path_entries: native.sys_path_entries.clone(),
        cwd: workdir.to_path_buf(),
        envs,
        argv,
    };
    let output = execute_plan(runner, &plan, interactive, false)?;
    let details = json!({
        "mode": "console_script",
        "script": script,
        "distribution": &candidate.dist,
        "entry_point": &candidate.entry_point,
        "args": extra_args,
        "interactive": interactive,
        "execution": {
            "runtime": plan.runtime_path.display().to_string(),
            "profile_oid": native.profile_oid.clone(),
            "sys_path": plan
                .sys_path_entries
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>(),
        }
    });
    Ok(outcome_from_output(
        "run",
        script,
        &output,
        "px run",
        Some(details),
    ))
}

#[allow(clippy::too_many_arguments)]
fn run_executable(
    core_ctx: &CommandContext,
    runner: &dyn CommandRunner,
    py_ctx: &PythonContext,
    program: &str,
    extra_args: &[String],
    command_args: &Value,
    workdir: &Path,
    interactive: bool,
) -> Result<ExecutionOutcome> {
    if let Some(subcommand) = mutating_pip_invocation(program, extra_args, py_ctx) {
        return Ok(pip_mutation_outcome(program, &subcommand, extra_args));
    }
    let (mut envs, _) = build_env_with_preflight(core_ctx, py_ctx, command_args)?;

    #[cfg(windows)]
    let mut exec_program = program.to_string();
    #[cfg(not(windows))]
    let exec_program = program.to_string();

    #[cfg(windows)]
    let mut exec_args: Vec<String> = extra_args.to_vec();
    #[cfg(not(windows))]
    let exec_args: Vec<String> = extra_args.to_vec();

    #[cfg(windows)]
    {
        let path = Path::new(program);
        if path.is_file()
            && path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| {
                    ext.eq_ignore_ascii_case("cmd") || ext.eq_ignore_ascii_case("bat")
                })
        {
            exec_program = "cmd".to_string();
            let mut argv = Vec::with_capacity(extra_args.len() + 2);
            argv.push("/C".to_string());
            argv.push(program.to_string());
            argv.extend(extra_args.iter().cloned());
            exec_args = argv;
        } else {
            let should_run_via_python = path.is_file()
                && (path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| {
                        ext.eq_ignore_ascii_case("py") || ext.eq_ignore_ascii_case("pyw")
                    })
                    || {
                        let mut file = match fs::File::open(path) {
                            Ok(file) => file,
                            Err(_) => {
                                return Ok(ExecutionOutcome::failure(
                                    "failed to execute program",
                                    json!({ "error": format!("failed to open {}", path.display()) }),
                                ))
                            }
                        };
                        let mut buf = [0u8; 256];
                        let read = file.read(&mut buf).unwrap_or(0);
                        let prefix = std::str::from_utf8(&buf[..read]).unwrap_or_default();
                        prefix.lines().next().is_some_and(|line| {
                            line.starts_with("#!") && line.to_ascii_lowercase().contains("python")
                        })
                    });
            if should_run_via_python {
                exec_program = py_ctx.python.clone();
                let mut argv = Vec::with_capacity(extra_args.len() + 1);
                argv.push(program.to_string());
                argv.extend(extra_args.iter().cloned());
                exec_args = argv;
            }
        }
    }

    let uses_px_python = program_matches_python(&exec_program, py_ctx);
    let needs_stdin = uses_px_python && exec_args.first().map(|arg| arg == "-").unwrap_or(false);
    if !uses_px_python {
        envs.retain(|(key, _)| key != "PX_PYTHON");
    }
    let interactive = interactive || needs_stdin;
    let output = if needs_stdin {
        match runner.run_command_with_stdin(&exec_program, &exec_args, &envs, workdir, true) {
            Ok(output) => output,
            Err(err) => {
                return Ok(command_start_error_outcome(
                    "run",
                    &exec_program,
                    &exec_args,
                    err,
                ))
            }
        }
    } else if interactive {
        match runner.run_command_passthrough(&exec_program, &exec_args, &envs, workdir) {
            Ok(output) => output,
            Err(err) => {
                return Ok(command_start_error_outcome(
                    "run",
                    &exec_program,
                    &exec_args,
                    err,
                ))
            }
        }
    } else {
        match runner.run_command(&exec_program, &exec_args, &envs, workdir) {
            Ok(output) => output,
            Err(err) => {
                return Ok(command_start_error_outcome(
                    "run",
                    &exec_program,
                    &exec_args,
                    err,
                ))
            }
        }
    };
    let mut details = json!({
        "mode": if uses_px_python { "passthrough" } else { "executable" },
        "program": program,
        "args": extra_args,
        "interactive": interactive,
    });
    if uses_px_python {
        details["uses_px_python"] = Value::Bool(true);
    }
    Ok(outcome_from_output(
        "run",
        program,
        &output,
        "px run",
        Some(details),
    ))
}

#[allow(clippy::too_many_arguments)]
fn run_executable_cas_native(
    core_ctx: &CommandContext,
    runner: &dyn CommandRunner,
    native: &CasNativeRunContext,
    program: &str,
    extra_args: &[String],
    command_args: &Value,
    workdir: &Path,
    interactive: bool,
) -> Result<ExecutionOutcome> {
    let display_program = program.to_string();
    let is_python_alias = is_python_alias_target(program);
    let initial_program = if is_python_alias {
        native.py_ctx.python.clone()
    } else {
        display_program.clone()
    };

    if let Some(subcommand) = mutating_pip_invocation(&initial_program, extra_args, &native.py_ctx)
    {
        return Ok(pip_mutation_outcome(
            &display_program,
            &subcommand,
            extra_args,
        ));
    }

    let mut missing_console_script = false;
    if !is_python_alias
        && Path::new(&display_program).components().count() == 1
        && !program_matches_python(&display_program, &native.py_ctx)
    {
        match resolve_console_script_candidate(core_ctx, native, &display_program) {
            Ok(Some(candidate)) => {
                return run_console_script_cas_native(
                    core_ctx,
                    runner,
                    native,
                    &display_program,
                    &candidate,
                    extra_args,
                    command_args,
                    workdir,
                    interactive,
                );
            }
            Ok(None) => {
                missing_console_script = true;
            }
            Err(outcome) => return Ok(outcome),
        }
    }

    let (mut envs, _) = build_env_with_preflight(core_ctx, &native.py_ctx, command_args)?;
    if missing_console_script && !program_on_path(&display_program, &envs) {
        return Ok(ExecutionOutcome::user_error(
            "native execution could not resolve command",
            json!({
                "reason": "cas_native_unresolved_console_script",
                "program": display_program,
            }),
        ));
    }

    #[cfg(windows)]
    let mut exec_program = initial_program;
    #[cfg(not(windows))]
    let exec_program = initial_program;

    #[cfg(windows)]
    let mut exec_args: Vec<String> = extra_args.to_vec();
    #[cfg(not(windows))]
    let exec_args: Vec<String> = extra_args.to_vec();

    #[cfg(windows)]
    {
        if !is_python_alias && !program_matches_python(&display_program, &native.py_ctx) {
            let program_path = Path::new(&display_program);
            let resolved = if program_path.is_file() {
                Some(program_path.to_path_buf())
            } else if program_path.components().count() == 1 {
                resolve_program_on_path(&display_program, &envs)
            } else {
                None
            };
            if let Some(path) = resolved {
                if path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| {
                        ext.eq_ignore_ascii_case("cmd") || ext.eq_ignore_ascii_case("bat")
                    })
                {
                    exec_program = "cmd".to_string();
                    let mut argv = Vec::with_capacity(extra_args.len() + 2);
                    argv.push("/C".to_string());
                    argv.push(path.display().to_string());
                    argv.extend(extra_args.iter().cloned());
                    exec_args = argv;
                } else {
                    let should_run_via_python = path
                        .extension()
                        .and_then(|ext| ext.to_str())
                        .is_some_and(|ext| {
                            ext.eq_ignore_ascii_case("py") || ext.eq_ignore_ascii_case("pyw")
                        })
                        || {
                            let mut file = match fs::File::open(&path) {
                                Ok(file) => file,
                                Err(_) => {
                                    return Ok(ExecutionOutcome::failure(
                                        "failed to execute program",
                                        json!({ "error": format!("failed to open {}", path.display()) }),
                                    ))
                                }
                            };
                            let mut buf = [0u8; 256];
                            let read = file.read(&mut buf).unwrap_or(0);
                            let prefix = std::str::from_utf8(&buf[..read]).unwrap_or_default();
                            prefix.lines().next().is_some_and(|line| {
                                line.starts_with("#!")
                                    && line.to_ascii_lowercase().contains("python")
                            })
                        };
                    if should_run_via_python {
                        exec_program = native.py_ctx.python.clone();
                        let mut argv = Vec::with_capacity(extra_args.len() + 1);
                        argv.push(path.display().to_string());
                        argv.extend(extra_args.iter().cloned());
                        exec_args = argv;
                    }
                }
            }
        }
    }

    let uses_px_python = program_matches_python(&exec_program, &native.py_ctx);
    let needs_stdin = uses_px_python && exec_args.first().is_some_and(|arg| arg == "-");
    if uses_px_python {
        apply_runtime_python_home(&mut envs, &native.runtime_path);
        apply_profile_env_vars(&mut envs, &native.env_vars);
        if python_args_disable_site(&exec_args) {
            apply_cas_native_sys_path_for_no_site(&mut envs, &native.sys_path_entries);
        }
    } else {
        envs.retain(|(key, _)| key != "PX_PYTHON");
    }
    let interactive = interactive || needs_stdin;
    let mut argv = Vec::with_capacity(exec_args.len() + 1);
    argv.push(exec_program.clone());
    argv.extend(exec_args.iter().cloned());
    let plan = ProcessPlan {
        runtime_path: native.runtime_path.clone(),
        sys_path_entries: native.sys_path_entries.clone(),
        cwd: workdir.to_path_buf(),
        envs,
        argv,
    };
    let output = execute_plan(runner, &plan, interactive, needs_stdin)?;
    let mut details = json!({
        "mode": if uses_px_python { "passthrough" } else { "executable" },
        "program": &display_program,
        "args": extra_args,
        "interactive": interactive,
        "execution": {
            "runtime": plan.runtime_path.display().to_string(),
            "profile_oid": native.profile_oid.clone(),
            "sys_path": plan
                .sys_path_entries
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>(),
        }
    });
    if uses_px_python {
        details["uses_px_python"] = Value::Bool(true);
    }
    Ok(outcome_from_output(
        "run",
        &display_program,
        &output,
        "px run",
        Some(details),
    ))
}

fn program_matches_python(program: &str, py_ctx: &PythonContext) -> bool {
    let program_path = Path::new(program);
    let python_path = Path::new(&py_ctx.python);
    program_path == python_path
        || program_path
            .file_name()
            .and_then(|p| python_path.file_name().filter(|q| q == &p))
            .is_some()
}

fn mutating_pip_invocation(
    program: &str,
    args: &[String],
    py_ctx: &PythonContext,
) -> Option<String> {
    let pip_args = pip_args_for_invocation(program, args, py_ctx)?;
    let subcommand = pip_subcommand(pip_args)?;
    if is_mutating_pip_subcommand(&subcommand) {
        Some(subcommand)
    } else {
        None
    }
}

fn pip_args_for_invocation<'a>(
    program: &'a str,
    args: &'a [String],
    py_ctx: &PythonContext,
) -> Option<&'a [String]> {
    if is_pip_program(Path::new(program)) {
        return Some(args);
    }
    if program_matches_python(program, py_ctx)
        && args.len() >= 2
        && args[0] == "-m"
        && is_pip_module(&args[1])
    {
        return Some(&args[2..]);
    }
    None
}

fn is_pip_program(program: &Path) -> bool {
    program
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| {
            let lower = name.to_ascii_lowercase();
            lower
                .strip_prefix("pip")
                .map(|rest| {
                    rest.is_empty() || rest.chars().all(|ch| ch.is_ascii_digit() || ch == '.')
                })
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

fn is_pip_module(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower == "pip.__main__" {
        return true;
    }
    lower
        .strip_prefix("pip")
        .map(|rest| rest.is_empty() || rest.chars().all(|ch| ch.is_ascii_digit() || ch == '.'))
        .unwrap_or(false)
}

fn pip_subcommand(args: &[String]) -> Option<String> {
    const KNOWN_SUBCOMMANDS: &[&str] = &[
        "install",
        "uninstall",
        "download",
        "freeze",
        "list",
        "show",
        "check",
        "config",
        "search",
        "wheel",
        "hash",
        "completion",
        "debug",
        "help",
        "cache",
        "index",
        "inspect",
    ];
    for arg in args {
        if arg == "--" {
            break;
        }
        if arg.starts_with('-') {
            continue;
        }
        let lower = arg.to_ascii_lowercase();
        if KNOWN_SUBCOMMANDS.contains(&lower.as_str()) {
            return Some(lower);
        }
    }
    None
}

fn is_mutating_pip_subcommand(subcommand: &str) -> bool {
    matches!(subcommand, "install" | "uninstall")
}

fn pip_mutation_outcome(program: &str, subcommand: &str, args: &[String]) -> ExecutionOutcome {
    ExecutionOutcome::user_error(
        "pip cannot modify px-managed environments",
        json!({
            "code": crate::diag_commands::RUN,
            "reason": "pip_mutation_forbidden",
            "program": program,
            "subcommand": subcommand,
            "args": args,
            "hint": "px envs are immutable CAS materializations; use `px add/remove/update/sync` to change dependencies."
        }),
    )
}

fn build_env_with_preflight(
    ctx: &CommandContext,
    py_ctx: &PythonContext,
    command_args: &Value,
) -> Result<(EnvPairs, Option<bool>)> {
    let mut envs = py_ctx.base_env(command_args)?;
    if std::env::var("PX_PYTEST_PERF_BASELINE").is_err() {
        if let Some(baseline) = pytest_perf_baseline(py_ctx) {
            envs.push(("PX_PYTEST_PERF_BASELINE".into(), baseline));
        }
    }
    let preflight = preflight_plugins(ctx, py_ctx, &envs)?;
    if let Some(ok) = preflight {
        envs.push((
            "PX_PLUGIN_PREFLIGHT".into(),
            if ok { "1".into() } else { "0".into() },
        ));
    }
    Ok((envs, preflight))
}

fn pytest_perf_baseline(py_ctx: &PythonContext) -> Option<String> {
    let canonical_root = py_ctx.project_root.canonicalize().ok()?;
    Some(format!(
        "{}{{extras}}@{}",
        py_ctx.project_name,
        canonical_root.to_string_lossy()
    ))
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
mod tests;
