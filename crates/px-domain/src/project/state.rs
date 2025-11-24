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

#[must_use]
pub fn canonical_state(
    manifest_exists: bool,
    lock_exists: bool,
    manifest_clean: bool,
    lock_issue: bool,
    env_clean: bool,
    deps_empty: bool,
) -> ProjectStateKind {
    if !manifest_exists {
        ProjectStateKind::Uninitialized
    } else if !lock_exists || !manifest_clean || lock_issue {
        ProjectStateKind::NeedsLock
    } else if !env_clean {
        ProjectStateKind::NeedsEnv
    } else if deps_empty {
        ProjectStateKind::InitializedEmpty
    } else {
        ProjectStateKind::Consistent
    }
}

#[derive(Clone, Debug)]
pub struct ProjectStateReport {
    pub manifest_exists: bool,
    pub lock_exists: bool,
    pub env_exists: bool,
    pub manifest_clean: bool,
    pub env_clean: bool,
    pub deps_empty: bool,
    pub canonical: ProjectStateKind,
    pub manifest_fingerprint: Option<String>,
    pub lock_fingerprint: Option<String>,
    pub lock_id: Option<String>,
    pub lock_issue: Option<Vec<String>>,
    pub env_issue: Option<Value>,
}

impl ProjectStateReport {
    #[allow(clippy::too_many_arguments)]
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
        lock_issue: Option<Vec<String>>,
        env_issue: Option<Value>,
    ) -> Self {
        let canonical = canonical_state(
            manifest_exists,
            lock_exists,
            manifest_clean,
            lock_issue.is_some(),
            env_clean,
            deps_empty,
        );

        Self {
            manifest_exists,
            lock_exists,
            env_exists,
            manifest_clean,
            env_clean,
            deps_empty,
            canonical,
            manifest_fingerprint,
            lock_fingerprint,
            lock_id,
            lock_issue,
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
            "deps_empty": self.deps_empty,
            "lock_issue": self.lock_issue.is_some(),
            "consistent": self.is_consistent(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_state_matrix() {
        struct Case {
            manifest_exists: bool,
            lock_exists: bool,
            manifest_clean: bool,
            lock_issue: bool,
            env_clean: bool,
            deps_empty: bool,
            expected: ProjectStateKind,
        }

        let cases = [
            Case {
                manifest_exists: false,
                lock_exists: false,
                manifest_clean: false,
                lock_issue: false,
                env_clean: false,
                deps_empty: false,
                expected: ProjectStateKind::Uninitialized,
            },
            Case {
                manifest_exists: true,
                lock_exists: false,
                manifest_clean: false,
                lock_issue: false,
                env_clean: false,
                deps_empty: false,
                expected: ProjectStateKind::NeedsLock,
            },
            Case {
                manifest_exists: true,
                lock_exists: true,
                manifest_clean: true,
                lock_issue: true,
                env_clean: true,
                deps_empty: false,
                expected: ProjectStateKind::NeedsLock,
            },
            Case {
                manifest_exists: true,
                lock_exists: true,
                manifest_clean: true,
                lock_issue: false,
                env_clean: false,
                deps_empty: false,
                expected: ProjectStateKind::NeedsEnv,
            },
            Case {
                manifest_exists: true,
                lock_exists: true,
                manifest_clean: true,
                lock_issue: false,
                env_clean: true,
                deps_empty: true,
                expected: ProjectStateKind::InitializedEmpty,
            },
            Case {
                manifest_exists: true,
                lock_exists: true,
                manifest_clean: true,
                lock_issue: false,
                env_clean: true,
                deps_empty: false,
                expected: ProjectStateKind::Consistent,
            },
        ];

        for case in cases {
            let actual = canonical_state(
                case.manifest_exists,
                case.lock_exists,
                case.manifest_clean,
                case.lock_issue,
                case.env_clean,
                case.deps_empty,
            );
            assert_eq!(
                actual, case.expected,
                "unexpected state for manifest_exists={}, lock_exists={}, manifest_clean={}, lock_issue={}, env_clean={}, deps_empty={}",
                case.manifest_exists, case.lock_exists, case.manifest_clean, case.lock_issue, case.env_clean, case.deps_empty
            );
        }
    }

    #[test]
    fn lock_issue_forces_needs_lock() {
        let report = ProjectStateReport::new(
            true,
            true,
            true,
            true,
            true,
            false,
            Some("mf".into()),
            Some("lf".into()),
            Some("lid".into()),
            Some(vec!["mode mismatch".into()]),
            None,
        );
        assert_eq!(report.canonical, ProjectStateKind::NeedsLock);
    }

    #[test]
    fn env_drift_marks_needs_env() {
        let report = ProjectStateReport::new(
            true,
            true,
            true,
            true,
            false,
            false,
            Some("mf".into()),
            Some("lf".into()),
            Some("lid".into()),
            None,
            None,
        );
        assert_eq!(report.canonical, ProjectStateKind::NeedsEnv);
    }

    #[test]
    fn initialized_empty_is_consistent_variant() {
        let report = ProjectStateReport::new(
            true,
            true,
            true,
            true,
            true,
            true,
            Some("mf".into()),
            Some("mf".into()),
            Some("lid".into()),
            None,
            None,
        );
        assert_eq!(report.canonical, ProjectStateKind::InitializedEmpty);
        assert!(report.is_consistent());
    }
}
