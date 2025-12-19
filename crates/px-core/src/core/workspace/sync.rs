use std::collections::HashMap;

use anyhow::{anyhow, Context, Result};
use serde_json::json;

use crate::core::runtime::{ensure_profile_env, workspace_env_owner_id};
use crate::store::cas::{global_store, run_gc_with_env_policy, OwnerId, OwnerType};
use crate::{
    compute_lock_hash, dependency_name, detect_runtime_metadata, prepare_project_runtime,
    resolve_dependencies_with_effects, resolve_pins, write_python_environment_markers,
    CommandContext, ExecutionOutcome, InstallUserError, StoredEnvironment, StoredRuntime,
    PX_VERSION,
};
use px_domain::api::{
    load_lockfile_optional, ResolvedDependency, WorkspaceLock,
    WorkspaceMember as WorkspaceLockMember, WorkspaceOwner,
};

use super::state::persist_workspace_state;
use super::{
    evaluate_workspace_state, load_workspace_snapshot, load_workspace_state, workspace_violation,
    StateViolation, WorkspaceScope, WorkspaceSnapshot, WorkspaceStateKind, WorkspaceSyncRequest,
};

pub fn workspace_sync(
    ctx: &CommandContext,
    scope: WorkspaceScope,
    request: &WorkspaceSyncRequest,
) -> Result<ExecutionOutcome> {
    let workspace_root = match &scope {
        WorkspaceScope::Root(ws) | WorkspaceScope::Member { workspace: ws, .. } => {
            ws.config.root.clone()
        }
    };
    let workspace = load_workspace_snapshot(&workspace_root)?;
    let state = evaluate_workspace_state(ctx, &workspace)?;
    if request.dry_run {
        let action = if !state.lock_exists || !state.manifest_clean || request.force_resolve {
            "resolve_lock"
        } else if state.env_clean {
            "noop"
        } else {
            "sync_env"
        };
        return Ok(ExecutionOutcome::success(
            format!("workspace {action} (dry-run)"),
            json!({
                "workspace": workspace.config.root.display().to_string(),
                "lockfile": workspace.lock_path.display().to_string(),
                "action": action,
                "state": state.canonical.as_str(),
                "dry_run": true,
            }),
        ));
    }

    if request.frozen {
        if !state.lock_exists || matches!(state.canonical, WorkspaceStateKind::NeedsLock) {
            return Ok(workspace_violation(
                "sync",
                &workspace,
                &state,
                StateViolation::MissingLock,
            ));
        }
        if !state.env_clean {
            refresh_workspace_site(ctx, &workspace)?;
        }
        return Ok(ExecutionOutcome::success(
            "workspace environment synced from existing lock",
            json!({
                "workspace": workspace.config.root.display().to_string(),
                "lockfile": workspace.lock_path.display().to_string(),
                "state": WorkspaceStateKind::Consistent.as_str(),
                "mode": "frozen",
            }),
        ));
    }

    if request.force_resolve || !state.lock_exists || !state.manifest_clean {
        resolve_workspace(ctx, &workspace)?;
    }
    refresh_workspace_site(ctx, &workspace)?;

    Ok(ExecutionOutcome::success(
        "workspace lock and environment updated",
        json!({
            "workspace": workspace.config.root.display().to_string(),
            "lockfile": workspace.lock_path.display().to_string(),
            "state": WorkspaceStateKind::Consistent.as_str(),
        }),
    ))
}

fn resolve_workspace(ctx: &CommandContext, workspace: &WorkspaceSnapshot) -> Result<()> {
    let resolved =
        resolve_dependencies_with_effects(ctx.effects(), &workspace.lock_snapshot(), true)
            .map_err(|err| match err.downcast::<InstallUserError>() {
                Ok(user) => InstallUserError::new(user.message, user.details),
                Err(other) => InstallUserError::new(
                    "dependency resolution failed",
                    json!({ "error": other.to_string() }),
                ),
            })?;
    let resolved_deps = resolve_pins(ctx, &resolved.pins, ctx.config().resolver.force_sdist)?;
    let workspace_lock = build_workspace_lock(workspace, &resolved_deps);
    let contents = px_domain::api::render_lockfile_with_workspace(
        &workspace.lock_snapshot(),
        &resolved_deps,
        PX_VERSION,
        Some(&workspace_lock),
    )?;
    if let Some(parent) = workspace.lock_path.parent() {
        ctx.fs().create_dir_all(parent)?;
    }
    ctx.fs()
        .write(&workspace.lock_path, contents.as_bytes())
        .context("failed to write workspace lockfile")?;
    Ok(())
}

pub(super) fn refresh_workspace_site(
    ctx: &CommandContext,
    workspace: &WorkspaceSnapshot,
) -> Result<()> {
    let previous_env = load_workspace_state(ctx.fs(), &workspace.config.root)
        .ok()
        .and_then(|state| state.current_env);
    let snapshot = workspace.lock_snapshot();
    let _ = prepare_project_runtime(&snapshot)?;
    let lock = load_lockfile_optional(&workspace.lock_path)?.ok_or_else(|| {
        anyhow!(
            "workspace lockfile missing at {}",
            workspace.lock_path.display()
        )
    })?;
    let runtime = detect_runtime_metadata(ctx, &snapshot)?;
    let lock_id = lock
        .lock_id
        .clone()
        .unwrap_or(compute_lock_hash(&workspace.lock_path)?);
    let env_owner = OwnerId {
        owner_type: OwnerType::WorkspaceEnv,
        owner_id: workspace_env_owner_id(&workspace.config.root, &lock_id, &runtime.version)?,
    };
    let cas_profile = ensure_profile_env(ctx, &snapshot, &lock, &runtime, &env_owner)?;
    let env_python = write_python_environment_markers(
        &cas_profile.env_path,
        &runtime,
        &cas_profile.runtime_path,
        ctx.fs(),
    )?;
    let runtime_state = StoredRuntime {
        path: cas_profile.runtime_path.display().to_string(),
        version: runtime.version.clone(),
        platform: runtime.platform.clone(),
    };
    let env_state = StoredEnvironment {
        id: cas_profile.profile_oid.clone(),
        lock_id,
        platform: runtime.platform.clone(),
        site_packages: crate::core::runtime::site_packages_dir(
            &cas_profile.env_path,
            &runtime.version,
        )
        .display()
        .to_string(),
        env_path: Some(cas_profile.env_path.display().to_string()),
        profile_oid: Some(cas_profile.profile_oid.clone()),
        python: crate::StoredPython {
            path: env_python.display().to_string(),
            version: runtime.version.clone(),
        },
    };
    let local_envs = workspace.config.root.join(".px").join("envs");
    ctx.fs().create_dir_all(&local_envs)?;
    let current = local_envs.join("current");
    crate::core::fs::replace_dir_link(&cas_profile.env_path, &current)?;
    persist_workspace_state(ctx.fs(), &workspace.config.root, env_state, runtime_state)?;

    if let Some(prev) = previous_env {
        if let Some(prev_profile) = prev.profile_oid.as_deref() {
            if prev_profile != cas_profile.profile_oid {
                let store = global_store();
                if let Ok(prev_owner_id) = workspace_env_owner_id(
                    &workspace.config.root,
                    &prev.lock_id,
                    &prev.python.version,
                ) {
                    let prev_owner = OwnerId {
                        owner_type: OwnerType::WorkspaceEnv,
                        owner_id: prev_owner_id,
                    };
                    if store.remove_ref(&prev_owner, prev_profile)?
                        && store.refs_for(prev_profile)?.is_empty()
                    {
                        let profile_owner = OwnerId {
                            owner_type: OwnerType::Profile,
                            owner_id: prev_profile.to_string(),
                        };
                        let _ = store.remove_owner_refs(&profile_owner)?;
                        let _ = store.remove_env_materialization(prev_profile);
                    }
                }
            }
        }
    }

    let _ = run_gc_with_env_policy(global_store());
    Ok(())
}

fn build_workspace_lock(
    workspace: &WorkspaceSnapshot,
    resolved: &[ResolvedDependency],
) -> WorkspaceLock {
    let mut member_lookup: HashMap<String, Vec<String>> = HashMap::new();
    let mut members = workspace
        .members
        .iter()
        .map(|member| {
            let mut deps = member.snapshot.requirements.clone();
            deps.sort();
            for dep in &deps {
                let name = dependency_name(dep);
                if !name.is_empty() {
                    member_lookup
                        .entry(name.to_lowercase())
                        .or_default()
                        .push(member.rel_path.clone());
                }
            }
            WorkspaceLockMember {
                name: member.snapshot.name.clone(),
                path: member.rel_path.clone(),
                manifest_fingerprint: member.snapshot.manifest_fingerprint.clone(),
                dependencies: deps,
            }
        })
        .collect::<Vec<_>>();
    members.sort_by(|a, b| a.path.cmp(&b.path));

    let mut owners = Vec::new();
    for dep in resolved {
        let key = dep.name.to_lowercase();
        let mut owned_by = member_lookup.get(&key).cloned().unwrap_or_default();
        if owned_by.is_empty() {
            owned_by.push("external".to_string());
        } else {
            owned_by.sort();
            owned_by.dedup();
        }
        owners.push(WorkspaceOwner {
            name: dep.name.clone(),
            owners: owned_by,
        });
    }
    owners.sort_by(|a, b| a.name.cmp(&b.name));

    WorkspaceLock { members, owners }
}
