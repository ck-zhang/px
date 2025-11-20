use serde_json::{json, Value};

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
    pub fn new(
        manifest_exists: bool,
        lock_exists: bool,
        env_exists: bool,
        manifest_clean: bool,
        env_clean: bool,
        deps_empty: bool,
        manifest_fingerprint: Option<String>,
        lock_fingerprint: Option<String>,
        lock_id: Option<String>,
        env_issue: Option<Value>,
    ) -> Self {
        let canonical = if !manifest_exists {
            ProjectStateKind::Uninitialized
        } else if !lock_exists || !manifest_clean {
            ProjectStateKind::NeedsLock
        } else if !env_clean {
            ProjectStateKind::NeedsEnv
        } else if deps_empty {
            ProjectStateKind::InitializedEmpty
        } else {
            ProjectStateKind::Consistent
        };

        Self {
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
        }
    }

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
