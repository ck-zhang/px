use std::collections::{BTreeMap, HashSet};
use std::env;
use std::fs;
use std::io::{Cursor, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tar::Archive;
use tempfile::TempDir;
use tracing::{debug, warn};

use super::cas_env::{
    ensure_profile_env, ensure_profile_manifest, materialize_pkg_archive, project_env_owner_id,
    workspace_env_owner_id,
};
use super::script::{detect_inline_script, run_inline_script};
use crate::core::runtime::artifacts::dependency_name;
use crate::core::runtime::facade::{
    build_pythonpath, compute_lock_hash_bytes, detect_runtime_metadata, marker_env_for_snapshot,
    prepare_project_runtime, select_python_from_site, ManifestSnapshot,
};
use crate::core::sandbox::{discover_site_packages, run_pxapp_bundle};
use crate::project::evaluate_project_state;
use crate::run_plan::{plan_run_target, RunTargetPlan};
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
pub(crate) use sandbox::{sandbox_runner_for_context, SandboxCommandRunner, SandboxRunContext};

pub(crate) use ref_tree::{git_repo_root, materialize_ref_tree, validate_lock_for_ref};
pub use request::{RunRequest, TestRequest};
use workdir::{invocation_workdir, map_workdir};

type EnvPairs = Vec<(String, String)>;

// Mapping note: `run/mod.rs` was split for reviewability; see `commit.rs`, `errors.rs`, `ref_tree.rs`, `test_exec.rs`.
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

fn pxapp_path_from_request(request: &RunRequest) -> Option<PathBuf> {
    let entry = request.entry.as_ref()?;
    let path = PathBuf::from(entry);
    if !path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("pxapp"))
        .unwrap_or(false)
    {
        return None;
    }
    if path.exists() {
        Some(path)
    } else {
        None
    }
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
    let interactive = request.interactive.unwrap_or_else(|| {
        !strict && std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
    });
    if let Some(bundle) = pxapp_path_from_request(request) {
        if request.at.is_some() {
            return Ok(ExecutionOutcome::user_error(
                "px run <bundle.pxapp> does not support --at",
                json!({
                    "code": "PX903",
                    "reason": "pxapp_at_ref_unsupported",
                    "path": bundle.display().to_string(),
                }),
            ));
        }
        return run_pxapp_bundle(ctx, &bundle, &request.args, interactive);
    }
    let target = request
        .entry
        .clone()
        .or_else(|| request.target.clone())
        .unwrap_or_default();

    if !target.trim().is_empty() {
        let reference = match parse_run_reference_target(&target) {
            Ok(reference) => reference,
            Err(outcome) => return Ok(outcome),
        };
        if let Some(reference) = reference {
            return run_reference_target(ctx, request, &reference, &target, interactive, strict);
        }
    }

    if let Some(at_ref) = &request.at {
        return run_project_at_ref(ctx, request, at_ref);
    }

    let mut sandbox: Option<SandboxRunContext> = None;

    if !target.trim().is_empty() {
        if let Some(inline) = match detect_inline_script(&target) {
            Ok(result) => result,
            Err(outcome) => return Ok(outcome),
        } {
            let command_args = json!({
                "target": &target,
                "args": &request.args,
            });
            if request.sandbox {
                let snapshot = match manifest_snapshot() {
                    Ok(snapshot) => snapshot,
                    Err(err) => {
                        if is_missing_project_error(&err) {
                            return Ok(missing_project_outcome());
                        }
                        let msg = err.to_string();
                        if msg.contains("pyproject.toml not found") {
                            let root = ctx.project_root().unwrap_or_else(|_| {
                                env::current_dir().unwrap_or(PathBuf::from("."))
                            });
                            return Ok(missing_pyproject_outcome("run", &root));
                        }
                        return Err(err);
                    }
                };
                let state_report = match evaluate_project_state(ctx, &snapshot) {
                    Ok(report) => report,
                    Err(err) => {
                        return Ok(ExecutionOutcome::failure(
                            "failed to evaluate project state",
                            json!({ "error": err.to_string() }),
                        ))
                    }
                };
                let guard = match guard_for_execution(strict, &snapshot, &state_report, "run") {
                    Ok(guard) => guard,
                    Err(outcome) => return Ok(outcome),
                };
                if matches!(guard, crate::EnvGuard::AutoSync) {
                    if let Err(outcome) = python_context_with_mode(ctx, guard) {
                        return Ok(outcome);
                    }
                }
                match prepare_project_sandbox(ctx, &snapshot) {
                    Ok(sbx) => sandbox = Some(sbx),
                    Err(outcome) => return Ok(outcome),
                }
            }
            let mut outcome = match run_inline_script(
                ctx,
                sandbox.as_mut(),
                inline,
                &request.args,
                &command_args,
                interactive,
                strict,
            ) {
                Ok(outcome) => outcome,
                Err(outcome) => outcome,
            };
            if let Some(ref sbx) = sandbox {
                attach_sandbox_details(&mut outcome, sbx);
            }
            return Ok(outcome);
        }
    }

    if target.trim().is_empty() {
        match manifest_snapshot() {
            Ok(_) => return Ok(run_target_required_outcome()),
            Err(err) => {
                if is_missing_project_error(&err) {
                    return Ok(missing_project_outcome());
                }
                let msg = err.to_string();
                if msg.contains("pyproject.toml not found") {
                    let root = ctx
                        .project_root()
                        .unwrap_or_else(|_| env::current_dir().unwrap_or(PathBuf::from(".")));
                    return Ok(missing_pyproject_outcome("run", &root));
                }
                return Err(err);
            }
        }
    }
    let plan = match super::execution_plan::plan_run_execution(
        ctx,
        strict,
        request.sandbox,
        &target,
        &request.args,
    ) {
        Ok(plan) => plan,
        Err(outcome) => return Ok(outcome),
    };

    let mut workspace_cas_native_fallback: Option<CasNativeFallback> = None;
    if matches!(
        plan.context,
        super::execution_plan::PlanContext::Workspace { .. }
    ) && matches!(
        plan.engine.mode,
        super::execution_plan::EngineMode::MaterializedEnv
    ) {
        if let Some(code) = plan.engine.fallback_reason_code.as_deref() {
            if let Some(reason) = match code {
                "missing_artifacts" => Some(CasNativeFallbackReason::MissingArtifacts),
                _ => None,
            } {
                let summary = "cached artifacts missing".to_string();
                debug!(
                    CAS_NATIVE_FALLBACK = reason.as_str(),
                    error = %summary,
                    "CAS_NATIVE_FALLBACK={} falling back to env materialization",
                    reason.as_str()
                );
                workspace_cas_native_fallback = Some(CasNativeFallback { reason, summary });
            }
        }
    }
    if matches!(
        plan.context,
        super::execution_plan::PlanContext::Workspace { .. }
    ) && matches!(
        plan.engine.mode,
        super::execution_plan::EngineMode::CasNative
    ) {
        let scope = match discover_workspace_scope() {
            Ok(scope) => scope,
            Err(err) => {
                return Ok(ExecutionOutcome::failure(
                    "failed to detect workspace",
                    json!({ "error": err.to_string() }),
                ));
            }
        };
        if let Some(WorkspaceScope::Member {
            workspace,
            member_root,
        }) = scope
        {
            match prepare_cas_native_workspace_run_context(ctx, &workspace, &member_root) {
                Ok(native_ctx) => {
                    let deps = DependencyContext::from_sources(
                        &workspace.dependencies,
                        Some(&workspace.lock_path),
                    );
                    let mut command_args = json!({
                        "target": &target,
                        "args": &request.args,
                    });
                    deps.inject(&mut command_args);
                    let workdir = invocation_workdir(&native_ctx.py_ctx.project_root);
                    let host_runner = HostCommandRunner::new(ctx);
                    let plan = plan_run_target(
                        &native_ctx.py_ctx,
                        &member_root.join("pyproject.toml"),
                        &target,
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
                        let summary = cas_native_fallback_summary(&outcome);
                        debug!(
                            CAS_NATIVE_FALLBACK = reason.as_str(),
                            error = %summary,
                            "CAS_NATIVE_FALLBACK={} falling back to env materialization",
                            reason.as_str()
                        );
                        workspace_cas_native_fallback = Some(CasNativeFallback { reason, summary });
                    } else {
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
                    let summary = cas_native_fallback_summary(&outcome);
                    debug!(
                        CAS_NATIVE_FALLBACK = reason.as_str(),
                        error = %summary,
                        "CAS_NATIVE_FALLBACK={} falling back to env materialization",
                        reason.as_str()
                    );
                    workspace_cas_native_fallback = Some(CasNativeFallback { reason, summary });
                }
            }
        }
    }

    if let Some(ws_ctx) = match prepare_workspace_run_context(ctx, strict, "run", request.sandbox) {
        Ok(result) => result,
        Err(outcome) => return Ok(outcome),
    } {
        if request.sandbox
            && strict
            && !matches!(
                ws_ctx.state.canonical,
                WorkspaceStateKind::Consistent | WorkspaceStateKind::InitializedEmpty
            )
        {
            return Ok(sandbox_workspace_env_inconsistent(
                &ws_ctx.workspace_root,
                &ws_ctx.state,
            ));
        }
        if request.sandbox {
            match prepare_workspace_sandbox(ctx, &ws_ctx) {
                Ok(sbx) => sandbox = Some(sbx),
                Err(outcome) => return Ok(outcome),
            }
        }
        let workdir = invocation_workdir(&ws_ctx.py_ctx.project_root);
        let deps = DependencyContext::from_sources(&ws_ctx.workspace_deps, Some(&ws_ctx.lock_path));
        let mut command_args = json!({
            "target": &target,
            "args": &request.args,
        });
        deps.inject(&mut command_args);
        let host_runner = HostCommandRunner::new(ctx);
        let sandbox_runner = match sandbox {
            Some(ref mut sbx) => {
                let runner = match sandbox_runner_for_context(&ws_ctx.py_ctx, sbx, &workdir) {
                    Ok(runner) => runner,
                    Err(outcome) => return Ok(outcome),
                };
                Some(runner)
            }
            None => None,
        };
        let runner: &dyn CommandRunner = match sandbox_runner.as_ref() {
            Some(runner) => runner,
            None => &host_runner,
        };
        let plan = plan_run_target(&ws_ctx.py_ctx, &ws_ctx.manifest_path, &target, &workdir)?;
        let mut outcome = match plan {
            RunTargetPlan::Script(path) => run_project_script(
                ctx,
                runner,
                &ws_ctx.py_ctx,
                &path,
                &request.args,
                &command_args,
                &workdir,
                interactive,
                if sandbox.is_some() {
                    "python"
                } else {
                    &ws_ctx.py_ctx.python
                },
            )?,
            RunTargetPlan::Executable(program) => run_executable(
                ctx,
                runner,
                &ws_ctx.py_ctx,
                &program,
                &request.args,
                &command_args,
                &workdir,
                interactive,
            )?,
        };
        attach_autosync_details(&mut outcome, ws_ctx.sync_report);
        if let Some(ref fallback) = workspace_cas_native_fallback {
            attach_cas_native_fallback(&mut outcome, fallback);
        }
        if let Some(ref sbx) = sandbox {
            attach_sandbox_details(&mut outcome, sbx);
        }
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

    let mut cas_native_fallback: Option<CasNativeFallback> = None;
    if matches!(
        plan.context,
        super::execution_plan::PlanContext::Project { .. }
    ) && matches!(
        plan.engine.mode,
        super::execution_plan::EngineMode::MaterializedEnv
    ) {
        if let Some(code) = plan.engine.fallback_reason_code.as_deref() {
            if let Some(reason) = match code {
                "missing_artifacts" => Some(CasNativeFallbackReason::MissingArtifacts),
                _ => None,
            } {
                let summary = "cached artifacts missing".to_string();
                debug!(
                    CAS_NATIVE_FALLBACK = reason.as_str(),
                    error = %summary,
                    "CAS_NATIVE_FALLBACK={} falling back to env materialization",
                    reason.as_str()
                );
                cas_native_fallback = Some(CasNativeFallback { reason, summary });
            }
        }
    }
    if matches!(
        plan.context,
        super::execution_plan::PlanContext::Project { .. }
    ) && matches!(
        plan.engine.mode,
        super::execution_plan::EngineMode::CasNative
    ) {
        match prepare_cas_native_run_context(ctx, &snapshot) {
            Ok(native_ctx) => {
                let manifest = native_ctx.py_ctx.project_root.join("pyproject.toml");
                let deps = DependencyContext::from_sources(
                    &snapshot.requirements,
                    Some(&snapshot.lock_path),
                );
                let mut command_args = json!({
                    "target": &target,
                    "args": &request.args,
                });
                deps.inject(&mut command_args);
                let workdir = invocation_workdir(&native_ctx.py_ctx.project_root);
                let host_runner = HostCommandRunner::new(ctx);
                let plan = plan_run_target(&native_ctx.py_ctx, &manifest, &target, &workdir)?;
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
                    let summary = cas_native_fallback_summary(&outcome);
                    debug!(
                        CAS_NATIVE_FALLBACK = reason.as_str(),
                        error = %summary,
                        "CAS_NATIVE_FALLBACK={} falling back to env materialization",
                        reason.as_str()
                    );
                    cas_native_fallback = Some(CasNativeFallback { reason, summary });
                } else {
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
                let summary = cas_native_fallback_summary(&outcome);
                debug!(
                    CAS_NATIVE_FALLBACK = reason.as_str(),
                    error = %summary,
                    "CAS_NATIVE_FALLBACK={} falling back to env materialization",
                    reason.as_str()
                );
                cas_native_fallback = Some(CasNativeFallback { reason, summary });
            }
        }
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
    if request.sandbox {
        match prepare_project_sandbox(ctx, &snapshot) {
            Ok(sbx) => sandbox = Some(sbx),
            Err(outcome) => return Ok(outcome),
        }
    }
    let manifest = py_ctx.project_root.join("pyproject.toml");
    let deps = DependencyContext::from_sources(&snapshot.requirements, Some(&snapshot.lock_path));
    let mut command_args = json!({
        "target": &target,
        "args": &request.args,
    });
    deps.inject(&mut command_args);
    let workdir = invocation_workdir(&py_ctx.project_root);
    let host_runner = HostCommandRunner::new(ctx);
    let sandbox_runner = match sandbox {
        Some(ref mut sbx) => {
            let runner = match sandbox_runner_for_context(&py_ctx, sbx, &workdir) {
                Ok(runner) => runner,
                Err(outcome) => return Ok(outcome),
            };
            Some(runner)
        }
        None => None,
    };
    let runner: &dyn CommandRunner = match sandbox_runner.as_ref() {
        Some(runner) => runner,
        None => &host_runner,
    };

    let plan = plan_run_target(&py_ctx, &manifest, &target, &workdir)?;
    let mut outcome = match plan {
        RunTargetPlan::Script(path) => run_project_script(
            ctx,
            runner,
            &py_ctx,
            &path,
            &request.args,
            &command_args,
            &workdir,
            interactive,
            if sandbox.is_some() {
                "python"
            } else {
                &py_ctx.python
            },
        )?,
        RunTargetPlan::Executable(program) => run_executable(
            ctx,
            runner,
            &py_ctx,
            &program,
            &request.args,
            &command_args,
            &workdir,
            interactive,
        )?,
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
        runner.run_command_passthrough(program, &args, &envs, workdir)?
    } else {
        runner.run_command(program, &args, &envs, workdir)?
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

fn apply_profile_env_vars(envs: &mut EnvPairs, vars: &BTreeMap<String, Value>) {
    for (key, value) in vars {
        set_env_pair(envs, key, json_env_value(value));
    }
}

fn apply_runtime_python_home(envs: &mut EnvPairs, runtime: &Path) {
    let Some(runtime_root) = runtime.parent().and_then(|bin| bin.parent()) else {
        return;
    };
    set_env_pair(envs, "PYTHONHOME", runtime_root.display().to_string());
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
    let uses_px_python = program_matches_python(program, py_ctx);
    let needs_stdin = uses_px_python && extra_args.first().map(|arg| arg == "-").unwrap_or(false);
    if !uses_px_python {
        envs.retain(|(key, _)| key != "PX_PYTHON");
    }
    let interactive = interactive || needs_stdin;
    let output = if needs_stdin {
        runner.run_command_with_stdin(program, extra_args, &envs, workdir, true)?
    } else if interactive {
        runner.run_command_passthrough(program, extra_args, &envs, workdir)?
    } else {
        runner.run_command(program, extra_args, &envs, workdir)?
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
    let exec_program = if is_python_alias {
        native.py_ctx.python.clone()
    } else {
        display_program.clone()
    };

    if let Some(subcommand) = mutating_pip_invocation(&exec_program, extra_args, &native.py_ctx) {
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
    let uses_px_python = program_matches_python(&exec_program, &native.py_ctx);
    let needs_stdin = uses_px_python && extra_args.first().is_some_and(|arg| arg == "-");
    if uses_px_python {
        apply_runtime_python_home(&mut envs, &native.runtime_path);
        apply_profile_env_vars(&mut envs, &native.env_vars);
    } else {
        envs.retain(|(key, _)| key != "PX_PYTHON");
    }
    let interactive = interactive || needs_stdin;
    let mut argv = Vec::with_capacity(extra_args.len() + 1);
    argv.push(exec_program.clone());
    argv.extend(extra_args.iter().cloned());
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
