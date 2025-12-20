// Moved from `run/mod.rs` to keep git-ref execution code cohesive.
use super::*;

struct CommitRunContext {
    py_ctx: PythonContext,
    manifest_path: PathBuf,
    deps: DependencyContext,
    lock: px_domain::api::LockSnapshot,
    profile_oid: String,
    env_root: PathBuf,
    site_packages: Option<PathBuf>,
    _temp_guard: Option<TempDir>,
}

pub(super) fn run_project_at_ref(
    ctx: &CommandContext,
    request: &RunRequest,
    git_ref: &str,
) -> Result<ExecutionOutcome> {
    let strict = true;
    let target = request
        .entry
        .clone()
        .or_else(|| request.target.clone())
        .unwrap_or_default();
    if ctx.global.json && request.interactive == Some(true) {
        return Ok(ExecutionOutcome::user_error(
            "--json requires non-interactive execution",
            json!({
                "code": crate::diag_commands::RUN,
                "reason": "json_interactive_conflict",
                "hint": "Drop `--interactive` or use `--non-interactive` when `--json` is set.",
            }),
        ));
    }
    let interactive = request
        .interactive
        .unwrap_or_else(|| !ctx.global.json && std::io::stdin().is_terminal() && std::io::stdout().is_terminal());
    let command_args = json!({
        "target": &target,
        "args": &request.args,
        "git_ref": git_ref,
    });

    let commit_ctx = match prepare_commit_python_context(ctx, git_ref) {
        Ok(value) => value,
        Err(outcome) => return Ok(outcome),
    };
    let mut sandbox: Option<SandboxRunContext> = None;
    let invocation_root = ctx.project_root().ok();
    let workdir = map_workdir(invocation_root.as_deref(), &commit_ctx.py_ctx.project_root);
    let mut command_args = command_args;
    commit_ctx.deps.inject(&mut command_args);

    if request.sandbox {
        match prepare_commit_sandbox(
            &commit_ctx.manifest_path,
            &commit_ctx.lock,
            &commit_ctx.profile_oid,
            &commit_ctx.env_root,
            commit_ctx.site_packages.as_deref(),
        ) {
            Ok(sbx) => sandbox = Some(sbx),
            Err(outcome) => return Ok(outcome),
        }
    }
    if target.trim().is_empty() {
        return Ok(run_target_required_outcome());
    }
    if let Some(inline) = match detect_inline_script(&target) {
        Ok(result) => result,
        Err(outcome) => return Ok(outcome),
    } {
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
    let plan = plan_run_target(
        &commit_ctx.py_ctx,
        &commit_ctx.manifest_path,
        &target,
        &workdir,
    )?;
    let host_runner = HostCommandRunner::new(ctx);
    let sandbox_runner = match sandbox {
        Some(ref mut sbx) => {
            let runner = match sandbox_runner_for_context(&commit_ctx.py_ctx, sbx, &workdir) {
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
    let outcome = match plan {
        RunTargetPlan::Script(path) => run_project_script(
            ctx,
            runner,
            &commit_ctx.py_ctx,
            &path,
            &request.args,
            &command_args,
            &workdir,
            interactive,
            if sandbox.is_some() {
                "python"
            } else {
                &commit_ctx.py_ctx.python
            },
        )?,
        RunTargetPlan::Executable(program) => run_executable(
            ctx,
            runner,
            &commit_ctx.py_ctx,
            &program,
            &request.args,
            &command_args,
            &workdir,
            interactive,
        )?,
    };
    let mut outcome = outcome;
    if let Some(ref sbx) = sandbox {
        attach_sandbox_details(&mut outcome, sbx);
    }
    Ok(outcome)
}

pub(super) fn run_tests_at_ref(
    ctx: &CommandContext,
    request: &TestRequest,
    git_ref: &str,
) -> Result<ExecutionOutcome> {
    let commit_ctx = match prepare_commit_python_context(ctx, git_ref) {
        Ok(value) => value,
        Err(outcome) => return Ok(outcome),
    };
    let _stdlib_guard = commit_stdlib_guard(ctx, git_ref);
    let invocation_root = ctx.project_root().ok();
    let workdir = map_workdir(invocation_root.as_deref(), &commit_ctx.py_ctx.project_root);
    let mut sandbox: Option<SandboxRunContext> = None;
    if request.sandbox {
        match prepare_commit_sandbox(
            &commit_ctx.manifest_path,
            &commit_ctx.lock,
            &commit_ctx.profile_oid,
            &commit_ctx.env_root,
            commit_ctx.site_packages.as_deref(),
        ) {
            Ok(sbx) => sandbox = Some(sbx),
            Err(outcome) => return Ok(outcome),
        }
    }
    let host_runner = HostCommandRunner::new(ctx);
    let sandbox_runner = match sandbox {
        Some(ref mut sbx) => {
            let runner = match sandbox_runner_for_context(&commit_ctx.py_ctx, sbx, &workdir) {
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
    let mut outcome =
        run_tests_for_context(ctx, runner, &commit_ctx.py_ctx, request, None, &workdir)?;
    if let Some(ref sbx) = sandbox {
        attach_sandbox_details(&mut outcome, sbx);
    }
    Ok(outcome)
}

fn prepare_commit_python_context(
    ctx: &CommandContext,
    git_ref: &str,
) -> Result<CommitRunContext, ExecutionOutcome> {
    let repo_root = git_repo_root()?;
    let scope = crate::workspace::discover_workspace_scope().map_err(|err| {
        ExecutionOutcome::failure(
            "failed to detect workspace for --at",
            json!({ "error": err.to_string() }),
        )
    })?;
    if let Some(crate::workspace::WorkspaceScope::Member {
        workspace,
        member_root,
    }) = scope
    {
        return commit_workspace_context(
            ctx,
            git_ref,
            &repo_root,
            &workspace.config.root,
            &member_root,
        );
    }
    commit_project_context(ctx, git_ref, &repo_root)
}

fn commit_project_context(
    ctx: &CommandContext,
    git_ref: &str,
    repo_root: &Path,
) -> Result<CommitRunContext, ExecutionOutcome> {
    let project_root = match ctx.project_root() {
        Ok(root) => root,
        Err(err) => {
            if is_missing_project_error(&err) {
                return Err(missing_project_outcome());
            }
            return Err(ExecutionOutcome::failure(
                "failed to resolve project root",
                json!({ "error": err.to_string() }),
            ));
        }
    };
    let rel_root = match project_root.strip_prefix(repo_root) {
        Ok(rel) => rel.to_path_buf(),
        Err(_) => {
            return Err(ExecutionOutcome::user_error(
                "px --at requires running inside a git repository",
                json!({
                    "reason": "project_outside_repo",
                    "project_root": project_root.display().to_string(),
                    "repo_root": repo_root.display().to_string(),
                }),
            ))
        }
    };
    let archive = materialize_ref_tree(repo_root, git_ref)?;
    let project_root_at_ref = archive.path().join(&rel_root);
    let manifest_rel = rel_root.join("pyproject.toml");
    let manifest_path = project_root_at_ref.join("pyproject.toml");
    let manifest_contents = fs::read_to_string(&manifest_path).map_err(|err| {
        ExecutionOutcome::user_error(
            "pyproject.toml not found at the requested git ref",
            json!({
                "git_ref": git_ref,
                "path": manifest_rel.display().to_string(),
                "reason": "pyproject_missing_at_ref",
                "error": err.to_string(),
            }),
        )
    })?;
    let manifest_doc: toml_edit::DocumentMut = manifest_contents
        .parse::<toml_edit::DocumentMut>()
        .map_err(|err| {
            ExecutionOutcome::failure(
                "failed to parse pyproject.toml from git ref",
                json!({
                    "git_ref": git_ref,
                    "path": manifest_rel.display().to_string(),
                    "error": err.to_string(),
                }),
            )
        })?;
    if !manifest_has_px(&manifest_doc) {
        return Err(ExecutionOutcome::user_error(
            "px project not found at the requested git ref",
            json!({
                "git_ref": git_ref,
                "path": manifest_rel.display().to_string(),
                "reason": "missing_px_metadata",
            }),
        ));
    }
    let snapshot = px_domain::api::ProjectSnapshot::from_document(
        &project_root_at_ref,
        &manifest_path,
        manifest_doc,
    )
    .map_err(|err| {
        ExecutionOutcome::failure(
            "failed to load pyproject.toml from git ref",
            json!({
                "git_ref": git_ref,
                "path": manifest_rel.display().to_string(),
                "error": err.to_string(),
            }),
        )
    })?;
    let lock_rel = rel_root.join("px.lock");
    let lock_path = project_root_at_ref.join("px.lock");
    let lock_contents = fs::read_to_string(&lock_path).map_err(|err| {
        ExecutionOutcome::user_error(
            "px.lock not found at the requested git ref",
            json!({
                "git_ref": git_ref,
                "path": lock_rel.display().to_string(),
                "reason": "px_lock_missing_at_ref",
                "error": err.to_string(),
            }),
        )
    })?;
    let lock = px_domain::api::parse_lockfile(&lock_contents).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to parse px.lock from git ref",
            json!({
                "git_ref": git_ref,
                "path": lock_rel.display().to_string(),
                "error": err.to_string(),
                "reason": "invalid_lock_at_ref",
            }),
        )
    })?;
    let marker_env = marker_env_for_snapshot(&snapshot);
    let lock_id = validate_lock_for_ref(
        &snapshot,
        &lock,
        &lock_contents,
        git_ref,
        &lock_rel,
        marker_env.as_ref(),
    )?;
    let runtime = detect_runtime_metadata(ctx, &snapshot).map_err(|err| {
        install_error_outcome(err, "python runtime unavailable for git-ref execution")
    })?;
    let owner_id =
        project_env_owner_id(&project_root, &lock_id, &runtime.version).map_err(|err| {
            ExecutionOutcome::failure(
                "failed to compute project environment identity",
                json!({ "error": err.to_string() }),
            )
        })?;
    let env_owner = OwnerId {
        owner_type: OwnerType::ProjectEnv,
        owner_id,
    };
    let cas_profile =
        ensure_profile_env(ctx, &snapshot, &lock, &runtime, &env_owner).map_err(|err| {
            install_error_outcome(err, "failed to prepare environment for git-ref execution")
        })?;
    let env_root = cas_profile.env_path.clone();
    let site_packages = discover_site_packages(&env_root);
    let paths = build_pythonpath(ctx.fs(), &project_root_at_ref, Some(env_root.clone())).map_err(
        |err| {
            ExecutionOutcome::failure(
                "failed to assemble PYTHONPATH for git-ref execution",
                json!({ "error": err.to_string() }),
            )
        },
    )?;
    let runtime_path = cas_profile.runtime_path.display().to_string();
    let python = select_python_from_site(&paths.site_bin, &runtime_path, &runtime.version);
    let deps = DependencyContext::from_sources(&snapshot.requirements, Some(&lock_path));
    let pyc_cache_prefix = if env::var_os("PYTHONPYCACHEPREFIX").is_some() {
        None
    } else {
        match crate::store::ensure_pyc_cache_prefix(&ctx.cache().path, &cas_profile.profile_oid) {
            Ok(prefix) => Some(prefix),
            Err(err) => {
                let prefix =
                    crate::store::pyc_cache_prefix(&ctx.cache().path, &cas_profile.profile_oid);
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
    };
    Ok(CommitRunContext {
        py_ctx: PythonContext {
            state_root: project_root_at_ref.clone(),
            project_root: project_root_at_ref,
            project_name: snapshot.name.clone(),
            python,
            pythonpath: paths.pythonpath,
            allowed_paths: paths.allowed_paths,
            site_bin: paths.site_bin,
            pep582_bin: paths.pep582_bin,
            pyc_cache_prefix,
            px_options: snapshot.px_options.clone(),
        },
        manifest_path,
        deps,
        lock: lock.clone(),
        profile_oid: cas_profile.profile_oid.clone(),
        env_root,
        site_packages,
        _temp_guard: Some(archive),
    })
}

fn commit_workspace_context(
    ctx: &CommandContext,
    git_ref: &str,
    repo_root: &Path,
    workspace_root: &Path,
    member_root: &Path,
) -> Result<CommitRunContext, ExecutionOutcome> {
    let workspace_rel = match workspace_root.strip_prefix(repo_root) {
        Ok(rel) => rel.to_path_buf(),
        Err(_) => {
            return Err(ExecutionOutcome::user_error(
                "px --at requires running inside a git repository",
                json!({
                    "reason": "workspace_outside_repo",
                    "workspace_root": workspace_root.display().to_string(),
                    "repo_root": repo_root.display().to_string(),
                }),
            ))
        }
    };
    let archive = materialize_ref_tree(repo_root, git_ref)?;
    let archive_root = archive.path().to_path_buf();
    let workspace_root_at_ref = archive_root.join(&workspace_rel);
    let workspace_manifest_rel = workspace_rel.join("pyproject.toml");
    let workspace_manifest_path = workspace_root_at_ref.join("pyproject.toml");
    let workspace_manifest = fs::read_to_string(&workspace_manifest_path).map_err(|err| {
        ExecutionOutcome::user_error(
            "workspace manifest not found at the requested git ref",
            json!({
                "git_ref": git_ref,
                "path": workspace_manifest_rel.display().to_string(),
                "reason": "workspace_manifest_missing_at_ref",
                "error": err.to_string(),
            }),
        )
    })?;
    let manifest_path = workspace_manifest_path.clone();
    let workspace_doc: toml_edit::DocumentMut = workspace_manifest
        .parse::<toml_edit::DocumentMut>()
        .map_err(|err| {
            ExecutionOutcome::failure(
                "failed to parse workspace manifest from git ref",
                json!({
                    "git_ref": git_ref,
                    "path": workspace_manifest_rel.display().to_string(),
                    "error": err.to_string(),
                }),
            )
        })?;
    if !px_domain::api::manifest_has_workspace(&workspace_doc) {
        return Err(ExecutionOutcome::user_error(
            "px workspace not found at the requested git ref",
            json!({
                "git_ref": git_ref,
                "path": workspace_manifest_rel.display().to_string(),
                "reason": "missing_workspace_metadata",
            }),
        ));
    }
    let mut workspace_config = px_domain::api::workspace_config_from_doc(
        &workspace_root_at_ref,
        &manifest_path,
        &workspace_doc,
    )
    .map_err(|err| {
        ExecutionOutcome::failure(
            "failed to load workspace manifest from git ref",
            json!({
                "git_ref": git_ref,
                "path": workspace_manifest_rel.display().to_string(),
                "error": err.to_string(),
            }),
        )
    })?;
    let member_rel = member_root
        .strip_prefix(workspace_root)
        .unwrap_or(member_root)
        .to_path_buf();
    let member_root_at_ref = workspace_root_at_ref.join(&member_rel);
    if !workspace_config
        .members
        .iter()
        .any(|member| member == &member_rel)
    {
        return Err(ExecutionOutcome::user_error(
            "current directory is not a workspace member at the requested git ref",
            json!({
                "git_ref": git_ref,
                "member": member_rel.display().to_string(),
                "reason": "workspace_member_not_found",
            }),
        ));
    }

    let mut members = Vec::new();
    for rel in &workspace_config.members {
        let abs_root = workspace_root_at_ref.join(rel);
        let member_manifest_rel = workspace_rel.join(rel).join("pyproject.toml");
        let manifest_path = abs_root.join("pyproject.toml");
        let contents = fs::read_to_string(&manifest_path).map_err(|err| {
            ExecutionOutcome::user_error(
                "workspace member manifest not found at git ref",
                json!({
                    "git_ref": git_ref,
                    "path": member_manifest_rel.display().to_string(),
                    "reason": "member_manifest_missing_at_ref",
                    "error": err.to_string(),
                }),
            )
        })?;
        let doc: toml_edit::DocumentMut =
            contents.parse::<toml_edit::DocumentMut>().map_err(|err| {
                ExecutionOutcome::failure(
                    "failed to parse workspace member manifest from git ref",
                    json!({
                        "git_ref": git_ref,
                        "path": member_manifest_rel.display().to_string(),
                        "error": err.to_string(),
                    }),
                )
            })?;
        if !manifest_has_px(&doc) {
            return Err(ExecutionOutcome::user_error(
                "px project not found in workspace member at git ref",
                json!({
                    "git_ref": git_ref,
                    "path": member_manifest_rel.display().to_string(),
                    "reason": "missing_px_metadata",
                }),
            ));
        }
        let snapshot =
            px_domain::api::ProjectSnapshot::from_document(&abs_root, &manifest_path, doc)
                .map_err(|err| {
                    ExecutionOutcome::failure(
                        "failed to load workspace member from git ref",
                        json!({
                            "git_ref": git_ref,
                            "path": member_manifest_rel.display().to_string(),
                            "error": err.to_string(),
                        }),
                    )
                })?;
        let rel_path = abs_root
            .strip_prefix(&workspace_root_at_ref)
            .unwrap_or(&abs_root)
            .display()
            .to_string();
        members.push(crate::workspace::WorkspaceMember {
            rel_path,
            root: abs_root,
            snapshot,
        });
    }
    if workspace_config.name.is_none() {
        workspace_config.name = workspace_root
            .file_name()
            .and_then(|s| s.to_str())
            .map(std::string::ToString::to_string);
    }
    let member_snapshot = members
        .iter()
        .find(|member| member.root == member_root_at_ref)
        .map(|member| member.snapshot.clone());
    let python_requirement = crate::workspace::derive_workspace_python(&workspace_config, &members)
        .map_err(|err| {
            ExecutionOutcome::failure(
                "workspace python requirement is inconsistent at git ref",
                json!({
                    "git_ref": git_ref,
                    "error": err.to_string(),
                }),
            )
        })?;
    let manifest_fingerprint = px_domain::api::workspace_manifest_fingerprint(
        &workspace_config,
        &members
            .iter()
            .map(|m| m.snapshot.clone())
            .collect::<Vec<_>>(),
    )
    .map_err(|err| {
        ExecutionOutcome::failure(
            "failed to fingerprint workspace manifest from git ref",
            json!({
                "git_ref": git_ref,
                "error": err.to_string(),
            }),
        )
    })?;
    let mut dependencies = Vec::new();
    for member in &members {
        dependencies.extend(member.snapshot.requirements.clone());
    }
    dependencies.retain(|dep| !dep.trim().is_empty());
    let workspace_name = workspace_config
        .name
        .clone()
        .or_else(|| {
            workspace_root
                .file_name()
                .and_then(|s| s.to_str())
                .map(std::string::ToString::to_string)
        })
        .or_else(|| {
            workspace_root_at_ref
                .file_name()
                .and_then(|s| s.to_str())
                .map(std::string::ToString::to_string)
        })
        .unwrap_or_else(|| "workspace".to_string());
    let px_options = px_domain::api::px_options_from_doc(&workspace_doc);
    let workspace_snapshot = crate::workspace::WorkspaceSnapshot {
        config: workspace_config.clone(),
        members,
        manifest_fingerprint,
        lock_path: workspace_root_at_ref.join("px.workspace.lock"),
        python_requirement,
        python_override: workspace_config.python.clone(),
        dependencies,
        name: workspace_name,
        px_options,
    };

    let lock_rel = workspace_rel.join("px.workspace.lock");
    let lock_path = workspace_root_at_ref.join("px.workspace.lock");
    let lock_contents = fs::read_to_string(&lock_path).map_err(|err| {
        ExecutionOutcome::user_error(
            "workspace lockfile not found at the requested git ref",
            json!({
                "git_ref": git_ref,
                "path": lock_rel.display().to_string(),
                "reason": "workspace_lock_missing_at_ref",
                "error": err.to_string(),
            }),
        )
    })?;
    let lock = px_domain::api::parse_lockfile(&lock_contents).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to parse px.workspace.lock from git ref",
            json!({
                "git_ref": git_ref,
                "path": lock_rel.display().to_string(),
                "error": err.to_string(),
                "reason": "invalid_lock_at_ref",
            }),
        )
    })?;
    let marker_env = ctx.marker_environment().ok();
    let lock_id = validate_lock_for_ref(
        &workspace_snapshot.lock_snapshot(),
        &lock,
        &lock_contents,
        git_ref,
        &lock_rel,
        marker_env.as_ref(),
    )?;
    let runtime =
        detect_runtime_metadata(ctx, &workspace_snapshot.lock_snapshot()).map_err(|err| {
            install_error_outcome(
                err,
                "python runtime unavailable for git-ref workspace execution",
            )
        })?;
    let owner_id =
        workspace_env_owner_id(workspace_root, &lock_id, &runtime.version).map_err(|err| {
            ExecutionOutcome::failure(
                "failed to compute workspace environment identity",
                json!({ "error": err.to_string() }),
            )
        })?;
    let env_owner = OwnerId {
        owner_type: OwnerType::WorkspaceEnv,
        owner_id,
    };
    let cas_profile = ensure_profile_env(
        ctx,
        &workspace_snapshot.lock_snapshot(),
        &lock,
        &runtime,
        &env_owner,
    )
    .map_err(|err| {
        install_error_outcome(
            err,
            "failed to prepare workspace environment for git-ref execution",
        )
    })?;
    let env_root = cas_profile.env_path.clone();
    let site_packages = discover_site_packages(&env_root);
    let paths =
        build_pythonpath(ctx.fs(), &member_root_at_ref, Some(env_root.clone())).map_err(|err| {
            ExecutionOutcome::failure(
                "failed to assemble PYTHONPATH for git-ref execution",
                json!({ "error": err.to_string() }),
            )
        })?;
    let mut combined = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut push_unique = |path: PathBuf| {
        if seen.insert(path.clone()) {
            combined.push(path);
        }
    };
    let current_src = member_root_at_ref.join("src");
    if current_src.exists() {
        push_unique(current_src);
    }
    push_unique(member_root_at_ref.to_path_buf());
    for member in &workspace_snapshot.config.members {
        let abs = workspace_snapshot.config.root.join(member);
        let src = abs.join("src");
        if src.exists() {
            push_unique(src);
        }
        push_unique(abs);
    }
    for path in &paths.allowed_paths {
        push_unique(path.clone());
    }
    let pythonpath = env::join_paths(&combined)
        .map_err(|err| {
            ExecutionOutcome::failure(
                "failed to assemble PYTHONPATH for git-ref execution",
                json!({ "error": err.to_string() }),
            )
        })?
        .into_string()
        .map_err(|_| {
            ExecutionOutcome::failure(
                "failed to assemble PYTHONPATH for git-ref execution",
                json!({ "error": "contains non-utf8 data" }),
            )
        })?;
    let runtime_path = cas_profile.runtime_path.display().to_string();
    let python = select_python_from_site(&paths.site_bin, &runtime_path, &runtime.version);
    let px_options = member_snapshot
        .as_ref()
        .map(|member| member.px_options.clone())
        .unwrap_or_default();
    let project_name = member_snapshot
        .as_ref()
        .map(|member| member.name.clone())
        .unwrap_or_else(|| {
            member_root_at_ref
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_string()
        });
    let deps = DependencyContext::from_sources(&workspace_snapshot.dependencies, Some(&lock_path));
    let pyc_cache_prefix = if env::var_os("PYTHONPYCACHEPREFIX").is_some() {
        None
    } else {
        match crate::store::ensure_pyc_cache_prefix(&ctx.cache().path, &cas_profile.profile_oid) {
            Ok(prefix) => Some(prefix),
            Err(err) => {
                let prefix =
                    crate::store::pyc_cache_prefix(&ctx.cache().path, &cas_profile.profile_oid);
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
    };
    Ok(CommitRunContext {
        py_ctx: PythonContext {
            project_root: member_root_at_ref.clone(),
            state_root: member_root_at_ref.clone(),
            project_name,
            python,
            pythonpath,
            allowed_paths: combined,
            site_bin: paths.site_bin,
            pep582_bin: paths.pep582_bin,
            pyc_cache_prefix,
            px_options,
        },
        manifest_path: member_root_at_ref.join("pyproject.toml"),
        deps,
        lock: lock.clone(),
        profile_oid: cas_profile.profile_oid.clone(),
        env_root,
        site_packages,
        _temp_guard: Some(archive),
    })
}
