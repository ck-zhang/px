// Mapping note: `run/mod.rs` keeps the low-level execution helpers; higher-level entrypoints live in `driver.rs`.

use super::*;

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
    let plan = match super::super::execution_plan::plan_run_execution(
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
        super::super::execution_plan::PlanContext::Workspace { .. }
    ) && matches!(
        plan.engine.mode,
        super::super::execution_plan::EngineMode::MaterializedEnv
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
        super::super::execution_plan::PlanContext::Workspace { .. }
    ) && matches!(
        plan.engine.mode,
        super::super::execution_plan::EngineMode::CasNative
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
        super::super::execution_plan::PlanContext::Project { .. }
    ) && matches!(
        plan.engine.mode,
        super::super::execution_plan::EngineMode::MaterializedEnv
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
        super::super::execution_plan::PlanContext::Project { .. }
    ) && matches!(
        plan.engine.mode,
        super::super::execution_plan::EngineMode::CasNative
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
