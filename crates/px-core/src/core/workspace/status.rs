use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{json, Value};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use crate::core::status::runtime_source_for;
use crate::{
    compute_lock_hash, detect_runtime_metadata, CommandContext, CommandStatus, EnvHealth,
    EnvStatus, ExecutionOutcome, LockHealth, LockStatus, NextAction, NextActionKind, RuntimeRole,
    RuntimeSource, RuntimeStatus, StatusContext, StatusContextKind, StatusPayload,
    StoredEnvironment, StoredRuntime, WorkspaceMemberStatus, WorkspaceStatusPayload,
};
use px_domain::api::{
    load_lockfile_optional, read_workspace_config, sandbox_config_from_manifest,
    workspace_member_for_path, ProjectSnapshot, WorkspaceConfig,
};

use super::{
    evaluate_workspace_state, load_workspace_snapshot, load_workspace_state, WorkspaceMember,
    WorkspaceScope, WorkspaceSnapshot, WorkspaceStateKind,
};

pub fn workspace_status(ctx: &CommandContext, scope: WorkspaceScope) -> Result<ExecutionOutcome> {
    let workspace_root = match &scope {
        WorkspaceScope::Root(ws) | WorkspaceScope::Member { workspace: ws, .. } => {
            ws.config.root.clone()
        }
    };
    let cwd = std::env::current_dir().context("unable to determine current directory")?;
    let payload = workspace_status_payload(ctx, &workspace_root, &cwd)?;
    let status = if payload.is_consistent() {
        CommandStatus::Ok
    } else {
        CommandStatus::UserError
    };
    let details = serde_json::to_value(payload).unwrap_or_else(|_| json!({}));
    Ok(ExecutionOutcome {
        status,
        message: "workspace status".to_string(),
        details,
    })
}

pub(crate) fn workspace_status_payload(
    ctx: &CommandContext,
    workspace_root: &Path,
    cwd: &Path,
) -> Result<StatusPayload> {
    match load_workspace_snapshot(workspace_root) {
        Ok(snapshot) => workspace_payload_from_snapshot(ctx, snapshot, cwd),
        Err(_) => workspace_payload_tolerant(ctx, workspace_root, cwd),
    }
}

fn workspace_payload_from_snapshot(
    ctx: &CommandContext,
    snapshot: WorkspaceSnapshot,
    cwd: &Path,
) -> Result<StatusPayload> {
    let state_report = evaluate_workspace_state(ctx, &snapshot)?;
    let members = snapshot
        .members
        .iter()
        .map(|member| WorkspaceMemberStatus {
            path: member.rel_path.clone(),
            included: true,
            manifest_status: "ok".to_string(),
            manifest_fingerprint: Some(member.snapshot.manifest_fingerprint.clone()),
        })
        .collect::<Vec<_>>();
    let lock = load_lockfile_optional(&snapshot.lock_path)?;
    let lock_status = workspace_lock_status(
        &snapshot.lock_path,
        lock.as_ref(),
        state_report.manifest_clean,
    );
    let env_state = load_workspace_state(ctx.fs(), &snapshot.config.root)?;
    let env_status = workspace_env_status(
        &snapshot.config.root,
        env_state.current_env.clone(),
        state_report.lock_id.clone(),
        state_report.env_issue.clone(),
        state_report.env_clean,
    );
    let runtime_status = workspace_runtime_status(ctx, Some(&snapshot), env_state.runtime.clone());
    let workspace_payload = WorkspaceStatusPayload {
        manifest_exists: state_report.manifest_exists,
        lock_exists: state_report.lock_exists,
        env_exists: state_report.env_exists,
        manifest_clean: state_report.manifest_clean,
        env_clean: state_report.env_clean,
        deps_empty: state_report.deps_empty,
        state: workspace_state_label(state_report.canonical),
        manifest_fingerprint: state_report.manifest_fingerprint.clone(),
        lock_fingerprint: state_report.lock_fingerprint.clone(),
        lock_id: state_report.lock_id.clone(),
        lock_issue: state_report.lock_issue.clone(),
        env_issue: state_report.env_issue.clone(),
        members,
    };

    let next_action = match state_report.canonical {
        WorkspaceStateKind::Consistent | WorkspaceStateKind::InitializedEmpty => NextAction {
            kind: NextActionKind::None,
            command: None,
            scope: None,
        },
        WorkspaceStateKind::NeedsEnv | WorkspaceStateKind::NeedsLock => NextAction {
            kind: NextActionKind::SyncWorkspace,
            command: Some("px sync".to_string()),
            scope: Some(snapshot.config.root.display().to_string()),
        },
        WorkspaceStateKind::Uninitialized => NextAction {
            kind: NextActionKind::Init,
            command: Some("px init".to_string()),
            scope: Some(snapshot.config.root.display().to_string()),
        },
    };

    let (project_name, project_root, member_path, kind) =
        workspace_context(&snapshot.config, cwd, &snapshot.members);
    let warnings = collect_sandbox_warnings(&snapshot);

    Ok(StatusPayload {
        context: StatusContext {
            kind,
            project_name,
            workspace_name: Some(snapshot.name.clone()),
            project_root: project_root.map(|p| p.display().to_string()),
            workspace_root: Some(snapshot.config.root.display().to_string()),
            member_path,
        },
        project: None,
        workspace: Some(workspace_payload),
        runtime: Some(runtime_status),
        lock: Some(lock_status),
        env: Some(env_status),
        next_action,
        warnings,
    })
}

fn workspace_payload_tolerant(
    ctx: &CommandContext,
    workspace_root: &Path,
    cwd: &Path,
) -> Result<StatusPayload> {
    let config = read_workspace_config(workspace_root)?;
    let workspace_name = workspace_display_name(&config);
    let mut member_statuses = Vec::new();
    let mut member_snapshots = Vec::new();
    let mut member_issues = Vec::new();
    let mut warnings = Vec::new();

    for rel in &config.members {
        let abs = config.root.join(rel);
        let rel_path = rel.display().to_string();
        let manifest_path = abs.join("pyproject.toml");
        if !manifest_path.exists() {
            member_statuses.push(WorkspaceMemberStatus {
                path: rel_path.clone(),
                included: true,
                manifest_status: "missing_pyproject".to_string(),
                manifest_fingerprint: None,
            });
            member_issues.push(format!("{rel_path}: pyproject.toml missing"));
            continue;
        }
        match ProjectSnapshot::read_from(&abs) {
            Ok(snapshot) => {
                member_statuses.push(WorkspaceMemberStatus {
                    path: rel_path.clone(),
                    included: true,
                    manifest_status: "ok".to_string(),
                    manifest_fingerprint: Some(snapshot.manifest_fingerprint.clone()),
                });
                member_snapshots.push(snapshot);
            }
            Err(err) => {
                member_statuses.push(WorkspaceMemberStatus {
                    path: rel_path.clone(),
                    included: true,
                    manifest_status: "invalid_pyproject".to_string(),
                    manifest_fingerprint: None,
                });
                member_issues.push(format!("{rel_path}: {err}"));
            }
        }
    }

    let manifest_exists = config.manifest_path.exists();
    let deps_empty = member_snapshots
        .iter()
        .all(|snapshot| snapshot.requirements.is_empty());
    let lock_path = config.root.join("px.workspace.lock");
    let lock = load_lockfile_optional(&lock_path)?;
    let lock_id = lock
        .as_ref()
        .and_then(|lock| lock.lock_id.clone())
        .or_else(|| {
            lock.as_ref()
                .and_then(|_| compute_lock_hash(&lock_path).ok())
        });
    let lock_status = workspace_lock_status(&lock_path, lock.as_ref(), false);
    let env_state = load_workspace_state(ctx.fs(), &config.root)?;
    let env_exists = env_state
        .current_env
        .as_ref()
        .map(|env| PathBuf::from(&env.site_packages).exists())
        .unwrap_or(false);
    let mut env_issue = None;
    if !env_exists {
        env_issue = Some(json!({ "reason": "missing_env" }));
    } else if let (Some(env), Some(expected)) = (env_state.current_env.as_ref(), lock_id.as_ref()) {
        if env.lock_id != *expected {
            env_issue = Some(json!({
                "reason": "env_outdated",
                "expected_lock_id": expected,
                "current_lock_id": env.lock_id,
            }));
        }
    }
    let env_status = workspace_env_status(
        &config.root,
        env_state.current_env.clone(),
        lock_id.clone(),
        env_issue.clone(),
        false,
    );
    let runtime_status = workspace_runtime_status(ctx, None, env_state.runtime.clone());
    for rel in &config.members {
        let manifest = config.root.join(rel).join("pyproject.toml");
        if let Ok(cfg) = sandbox_config_from_manifest(&manifest) {
            if cfg.defined {
                warnings.push(format!(
                    "workspace sandbox config is authoritative; member {} defines [tool.px.sandbox] which is ignored",
                    rel.display()
                ));
            }
        }
    }
    let workspace_payload = WorkspaceStatusPayload {
        manifest_exists,
        lock_exists: lock.is_some(),
        env_exists,
        manifest_clean: false,
        env_clean: false,
        deps_empty,
        state: workspace_state_label(WorkspaceStateKind::NeedsLock),
        manifest_fingerprint: None,
        lock_fingerprint: lock
            .as_ref()
            .and_then(|lock| lock.manifest_fingerprint.clone()),
        lock_id,
        lock_issue: Some(member_issues.clone()),
        env_issue: env_issue.clone(),
        members: member_statuses,
    };
    let next_action = NextAction {
        kind: NextActionKind::SyncWorkspace,
        command: Some("px sync".to_string()),
        scope: Some(config.root.display().to_string()),
    };
    let member_root = workspace_member_for_path(&config, cwd);
    let member_path = member_root
        .as_ref()
        .map(|path| relativize(&config.root, path.clone()));
    let project_name = member_root.as_ref().and_then(|root| {
        member_snapshots
            .iter()
            .find(|m| m.root == *root)
            .map(|m| m.name.clone())
    });
    let kind = if member_path.is_some() {
        StatusContextKind::WorkspaceMember
    } else {
        StatusContextKind::Workspace
    };

    Ok(StatusPayload {
        context: StatusContext {
            kind,
            project_name,
            workspace_name: Some(workspace_name),
            project_root: member_root.map(|p| p.display().to_string()),
            workspace_root: Some(config.root.display().to_string()),
            member_path,
        },
        project: None,
        workspace: Some(workspace_payload),
        runtime: Some(runtime_status),
        lock: Some(lock_status),
        env: Some(env_status),
        next_action,
        warnings,
    })
}

fn workspace_context(
    config: &WorkspaceConfig,
    cwd: &Path,
    members: &[WorkspaceMember],
) -> (
    Option<String>,
    Option<PathBuf>,
    Option<String>,
    StatusContextKind,
) {
    let member_root = workspace_member_for_path(config, cwd);
    let member_path = member_root
        .as_ref()
        .map(|path| relativize(&config.root, path.clone()));
    let project_name = member_root.as_ref().and_then(|root| {
        members
            .iter()
            .find(|member| member.root == *root)
            .map(|member| member.snapshot.name.clone())
    });
    let kind = if member_path.is_some() {
        StatusContextKind::WorkspaceMember
    } else {
        StatusContextKind::Workspace
    };
    (project_name, member_root, member_path, kind)
}

pub(super) fn collect_sandbox_warnings(workspace: &WorkspaceSnapshot) -> Vec<String> {
    let mut warnings = Vec::new();
    for member in &workspace.members {
        if let Ok(config) = sandbox_config_from_manifest(&member.snapshot.manifest_path) {
            if config.defined {
                warnings.push(format!(
                    "workspace sandbox config is authoritative; member {} defines [tool.px.sandbox] which is ignored",
                    member.rel_path
                ));
            }
        }
    }
    warnings
}

fn workspace_display_name(config: &WorkspaceConfig) -> String {
    config
        .name
        .clone()
        .or_else(|| {
            config
                .root
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| "workspace".to_string())
}

fn workspace_runtime_status(
    ctx: &CommandContext,
    snapshot: Option<&WorkspaceSnapshot>,
    stored: Option<StoredRuntime>,
) -> RuntimeStatus {
    if let Some(runtime) = stored {
        return RuntimeStatus {
            version: Some(runtime.version.clone()),
            source: runtime_source_for(&runtime.path),
            role: RuntimeRole::Workspace,
            path: Some(runtime.path.clone()),
            platform: Some(runtime.platform.clone()),
        };
    }
    if let Some(snapshot) = snapshot {
        if let Ok(meta) = detect_runtime_metadata(ctx, &snapshot.lock_snapshot()) {
            return RuntimeStatus {
                version: Some(meta.version),
                source: runtime_source_for(&meta.path),
                role: RuntimeRole::Workspace,
                path: Some(meta.path),
                platform: Some(meta.platform),
            };
        }
    }
    RuntimeStatus {
        version: None,
        source: RuntimeSource::Unknown,
        role: RuntimeRole::Workspace,
        path: None,
        platform: None,
    }
}

fn workspace_env_status(
    root: &Path,
    env: Option<StoredEnvironment>,
    lock_id: Option<String>,
    env_issue: Option<Value>,
    env_clean: bool,
) -> EnvStatus {
    let mut status = if env_clean {
        EnvHealth::Clean
    } else if env.is_some() {
        EnvHealth::Stale
    } else {
        EnvHealth::Missing
    };
    if let Some(issue) = env_issue.as_ref() {
        if let Some(reason) = issue.get("reason").and_then(Value::as_str) {
            if reason == "missing_env" {
                status = EnvHealth::Missing;
            }
        }
    }
    let path = env
        .as_ref()
        .map(|env| relativize(root, PathBuf::from(&env.site_packages)));
    let last_built_at = env
        .as_ref()
        .and_then(|env| fs::metadata(&env.site_packages).ok())
        .and_then(|meta| meta.modified().ok())
        .and_then(format_system_time);
    EnvStatus {
        path,
        status,
        lock_id,
        last_built_at,
    }
}

fn workspace_lock_status(
    lock_path: &Path,
    lock: Option<&px_domain::api::LockSnapshot>,
    manifest_clean: bool,
) -> LockStatus {
    let status = if lock.is_none() {
        LockHealth::Missing
    } else if manifest_clean {
        LockHealth::Clean
    } else {
        LockHealth::Mismatch
    };
    let updated_at = lock
        .as_ref()
        .and_then(|_| fs::metadata(lock_path).ok())
        .and_then(|meta| meta.modified().ok())
        .and_then(format_system_time);
    LockStatus {
        file: Some(lock_path.display().to_string()),
        updated_at,
        mfingerprint: lock.and_then(|lock| lock.manifest_fingerprint.clone()),
        status,
    }
}

fn workspace_state_label(kind: WorkspaceStateKind) -> String {
    match kind {
        WorkspaceStateKind::Uninitialized => "WUninitialized",
        WorkspaceStateKind::InitializedEmpty => "WInitializedEmpty",
        WorkspaceStateKind::NeedsLock => "WNeedsLock",
        WorkspaceStateKind::NeedsEnv => "WNeedsEnv",
        WorkspaceStateKind::Consistent => "WConsistent",
    }
    .to_string()
}

fn relativize(base: &Path, target: PathBuf) -> String {
    target
        .strip_prefix(base)
        .unwrap_or(&target)
        .display()
        .to_string()
}

fn format_system_time(time: std::time::SystemTime) -> Option<String> {
    let duration = time.duration_since(std::time::UNIX_EPOCH).ok()?;
    let nanos: i128 = duration.as_nanos().try_into().ok()?;
    let timestamp = OffsetDateTime::from_unix_timestamp_nanos(nanos).ok()?;
    timestamp.format(&Rfc3339).ok()
}
