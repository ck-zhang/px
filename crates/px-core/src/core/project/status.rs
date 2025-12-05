use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use crate::core::status::runtime_source_for;
use crate::workspace::workspace_status_payload;
use crate::{
    compute_lock_hash, detect_runtime_metadata, load_project_state, CommandContext, CommandStatus,
    ExecutionOutcome, InstallUserError, ManifestSnapshot, ProjectStatusPayload, RuntimeRole,
    RuntimeSource, RuntimeStatus, StatusContext, StatusContextKind, StatusPayload,
};
use px_domain::{
    discover_project_root, discover_workspace_root, load_lockfile_optional,
    missing_project_guidance, MissingProjectGuidance, ProjectStateKind,
};

use super::evaluate_project_state;
use crate::core::runtime::{
    StoredEnvironment, StoredRuntime, MISSING_PROJECT_HINT, MISSING_PROJECT_MESSAGE,
};
use crate::{EnvHealth, EnvStatus, LockHealth, LockStatus, NextAction, NextActionKind};

/// Reports whether the manifest, lockfile, and environment are consistent.
///
/// # Errors
/// Returns an error if project metadata cannot be read or dependency verification fails.
pub fn project_status(ctx: &CommandContext) -> Result<ExecutionOutcome> {
    let cwd = std::env::current_dir().context("unable to determine current directory")?;
    if let Some(root) = discover_workspace_root()? {
        let payload = workspace_status_payload(ctx, &root, &cwd)?;
        return Ok(outcome_from_payload(payload));
    }

    let Some(root) = discover_project_root()? else {
        return Ok(missing_project_status(&cwd));
    };
    let payload = project_status_payload(ctx, &root)?;
    Ok(outcome_from_payload(payload))
}

fn outcome_from_payload(payload: StatusPayload) -> ExecutionOutcome {
    let status = if payload.is_consistent() {
        CommandStatus::Ok
    } else {
        CommandStatus::UserError
    };
    let message = match payload.context.kind {
        StatusContextKind::Project => "project status",
        StatusContextKind::Workspace | StatusContextKind::WorkspaceMember => "workspace status",
        StatusContextKind::None => "no project",
    };
    let details = serde_json::to_value(payload).unwrap_or_else(|_| json!({}));
    ExecutionOutcome {
        status,
        message: message.to_string(),
        details,
    }
}

fn project_status_payload(ctx: &CommandContext, root: &Path) -> Result<StatusPayload> {
    let snapshot = ManifestSnapshot::read_from(root)?;
    let state_report = evaluate_project_state(ctx, &snapshot)?;
    let state = load_project_state(ctx.fs(), &snapshot.root).map_err(|err| {
        InstallUserError::new(
            "px state file is unreadable",
            json!({
                "error": err.to_string(),
                "state": snapshot.root.join(".px").join("state.json"),
                "hint": "Remove or repair the corrupted .px/state.json file, then rerun the command.",
                "reason": "invalid_state",
            }),
        )
    })?;

    let lock = load_lockfile_optional(&snapshot.lock_path)?;
    let lock_fingerprint = lock
        .as_ref()
        .and_then(|lock| lock.manifest_fingerprint.clone());
    let lock_id = lock
        .as_ref()
        .and_then(|lock| lock.lock_id.clone())
        .or_else(|| {
            if lock.is_some() {
                compute_lock_hash(&snapshot.lock_path).ok()
            } else {
                None
            }
        });
    let lock_health = if lock.is_none() {
        LockHealth::Missing
    } else if state_report.manifest_clean && state_report.lock_issue.is_none() {
        LockHealth::Clean
    } else {
        LockHealth::Mismatch
    };
    let lock_updated_at = lock
        .as_ref()
        .and_then(|_| fs::metadata(&snapshot.lock_path).ok())
        .and_then(|meta| meta.modified().ok())
        .and_then(format_system_time);

    let env_status = project_env_status(
        &snapshot.root,
        state.current_env.clone(),
        lock_id.clone(),
        state_report.env_issue.clone(),
        state_report.env_clean,
    );
    let runtime_status = project_runtime_status(ctx, &snapshot, state.runtime.clone());
    let deps_empty = state_report.deps_empty;
    let project_state = project_state_label(state_report.canonical);

    let project_payload = ProjectStatusPayload {
        manifest_exists: state_report.manifest_exists,
        lock_exists: state_report.lock_exists,
        env_exists: state_report.env_exists,
        manifest_clean: state_report.manifest_clean,
        env_clean: state_report.env_clean,
        deps_empty,
        state: project_state,
        manifest_fingerprint: state_report.manifest_fingerprint.clone(),
        lock_fingerprint,
        lock_id: lock_id.clone(),
        lock_issue: state_report.lock_issue.clone(),
        env_issue: state_report.env_issue.clone(),
    };

    let lock_status = LockStatus {
        file: Some(snapshot.lock_path.display().to_string()),
        updated_at: lock_updated_at,
        mfingerprint: lock
            .as_ref()
            .and_then(|lock| lock.manifest_fingerprint.clone()),
        status: lock_health,
    };

    let next_action = match state_report.canonical {
        ProjectStateKind::Consistent | ProjectStateKind::InitializedEmpty => NextAction {
            kind: NextActionKind::None,
            command: None,
            scope: None,
        },
        ProjectStateKind::NeedsLock | ProjectStateKind::NeedsEnv => NextAction {
            kind: NextActionKind::Sync,
            command: Some("px sync".to_string()),
            scope: Some(snapshot.root.display().to_string()),
        },
        ProjectStateKind::Uninitialized => NextAction {
            kind: NextActionKind::Init,
            command: Some("px init".to_string()),
            scope: Some(snapshot.root.display().to_string()),
        },
    };

    Ok(StatusPayload {
        context: StatusContext {
            kind: StatusContextKind::Project,
            project_name: Some(snapshot.name.clone()),
            workspace_name: None,
            project_root: Some(snapshot.root.display().to_string()),
            workspace_root: None,
            member_path: None,
        },
        project: Some(project_payload),
        workspace: None,
        runtime: Some(runtime_status),
        lock: Some(lock_status),
        env: Some(env_status),
        next_action,
    })
}

fn project_runtime_status(
    ctx: &CommandContext,
    snapshot: &ManifestSnapshot,
    stored: Option<StoredRuntime>,
) -> RuntimeStatus {
    if let Some(runtime) = stored {
        return RuntimeStatus {
            version: Some(runtime.version.clone()),
            source: runtime_source_for(&runtime.path),
            role: RuntimeRole::Project,
            path: Some(runtime.path.clone()),
            platform: Some(runtime.platform.clone()),
        };
    }
    match detect_runtime_metadata(ctx, snapshot) {
        Ok(meta) => RuntimeStatus {
            version: Some(meta.version),
            source: runtime_source_for(&meta.path),
            role: RuntimeRole::Project,
            path: Some(meta.path),
            platform: Some(meta.platform),
        },
        Err(_) => RuntimeStatus {
            version: None,
            source: RuntimeSource::Unknown,
            role: RuntimeRole::Project,
            path: None,
            platform: None,
        },
    }
}

fn project_env_status(
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

fn project_state_label(kind: ProjectStateKind) -> String {
    match kind {
        ProjectStateKind::Uninitialized => "Uninitialized",
        ProjectStateKind::InitializedEmpty => "InitializedEmpty",
        ProjectStateKind::NeedsLock => "NeedsLock",
        ProjectStateKind::NeedsEnv => "NeedsEnv",
        ProjectStateKind::Consistent => "Consistent",
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

fn format_system_time(time: SystemTime) -> Option<String> {
    let duration = time.duration_since(std::time::UNIX_EPOCH).ok()?;
    let nanos: i128 = duration.as_nanos().try_into().ok()?;
    let timestamp = OffsetDateTime::from_unix_timestamp_nanos(nanos).ok()?;
    timestamp.format(&Rfc3339).ok()
}

fn missing_project_status(cwd: &Path) -> ExecutionOutcome {
    let guidance = missing_project_guidance().unwrap_or_else(|_| MissingProjectGuidance {
        message: format!("{MISSING_PROJECT_MESSAGE} {MISSING_PROJECT_HINT}"),
        hint: MISSING_PROJECT_HINT.to_string(),
    });
    let details = json!({
        "code": "PX001",
        "reason": "missing_project",
        "searched": cwd.display().to_string(),
        "why": ["No pyproject.toml with [tool.px] and no px.lock found in parent directories."],
        "fix": [guidance.hint.clone()],
        "hint": guidance.hint,
    });
    ExecutionOutcome::user_error(format!("PX001  {}", guidance.message), details)
}

pub(crate) fn issue_id_for(message: &str) -> String {
    let digest = Sha256::digest(message.as_bytes());
    let mut short = String::new();
    for byte in &digest[..6] {
        let _ = write!(&mut short, "{byte:02x}");
    }
    format!("ISS-{}", short.to_ascii_uppercase())
}
