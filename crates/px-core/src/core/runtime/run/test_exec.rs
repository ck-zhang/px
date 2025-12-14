// Test execution + runners (pytest/builtin/script).
// Moved from `run/mod.rs` to keep the public entrypoints readable.
use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TestReporter {
    Px,
    Pytest,
}

#[derive(Clone, Debug)]
enum TestRunner {
    Pytest,
    Builtin,
    Script(PathBuf),
}

#[allow(clippy::too_many_arguments)]
fn run_pytest_runner(
    ctx: &CommandContext,
    runner: &dyn CommandRunner,
    py_ctx: &PythonContext,
    envs: EnvPairs,
    test_args: &[String],
    stream_runner: bool,
    allow_builtin_fallback: bool,
    workdir: &Path,
) -> Result<ExecutionOutcome> {
    let reporter = test_reporter_from_env();
    let (envs, pytest_cmd) = build_pytest_invocation(ctx, py_ctx, envs, test_args, reporter)?;
    let output = run_python_command(runner, py_ctx, &pytest_cmd, &envs, stream_runner, workdir)?;
    if output.code == 0 {
        let mut outcome = test_success("pytest", output, stream_runner, test_args);
        if let TestReporter::Px = reporter {
            mark_reporter_rendered(&mut outcome);
        }
        return Ok(outcome);
    }
    if missing_pytest(&output.stderr) {
        if ctx.config().test.fallback_builtin || allow_builtin_fallback {
            return run_builtin_tests(ctx, runner, py_ctx, envs, stream_runner, workdir);
        }
        return Ok(missing_pytest_outcome(output, test_args));
    }
    let mut outcome = test_failure("pytest", output, stream_runner, test_args);
    if let TestReporter::Px = reporter {
        mark_reporter_rendered(&mut outcome);
    }
    Ok(outcome)
}

fn mark_reporter_rendered(outcome: &mut ExecutionOutcome) {
    match &mut outcome.details {
        Value::Object(map) => {
            map.insert("reporter_rendered".into(), Value::Bool(true));
        }
        Value::Null => {
            outcome.details = json!({ "reporter_rendered": true });
        }
        other => {
            let prev = other.take();
            outcome.details = json!({ "value": prev, "reporter_rendered": true });
        }
    }
}

pub(super) fn build_pytest_invocation(
    ctx: &CommandContext,
    py_ctx: &PythonContext,
    mut envs: EnvPairs,
    test_args: &[String],
    reporter: TestReporter,
) -> Result<(EnvPairs, Vec<String>)> {
    let mut defaults = default_pytest_flags(reporter);
    if let TestReporter::Px = reporter {
        let plugin_path = ensure_px_pytest_plugin(ctx, py_ctx)?;
        let plugin_dir = plugin_path
            .parent()
            .unwrap_or(py_ctx.project_root.as_path());
        append_pythonpath(&mut envs, plugin_dir);
        append_allowed_paths(&mut envs, plugin_dir);
        defaults.extend_from_slice(&["-p".to_string(), "px_pytest_plugin".to_string()]);
    }
    let pytest_cmd = build_pytest_command_with_defaults(&py_ctx.project_root, test_args, &defaults);
    Ok((envs, pytest_cmd))
}

pub(super) fn default_pytest_flags(reporter: TestReporter) -> Vec<String> {
    let mut flags = vec![
        "--color=yes".to_string(),
        "--tb=short".to_string(),
        "--ignore=.px".to_string(),
    ];
    if matches!(reporter, TestReporter::Px | TestReporter::Pytest) {
        flags.push("-q".to_string());
    }
    flags
}

fn test_reporter_from_env() -> TestReporter {
    match std::env::var("PX_TEST_REPORTER")
        .ok()
        .as_deref()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("pytest") => TestReporter::Pytest,
        Some("px") | None => TestReporter::Px,
        _ => TestReporter::Px,
    }
}

fn append_pythonpath(envs: &mut EnvPairs, plugin_dir: &Path) {
    let plugin_entry = plugin_dir.display().to_string();
    if let Some((_, value)) = envs.iter_mut().find(|(key, _)| key == "PYTHONPATH") {
        let mut parts: Vec<_> = env::split_paths(value).collect();
        if !parts.iter().any(|p| p == plugin_dir) {
            parts.insert(0, plugin_dir.to_path_buf());
            if let Ok(joined) = env::join_paths(parts) {
                if let Ok(strval) = joined.into_string() {
                    *value = strval;
                }
            }
        }
    } else {
        envs.push(("PYTHONPATH".into(), plugin_entry));
    }
}

fn append_allowed_paths(envs: &mut EnvPairs, path: &Path) {
    if let Some((_, value)) = envs.iter_mut().find(|(key, _)| key == "PX_ALLOWED_PATHS") {
        let mut parts: Vec<_> = env::split_paths(value).collect();
        if !parts.iter().any(|p| p == path) {
            parts.insert(0, path.to_path_buf());
            if let Ok(joined) = env::join_paths(parts) {
                if let Ok(strval) = joined.into_string() {
                    *value = strval;
                }
            }
        }
    }
}

fn ensure_px_pytest_plugin(ctx: &CommandContext, py_ctx: &PythonContext) -> Result<PathBuf> {
    let plugin_dir = py_ctx.project_root.join(".px").join("plugins");
    ctx.fs()
        .create_dir_all(&plugin_dir)
        .context("creating px plugin dir")?;
    let plugin_path = plugin_dir.join("px_pytest_plugin.py");
    ctx.fs()
        .write(&plugin_path, PX_PYTEST_PLUGIN.as_bytes())
        .context("writing pytest reporter plugin")?;
    Ok(plugin_path)
}

fn run_python_command(
    runner: &dyn CommandRunner,
    py_ctx: &PythonContext,
    args: &[String],
    envs: &[(String, String)],
    stream_runner: bool,
    cwd: &Path,
) -> Result<crate::RunOutput> {
    let mut envs = envs.to_vec();
    if let Some(merged) = merged_pythonpath(&envs) {
        envs.retain(|(key, _)| key != "PYTHONPATH");
        envs.push(("PYTHONPATH".into(), merged));
    }
    if stream_runner {
        runner.run_command_streaming(&py_ctx.python, args, &envs, cwd)
    } else {
        runner.run_command(&py_ctx.python, args, &envs, cwd)
    }
}

pub(super) fn merged_pythonpath(envs: &[(String, String)]) -> Option<String> {
    use std::collections::HashSet;

    let allowed = envs
        .iter()
        .find(|(key, _)| key == "PX_ALLOWED_PATHS")
        .map(|(_, value)| value)?;

    let mut paths = Vec::new();
    let mut seen = HashSet::new();
    let mut push_unique = |path: std::path::PathBuf| {
        if seen.insert(path.clone()) {
            paths.push(path);
        }
    };

    for entry in std::env::split_paths(allowed) {
        push_unique(entry);
    }

    if let Some((_, pythonpath)) = envs.iter().find(|(key, _)| key == "PYTHONPATH") {
        for entry in std::env::split_paths(pythonpath) {
            push_unique(entry);
        }
    }

    std::env::join_paths(paths)
        .ok()
        .and_then(|joined| joined.into_string().ok())
}

pub(super) fn test_project_outcome(
    ctx: &CommandContext,
    request: &TestRequest,
) -> Result<ExecutionOutcome> {
    if let Some(at_ref) = &request.at {
        return run_tests_at_ref(ctx, request, at_ref);
    }
    let strict = request.frozen || ctx.env_flag_enabled("CI");
    let mut sandbox: Option<SandboxRunContext> = None;

    let plan = match super::super::execution_plan::plan_test_execution(
        ctx,
        strict,
        request.sandbox,
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
                let root = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                return Ok(missing_pyproject_outcome("test", &root));
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
    let guard = match guard_for_execution(strict, &snapshot, &state_report, "test") {
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

pub(super) fn find_runtests_script(project_root: &Path) -> Option<PathBuf> {
    ["tests/runtests.py", "runtests.py"]
        .iter()
        .map(|rel| project_root.join(rel))
        .find(|candidate| candidate.is_file())
}

pub(super) fn run_tests_for_context(
    ctx: &CommandContext,
    runner: &dyn CommandRunner,
    py_ctx: &PythonContext,
    request: &TestRequest,
    sync_report: Option<crate::EnvironmentSyncReport>,
    workdir: &Path,
) -> Result<ExecutionOutcome> {
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
                allow_missing_pytest_fallback,
                workdir,
            )?
        }
    };
    attach_autosync_details(&mut outcome, sync_report);
    Ok(outcome)
}

fn run_tests_for_context_cas_native(
    ctx: &CommandContext,
    runner: &dyn CommandRunner,
    native: &CasNativeRunContext,
    request: &TestRequest,
    sync_report: Option<crate::EnvironmentSyncReport>,
    workdir: &Path,
) -> Result<ExecutionOutcome> {
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
                allow_missing_pytest_fallback,
                workdir,
            )?
        }
    };
    attach_autosync_details(&mut outcome, sync_report);
    Ok(outcome)
}

#[allow(clippy::too_many_arguments)]
fn run_script_runner(
    _ctx: &CommandContext,
    runner: &dyn CommandRunner,
    py_ctx: &PythonContext,
    mut envs: EnvPairs,
    script: &Path,
    args: &[String],
    stream_runner: bool,
    workdir: &Path,
) -> Result<ExecutionOutcome> {
    let runner_label = script
        .strip_prefix(&py_ctx.project_root)
        .unwrap_or(script)
        .display()
        .to_string();
    envs.push(("PX_TEST_RUNNER".into(), runner_label.clone()));
    let mut cmd_args = vec![script.display().to_string()];
    cmd_args.extend_from_slice(args);
    let output = run_python_command(runner, py_ctx, &cmd_args, &envs, stream_runner, workdir)?;
    if output.code == 0 {
        Ok(test_success(&runner_label, output, stream_runner, args))
    } else {
        Ok(test_failure(&runner_label, output, stream_runner, args))
    }
}

fn run_builtin_tests(
    _core_ctx: &CommandContext,
    runner: &dyn CommandRunner,
    ctx: &PythonContext,
    mut envs: Vec<(String, String)>,
    stream_runner: bool,
    workdir: &Path,
) -> Result<ExecutionOutcome> {
    if let Some(path) = ensure_stdlib_tests_available(ctx)? {
        append_pythonpath(&mut envs, &path);
    }
    envs.push(("PX_TEST_RUNNER".into(), "builtin".into()));
    let script = "from sample_px_app import cli\nassert cli.greet() == 'Hello, World!'\nprint('px fallback test passed')";
    let args = vec!["-c".to_string(), script.to_string()];
    let output = run_python_command(runner, ctx, &args, &envs, stream_runner, workdir)?;
    let runner_args: Vec<String> = Vec::new();
    Ok(test_success("builtin", output, stream_runner, &runner_args))
}

fn test_success(
    runner: &str,
    output: crate::RunOutput,
    stream_runner: bool,
    args: &[String],
) -> ExecutionOutcome {
    ExecutionOutcome::success(
        format!("{runner} ok"),
        test_details(runner, output, stream_runner, args, None),
    )
}

fn ensure_stdlib_tests_available(py_ctx: &PythonContext) -> Result<Option<PathBuf>> {
    const DISCOVER_SCRIPT: &str =
        "import json, sys, sysconfig; print(json.dumps({'version': sys.version.split()[0], 'stdlib': sysconfig.get_path('stdlib')}))";
    let output = Command::new(&py_ctx.python)
        .arg("-c")
        .arg(DISCOVER_SCRIPT)
        .output()
        .context("probing python stdlib path")?;
    if !output.status.success() {
        bail!(
            "python exited with {} while probing stdlib",
            output.status.code().unwrap_or(-1)
        );
    }
    let payload: Value =
        serde_json::from_slice(&output.stdout).context("invalid stdlib probe payload")?;
    let runtime_version = payload
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let Some((major, minor)) = parse_python_version(&runtime_version) else {
        return Ok(None);
    };
    let stdlib = payload
        .get("stdlib")
        .and_then(Value::as_str)
        .context("python stdlib path unavailable")?;
    let tests_dir = PathBuf::from(stdlib).join("test");
    if tests_dir.exists() {
        return Ok(None);
    }

    // Avoid mutating the system stdlib; stage tests under the project .px directory.
    let staging_base = env::var_os("PX_STDLIB_STAGING_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| py_ctx.project_root.join(".px").join("stdlib-tests"));
    let staged_root = staging_base.join(format!("{major}.{minor}"));
    let staged_tests = staged_root.join("test");
    if staged_tests.exists() {
        return Ok(Some(staged_root));
    }

    if let Some((host_python, source_tests)) = host_stdlib_tests(&major, &minor, &runtime_version) {
        if copy_stdlib_tests(&source_tests, &staged_tests, &host_python).is_ok() {
            return Ok(Some(staged_root));
        }
    }
    if download_stdlib_tests(&runtime_version, &staged_tests)? {
        return Ok(Some(staged_root));
    }

    warn!(
        runtime = %runtime_version,
        tests_dir = %tests_dir.display(),
        "stdlib test suite missing; proceeding without staging tests"
    );
    Ok(None)
}

fn host_stdlib_tests(
    major: &str,
    minor: &str,
    runtime_version: &str,
) -> Option<(PathBuf, PathBuf)> {
    let candidates = [
        format!("python{major}.{minor}"),
        format!("python{major}"),
        "python".to_string(),
    ];
    for candidate in candidates {
        let output = Command::new(&candidate)
            .arg("-c")
            .arg(
                "import json, sys, sysconfig; print(json.dumps({'stdlib': sysconfig.get_path('stdlib'), 'version': sys.version.split()[0], 'executable': sys.executable}))",
            )
            .output();
        let Ok(output) = output else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        let Ok(payload) = serde_json::from_slice::<Value>(&output.stdout) else {
            continue;
        };
        let detected_version = payload
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if !detected_version.starts_with(&format!("{major}.{minor}")) {
            continue;
        }
        let stdlib = payload
            .get("stdlib")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if stdlib.is_empty() {
            continue;
        }
        let tests = PathBuf::from(stdlib).join("test");
        if !tests.exists() {
            continue;
        }
        let exe = payload
            .get("executable")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(candidate.clone()));
        debug!(
            version = %runtime_version,
            source = %exe.display(),
            tests = %tests.display(),
            "found host stdlib tests"
        );
        return Some((exe, tests));
    }
    None
}

fn download_stdlib_tests(version: &str, dest: &Path) -> Result<bool> {
    let url = format!("https://www.python.org/ftp/python/{version}/Python-{version}.tgz");
    let client = crate::core::runtime::build_http_client()?;
    let response = match client.get(&url).send() {
        Ok(resp) => resp,
        Err(err) => {
            debug!(error = %err, url = %url, "failed to download cpython sources");
            return Ok(false);
        }
    };
    if !response.status().is_success() {
        debug!(
            status = %response.status(),
            url = %url,
            "cpython source archive unavailable for stdlib tests"
        );
        return Ok(false);
    }
    let bytes = match response.bytes() {
        Ok(bytes) => bytes,
        Err(err) => {
            debug!(%err, url = %url, "failed to read cpython source archive for stdlib tests");
            return Ok(false);
        }
    };
    let mut archive = Archive::new(GzDecoder::new(Cursor::new(bytes)));
    if dest.exists() {
        fs::remove_dir_all(dest)
            .with_context(|| format!("clearing existing stdlib tests at {}", dest.display()))?;
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating stdlib parent {}", parent.display()))?;
    }
    let prefix = PathBuf::from(format!("Python-{version}/Lib/test"));
    let mut extracted = false;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?;
        let Ok(rel) = path.strip_prefix(&prefix) else {
            continue;
        };
        let dest_path = dest.join(rel);
        if let Some(parent) = dest_path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        entry.unpack(&dest_path)?;
        extracted = true;
    }
    Ok(extracted)
}

fn copy_stdlib_tests(source: &Path, dest: &Path, python: &Path) -> Result<()> {
    let script = r#"
import shutil
import sys
from pathlib import Path

src = Path(sys.argv[1])
dest = Path(sys.argv[2])
shutil.copytree(src, dest, dirs_exist_ok=True, symlinks=True)
"#;
    if dest.exists() {
        fs::remove_dir_all(dest)
            .with_context(|| format!("removing previous stdlib tests at {}", dest.display()))?;
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating stdlib parent {}", parent.display()))?;
    }
    let status = Command::new(python)
        .arg("-c")
        .arg(script)
        .arg(source.as_os_str())
        .arg(dest.as_os_str())
        .status()
        .with_context(|| format!("copying stdlib tests using {}", python.display()))?;
    if !status.success() {
        bail!(
            "python exited with {} while copying stdlib tests",
            status.code().unwrap_or(-1)
        );
    }
    Ok(())
}

fn parse_python_version(version: &str) -> Option<(String, String)> {
    let mut parts = version.split('.');
    let major = parts.next()?.to_string();
    let minor = parts.next().unwrap_or_default().to_string();
    if major.is_empty() || minor.is_empty() {
        return None;
    }
    Some((major, minor))
}

fn test_failure(
    runner: &str,
    output: crate::RunOutput,
    stream_runner: bool,
    args: &[String],
) -> ExecutionOutcome {
    let code = output.code;
    let mut details = test_details(runner, output, stream_runner, args, Some("tests_failed"));
    if let Value::Object(map) = &mut details {
        map.insert("suppress_cli_frame".into(), Value::Bool(true));
    }
    ExecutionOutcome::failure(format!("{runner} failed (exit {code})"), details)
}

fn missing_pytest_outcome(output: crate::RunOutput, args: &[String]) -> ExecutionOutcome {
    ExecutionOutcome::user_error(
        "pytest is not available in the project environment",
        json!({
            "stdout": output.stdout,
            "stderr": output.stderr,
            "hint": "Add pytest to your project with `px add pytest`, then rerun `px test`.",
            "reason": "missing_pytest",
            "code": crate::diag_commands::TEST,
            "runner": "pytest",
            "args": args,
        }),
    )
}

fn test_details(
    runner: &str,
    output: crate::RunOutput,
    stream_runner: bool,
    args: &[String],
    reason: Option<&str>,
) -> serde_json::Value {
    let mut details = json!({
        "runner": runner,
        "stdout": output.stdout,
        "stderr": output.stderr,
        "code": output.code,
        "args": args,
        "streamed": stream_runner,
    });
    if let Some(reason) = reason {
        if let Some(map) = details.as_object_mut() {
            map.insert("reason".to_string(), json!(reason));
        }
    }
    details
}

pub(super) fn missing_pytest(stderr: &str) -> bool {
    let lowered = stderr.to_ascii_lowercase();
    if !lowered.contains("no module named") {
        return false;
    }
    lowered.contains("no module named 'pytest'")
        || lowered.contains("no module named \"pytest\"")
        || lowered
            .split_once("no module named")
            .map(|(_, rest)| rest.trim_start().starts_with("pytest"))
            .unwrap_or(false)
}

#[cfg(test)]
pub(super) fn build_pytest_command(project_root: &Path, extra_args: &[String]) -> Vec<String> {
    build_pytest_command_with_defaults(project_root, extra_args, &[])
}

fn build_pytest_command_with_defaults(
    project_root: &Path,
    extra_args: &[String],
    defaults: &[String],
) -> Vec<String> {
    let mut pytest_cmd = vec!["-m".to_string(), "pytest".to_string()];
    pytest_cmd.extend(defaults.iter().cloned());
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

const PX_PYTEST_PLUGIN: &str = r#"import os
import sys
import time
import pytest
from _pytest._io.terminalwriter import TerminalWriter


class PxTerminalReporter:
    def __init__(self, config):
        self.config = config
        self._tw = TerminalWriter(file=sys.stdout)
        self._tw.hasmarkup = True
        self.session_start = time.time()
        self.collection_start = None
        self.collection_duration = 0.0
        self.collected = 0
        self.files = []
        self._current_file = None
        self.failures = []
        self.collection_errors = []
        self.stats = {"passed": 0, "failed": 0, "skipped": 0, "error": 0, "xfailed": 0, "xpassed": 0}
        self.exitstatus = 0
        self.spinner_frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]
        self.spinner_index = 0
        self.last_progress_len = 0
        self._spinner_active = True

    def pytest_sessionstart(self, session):
        import platform

        py_ver = platform.python_version()
        root = str(self.config.rootpath)
        cfg = self.config.inifile or "auto-detected"
        self._tw.line(f"px test  •  Python {py_ver}  •  pytest {pytest.__version__}", cyan=True, bold=True)
        self._tw.line(f"root:   {root}")
        self._tw.line(f"config: {cfg}")
        self.collection_start = time.time()
        self._render_progress(note="collecting", force=True)

    def pytest_collection_finish(self, session):
        self.collected = len(session.items)
        files = {str(item.fspath) for item in session.items}
        self.files = sorted(files)
        self.collection_duration = time.time() - (self.collection_start or self.session_start)
        label = "tests" if self.collected != 1 else "test"
        file_label = "files" if len(self.files) != 1 else "file"
        self._spinner_active = False
        self._clear_spinner(newline=True)
        self._tw.line(f"collected {self.collected} {label} from {len(self.files)} {file_label} in {self.collection_duration:.2f}s")
        self._tw.line("")

    def pytest_collectreport(self, report):
        if report.failed:
            self.stats["error"] += 1
            summary = getattr(report, "longreprtext", "") or getattr(report, "longrepr", "")
            self.collection_errors.append((str(report.fspath), str(summary)))
        self._render_progress(note="collecting")

    def pytest_runtest_logreport(self, report):
        if report.when not in ("setup", "call", "teardown"):
            return
        status = None
        if report.passed and report.when == "call":
            status = "passed"
            self.stats["passed"] += 1
        elif report.skipped:
            status = "skipped"
            self.stats["skipped"] += 1
        elif report.failed:
            status = "failed" if report.when == "call" else "error"
            self.stats[status] += 1

        if status:
            file_path = str(report.location[0])
            name = report.location[2]
            duration = getattr(report, "duration", 0.0)
            self._print_test_result(file_path, name, status, duration)

        if report.failed:
            self.failures.append(report)

    def pytest_sessionfinish(self, session, exitstatus):
        self.exitstatus = exitstatus
        self._spinner_active = False
        self._clear_spinner(newline=True)
        if self.failures:
            self._render_failures()
        if self.collection_errors:
            self._render_collection_errors()
        self._render_summary(exitstatus)

    # --- rendering helpers ---
    def _render_progress(self, note="", force=False):
        if not force and not self._spinner_active:
            return
        total = self.collected or "?"
        completed = (
            self.stats["passed"]
            + self.stats["failed"]
            + self.stats["skipped"]
            + self.stats["error"]
            + self.stats["xfailed"]
            + self.stats["xpassed"]
        )
        frame = self.spinner_frames[self.spinner_index % len(self.spinner_frames)]
        self.spinner_index += 1
        line = f"\r{frame} {completed}/{total} • pass:{self.stats['passed']} fail:{self.stats['failed']} skip:{self.stats['skipped']} err:{self.stats['error']}"
        if note:
            line += f" • {note}"
        padding = max(self.last_progress_len - len(line), 0)
        sys.stdout.write(line + (" " * padding))
        sys.stdout.flush()
        self.last_progress_len = len(line)

    def _clear_spinner(self, newline: bool = False):
        if self.last_progress_len:
            end = "\n" if newline else "\r"
            sys.stdout.write("\r" + " " * self.last_progress_len + end)
            sys.stdout.flush()
            self.last_progress_len = 0

    def _print_test_result(self, file_path, name, status, duration):
        if self._current_file != file_path:
            self._current_file = file_path
            self._tw.line("")
            self._tw.line(file_path)
        icon, color = self._status_icon(status)
        dur = f"{duration:.2f}s"
        line = f"  {icon} {name}  {dur}"
        self._tw.line(line, **color)

    def _render_failures(self):
        self._tw.line(f"FAILURES ({len(self.failures)})", red=True, bold=True)
        self._tw.line("-" * 11)
        for idx, report in enumerate(self.failures, start=1):
            self._render_single_failure(idx, report)

    def _render_collection_errors(self):
        self._tw.line(f"COLLECTION ERRORS ({len(self.collection_errors)})", red=True, bold=True)
        self._tw.line("-" * 19)
        for idx, (path, summary) in enumerate(self.collection_errors, start=1):
            self._tw.line("")
            self._tw.line(f"{idx}) {path}", bold=True)
            if summary:
                for line in str(summary).splitlines():
                    self._tw.line(f"   {line}", red=True)

    def _render_single_failure(self, idx, report):
        path, lineno = self._failure_lineno(report)
        self._tw.line("")
        self._tw.line(f"{idx}) {report.nodeid}", bold=True)
        self._tw.line("")
        message = self._failure_message(report)
        if message:
            self._tw.line(f"   {message}", red=True)
            self._tw.line("")
        snippet = self._load_snippet(path, lineno)
        if snippet:
            file_line = f"   {path}:{lineno}"
            self._tw.line(file_line)
            for i, text in snippet:
                pointer = "→" if i == lineno else " "
                self._tw.line(f"  {pointer}{i:>4}  {text}")
            self._tw.line("")
        explanation = self._assertion_explanation(report)
        if explanation:
            self._tw.line("   Explanation:")
            for line in explanation:
                self._tw.line(f"     {line}")

    def _render_summary(self, exitstatus):
        total = sum(self.stats.values())
        duration = time.time() - self.session_start
        status_label = "✓ PASSED" if exitstatus == 0 else "✗ FAILED"
        status_color = {"green": exitstatus == 0, "red": exitstatus != 0, "bold": True}
        self._tw.line("")
        self._tw.line(f"RESULT   {status_label} (exit code {exitstatus})", **status_color)
        self._tw.line(f"TOTAL    {total} tests in {duration:.2f}s")
        self._tw.line(f"PASSED   {self.stats['passed']}")
        self._tw.line(f"FAILED   {self.stats['failed']}")
        self._tw.line(f"SKIPPED  {self.stats['skipped']}")
        self._tw.line(f"ERRORS   {self.stats['error']}")

    # --- utility helpers ---
    def _status_icon(self, status):
        if status in ("passed", "xpassed"):
            return "✓", {"green": True}
        if status in ("skipped", "xfailed"):
            return "∙", {"yellow": True}
        return "✗", {"red": True, "bold": True}

    def _failure_message(self, report):
        longrepr = getattr(report, "longrepr", None)
        if hasattr(longrepr, "reprcrash") and longrepr.reprcrash:
            return longrepr.reprcrash.message
        if hasattr(report, "longreprtext"):
            return report.longreprtext.splitlines()[0]
        return str(longrepr) if longrepr else "test failed"

    def _load_snippet(self, path, lineno, context=2):
        path = str(path)
        try:
            with open(path, "r", encoding="utf-8") as f:
                lines = f.readlines()
        except OSError:
            return None
        start = max(0, lineno - context - 1)
        end = min(len(lines), lineno + context)
        snippet = []
        for idx in range(start, end):
            text = lines[idx].rstrip("\n")
            snippet.append((idx + 1, text))
        return snippet

    def _failure_lineno(self, report):
        longrepr = getattr(report, "longrepr", None)
        if hasattr(longrepr, "reprcrash") and longrepr.reprcrash:
            return str(longrepr.reprcrash.path), longrepr.reprcrash.lineno
        path, lineno, _ = report.location
        return str(path), lineno + 1

    def _assertion_explanation(self, report):
        longrepr = getattr(report, "longrepr", None)
        summary = None
        if hasattr(longrepr, "reprcrash") and longrepr.reprcrash:
            summary = longrepr.reprcrash.message or ""
        if summary:
            lowered = summary.lower()
            if "did not raise" in lowered:
                expected = summary.split("DID NOT RAISE")[-1].strip()
                expected = expected or "expected exception"
                summary = f"Expected {expected} to be raised, but none was."
            elif "assert" in lowered and "==" in summary:
                parts = summary.split("==", 1)
                left = parts[0].replace("AssertionError:", "").replace("assert", "", 1).strip()
                right = parts[1].strip()
                summary = f"Expected: {right}"
                if left:
                    summary += f"\n     Actual:   {left}"
            else:
                summary = summary.replace("AssertionError:", "").strip()
        if not summary:
            return None
        parts = summary.split("\n")
        return [part for part in parts if part.strip()]


def pytest_configure(config):
    config.option.color = "yes"
    pm = config.pluginmanager
    reporter = PxTerminalReporter(config)
    default = pm.getplugin("terminalreporter")
    if default:
        pm.unregister(default)
        pm.register(reporter, "terminalreporter")
        config._px_reporter_registered = True
    else:
        config._px_reporter_registered = False
    config._px_reporter = reporter


def pytest_sessionstart(session):
    config = session.config
    reporter = getattr(config, "_px_reporter", None)
    if reporter is None:
        return
    if not getattr(config, "_px_reporter_registered", False):
        pm = config.pluginmanager
        default = pm.getplugin("terminalreporter")
        if default and default is not reporter:
            pm.unregister(default)
        pm.register(reporter, "terminalreporter")
        config._px_reporter_registered = True
reporter.pytest_sessionstart(session)
"#;
