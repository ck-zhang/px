use anyhow::Result;
use serde_json::{json, Value};

use crate::{
    compute_lock_hash, ensure_env_matches_lock, load_project_state, CommandContext,
    InstallUserError, ManifestSnapshot,
};
use px_domain::{detect_lock_drift, load_lockfile_optional};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectStateKind {
    Uninitialized,
    InitializedEmpty,
    NeedsLock,
    NeedsEnv,
    Consistent,
}

impl ProjectStateKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ProjectStateKind::Uninitialized => "uninitialized",
            ProjectStateKind::InitializedEmpty => "initialized-empty",
            ProjectStateKind::NeedsLock => "needs-lock",
            ProjectStateKind::NeedsEnv => "needs-env",
            ProjectStateKind::Consistent => "consistent",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ProjectStateReport {
    pub manifest_exists: bool,
    pub lock_exists: bool,
    pub env_exists: bool,
    pub manifest_clean: bool,
    pub env_clean: bool,
    pub canonical: ProjectStateKind,
    pub manifest_fingerprint: Option<String>,
    pub lock_fingerprint: Option<String>,
    pub lock_id: Option<String>,
    pub env_issue: Option<Value>,
}

impl ProjectStateReport {
    #[must_use]
    pub fn is_consistent(&self) -> bool {
        matches!(
            self.canonical,
            ProjectStateKind::Consistent | ProjectStateKind::InitializedEmpty
        )
    }

    #[must_use]
    pub fn flags_json(&self) -> Value {
        json!({
            "manifest_exists": self.manifest_exists,
            "lock_exists": self.lock_exists,
            "env_exists": self.env_exists,
            "manifest_clean": self.manifest_clean,
            "env_clean": self.env_clean,
            "consistent": self.is_consistent(),
        })
    }
}

pub(crate) fn evaluate_project_state(
    ctx: &CommandContext,
    snapshot: &ManifestSnapshot,
) -> Result<ProjectStateReport> {
    let manifest_exists = snapshot.manifest_path.exists();
    let manifest_fingerprint = manifest_exists.then(|| snapshot.manifest_fingerprint.clone());
    let lock = load_lockfile_optional(&snapshot.lock_path)?;
    let lock_exists = lock.is_some();
    let lock_fingerprint = lock
        .as_ref()
        .and_then(|lock| lock.manifest_fingerprint.clone());
    let mut manifest_clean = false;
    if manifest_exists && lock_exists {
        manifest_clean = match (&manifest_fingerprint, &lock_fingerprint) {
            (Some(manifest), Some(lock_fp)) => manifest == lock_fp,
            (Some(_), None) => detect_lock_drift(snapshot, lock.as_ref().unwrap(), None).is_empty(),
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

    let state = load_project_state(ctx.fs(), &snapshot.root);
    let env_exists = state.current_env.is_some();
    let mut env_clean = false;
    let mut env_issue = None;
    if manifest_clean {
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

    let canonical = if !manifest_exists {
        ProjectStateKind::Uninitialized
    } else if !lock_exists || !manifest_clean {
        ProjectStateKind::NeedsLock
    } else if !env_clean {
        ProjectStateKind::NeedsEnv
    } else if snapshot.dependencies.is_empty() {
        ProjectStateKind::InitializedEmpty
    } else {
        ProjectStateKind::Consistent
    };

    Ok(ProjectStateReport {
        manifest_exists,
        lock_exists,
        env_exists,
        manifest_clean,
        env_clean,
        canonical,
        manifest_fingerprint,
        lock_fingerprint,
        lock_id,
        env_issue,
    })
}
