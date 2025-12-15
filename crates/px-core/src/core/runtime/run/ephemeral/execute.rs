use super::super::*;
use super::{input, python_context, snapshot};

use std::env;
use std::path::{Path, PathBuf};

use serde_json::json;

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
    let input = match input::detect_ephemeral_input(&invocation_root, Some(target)) {
        Ok(input) => input,
        Err(outcome) => return Ok(outcome),
    };

    let pinned_required = request.frozen || ctx.env_flag_enabled("CI");
    if pinned_required {
        if let Err(outcome) =
            input::enforce_pinned_inputs("run", &invocation_root, &input, request.frozen)
        {
            return Ok(outcome);
        }
    }

    let (snapshot, runtime, sync_report) =
        match snapshot::prepare_ephemeral_snapshot(ctx, &invocation_root, &input, request.frozen) {
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

    let py_ctx = match python_context::ephemeral_python_context(
        ctx,
        &snapshot,
        &runtime,
        &invocation_root,
    ) {
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
    let input = match input::detect_ephemeral_input(&invocation_root, None) {
        Ok(input) => input,
        Err(outcome) => return Ok(outcome),
    };

    let pinned_required = request.frozen || ctx.env_flag_enabled("CI");
    if pinned_required {
        if let Err(outcome) =
            input::enforce_pinned_inputs("test", &invocation_root, &input, request.frozen)
        {
            return Ok(outcome);
        }
    }

    let (snapshot, runtime, sync_report) =
        match snapshot::prepare_ephemeral_snapshot(ctx, &invocation_root, &input, request.frozen) {
            Ok(result) => result,
            Err(outcome) => return Ok(outcome),
        };

    let workdir = invocation_workdir(&invocation_root);
    let host_runner = HostCommandRunner::new(ctx);

    let mut cas_native_fallback: Option<CasNativeFallback> = None;
    if !request.sandbox {
        match prepare_cas_native_run_context(ctx, &snapshot, &invocation_root) {
            Ok(native_ctx) => {
                let outcome = super::super::test_exec::run_tests_for_context_cas_native(
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

    let py_ctx = match python_context::ephemeral_python_context(
        ctx,
        &snapshot,
        &runtime,
        &invocation_root,
    ) {
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
