use std::env;
use std::path::PathBuf;

use serde_json::json;

use crate::{prepare_project_runtime, CommandContext, ExecutionOutcome, PythonContext};

use super::sync::refresh_workspace_site;
use super::{
    discover_workspace_scope, evaluate_workspace_state, load_workspace_state, workspace_violation,
    StateViolation, WorkspaceRunContext, WorkspaceScope, WorkspaceStateKind,
};

pub fn prepare_workspace_run_context(
    ctx: &CommandContext,
    strict: bool,
    command: &str,
    sandbox: bool,
) -> Result<Option<WorkspaceRunContext>, ExecutionOutcome> {
    let scope = discover_workspace_scope().map_err(|err| {
        ExecutionOutcome::failure(
            "failed to detect workspace",
            json!({ "error": err.to_string() }),
        )
    })?;
    let Some(scope) = scope else {
        return Ok(None);
    };
    let (workspace, member_root) = match scope {
        WorkspaceScope::Member {
            workspace,
            member_root,
        } => (workspace, member_root),
        WorkspaceScope::Root(workspace) if command == "pack" => {
            let root = workspace.config.root.clone();
            (workspace, root)
        }
        _ => return Ok(None),
    };
    let state = evaluate_workspace_state(ctx, &workspace).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to evaluate workspace state",
            json!({ "error": err.to_string() }),
        )
    })?;
    if !state.lock_exists {
        return Err(workspace_violation(
            command,
            &workspace,
            &state,
            StateViolation::MissingLock,
        ));
    }
    if matches!(state.canonical, WorkspaceStateKind::NeedsLock) {
        return Err(workspace_violation(
            command,
            &workspace,
            &state,
            StateViolation::LockDrift,
        ));
    }

    if strict && !state.env_clean {
        return Err(workspace_violation(
            command,
            &workspace,
            &state,
            StateViolation::EnvDrift,
        ));
    }

    let mut sync_report = None;
    if !strict && !state.env_clean {
        if sandbox {
            return Err(workspace_violation(
                command,
                &workspace,
                &state,
                StateViolation::EnvDrift,
            ));
        }
        eprintln!("px ▸ Syncing workspace environment…");
        refresh_workspace_site(ctx, &workspace).map_err(|err| {
            ExecutionOutcome::failure(
                "failed to prepare workspace environment",
                json!({ "error": err.to_string() }),
            )
        })?;
        let issue = state
            .env_issue
            .as_ref()
            .and_then(crate::issue_from_details)
            .unwrap_or(crate::EnvironmentIssue::EnvOutdated);
        sync_report = Some(crate::EnvironmentSyncReport::new(issue));
    }
    let state_file = load_workspace_state(ctx.fs(), &workspace.config.root).map_err(|err| {
        ExecutionOutcome::failure(
            "workspace state unreadable",
            json!({ "error": err.to_string() }),
        )
    })?;
    let Some(env) = state_file.current_env else {
        return Err(workspace_violation(
            command,
            &workspace,
            &state,
            StateViolation::EnvDrift,
        ));
    };
    let _runtime = prepare_project_runtime(&workspace.lock_snapshot()).map_err(|err| {
        ExecutionOutcome::failure(
            "workspace runtime unavailable",
            json!({ "error": err.to_string() }),
        )
    })?;
    let site_dir = PathBuf::from(&env.site_packages);
    let manifest_path = member_root.join("pyproject.toml");
    if manifest_path.exists() {
        if let Err(err) = crate::ensure_version_file(&manifest_path) {
            return Err(ExecutionOutcome::failure(
                "failed to prepare workspace version file",
                json!({ "error": err.to_string() }),
            ));
        }
    }
    let paths =
        crate::build_pythonpath(ctx.fs(), &member_root, Some(site_dir.clone())).map_err(|err| {
            ExecutionOutcome::failure(
                "failed to build workspace PYTHONPATH",
                json!({ "error": err.to_string() }),
            )
        })?;
    let mut combined = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let push_unique =
        |paths: &mut Vec<PathBuf>, seen: &mut std::collections::HashSet<PathBuf>, path: PathBuf| {
            if seen.insert(path.clone()) {
                paths.push(path);
            }
        };
    let current_src = member_root.join("src");
    if current_src.exists() {
        push_unique(&mut combined, &mut seen, current_src);
    }
    push_unique(&mut combined, &mut seen, member_root.clone());
    for member in &workspace.config.members {
        let abs = workspace.config.root.join(member);
        let src = abs.join("src");
        if src.exists() {
            push_unique(&mut combined, &mut seen, src);
        }
        push_unique(&mut combined, &mut seen, abs);
    }
    for path in paths.allowed_paths {
        push_unique(&mut combined, &mut seen, path);
    }
    let allowed_paths = combined;
    let pythonpath = env::join_paths(&allowed_paths)
        .map_err(|err| {
            ExecutionOutcome::failure(
                "failed to assemble workspace PYTHONPATH",
                json!({ "error": err.to_string() }),
            )
        })?
        .into_string()
        .map_err(|_| {
            ExecutionOutcome::failure(
                "failed to assemble workspace PYTHONPATH",
                json!({ "error": "contains non-utf8 data" }),
            )
        })?;
    let member_data = workspace
        .members
        .iter()
        .find(|member| member.root == member_root);
    let px_options = member_data
        .map(|member| member.snapshot.px_options.clone())
        .unwrap_or_default();
    let project_name = member_data
        .map(|member| member.snapshot.name.clone())
        .or_else(|| {
            member_root
                .file_name()
                .and_then(|name| name.to_str())
                .map(std::string::ToString::to_string)
        })
        .unwrap_or_default();
    let python_path = env.python.path.clone();
    let profile_oid = env.profile_oid.clone().or_else(|| Some(env.id.clone()));
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
    let py_ctx = PythonContext {
        project_root: member_root.clone(),
        state_root: member_root.clone(),
        project_name,
        python: python_path,
        pythonpath,
        allowed_paths,
        site_bin: paths.site_bin,
        pep582_bin: paths.pep582_bin,
        pyc_cache_prefix,
        px_options,
    };
    Ok(Some(WorkspaceRunContext {
        py_ctx,
        manifest_path: member_root.join("pyproject.toml"),
        sync_report,
        workspace_deps: workspace.dependencies.clone(),
        lock_path: workspace.lock_path.clone(),
        profile_oid,
        workspace_root: workspace.config.root.clone(),
        workspace_manifest: workspace.config.manifest_path.clone(),
        site_packages: site_dir,
        state,
    }))
}
