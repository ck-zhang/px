use std::path::{Path, PathBuf};
use std::fs;

use anyhow::Result as AnyhowResult;
use serde_json::json;
use toml_edit::DocumentMut;

use super::execution_plan;
use super::run::{
    git_repo_root, install_error_outcome, materialize_ref_tree, parse_run_reference_target,
    validate_lock_for_ref,
};
use crate::tooling::run_target_required_outcome;
use crate::{marker_env_for_snapshot, CommandContext, ExecutionOutcome, RunRequest};
use px_domain::ProjectSnapshot;

fn sh_quote(raw: &str) -> String {
    if raw.is_empty() {
        return "''".to_string();
    }
    let safe = raw.chars().all(|ch| {
        ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '/' | ':' | '@' | '+')
    });
    if safe {
        raw.to_string()
    } else {
        let escaped = raw.replace('\'', "'\"'\"'");
        format!("'{escaped}'")
    }
}

fn render_plan_human(plan: &execution_plan::ExecutionPlan, verbose: u8) -> String {
    let mut lines = Vec::new();
    match &plan.provenance.source {
        execution_plan::SourceProvenance::WorkingTree => lines.push("source: working_tree".to_string()),
        execution_plan::SourceProvenance::GitRef {
            git_ref,
            manifest_repo_path,
            ..
        } => lines.push(format!("source: git_ref {git_ref} ({manifest_repo_path})")),
        execution_plan::SourceProvenance::RepoSnapshot {
            locator,
            git_ref,
            commit,
            repo_snapshot_oid,
            script_repo_path,
        } => {
            let mut line = match (commit.as_deref(), git_ref.as_deref()) {
                (Some(commit), _) => {
                    format!("source: repo_snapshot {locator}@{commit}:{script_repo_path}")
                }
                (None, Some(git_ref)) => {
                    format!("source: repo_snapshot {locator}@{git_ref}:{script_repo_path}")
                }
                (None, None) => format!("source: repo_snapshot {locator}:{script_repo_path}"),
            };
            if verbose > 0 {
                if let Some(oid) = repo_snapshot_oid.as_deref() {
                    line.push_str(&format!(" (oid={oid})"));
                }
            }
            lines.push(line);
        }
    }
    let mut engine = plan.engine.mode.to_string();
    if verbose > 0 {
        if let Some(code) = plan.engine.fallback_reason_code.as_deref() {
            engine = format!("{engine} (fallback={code})");
        }
    }
    lines.push(format!("engine: {engine}"));
    if let Some(version) = plan.runtime.python_version.as_deref() {
        if let Some(abi) = plan.runtime.python_abi.as_deref() {
            lines.push(format!("runtime: {} (version={version} abi={abi})", plan.runtime.executable));
        } else {
            lines.push(format!("runtime: {} (version={version})", plan.runtime.executable));
        }
    } else {
        lines.push(format!("runtime: {}", plan.runtime.executable));
    }
    if let Some(profile) = plan.lock_profile.profile_oid.as_deref() {
        lines.push(format!("profile_oid: {profile}"));
    }
    if let Some(lock_id) = plan.lock_profile.l_id.as_deref().or(plan.lock_profile.wl_id.as_deref()) {
        lines.push(format!("lock_id: {lock_id}"));
    }
    lines.push(format!("workdir: {}", plan.working_dir));
    lines.push(format!(
        "argv: {}",
        plan.target_resolution
            .argv
            .iter()
            .map(|s| sh_quote(s))
            .collect::<Vec<_>>()
            .join(" ")
    ));
    if plan.sys_path.summary.count > 0 {
        lines.push(format!("sys.path: {} entries", plan.sys_path.summary.count));
    }
    if plan.would_repair_env {
        lines.push("would_repair_env: true (run `px sync`)".to_string());
    }
    lines.join("\n")
}

fn manifest_has_px(doc: &DocumentMut) -> bool {
    doc.get("tool")
        .and_then(toml_edit::Item::as_table)
        .and_then(|tool| tool.get("px"))
        .is_some()
}

fn map_workdir(invocation_root: Option<&Path>, context_root: &Path) -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| context_root.to_path_buf());
    if let Some(root) = invocation_root {
        if let Ok(rel) = cwd.strip_prefix(root) {
            return context_root.join(rel);
        }
    }
    if cwd.starts_with(context_root) {
        cwd
    } else {
        context_root.to_path_buf()
    }
}

fn is_python_alias(target: &str) -> bool {
    let lower = target.to_ascii_lowercase();
    lower == "python"
        || lower == "python3"
        || lower.starts_with("python3.")
        || lower == "py"
        || lower == "py3"
}

fn detect_script_under_root(root: &Path, cwd: &Path, target: &str) -> Option<PathBuf> {
    fn resolve(base: &Path, project_root: &Path, target: &str) -> Option<PathBuf> {
        let candidate = if Path::new(target).is_absolute() {
            PathBuf::from(target)
        } else {
            base.join(target)
        };
        let canonical = candidate.canonicalize().ok()?;
        if canonical.starts_with(project_root) && canonical.is_file() {
            Some(canonical)
        } else {
            None
        }
    }
    resolve(root, root, target)
        .or_else(|| resolve(cwd, root, target))
        .filter(|path| path.starts_with(root))
}

fn runtime_plan_for_executable(executable: &str, python_version: Option<String>) -> execution_plan::RuntimePlan {
    let tags = crate::python_sys::detect_interpreter_tags(executable).ok();
    let python_abi = tags
        .as_ref()
        .and_then(|t| t.abi.first().cloned())
        .or_else(|| tags.as_ref().and_then(|t| t.supported.first().map(|t| t.abi.clone())));
    execution_plan::RuntimePlan {
        python_version,
        python_abi,
        runtime_oid: None,
        executable: executable.to_string(),
    }
}

fn plan_default_target_resolution(
    runtime_executable: &str,
    target: &str,
    args: &[String],
) -> execution_plan::TargetResolution {
    if is_python_alias(target) {
        if args.len() >= 2 && args[0] == "-m" {
            let module = args[1].clone();
            let mut argv = Vec::with_capacity(args.len() + 1);
            argv.push(runtime_executable.to_string());
            argv.extend(args.iter().cloned());
            return execution_plan::TargetResolution {
                kind: execution_plan::TargetKind::Module,
                resolved: module,
                argv,
            };
        }
        let mut argv = Vec::with_capacity(args.len() + 1);
        argv.push(runtime_executable.to_string());
        argv.extend(args.iter().cloned());
        return execution_plan::TargetResolution {
            kind: execution_plan::TargetKind::Python,
            resolved: runtime_executable.to_string(),
            argv,
        };
    }

    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(target.to_string());
    argv.extend(args.iter().cloned());
    execution_plan::TargetResolution {
        kind: execution_plan::TargetKind::Executable,
        resolved: target.to_string(),
        argv,
    }
}

fn strict_for_request(ctx: &CommandContext, request: &RunRequest) -> bool {
    request.frozen || ctx.env_flag_enabled("CI")
}

fn normalize_pinned_commit(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let normalized = trimmed.to_ascii_lowercase();
    let len = normalized.len();
    if !(len == 40 || len == 64) {
        return None;
    }
    if !normalized.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return None;
    }
    Some(normalized)
}

fn plan_run_by_reference(
    ctx: &CommandContext,
    request: &RunRequest,
    reference: &super::run::RunReferenceTarget,
    strict: bool,
) -> std::result::Result<execution_plan::ExecutionPlan, ExecutionOutcome> {
    if request.at.is_some() {
        return Err(ExecutionOutcome::user_error(
            "px run <ref>:<script> does not support --at",
            json!({
                "reason": "run_reference_at_ref_unsupported",
                "hint": "remove --at and pin the repository commit in the run target instead",
            }),
        ));
    }
    if request.sandbox {
        return Err(ExecutionOutcome::user_error(
            "px run <ref>:<script> does not support --sandbox",
            json!({
                "reason": "run_reference_sandbox_unsupported",
                "hint": "omit --sandbox for run-by-reference targets",
            }),
        ));
    }

    // Do not resolve floating refs or fetch snapshots; only report pinned commits and cached oids.
    let ref_value = reference
        .git_ref
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string());
    let commit = ref_value
        .as_deref()
        .and_then(normalize_pinned_commit);
    let git_ref = match (&commit, &ref_value) {
        (Some(_), _) => None,
        (None, Some(value)) => Some(value.clone()),
        (None, None) => None,
    };

    if commit.is_none() {
        let raw_ref = ref_value.as_deref().unwrap_or_default().trim();
        if !request.allow_floating {
            if !raw_ref.is_empty()
                && raw_ref.chars().all(|ch| ch.is_ascii_hexdigit())
                && raw_ref.len() != 40
                && raw_ref.len() != 64
            {
                return Err(ExecutionOutcome::user_error(
                    "run-by-reference requires a full commit SHA",
                    json!({
                        "reason": "run_reference_requires_full_sha",
                        "ref": raw_ref,
                        "hint": "use a full 40-character commit SHA (example: git rev-parse HEAD)",
                        "recommendation": {
                            "command": "px run --allow-floating <TARGET> [-- args...]",
                            "hint": "use --allow-floating to resolve a short SHA or branch/tag at runtime",
                        }
                    }),
                ));
            }
            return Err(ExecutionOutcome::user_error(
                "run-by-reference requires a pinned commit SHA",
                json!({
                    "reason": "run_reference_requires_pin",
                    "hint": "add @<sha> to the repo reference, or pass --allow-floating to resolve a branch/tag at runtime",
                    "recommendation": {
                        "command": "px run --allow-floating <TARGET> [-- args...]",
                        "hint": "floating refs are refused under --frozen or CI=1",
                    }
                }),
            ));
        }
        if strict {
            return Err(ExecutionOutcome::user_error(
                "floating git refs are disabled under --frozen or CI=1",
                json!({
                    "reason": "run_reference_floating_disallowed",
                    "hint": "pin a full commit SHA in the run target (use @<sha>)",
                }),
            ));
        }
        if !ctx.is_online() {
            return Err(ExecutionOutcome::user_error(
                "floating git refs require online mode",
                json!({
                    "reason": "run_reference_offline_floating",
                    "hint": "re-run with --online / set PX_ONLINE=1, or pin a full commit SHA",
                }),
            ));
        }
    }

    let store = crate::store::cas::global_store();
    let repo_snapshot_oid = commit
        .as_deref()
        .and_then(|commit| {
            let spec = crate::RepoSnapshotSpec {
                locator: reference.locator.clone(),
                commit: commit.to_string(),
                subdir: None,
            };
            store.lookup_repo_snapshot_oid(&spec).ok().flatten()
        });

    let runtime_exe = ctx
        .python_runtime()
        .detect_interpreter()
        .unwrap_or_else(|_| "python".to_string());
    let runtime = runtime_plan_for_executable(&runtime_exe, None);

    let mut argv = Vec::with_capacity(request.args.len() + 2);
    argv.push(runtime_exe.clone());
    argv.push(reference.script_path.display().to_string());
    argv.extend(request.args.iter().cloned());
    let target_resolution = execution_plan::TargetResolution {
        kind: execution_plan::TargetKind::File,
        resolved: reference.script_path.display().to_string(),
        argv,
    };

    Ok(execution_plan::ExecutionPlan {
        schema_version: 1,
        context: execution_plan::PlanContext::UrlRun {
            locator: reference.locator.clone(),
            git_ref: git_ref.clone(),
            commit: commit.clone(),
            script_repo_path: reference.script_path.display().to_string(),
        },
        runtime,
        lock_profile: execution_plan::LockProfilePlan {
            l_id: None,
            wl_id: None,
            tool_lock_id: None,
            profile_oid: None,
            env_id: None,
        },
        engine: execution_plan::EnginePlan {
            mode: execution_plan::EngineMode::MaterializedEnv,
            fallback_reason_code: None,
        },
        target_resolution,
        working_dir: "<repo_snapshot_root>".to_string(),
        sys_path: execution_plan::SysPathPlan {
            entries: Vec::new(),
            summary: execution_plan::SysPathSummary {
                first: Vec::new(),
                count: 0,
            },
        },
        provenance: execution_plan::ProvenancePlan {
            sandbox: execution_plan::SandboxPlan {
                enabled: false,
                sbx_id: None,
                base: None,
                capabilities: Vec::new(),
            },
            source: execution_plan::SourceProvenance::RepoSnapshot {
                locator: reference.locator.clone(),
                git_ref,
                commit,
                repo_snapshot_oid,
                script_repo_path: reference.script_path.display().to_string(),
            },
        },
        would_repair_env: !strict,
    })
}

fn plan_run_at_ref(
    ctx: &CommandContext,
    request: &RunRequest,
    git_ref: &str,
    target: &str,
) -> std::result::Result<execution_plan::ExecutionPlan, ExecutionOutcome> {
    let repo_root = git_repo_root()?;
    let scope = crate::workspace::discover_workspace_scope().map_err(|err| {
        ExecutionOutcome::failure(
            "failed to detect workspace for --at",
            json!({ "error": err.to_string() }),
        )
    })?;
    let archive = materialize_ref_tree(&repo_root, git_ref)?;
    let archive_root = archive.path().to_path_buf();

    match scope {
        Some(crate::workspace::WorkspaceScope::Member {
            workspace,
            member_root,
        }) => {
            let workspace_root = workspace.config.root.clone();
            let workspace_rel = workspace_root.strip_prefix(&repo_root).map_err(|_| {
                ExecutionOutcome::user_error(
                    "px --at requires running inside a git repository",
                    json!({
                        "reason": "workspace_outside_repo",
                        "workspace_root": workspace_root.display().to_string(),
                        "repo_root": repo_root.display().to_string(),
                    }),
                )
            })?;
            let workspace_root_at_ref = archive_root.join(workspace_rel);
            let workspace_at_ref = crate::workspace::load_workspace_snapshot(&workspace_root_at_ref)
                .map_err(|err| {
                    ExecutionOutcome::failure(
                        "failed to load workspace manifest from git ref",
                        json!({
                            "git_ref": git_ref,
                            "error": err.to_string(),
                        }),
                    )
                })?;
            let member_rel = member_root
                .strip_prefix(&workspace_root)
                .unwrap_or(&member_root)
                .to_path_buf();
            if !workspace_at_ref
                .config
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

            let lock_rel = workspace_rel.join("px.workspace.lock");
            let lock_path = workspace_root_at_ref.join("px.workspace.lock");
            let lock_contents = std::fs::read_to_string(&lock_path).map_err(|err| {
                ExecutionOutcome::user_error(
                    "workspace lockfile not found at the requested git ref",
                    json!({
                        "git_ref": git_ref,
                        "path": lock_rel.display().to_string(),
                        "reason": "px_lock_missing_at_ref",
                        "error": err.to_string(),
                    }),
                )
            })?;
            let lock = px_domain::parse_lockfile(&lock_contents).map_err(|err| {
                ExecutionOutcome::failure(
                    "failed to parse workspace lockfile from git ref",
                    json!({
                        "git_ref": git_ref,
                        "path": lock_rel.display().to_string(),
                        "error": err.to_string(),
                        "reason": "invalid_lock_at_ref",
                    }),
                )
            })?;
            let marker_env = ctx.marker_environment().ok();
            let snapshot = workspace_at_ref.lock_snapshot();
            let lock_id = validate_lock_for_ref(
                &snapshot,
                &lock,
                &lock_contents,
                git_ref,
                &lock_rel,
                marker_env.as_ref(),
            )?;
            let _ = crate::prepare_project_runtime(&snapshot)
                .map_err(|err| install_error_outcome(err, "python runtime unavailable for git-ref execution"))?;
            let runtime = crate::detect_runtime_metadata(ctx, &snapshot)
                .map_err(|err| install_error_outcome(err, "python runtime unavailable for git-ref execution"))?;

            let runtime_plan = runtime_plan_for_executable(&runtime.path, Some(runtime.version.clone()));
            let workdir = map_workdir(Some(&member_root), &workspace_root_at_ref.join(&member_rel));
            let target_resolution = if let Some(script) =
                detect_script_under_root(&workspace_root_at_ref.join(&member_rel), &workdir, target)
            {
                let rel = script.strip_prefix(&archive_root).unwrap_or(&script);
                let resolved = format!("{git_ref}:{}", rel.display());
                let mut argv = Vec::with_capacity(request.args.len() + 2);
                argv.push(runtime.path.clone());
                argv.push(resolved.clone());
                argv.extend(request.args.iter().cloned());
                execution_plan::TargetResolution {
                    kind: execution_plan::TargetKind::File,
                    resolved,
                    argv,
                }
            } else {
                plan_default_target_resolution(&runtime.path, target, &request.args)
            };

            let manifest_path = workspace_root_at_ref.join(&member_rel).join("pyproject.toml");
            let workspace_lock = lock.workspace.as_ref();
            let sandbox_plan = execution_plan::sandbox_plan(
                &manifest_path,
                request.sandbox,
                Some(&lock),
                workspace_lock,
                None,
                None,
            )?;

            let workdir_rel = workdir.strip_prefix(&archive_root).unwrap_or(&workdir);
            Ok(execution_plan::ExecutionPlan {
                schema_version: 1,
                context: execution_plan::PlanContext::Workspace {
                    workspace_root: workspace_root.display().to_string(),
                    workspace_manifest: workspace_root.join("pyproject.toml").display().to_string(),
                    workspace_lock_path: workspace_root.join("px.workspace.lock").display().to_string(),
                    member_root: member_root.display().to_string(),
                    member_manifest: member_root.join("pyproject.toml").display().to_string(),
                },
                runtime: runtime_plan,
                lock_profile: execution_plan::LockProfilePlan {
                    l_id: None,
                    wl_id: Some(lock_id.clone()),
                    tool_lock_id: None,
                    profile_oid: None,
                    env_id: super::cas_env::workspace_env_owner_id(
                        &workspace_root,
                        &lock_id,
                        &runtime.version,
                    )
                    .ok(),
                },
                engine: execution_plan::EnginePlan {
                    mode: execution_plan::EngineMode::MaterializedEnv,
                    fallback_reason_code: None,
                },
                target_resolution,
                working_dir: format!("{git_ref}:{}", workdir_rel.display()),
                sys_path: execution_plan::SysPathPlan {
                    entries: Vec::new(),
                    summary: execution_plan::SysPathSummary {
                        first: Vec::new(),
                        count: 0,
                    },
                },
                provenance: execution_plan::ProvenancePlan {
                    sandbox: sandbox_plan,
                    source: execution_plan::SourceProvenance::GitRef {
                        git_ref: git_ref.to_string(),
                        repo_root: repo_root.display().to_string(),
                        manifest_repo_path: workspace_rel.join("pyproject.toml").display().to_string(),
                        lock_repo_path: lock_rel.display().to_string(),
                    },
                },
                would_repair_env: true,
            })
        }
        _ => {
            let project_root = ctx.project_root().map_err(|err| {
                if crate::is_missing_project_error(&err) {
                    crate::missing_project_outcome()
                } else {
                    ExecutionOutcome::failure(
                        "failed to resolve project root",
                        json!({ "error": err.to_string() }),
                    )
                }
            })?;
            let rel_root = project_root.strip_prefix(&repo_root).map_err(|_| {
                ExecutionOutcome::user_error(
                    "px --at requires running inside a git repository",
                    json!({
                        "reason": "project_outside_repo",
                        "project_root": project_root.display().to_string(),
                        "repo_root": repo_root.display().to_string(),
                    }),
                )
            })?;
            let project_root_at_ref = archive_root.join(rel_root);
            let manifest_rel = rel_root.join("pyproject.toml");
            let manifest_path = project_root_at_ref.join("pyproject.toml");
            let manifest_contents = std::fs::read_to_string(&manifest_path).map_err(|err| {
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
            let manifest_doc: DocumentMut = manifest_contents.parse::<DocumentMut>().map_err(|err| {
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
            let snapshot = ProjectSnapshot::from_document(
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
            let lock_contents = std::fs::read_to_string(&lock_path).map_err(|err| {
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
            let lock = px_domain::parse_lockfile(&lock_contents).map_err(|err| {
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
            let _ = crate::prepare_project_runtime(&snapshot)
                .map_err(|err| install_error_outcome(err, "python runtime unavailable for git-ref execution"))?;
            let runtime = crate::detect_runtime_metadata(ctx, &snapshot)
                .map_err(|err| install_error_outcome(err, "python runtime unavailable for git-ref execution"))?;
            let runtime_plan = runtime_plan_for_executable(&runtime.path, Some(runtime.version.clone()));
            let workdir_fs = map_workdir(Some(&project_root), &project_root_at_ref);
            let workdir_rel = workdir_fs.strip_prefix(&archive_root).unwrap_or(&workdir_fs);
            let target_resolution = if let Some(script) =
                detect_script_under_root(&project_root_at_ref, &workdir_fs, target)
            {
                let rel = script.strip_prefix(&archive_root).unwrap_or(&script);
                let resolved = format!("{git_ref}:{}", rel.display());
                let mut argv = Vec::with_capacity(request.args.len() + 2);
                argv.push(runtime.path.clone());
                argv.push(resolved.clone());
                argv.extend(request.args.iter().cloned());
                execution_plan::TargetResolution {
                    kind: execution_plan::TargetKind::File,
                    resolved,
                    argv,
                }
            } else {
                plan_default_target_resolution(&runtime.path, target, &request.args)
            };
            let sandbox_plan = execution_plan::sandbox_plan(
                &manifest_path,
                request.sandbox,
                Some(&lock),
                None,
                None,
                None,
            )?;
            Ok(execution_plan::ExecutionPlan {
                schema_version: 1,
                context: execution_plan::PlanContext::Project {
                    project_root: project_root.display().to_string(),
                    manifest_path: project_root.join("pyproject.toml").display().to_string(),
                    lock_path: project_root.join("px.lock").display().to_string(),
                    project_name: snapshot.name.clone(),
                },
                runtime: runtime_plan,
                lock_profile: execution_plan::LockProfilePlan {
                    l_id: Some(lock_id.clone()),
                    wl_id: None,
                    tool_lock_id: None,
                    profile_oid: None,
                    env_id: super::cas_env::project_env_owner_id(
                        &project_root,
                        &lock_id,
                        &runtime.version,
                    )
                    .ok(),
                },
                engine: execution_plan::EnginePlan {
                    mode: execution_plan::EngineMode::MaterializedEnv,
                    fallback_reason_code: None,
                },
                target_resolution,
                working_dir: format!("{git_ref}:{}", workdir_rel.display()),
                sys_path: execution_plan::SysPathPlan {
                    entries: Vec::new(),
                    summary: execution_plan::SysPathSummary {
                        first: Vec::new(),
                        count: 0,
                    },
                },
                provenance: execution_plan::ProvenancePlan {
                    sandbox: sandbox_plan,
                    source: execution_plan::SourceProvenance::GitRef {
                        git_ref: git_ref.to_string(),
                        repo_root: repo_root.display().to_string(),
                        manifest_repo_path: manifest_rel.display().to_string(),
                        lock_repo_path: lock_rel.display().to_string(),
                    },
                },
                would_repair_env: true,
            })
        }
    }
}

pub fn explain_run(ctx: &CommandContext, request: &RunRequest) -> AnyhowResult<ExecutionOutcome> {
    let target = request
        .entry
        .clone()
        .or_else(|| request.target.clone())
        .unwrap_or_default();
    if target.trim().is_empty() {
        return Ok(run_target_required_outcome());
    }
    let strict = strict_for_request(ctx, request);

    if !target.trim().is_empty() {
        let reference = match parse_run_reference_target(&target) {
            Ok(reference) => reference,
            Err(outcome) => return Ok(outcome),
        };
        if let Some(reference) = reference {
            let plan = match plan_run_by_reference(ctx, request, &reference, strict) {
                Ok(plan) => plan,
                Err(outcome) => return Ok(outcome),
            };
            let details = serde_json::to_value(&plan)?;
            let message = render_plan_human(&plan, ctx.global.verbose);
            return Ok(ExecutionOutcome::success(message, details));
        }
    }

    if let Some(at_ref) = request.at.as_deref() {
        let plan = match plan_run_at_ref(ctx, request, at_ref, &target) {
            Ok(plan) => plan,
            Err(outcome) => return Ok(outcome),
        };
        let details = serde_json::to_value(&plan)?;
        let message = render_plan_human(&plan, ctx.global.verbose);
        return Ok(ExecutionOutcome::success(message, details));
    }

    let plan = match execution_plan::plan_run_execution(
        ctx,
        strict,
        request.sandbox,
        &target,
        &request.args,
    ) {
        Ok(plan) => plan,
        Err(outcome) => return Ok(outcome),
    };
    let details = serde_json::to_value(&plan)?;
    let message = render_plan_human(&plan, ctx.global.verbose);
    Ok(ExecutionOutcome::success(message, details))
}

pub fn explain_entrypoint(
    ctx: &CommandContext,
    name: &str,
) -> AnyhowResult<ExecutionOutcome> {
    let name = name.trim();
    if name.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            "entrypoint name is required",
            json!({
                "reason": "missing_entrypoint_name",
                "hint": "provide a console script name (example: `px explain entrypoint ruff`)",
            }),
        ));
    }

    let strict = ctx.env_flag_enabled("CI");
    let plan = match execution_plan::plan_run_execution(ctx, strict, false, "python", &[]) {
        Ok(plan) => plan,
        Err(outcome) => return Ok(outcome),
    };
    if plan.sys_path.entries.is_empty() {
        let mut details = json!({
            "reason": "missing_profile",
            "entrypoint": name,
        });
        if plan.would_repair_env {
            details["hint"] = json!("run `px sync` to build the environment before resolving entrypoints");
        }
        return Ok(ExecutionOutcome::user_error(
            "environment profile is not available",
            details,
        ));
    }

    #[derive(Clone, Debug)]
    struct Candidate {
        dist: String,
        version: Option<String>,
        entry_point: String,
        pkg_build_oid: Option<String>,
    }

    fn pkg_build_oid_from_sys_path(path: &Path) -> Option<String> {
        const PKG_BUILDS_DIR: &str = "pkg-builds";
        let mut iter = path.components().peekable();
        while let Some(comp) = iter.next() {
            let part = comp.as_os_str().to_string_lossy();
            if part == PKG_BUILDS_DIR {
                if let Some(next) = iter.next() {
                    let oid = next.as_os_str().to_string_lossy().to_string();
                    if !oid.is_empty() {
                        return Some(oid);
                    }
                }
            }
        }
        None
    }

    fn parse_console_scripts(contents: &str) -> Vec<(String, String)> {
        let mut in_console_scripts = false;
        let mut scripts = Vec::new();
        for line in contents.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
                continue;
            }
            if trimmed.starts_with('[') && trimmed.ends_with(']') {
                let section = &trimmed[1..trimmed.len() - 1];
                in_console_scripts = section.trim() == "console_scripts";
                continue;
            }
            if !in_console_scripts {
                continue;
            }
            let Some((key, value)) = trimmed.split_once('=') else {
                continue;
            };
            let key = key.trim();
            let value = value.trim();
            if key.is_empty() || value.is_empty() {
                continue;
            }
            scripts.push((key.to_string(), value.to_string()));
        }
        scripts
    }

    fn read_dist_metadata_name_version(dist_info: &Path) -> (String, Option<String>) {
        let metadata = dist_info.join("METADATA");
        let mut name = None;
        let mut version = None;
        if let Ok(contents) = fs::read_to_string(&metadata) {
            for line in contents.lines() {
                if name.is_none() {
                    if let Some(value) = line.strip_prefix("Name:") {
                        let trimmed = value.trim();
                        if !trimmed.is_empty() {
                            name = Some(trimmed.to_string());
                        }
                    }
                }
                if version.is_none() {
                    if let Some(value) = line.strip_prefix("Version:") {
                        let trimmed = value.trim();
                        if !trimmed.is_empty() {
                            version = Some(trimmed.to_string());
                        }
                    }
                }
                if name.is_some() && version.is_some() {
                    break;
                }
            }
        }
        let fallback = dist_info
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_string();
        (name.unwrap_or(fallback), version)
    }

    let mut candidates = Vec::<Candidate>::new();
    for entry in &plan.sys_path.entries {
        let sys_path = PathBuf::from(entry);
        if !sys_path.is_dir() {
            continue;
        }
        let pkg_build_oid = pkg_build_oid_from_sys_path(&sys_path);
        let Ok(entries) = fs::read_dir(&sys_path) else {
            continue;
        };
        let mut dist_infos = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("dist-info"))
                && path.is_dir()
            {
                dist_infos.push(path);
            }
        }
        dist_infos.sort();
        for dist_info in dist_infos {
            let entry_points = dist_info.join("entry_points.txt");
            if !entry_points.exists() {
                continue;
            }
            let contents = match fs::read_to_string(&entry_points) {
                Ok(contents) => contents,
                Err(_) => continue,
            };
            for (script, value) in parse_console_scripts(&contents) {
                if script != name {
                    continue;
                }
                let (dist, version) = read_dist_metadata_name_version(&dist_info);
                candidates.push(Candidate {
                    dist,
                    version,
                    entry_point: value,
                    pkg_build_oid: pkg_build_oid.clone(),
                });
            }
        }
    }

    candidates.sort_by(|a, b| {
        a.dist
            .cmp(&b.dist)
            .then(a.version.cmp(&b.version))
            .then(a.pkg_build_oid.cmp(&b.pkg_build_oid))
            .then(a.entry_point.cmp(&b.entry_point))
    });

    if candidates.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            format!("console script `{name}` not found in the current environment"),
            json!({
                "schema_version": 1,
                "reason": "entrypoint_not_found",
                "entrypoint": name,
                "hint": "check the entrypoint name, or run `px sync` to ensure the environment is up to date",
            }),
        ));
    }

    if candidates.len() > 1 {
        let rendered = candidates
            .iter()
            .map(|candidate| {
                json!({
                    "distribution": &candidate.dist,
                    "version": candidate.version.as_deref(),
                    "entry_point": &candidate.entry_point,
                    "pkg_build_oid": candidate.pkg_build_oid.as_deref(),
                })
            })
            .collect::<Vec<_>>();
        return Ok(ExecutionOutcome::user_error(
            format!("console script `{name}` is provided by multiple distributions"),
            json!({
                "schema_version": 1,
                "reason": "ambiguous_console_script",
                "entrypoint": name,
                "candidates": rendered,
                "hint": "Remove one of the distributions providing this script, or run a module directly via `px run python -m <module>`.",
            }),
        ));
    }

    let candidate = candidates
        .first()
        .expect("single candidate")
        .clone();
    let details = json!({
        "schema_version": 1,
        "entrypoint": name,
        "provider": {
            "distribution": candidate.dist,
            "version": candidate.version,
            "pkg_build_oid": candidate.pkg_build_oid,
        },
        "target": {
            "entry_point": candidate.entry_point,
        }
    });
    let dist = details["provider"]["distribution"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let version = details["provider"]["version"]
        .as_str()
        .map(|value| value.to_string());
    let pkg_build_oid = details["provider"]["pkg_build_oid"]
        .as_str()
        .map(|value| value.to_string());
    let entry_point = details["target"]["entry_point"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let version_display = version
        .as_deref()
        .map(|version| format!(" {version}"))
        .unwrap_or_default();
    let oid_display = pkg_build_oid
        .as_deref()
        .map(|oid| format!(" (pkg_build_oid={oid})"))
        .unwrap_or_default();
    let message = format!(
        "entrypoint: {name}\nprovider: {dist}{version_display}{oid_display}\ntarget: {entry_point}",
    );
    Ok(ExecutionOutcome::success(message, details))
}
