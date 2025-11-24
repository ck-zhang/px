use serde_json::json;

use crate::{
    missing_project_outcome,
    project::{evaluate_project_state, MutationCommand},
    ExecutionOutcome,
};
use px_domain::{ProjectSnapshot, ProjectStateKind, ProjectStateReport};

#[derive(Clone, Copy, Debug)]
pub(crate) enum StateViolation {
    MissingManifest,
    MissingLock,
    LockDrift,
    EnvDrift,
}

impl StateViolation {
    pub(crate) fn into_outcome(
        self,
        snapshot: &ProjectSnapshot,
        command: &str,
        state_report: &ProjectStateReport,
    ) -> ExecutionOutcome {
        let mut base = json!({
            "pyproject": snapshot.manifest_path.display().to_string(),
            "lockfile": snapshot.lock_path.display().to_string(),
            "state": state_report.canonical.as_str(),
        });
        match self {
            StateViolation::MissingManifest => missing_project_outcome(),
            StateViolation::MissingLock => {
                let hint = if command == "sync" {
                    "Run `px sync` without --frozen to generate px.lock before syncing.".to_string()
                } else {
                    format!("Run `px sync` before `px {command}`.")
                };
                base["hint"] = json!(hint);
                base["code"] = json!("PX120");
                base["reason"] = json!("missing_lock");
                ExecutionOutcome::user_error("px.lock not found", base)
            }
            StateViolation::LockDrift => {
                let mut details = base;
                details["hint"] = json!("Run `px sync` to update px.lock and the environment.");
                details["code"] = json!("PX120");
                details["reason"] = json!("lock_drift");
                if let Some(fp) = &state_report.lock_fingerprint {
                    details["lock_fingerprint"] = json!(fp);
                }
                if let Some(fp) = &state_report.manifest_fingerprint {
                    details["manifest_fingerprint"] = json!(fp);
                }
                if let Some(lock_id) = &state_report.lock_id {
                    details["lock_id"] = json!(lock_id);
                }
                if let Some(issues) = &state_report.lock_issue {
                    details["lock_issue"] = json!(issues);
                }
                let message = if !state_report.manifest_clean {
                    "Project manifest has changed since px.lock was created"
                } else {
                    "px.lock is out of date for this project"
                };
                ExecutionOutcome::user_error(message, details)
            }
            StateViolation::EnvDrift => {
                let mut reason = "env_outdated".to_string();
                let mut details = base;
                details["hint"] = json!(format!(
                    "Run `px sync` before `px {command}` (environment is out of sync)."
                ));
                details["code"] = json!("PX201");
                if let Some(issue) = &state_report.env_issue {
                    details["environment_issue"] = issue.clone();
                    if let Some(r) = issue.get("reason").and_then(serde_json::Value::as_str) {
                        reason = r.to_string();
                    }
                }
                if let Some(lock_id) = &state_report.lock_id {
                    details["lock_id"] = json!(lock_id);
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

    if matches!(state_report.canonical, ProjectStateKind::NeedsLock) {
        return Err(StateViolation::LockDrift.into_outcome(snapshot, command, state_report));
    }

    if strict {
        if matches!(state_report.canonical, ProjectStateKind::NeedsEnv) {
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
    if matches!(command, MutationCommand::Update) {
        if !state_report.lock_exists {
            return Err(StateViolation::MissingLock.into_outcome(snapshot, "update", state_report));
        }
        if matches!(state_report.canonical, ProjectStateKind::NeedsLock) {
            return Err(StateViolation::LockDrift.into_outcome(snapshot, "update", state_report));
        }
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
    use px_domain::ProjectStateKind;

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

    #[test]
    fn guard_allows_autosync_for_needs_env_in_dev() {
        let snap = dummy_snapshot();
        let rpt = report(true, true, true, true, false);
        let guard = guard_for_execution(false, &snap, &rpt, "run").expect("auto-sync allowed");
        assert!(matches!(guard, crate::EnvGuard::AutoSync));
    }

    #[test]
    fn guard_requires_env_in_frozen_mode() {
        let snap = dummy_snapshot();
        let rpt = report(true, true, true, true, false);
        let outcome = guard_for_execution(true, &snap, &rpt, "test").unwrap_err();
        assert_eq!(outcome.status, CommandStatus::UserError);
        let guard = guard_for_execution(false, &snap, &rpt, "test").expect("dev accepts autosync");
        assert!(matches!(guard, crate::EnvGuard::AutoSync));
    }

    #[test]
    fn guard_rejects_needs_lock_for_run_and_test() {
        let snap = dummy_snapshot();
        let rpt = ProjectStateReport::new(
            true,
            true,
            true,
            true,
            true,
            false,
            Some("mf".into()),
            Some("mf".into()),
            Some("lid".into()),
            Some(vec!["mode mismatch".into()]),
            None,
        );
        assert_eq!(rpt.canonical, ProjectStateKind::NeedsLock);
        let run_outcome = guard_for_execution(false, &snap, &rpt, "run").unwrap_err();
        assert_eq!(run_outcome.status, CommandStatus::UserError);
        let test_outcome = guard_for_execution(true, &snap, &rpt, "test").unwrap_err();
        assert_eq!(test_outcome.status, CommandStatus::UserError);
    }

    #[test]
    fn guard_rejects_lock_issue_even_when_clean() {
        let snap = dummy_snapshot();
        let rpt = ProjectStateReport::new(
            true,
            true,
            true,
            true,
            true,
            false,
            Some("mf".into()),
            Some("mf".into()),
            Some("lid".into()),
            Some(vec!["mode mismatch".into()]),
            None,
        );
        assert_eq!(rpt.canonical, ProjectStateKind::NeedsLock);
        let outcome = guard_for_execution(false, &snap, &rpt, "run").unwrap_err();
        assert_eq!(outcome.status, CommandStatus::UserError);
    }
}
