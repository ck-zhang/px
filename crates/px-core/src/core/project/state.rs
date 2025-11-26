use anyhow::Result;

use crate::{
    compute_lock_hash, ensure_env_matches_lock, load_project_state, marker_env_for_snapshot,
    CommandContext, ExecutionOutcome, InstallUserError, ManifestSnapshot,
};
use px_domain::{detect_lock_drift, load_lockfile_optional, ProjectStateReport};
use serde_json::json;

use super::MutationCommand;

pub(crate) fn evaluate_project_state(
    ctx: &CommandContext,
    snapshot: &ManifestSnapshot,
) -> Result<ProjectStateReport> {
    let manifest_exists = snapshot.manifest_path.exists();
    let manifest_fingerprint = manifest_exists.then(|| snapshot.manifest_fingerprint.clone());
    let lock = load_lockfile_optional(&snapshot.lock_path)?;
    let lock_exists = lock.is_some();
    let marker_env = marker_env_for_snapshot(snapshot);
    let lock_fingerprint = lock
        .as_ref()
        .and_then(|lock| lock.manifest_fingerprint.clone());
    let mut manifest_clean = false;
    let mut lock_issue = None;
    if manifest_exists && lock_exists {
        if let Some(lock) = &lock {
            let drift = detect_lock_drift(snapshot, lock, marker_env.as_ref());
            if !drift.is_empty() {
                lock_issue = Some(drift);
            }
        }
        manifest_clean = match (&manifest_fingerprint, &lock_fingerprint) {
            (Some(manifest), Some(lock_fp)) => manifest == lock_fp,
            (Some(_), None) => lock_issue.is_none(),
            _ => false,
        };
    }
    let mut lock_id = None;
    if let Some(lock) = &lock {
        lock_id = match lock.lock_id.clone() {
            Some(id) => Some(id),
            None => Some(compute_lock_hash(&snapshot.lock_path)?),
        };
    }

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
    let env_exists = state.current_env.is_some();
    let mut env_clean = false;
    let mut env_issue = None;
    if manifest_clean && lock_issue.is_none() {
        if let Some(lock_id) = lock_id.as_deref() {
            match ensure_env_matches_lock(ctx, snapshot, lock_id) {
                Ok(()) => env_clean = true,
                Err(err) => match err.downcast::<InstallUserError>() {
                    Ok(user) => env_issue = Some(user.details),
                    Err(other) => return Err(other),
                },
            }
        }
    }

    Ok(ProjectStateReport::new(
        manifest_exists,
        lock_exists,
        env_exists,
        manifest_clean,
        env_clean,
        snapshot.requirements.is_empty(),
        manifest_fingerprint,
        lock_fingerprint,
        lock_id,
        lock_issue,
        env_issue,
    ))
}

pub(crate) fn ensure_mutation_allowed(
    snapshot: &ManifestSnapshot,
    state_report: &ProjectStateReport,
    command: MutationCommand,
) -> Result<(), ExecutionOutcome> {
    crate::state_guard::ensure_mutation_allowed(snapshot, state_report, command)
}

#[cfg(test)]
mod tests {
    use super::*;
    use px_domain::{DependencyGroupSource, ProjectStateReport};
    use std::path::PathBuf;

    #[test]
    fn mutation_gate_blocks_update_without_lock() {
        let snapshot = ManifestSnapshot {
            root: PathBuf::from("/proj"),
            manifest_path: PathBuf::from("/proj/pyproject.toml"),
            lock_path: PathBuf::from("/proj/px.lock"),
            name: "demo".into(),
            python_requirement: ">=3.11".into(),
            dependencies: vec![],
            dependency_groups: vec![],
            declared_dependency_groups: vec![],
            dependency_group_source: DependencyGroupSource::None,
            group_dependencies: vec![],
            requirements: vec![],
            python_override: None,
            px_options: px_domain::PxOptions::default(),
            manifest_fingerprint: "mf".into(),
        };
        let state = ProjectStateReport::new(
            true,
            false,
            false,
            false,
            false,
            true,
            Some("mf".into()),
            None,
            None,
            None,
            None,
        );
        let outcome =
            ensure_mutation_allowed(&snapshot, &state, MutationCommand::Update).unwrap_err();
        assert_eq!(outcome.status, crate::CommandStatus::UserError);
        assert!(outcome.message.contains("px.lock not found"));
    }

    #[test]
    fn mutation_gate_blocks_update_with_manifest_drift() {
        let snapshot = ManifestSnapshot {
            root: PathBuf::from("/proj"),
            manifest_path: PathBuf::from("/proj/pyproject.toml"),
            lock_path: PathBuf::from("/proj/px.lock"),
            name: "demo".into(),
            python_requirement: ">=3.11".into(),
            dependencies: vec![],
            dependency_groups: vec![],
            declared_dependency_groups: vec![],
            dependency_group_source: DependencyGroupSource::None,
            group_dependencies: vec![],
            requirements: vec![],
            python_override: None,
            px_options: px_domain::PxOptions::default(),
            manifest_fingerprint: "mf".into(),
        };
        let state = ProjectStateReport::new(
            true,
            true,
            false,
            false,
            false,
            true,
            Some("mf".into()),
            Some("lf".into()),
            Some("lid".into()),
            None,
            None,
        );
        let outcome =
            ensure_mutation_allowed(&snapshot, &state, MutationCommand::Update).unwrap_err();
        assert_eq!(outcome.status, crate::CommandStatus::UserError);
        assert!(outcome.message.contains("Project manifest has changed"));
    }

    #[test]
    fn mutation_gate_allows_add_without_lock() {
        let snapshot = ManifestSnapshot {
            root: PathBuf::from("/proj"),
            manifest_path: PathBuf::from("/proj/pyproject.toml"),
            lock_path: PathBuf::from("/proj/px.lock"),
            name: "demo".into(),
            python_requirement: ">=3.11".into(),
            dependencies: vec![],
            dependency_groups: vec![],
            declared_dependency_groups: vec![],
            dependency_group_source: DependencyGroupSource::None,
            group_dependencies: vec![],
            requirements: vec![],
            python_override: None,
            px_options: px_domain::PxOptions::default(),
            manifest_fingerprint: "mf".into(),
        };
        let state = ProjectStateReport::new(
            true,
            false,
            false,
            false,
            false,
            true,
            Some("mf".into()),
            None,
            None,
            None,
            None,
        );
        let outcome = ensure_mutation_allowed(&snapshot, &state, MutationCommand::Add);
        assert!(outcome.is_ok());
    }

    #[test]
    fn mutation_gate_allows_add_with_manifest_drift() {
        let snapshot = ManifestSnapshot {
            root: PathBuf::from("/proj"),
            manifest_path: PathBuf::from("/proj/pyproject.toml"),
            lock_path: PathBuf::from("/proj/px.lock"),
            name: "demo".into(),
            python_requirement: ">=3.11".into(),
            dependencies: vec![],
            dependency_groups: vec![],
            declared_dependency_groups: vec![],
            dependency_group_source: DependencyGroupSource::None,
            group_dependencies: vec![],
            requirements: vec![],
            python_override: None,
            px_options: px_domain::PxOptions::default(),
            manifest_fingerprint: "mf".into(),
        };
        let state = ProjectStateReport::new(
            true,
            true,
            false,
            false,
            false,
            true,
            Some("mf".into()),
            Some("lf".into()),
            Some("lid".into()),
            None,
            None,
        );
        let outcome = ensure_mutation_allowed(&snapshot, &state, MutationCommand::Add);
        assert!(outcome.is_ok());
    }

    #[test]
    fn mutation_gate_blocks_update_on_lock_issue() {
        let snapshot = ManifestSnapshot {
            root: PathBuf::from("/proj"),
            manifest_path: PathBuf::from("/proj/pyproject.toml"),
            lock_path: PathBuf::from("/proj/px.lock"),
            name: "demo".into(),
            python_requirement: ">=3.11".into(),
            dependencies: vec!["demo==1.0".into()],
            dependency_groups: vec![],
            declared_dependency_groups: vec![],
            dependency_group_source: DependencyGroupSource::None,
            group_dependencies: vec![],
            requirements: vec!["demo==1.0".into()],
            python_override: None,
            px_options: px_domain::PxOptions::default(),
            manifest_fingerprint: "mf".into(),
        };
        let state = ProjectStateReport::new(
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
        let outcome =
            ensure_mutation_allowed(&snapshot, &state, MutationCommand::Update).unwrap_err();
        assert_eq!(outcome.status, crate::CommandStatus::UserError);
        assert!(
            outcome.message.contains("px.lock is out of date")
                || outcome.message.contains("px.lock")
        );
    }
}
