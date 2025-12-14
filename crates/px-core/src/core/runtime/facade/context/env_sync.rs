use anyhow::Result;
use serde_json::{json, Value};

use crate::context::CommandContext;
use crate::outcome::{ExecutionOutcome, InstallUserError};

use super::super::{
    ensure_project_environment_synced, install_snapshot, refresh_project_site, ManifestSnapshot,
};
use super::{EnvGuard, EnvironmentIssue, EnvironmentSyncReport};

pub(crate) fn ensure_environment_with_guard(
    ctx: &CommandContext,
    snapshot: &ManifestSnapshot,
    guard: EnvGuard,
) -> Result<Option<EnvironmentSyncReport>> {
    match ensure_project_environment_synced(ctx, snapshot) {
        Ok(()) => Ok(None),
        Err(err) => match err.downcast::<InstallUserError>() {
            Ok(user) => match guard {
                EnvGuard::Strict => Err(user.into()),
                EnvGuard::AutoSync => {
                    if let Some(issue) = EnvironmentIssue::from_details(&user.details) {
                        if issue.auto_fixable() {
                            auto_sync_environment(ctx, snapshot, issue)
                        } else {
                            Err(user.into())
                        }
                    } else {
                        Err(user.into())
                    }
                }
            },
            Err(err) => Err(err),
        },
    }
}

fn log_autosync_step(message: &str) {
    eprintln!("px ▸ {message}");
}

pub(crate) fn auto_sync_environment(
    ctx: &CommandContext,
    snapshot: &ManifestSnapshot,
    issue: EnvironmentIssue,
) -> Result<Option<EnvironmentSyncReport>> {
    if issue.needs_lock_resolution() {
        if let Some(message) = issue.lock_message() {
            log_autosync_step(message);
        }
        install_snapshot(ctx, snapshot, false, None)?;
    }
    log_autosync_step(issue.env_message());
    refresh_project_site(snapshot, ctx)?;
    Ok(Some(EnvironmentSyncReport::new(issue)))
}

pub(crate) fn attach_autosync_details(
    outcome: &mut ExecutionOutcome,
    report: Option<EnvironmentSyncReport>,
) {
    let Some(report) = report else {
        return;
    };
    let autosync = report.to_json();
    match outcome.details {
        Value::Object(ref mut map) => {
            map.insert("autosync".to_string(), autosync);
        }
        Value::Null => {
            outcome.details = json!({ "autosync": autosync });
        }
        ref mut other => {
            let previous = other.take();
            outcome.details = json!({
                "value": previous,
                "autosync": autosync,
            });
        }
    }
}
