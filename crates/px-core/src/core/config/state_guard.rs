use serde_json::json;

use crate::{
    missing_project_outcome,
    project::{evaluate_project_state, MutationCommand},
    ExecutionOutcome,
};
use px_domain::{ProjectSnapshot, ProjectStateReport};

#[derive(Clone, Copy, Debug)]
pub(crate) enum StateViolation {
    MissingManifest,
    MissingLock,
    ManifestDrift,
    EnvDrift,
}

impl StateViolation {
    pub(crate) fn into_outcome(
        self,
        snapshot: &ProjectSnapshot,
        command: &str,
        state_report: &ProjectStateReport,
    ) -> ExecutionOutcome {
        match self {
            StateViolation::MissingManifest => missing_project_outcome(),
            StateViolation::MissingLock => ExecutionOutcome::user_error(
                "px.lock not found",
                json!({
                    "pyproject": snapshot.manifest_path.display().to_string(),
                    "lockfile": snapshot.lock_path.display().to_string(),
                    "hint": format!("Run `px sync` before `px {command}`."),
                    "code": "PX120",
                    "reason": "missing_lock",
                }),
            ),
            StateViolation::ManifestDrift => {
                let mut details = json!({
                    "pyproject": snapshot.manifest_path.display().to_string(),
                    "lockfile": snapshot.lock_path.display().to_string(),
                    "hint": "Run `px sync` to update px.lock and the environment.",
                    "code": "PX120",
                    "reason": "lock_drift",
                });
                if let Some(fp) = &state_report.lock_fingerprint {
                    details["lock_fingerprint"] = json!(fp);
                }
                if let Some(fp) = &state_report.manifest_fingerprint {
                    details["manifest_fingerprint"] = json!(fp);
                }
                ExecutionOutcome::user_error(
                    "Project manifest has changed since px.lock was created",
                    details,
                )
            }
            StateViolation::EnvDrift => {
                let mut reason = "env_outdated".to_string();
                let mut details = json!({
                    "lockfile": snapshot.lock_path.display().to_string(),
                    "hint": format!(
                        "Run `px sync` before `px {command}` (environment is out of sync)."
                    ),
                    "code": "PX201",
                });
                if let Some(issue) = &state_report.env_issue {
                    details["environment_issue"] = issue.clone();
                    if let Some(r) = issue.get("reason").and_then(serde_json::Value::as_str) {
                        reason = r.to_string();
                    }
                }
                details["reason"] = json!(reason);
                let message = if reason == "missing_env" {
                    "project environment missing"
                } else {
                    "Project environment is out of sync with px.lock"
                };
                ExecutionOutcome::user_error(message, details)
            }
        }
    }
}

pub(crate) fn guard_for_execution(
    strict: bool,
    snapshot: &ProjectSnapshot,
    state_report: &ProjectStateReport,
    command: &'static str,
) -> Result<crate::EnvGuard, ExecutionOutcome> {
    if !state_report.lock_exists {
        return Err(StateViolation::MissingLock.into_outcome(snapshot, command, state_report));
    }

    if !state_report.manifest_clean {
        return Err(StateViolation::ManifestDrift.into_outcome(snapshot, command, state_report));
    }

    if strict {
        if !state_report.env_clean {
            return Err(StateViolation::EnvDrift.into_outcome(snapshot, command, state_report));
        }
        return Ok(crate::EnvGuard::Strict);
    }

    if state_report.env_clean {
        Ok(crate::EnvGuard::Strict)
    } else {
        Ok(crate::EnvGuard::AutoSync)
    }
}

pub(crate) fn ensure_mutation_allowed(
    snapshot: &ProjectSnapshot,
    state_report: &ProjectStateReport,
    command: MutationCommand,
) -> Result<(), ExecutionOutcome> {
    if !state_report.manifest_exists {
        return Err(missing_project_outcome());
    }
    if matches!(command, MutationCommand::Update) && !state_report.lock_exists {
        return Err(StateViolation::MissingLock.into_outcome(snapshot, "update", state_report));
    }
    if matches!(command, MutationCommand::Update)
        && state_report.lock_exists
        && !state_report.manifest_clean
    {
        return Err(StateViolation::ManifestDrift.into_outcome(snapshot, "update", state_report));
    }
    Ok(())
}

/// Convenience for commands that need state plus violations.
pub(crate) fn state_or_violation(
    ctx: &crate::CommandContext,
    snapshot: &ProjectSnapshot,
    command: &'static str,
) -> Result<ProjectStateReport, ExecutionOutcome> {
    let report = evaluate_project_state(ctx, snapshot).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to evaluate project state",
            json!({ "error": err.to_string() }),
        )
    })?;
    if !report.manifest_exists {
        return Err(StateViolation::MissingManifest.into_outcome(snapshot, command, &report));
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CommandStatus;

    fn dummy_snapshot() -> ProjectSnapshot {
        ProjectSnapshot {
            root: std::path::PathBuf::from("/proj"),
            manifest_path: std::path::PathBuf::from("/proj/pyproject.toml"),
            lock_path: std::path::PathBuf::from("/proj/px.lock"),
            name: "demo".into(),
            python_requirement: ">=3.11".into(),
            dependencies: vec![],
            python_override: None,
            manifest_fingerprint: "mf".into(),
        }
    }

    fn report(
        manifest_exists: bool,
        lock_exists: bool,
        env_exists: bool,
        manifest_clean: bool,
        env_clean: bool,
    ) -> ProjectStateReport {
        ProjectStateReport::new(
            manifest_exists,
            lock_exists,
            env_exists,
            manifest_clean,
            env_clean,
            false,
            Some("mf".into()),
            Some("lf".into()),
            Some("lid".into()),
            None,
        )
    }

    #[test]
    fn violation_codes_present() {
        let snap = dummy_snapshot();
        let rpt = report(true, false, false, false, false);
        let outcome = StateViolation::MissingLock.into_outcome(&snap, "run", &rpt);
        assert_eq!(outcome.status, CommandStatus::UserError);
        assert!(outcome
            .details
            .get("code")
            .and_then(serde_json::Value::as_str)
            .is_some());
    }

    #[test]
    fn guard_blocks_env_drift_in_strict() {
        let snap = dummy_snapshot();
        let rpt = report(true, true, true, true, false);
        let outcome = guard_for_execution(true, &snap, &rpt, "run").unwrap_err();
        assert_eq!(outcome.status, CommandStatus::UserError);
    }

    #[test]
    fn mutation_blocks_manifest_drift() {
        let snap = dummy_snapshot();
        let rpt = report(true, true, true, false, true);
        ensure_mutation_allowed(&snap, &rpt, MutationCommand::Add)
            .expect("add/remove allowed in manifest drift state");
    }

    #[test]
    fn mutation_allows_update_when_clean() {
        let snap = dummy_snapshot();
        let rpt = report(true, true, true, true, true);
        ensure_mutation_allowed(&snap, &rpt, MutationCommand::Update)
            .expect("update allowed when manifest/lock clean");
    }
}
