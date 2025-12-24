use std::env;
use std::path::{Path, PathBuf};

use serde_json::json;

use super::super::cas_env::{project_env_owner_id, workspace_env_owner_id};
use super::sandbox::sandbox_plan;
use super::sys_path::{summarize_sys_path, sys_path_for_profile};
use super::types::{
    EngineMode, EnginePlan, ExecutionPlan, LockProfilePlan, PlanContext, ProvenancePlan,
    RuntimePlan, SourceProvenance, SysPathPlan, TargetKind, TargetResolution,
};
use crate::core::config::state_guard;
use crate::core::store::cas::global_store;
use crate::python_sys::detect_interpreter_tags;
use crate::tooling::missing_pyproject_outcome;
use crate::workspace::{discover_workspace_scope, WorkspaceScope};
use crate::{
    detect_runtime_metadata, load_project_state, manifest_snapshot, prepare_project_runtime,
    CommandContext, ExecutionOutcome,
};
use px_domain::api::{load_lockfile_optional, verify_locked_artifacts};

fn map_workdir(invocation_root: Option<&Path>, context_root: &Path) -> PathBuf {
    let cwd = env::current_dir().unwrap_or_else(|_| context_root.to_path_buf());
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

fn runtime_plan_for_executable(
    executable: &str,
    python_version: Option<String>,
    runtime_oid: Option<String>,
) -> RuntimePlan {
    let tags = detect_interpreter_tags(executable).ok();
    let python_abi = tags
        .as_ref()
        .and_then(|t| t.abi.first().cloned())
        .or_else(|| {
            tags.as_ref()
                .and_then(|t| t.supported.first().map(|t| t.abi.clone()))
        });
    RuntimePlan {
        python_version,
        python_abi,
        runtime_oid,
        executable: executable.to_string(),
    }
}

fn default_target_resolution(executable: &str, target: &str, args: &[String]) -> TargetResolution {
    if is_python_alias(target) {
        if args.len() >= 2 && args[0] == "-m" {
            let module = args[1].clone();
            let mut argv = Vec::with_capacity(args.len() + 1);
            argv.push(executable.to_string());
            argv.extend(args.iter().cloned());
            return TargetResolution {
                kind: TargetKind::Module,
                resolved: module,
                argv,
            };
        }
        let mut argv = Vec::with_capacity(args.len() + 1);
        argv.push(executable.to_string());
        argv.extend(args.iter().cloned());
        return TargetResolution {
            kind: TargetKind::Python,
            resolved: executable.to_string(),
            argv,
        };
    }

    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(target.to_string());
    argv.extend(args.iter().cloned());
    TargetResolution {
        kind: TargetKind::Executable,
        resolved: target.to_string(),
        argv,
    }
}

fn plan_execution(
    ctx: &CommandContext,
    strict: bool,
    allow_lock_autosync: bool,
    sandbox: bool,
    command: &'static str,
    target: &str,
    args: &[String],
) -> Result<ExecutionPlan, ExecutionOutcome> {
    let scope = discover_workspace_scope().map_err(|err| {
        ExecutionOutcome::failure(
            "failed to detect workspace",
            json!({ "error": err.to_string() }),
        )
    })?;

    let workspace_ctx = match scope {
        Some(WorkspaceScope::Member {
            workspace,
            member_root,
        }) => Some((workspace, member_root)),
        Some(WorkspaceScope::Root(workspace)) if matches!(command, "run" | "test") => {
            let cwd = env::current_dir().unwrap_or_else(|_| workspace.config.root.clone());
            Some((workspace, cwd))
        }
        _ => None,
    };
    if let Some((workspace, member_root)) = workspace_ctx {
        let state = crate::workspace::evaluate_workspace_state(ctx, &workspace).map_err(|err| {
            ExecutionOutcome::failure(
                "failed to evaluate workspace state",
                json!({ "error": err.to_string() }),
            )
        })?;
        if !state.lock_exists && !allow_lock_autosync {
            return Err(crate::workspace::workspace_violation(
                command,
                &workspace,
                &state,
                crate::workspace::StateViolation::MissingLock,
            ));
        }
        if !state.manifest_clean && !allow_lock_autosync {
            return Err(crate::workspace::workspace_violation(
                command,
                &workspace,
                &state,
                crate::workspace::StateViolation::LockDrift,
            ));
        }
        if matches!(state.canonical, crate::workspace::WorkspaceStateKind::NeedsLock) {
            return Err(crate::workspace::workspace_violation(
                command,
                &workspace,
                &state,
                crate::workspace::StateViolation::LockDrift,
            ));
        }
        if strict && !state.env_clean {
            return Err(crate::workspace::workspace_violation(
                command,
                &workspace,
                &state,
                crate::workspace::StateViolation::EnvDrift,
            ));
        }

        let snapshot = workspace.lock_snapshot();
        let _ = prepare_project_runtime(&snapshot).map_err(|err| {
            ExecutionOutcome::failure(
                "workspace runtime unavailable",
                json!({ "error": err.to_string() }),
            )
        })?;
        let runtime = detect_runtime_metadata(ctx, &snapshot).map_err(|err| {
            ExecutionOutcome::failure(
                "workspace runtime unavailable",
                json!({ "error": err.to_string() }),
            )
        })?;
        let state_file = crate::workspace::load_workspace_state(ctx.fs(), &workspace.config.root)
            .map_err(|err| {
            ExecutionOutcome::failure(
                "workspace state unreadable",
                json!({ "error": err.to_string() }),
            )
        })?;
        let lock_for_sandbox = load_lockfile_optional(&workspace.lock_path).map_err(|err| {
            ExecutionOutcome::failure(
                "failed to load workspace lockfile",
                json!({
                    "lockfile": workspace.lock_path.display().to_string(),
                    "error": err.to_string(),
                }),
            )
        })?;
        let env_state = state_file.current_env.as_ref();
        let profile_oid =
            env_state.and_then(|env| env.profile_oid.clone().or_else(|| Some(env.id.clone())));
        let python_version = runtime.version.clone();
        let (runtime_exe, site_packages) = if let Some(env) = env_state {
            (
                env.python.path.clone(),
                Some(PathBuf::from(&env.site_packages)),
            )
        } else {
            (runtime.path.clone(), None)
        };

        let mut engine = EnginePlan {
            mode: if sandbox || strict {
                EngineMode::MaterializedEnv
            } else {
                EngineMode::CasNative
            },
            fallback_reason_code: None,
        };
        if matches!(engine.mode, EngineMode::CasNative) {
            if let Some(lock) = load_lockfile_optional(&workspace.lock_path).map_err(|err| {
                ExecutionOutcome::failure(
                    "failed to load workspace lockfile",
                    json!({
                        "lockfile": workspace.lock_path.display().to_string(),
                        "error": err.to_string(),
                    }),
                )
            })? {
                let missing = verify_locked_artifacts(&lock);
                if !missing.is_empty() {
                    engine.mode = EngineMode::MaterializedEnv;
                    engine.fallback_reason_code = Some("missing_artifacts".to_string());
                }
            }
        }
        let would_repair_env =
            !strict && (!state.lock_exists || !state.manifest_clean || !state.env_clean);

        let env_id = if matches!(engine.mode, EngineMode::MaterializedEnv) {
            state.lock_id.as_deref().and_then(|lock_id| {
                workspace_env_owner_id(&workspace.config.root, lock_id, &runtime.version).ok()
            })
        } else {
            None
        };

        let lock_profile = LockProfilePlan {
            l_id: None,
            wl_id: state.lock_id.clone(),
            tool_lock_id: None,
            profile_oid: profile_oid.clone(),
            env_id,
        };
        let sys_entries = profile_oid
            .as_deref()
            .and_then(|oid| sys_path_for_profile(oid).ok())
            .unwrap_or_default();
        let sys_path = SysPathPlan {
            summary: summarize_sys_path(&sys_entries),
            entries: sys_entries,
        };
        let runtime_oid = profile_oid.as_deref().and_then(|oid| {
            global_store()
                .load(oid)
                .ok()
                .and_then(|loaded| match loaded {
                    crate::LoadedObject::Profile { header, .. } => Some(header.runtime_oid),
                    _ => None,
                })
        });
        let runtime_plan =
            runtime_plan_for_executable(&runtime_exe, Some(python_version), runtime_oid);

        let workdir = map_workdir(Some(&member_root), &member_root);
        let target_resolution =
            if let Some(script) = detect_script_under_root(&member_root, &workdir, target) {
                let mut argv = Vec::with_capacity(args.len() + 2);
                argv.push(runtime_exe.clone());
                argv.push(script.display().to_string());
                argv.extend(args.iter().cloned());
                TargetResolution {
                    kind: TargetKind::File,
                    resolved: script.display().to_string(),
                    argv,
                }
            } else {
                default_target_resolution(&runtime_exe, target, args)
            };

        let manifest_path = member_root.join("pyproject.toml");
        let workspace_lock = lock_for_sandbox
            .as_ref()
            .and_then(|lock| lock.workspace.as_ref());
        let sandbox_plan = sandbox_plan(
            &manifest_path,
            sandbox,
            lock_for_sandbox.as_ref(),
            workspace_lock,
            profile_oid.as_deref(),
            site_packages.as_deref(),
        )?;

        return Ok(ExecutionPlan {
            schema_version: 1,
            context: PlanContext::Workspace {
                workspace_root: workspace.config.root.display().to_string(),
                workspace_manifest: workspace.config.manifest_path.display().to_string(),
                workspace_lock_path: workspace.lock_path.display().to_string(),
                member_root: member_root.display().to_string(),
                member_manifest: manifest_path.display().to_string(),
            },
            runtime: runtime_plan,
            lock_profile,
            engine,
            target_resolution,
            working_dir: workdir.display().to_string(),
            sys_path,
            provenance: ProvenancePlan {
                sandbox: sandbox_plan,
                source: SourceProvenance::WorkingTree,
            },
            would_repair_env,
        });
    }

    let snapshot = manifest_snapshot().map_err(|err| {
        if crate::is_missing_project_error(&err) {
            return crate::missing_project_outcome();
        }
        let msg = err.to_string();
        if msg.contains("pyproject.toml not found") {
            let root = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            missing_pyproject_outcome(command, &root)
        } else {
            ExecutionOutcome::failure("failed to load project manifest", json!({ "error": msg }))
        }
    })?;
    let state_report = state_guard::state_or_violation(ctx, &snapshot, command)?;
    let guard =
        state_guard::guard_for_execution(strict, allow_lock_autosync, &snapshot, &state_report, command)?;

    let _ = prepare_project_runtime(&snapshot).map_err(|err| {
        ExecutionOutcome::failure(
            "python runtime unavailable",
            json!({ "error": err.to_string() }),
        )
    })?;
    let runtime = detect_runtime_metadata(ctx, &snapshot).map_err(|err| {
        ExecutionOutcome::failure(
            "python runtime unavailable",
            json!({ "error": err.to_string() }),
        )
    })?;
    let state = load_project_state(ctx.fs(), &snapshot.root).map_err(|err| {
        ExecutionOutcome::failure(
            "project state unreadable",
            json!({ "error": err.to_string() }),
        )
    })?;
    let env_state = state.current_env.as_ref();
    let profile_oid =
        env_state.and_then(|env| env.profile_oid.clone().or_else(|| Some(env.id.clone())));
    let python_version = runtime.version.clone();
    let runtime_exe = env_state
        .map(|env| env.python.path.clone())
        .unwrap_or_else(|| runtime.path.clone());

    let mut engine = EnginePlan {
        mode: if sandbox || strict {
            EngineMode::MaterializedEnv
        } else {
            EngineMode::CasNative
        },
        fallback_reason_code: None,
    };
    if matches!(engine.mode, EngineMode::CasNative) {
        if let Some(lock) = load_lockfile_optional(&snapshot.lock_path).map_err(|err| {
            ExecutionOutcome::failure(
                "failed to load px.lock",
                json!({
                    "lockfile": snapshot.lock_path.display().to_string(),
                    "error": err.to_string(),
                }),
            )
        })? {
            let missing = verify_locked_artifacts(&lock);
            if !missing.is_empty() {
                engine.mode = EngineMode::MaterializedEnv;
                engine.fallback_reason_code = Some("missing_artifacts".to_string());
            }
        }
    }
    let would_repair_env = matches!(guard, crate::EnvGuard::AutoSync);

    let env_id = if matches!(engine.mode, EngineMode::MaterializedEnv) {
        state_report.lock_id.as_deref().and_then(|lock_id| {
            project_env_owner_id(&snapshot.root, lock_id, &runtime.version).ok()
        })
    } else {
        None
    };

    let lock_profile = LockProfilePlan {
        l_id: state_report.lock_id.clone(),
        wl_id: None,
        tool_lock_id: None,
        profile_oid: profile_oid.clone(),
        env_id,
    };
    let sys_entries = profile_oid
        .as_deref()
        .and_then(|oid| sys_path_for_profile(oid).ok())
        .unwrap_or_default();
    let sys_path = SysPathPlan {
        summary: summarize_sys_path(&sys_entries),
        entries: sys_entries,
    };
    let runtime_oid = profile_oid.as_deref().and_then(|oid| {
        global_store()
            .load(oid)
            .ok()
            .and_then(|loaded| match loaded {
                crate::LoadedObject::Profile { header, .. } => Some(header.runtime_oid),
                _ => None,
            })
    });
    let runtime_plan = runtime_plan_for_executable(&runtime_exe, Some(python_version), runtime_oid);

    let workdir = map_workdir(Some(&snapshot.root), &snapshot.root);
    let target_resolution =
        if let Some(script) = detect_script_under_root(&snapshot.root, &workdir, target) {
            let mut argv = Vec::with_capacity(args.len() + 2);
            argv.push(runtime_exe.clone());
            argv.push(script.display().to_string());
            argv.extend(args.iter().cloned());
            TargetResolution {
                kind: TargetKind::File,
                resolved: script.display().to_string(),
                argv,
            }
        } else {
            default_target_resolution(&runtime_exe, target, args)
        };

    let manifest_path = snapshot.root.join("pyproject.toml");
    let site_packages = env_state.map(|env| PathBuf::from(&env.site_packages));
    let lock_for_sandbox = load_lockfile_optional(&snapshot.lock_path).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to load px.lock",
            json!({
                "lockfile": snapshot.lock_path.display().to_string(),
                "error": err.to_string(),
            }),
        )
    })?;
    let sandbox_plan = sandbox_plan(
        &manifest_path,
        sandbox,
        lock_for_sandbox.as_ref(),
        None,
        profile_oid.as_deref(),
        site_packages.as_deref(),
    )?;

    Ok(ExecutionPlan {
        schema_version: 1,
        context: PlanContext::Project {
            project_root: snapshot.root.display().to_string(),
            manifest_path: snapshot.manifest_path.display().to_string(),
            lock_path: snapshot.lock_path.display().to_string(),
            project_name: snapshot.name.clone(),
        },
        runtime: runtime_plan,
        lock_profile,
        engine,
        target_resolution,
        working_dir: workdir.display().to_string(),
        sys_path,
        provenance: ProvenancePlan {
            sandbox: sandbox_plan,
            source: SourceProvenance::WorkingTree,
        },
        would_repair_env,
    })
}

pub(crate) fn plan_run_execution(
    ctx: &CommandContext,
    strict: bool,
    allow_lock_autosync: bool,
    sandbox: bool,
    target: &str,
    args: &[String],
) -> Result<ExecutionPlan, ExecutionOutcome> {
    plan_execution(ctx, strict, allow_lock_autosync, sandbox, "run", target, args)
}

pub(crate) fn plan_test_execution(
    ctx: &CommandContext,
    strict: bool,
    allow_lock_autosync: bool,
    sandbox: bool,
    args: &[String],
) -> Result<ExecutionPlan, ExecutionOutcome> {
    // Placeholder target resolution for tests; the runner is selected at execution time.
    let target = "pytest";
    plan_execution(ctx, strict, allow_lock_autosync, sandbox, "test", target, args)
}
