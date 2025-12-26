use super::*;

use super::super::super::execution_plan;
use super::builtin::run_builtin_tests;
use super::pytest::run_pytest_runner;
use super::script::run_script_runner;

#[derive(Clone, Debug)]
enum TestRunner {
    Pytest,
    Builtin,
    Script(PathBuf),
}

pub(in crate::core::runtime::run) fn test_project_outcome(
    ctx: &CommandContext,
    request: &TestRequest,
) -> Result<ExecutionOutcome> {
    let strict = request.frozen || ctx.env_flag_enabled("CI");
    let allow_lock_autosync =
        !strict && std::io::stdin().is_terminal() && std::io::stdout().is_terminal();

    if request.ephemeral {
        return super::super::ephemeral::test_ephemeral_outcome(ctx, request, strict);
    }

    if let Some(at_ref) = &request.at {
        return run_tests_at_ref(ctx, request, at_ref);
    }
    let mut sandbox: Option<SandboxRunContext> = None;

    let plan = match execution_plan::plan_test_execution(
        ctx,
        strict,
        allow_lock_autosync,
        request.sandbox,
        &request.args,
    ) {
        Ok(plan) => plan,
        Err(outcome) => return Ok(outcome),
    };

    let mut workspace_cas_native_fallback: Option<CasNativeFallback> = None;
    if matches!(plan.context, execution_plan::PlanContext::Workspace { .. })
        && matches!(
            plan.engine.mode,
            execution_plan::EngineMode::MaterializedEnv
        )
    {
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
    if matches!(plan.context, execution_plan::PlanContext::Workspace { .. })
        && matches!(plan.engine.mode, execution_plan::EngineMode::CasNative)
    {
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
                    let workdir = invocation_workdir(&native_ctx.py_ctx.project_root);
                    let host_runner = HostCommandRunner::new(ctx);
                    let outcome = run_tests_for_context_cas_native(
                        ctx,
                        &host_runner,
                        &native_ctx,
                        request,
                        None,
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

    if let Some(ws_ctx) = match prepare_workspace_run_context(ctx, strict, "test", request.sandbox)
    {
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
        let mut outcome = run_tests_for_context(
            ctx,
            runner,
            &ws_ctx.py_ctx,
            request,
            ws_ctx.sync_report,
            &workdir,
        )?;
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
                let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                return Ok(missing_pyproject_outcome("test", &root));
            }
            return Err(err);
        }
    };
    let mut cas_native_fallback: Option<CasNativeFallback> = None;
    if matches!(plan.context, execution_plan::PlanContext::Project { .. })
        && matches!(
            plan.engine.mode,
            execution_plan::EngineMode::MaterializedEnv
        )
    {
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
    if matches!(plan.context, execution_plan::PlanContext::Project { .. })
        && matches!(plan.engine.mode, execution_plan::EngineMode::CasNative)
    {
        match prepare_cas_native_run_context(ctx, &snapshot, &snapshot.root) {
            Ok(native_ctx) => {
                let workdir = invocation_workdir(&native_ctx.py_ctx.project_root);
                let host_runner = HostCommandRunner::new(ctx);
                let outcome = run_tests_for_context_cas_native(
                    ctx,
                    &host_runner,
                    &native_ctx,
                    request,
                    None,
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
    let state_report = match crate::state_guard::state_or_violation(ctx, &snapshot, "test") {
        Ok(report) => report,
        Err(outcome) => return Ok(outcome),
    };
    let guard = match guard_for_execution(
        strict,
        allow_lock_autosync,
        &snapshot,
        &state_report,
        "test",
    ) {
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
    let mut outcome = run_tests_for_context(ctx, runner, &py_ctx, request, sync_report, &workdir)?;
    if let Some(ref fallback) = cas_native_fallback {
        attach_cas_native_fallback(&mut outcome, fallback);
    }
    if let Some(ref sbx) = sandbox {
        attach_sandbox_details(&mut outcome, sbx);
    }
    Ok(outcome)
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

pub(in crate::core::runtime::run) fn find_runtests_script(project_root: &Path) -> Option<PathBuf> {
    ["tests/runtests.py", "runtests.py"]
        .iter()
        .map(|rel| project_root.join(rel))
        .find(|candidate| candidate.is_file())
}

pub(in crate::core::runtime::run) fn run_tests_for_context(
    ctx: &CommandContext,
    runner: &dyn CommandRunner,
    py_ctx: &PythonContext,
    request: &TestRequest,
    sync_report: Option<crate::EnvironmentSyncReport>,
    workdir: &Path,
) -> Result<ExecutionOutcome> {
    let strict = request.frozen || ctx.env_flag_enabled("CI");
    let command_args = json!({ "test_args": request.args });
    let (mut envs, _preflight) = build_env_with_preflight(ctx, py_ctx, &command_args)?;
    let stream_runner = !ctx.global.json;
    let allow_missing_pytest_fallback = sync_report
        .as_ref()
        .map(|report| report.action() == "env-recreate")
        .unwrap_or(false);

    let mut outcome = match select_test_runner(ctx, py_ctx) {
        TestRunner::Builtin => {
            run_builtin_tests(ctx, runner, py_ctx, envs, stream_runner, workdir)?
        }
        TestRunner::Script(script) => run_script_runner(
            ctx,
            runner,
            py_ctx,
            envs,
            &script,
            &request.args,
            stream_runner,
            workdir,
        )?,
        TestRunner::Pytest => {
            envs.push(("PX_TEST_RUNNER".into(), "pytest".into()));
            run_pytest_runner(
                ctx,
                runner,
                py_ctx,
                envs,
                &request.args,
                stream_runner,
                strict,
                allow_missing_pytest_fallback,
                workdir,
            )?
        }
    };
    attach_autosync_details(&mut outcome, sync_report);
    Ok(outcome)
}

pub(in crate::core::runtime::run) fn run_tests_for_context_cas_native(
    ctx: &CommandContext,
    runner: &dyn CommandRunner,
    native: &CasNativeRunContext,
    request: &TestRequest,
    sync_report: Option<crate::EnvironmentSyncReport>,
    workdir: &Path,
) -> Result<ExecutionOutcome> {
    let strict = request.frozen || ctx.env_flag_enabled("CI");
    let command_args = json!({ "test_args": request.args });
    let (mut envs, _preflight) = build_env_with_preflight(ctx, &native.py_ctx, &command_args)?;
    apply_runtime_python_home(&mut envs, &native.runtime_path);
    apply_profile_env_vars(&mut envs, &native.env_vars);

    let stream_runner = !ctx.global.json;
    let allow_missing_pytest_fallback = sync_report
        .as_ref()
        .map(|report| report.action() == "env-recreate")
        .unwrap_or(false);

    let mut outcome = match select_test_runner(ctx, &native.py_ctx) {
        TestRunner::Builtin => {
            run_builtin_tests(ctx, runner, &native.py_ctx, envs, stream_runner, workdir)?
        }
        TestRunner::Script(script) => run_script_runner(
            ctx,
            runner,
            &native.py_ctx,
            envs,
            &script,
            &request.args,
            stream_runner,
            workdir,
        )?,
        TestRunner::Pytest => {
            envs.push(("PX_TEST_RUNNER".into(), "pytest".into()));
            run_pytest_runner(
                ctx,
                runner,
                &native.py_ctx,
                envs,
                &request.args,
                stream_runner,
                strict,
                allow_missing_pytest_fallback,
                workdir,
            )?
        }
    };
    attach_autosync_details(&mut outcome, sync_report);
    Ok(outcome)
}
