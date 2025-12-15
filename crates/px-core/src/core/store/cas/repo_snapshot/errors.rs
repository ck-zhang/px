use anyhow;
use serde_json::{json, Value};
use url::Url;

/// User-facing issues while creating or materializing a `repo-snapshot` CAS object.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(super) enum RepoSnapshotIssue {
    #[error("repo-snapshot spec must be '<locator>@<commit>' (got '{spec}')")]
    InvalidSpec { spec: String },
    #[error("repo-snapshot commit must be a pinned full hex SHA (got '{commit}')")]
    InvalidCommit { commit: String },
    #[error("repo-snapshot locator must be a git URL like 'git+file:///abs/path/to/repo' (got '{locator}')")]
    InvalidLocator { locator: String },
    #[error("repo-snapshot locator must not include credentials (got '{locator}')")]
    LocatorContainsCredentials { locator: String },
    #[error("repo-snapshot locator must not include a query or fragment (got '{locator}')")]
    LocatorContainsQueryOrFragment { locator: String },
    #[error("repo-snapshot requires PX_ONLINE=1 to fetch '{locator}@{commit}' (offline mode)")]
    Offline { locator: String, commit: String },
    #[error("unsupported repo-snapshot locator '{locator}'")]
    UnsupportedLocator { locator: String },
    #[error("repo-snapshot subdir must be a relative path without '..' (got '{subdir}')")]
    InvalidSubdir { subdir: String },
    #[error("repo-snapshot subdir '{subdir}' does not exist at commit '{commit}'")]
    MissingSubdir { subdir: String, commit: String },
    #[error("git fetch failed for '{locator}@{commit}': {stderr}")]
    GitFetchFailed {
        locator: String,
        commit: String,
        stderr: String,
    },
    #[error("git archive failed for '{locator}@{commit}': {stderr}")]
    GitArchiveFailed {
        locator: String,
        commit: String,
        stderr: String,
    },
    #[error("git is required to create a repo-snapshot, but failed to invoke it: {error}")]
    GitInvocationFailed { error: String },
}

impl RepoSnapshotIssue {
    #[must_use]
    fn code(&self) -> &'static str {
        match self {
            Self::InvalidSpec { .. }
            | Self::InvalidCommit { .. }
            | Self::InvalidLocator { .. }
            | Self::LocatorContainsCredentials { .. }
            | Self::LocatorContainsQueryOrFragment { .. }
            | Self::InvalidSubdir { .. }
            | Self::MissingSubdir { .. } => "PX720",
            Self::Offline { .. } => "PX721",
            Self::UnsupportedLocator { .. } => "PX722",
            Self::GitFetchFailed { .. }
            | Self::GitArchiveFailed { .. }
            | Self::GitInvocationFailed { .. } => "PX723",
        }
    }

    #[must_use]
    fn reason(&self) -> &'static str {
        match self {
            Self::InvalidSpec { .. } => "invalid_repo_snapshot_spec",
            Self::InvalidCommit { .. } => "invalid_repo_snapshot_commit",
            Self::InvalidLocator { .. } => "invalid_repo_snapshot_locator",
            Self::LocatorContainsCredentials { .. } => "invalid_repo_snapshot_locator",
            Self::LocatorContainsQueryOrFragment { .. } => "invalid_repo_snapshot_locator",
            Self::Offline { .. } => "repo_snapshot_offline",
            Self::UnsupportedLocator { .. } => "unsupported_repo_snapshot_locator",
            Self::InvalidSubdir { .. } => "invalid_repo_snapshot_subdir",
            Self::MissingSubdir { .. } => "missing_repo_snapshot_subdir",
            Self::GitFetchFailed { .. } => "repo_snapshot_git_fetch_failed",
            Self::GitArchiveFailed { .. } => "repo_snapshot_git_archive_failed",
            Self::GitInvocationFailed { .. } => "repo_snapshot_git_unavailable",
        }
    }

    #[must_use]
    fn hint(&self) -> Option<&'static str> {
        match self {
            Self::InvalidSpec { .. } => Some("Use 'git+file:///abs/path/to/repo@<full_sha>'"),
            Self::InvalidCommit { .. } => Some("Use a full commit SHA (no branches/tags)."),
            Self::InvalidLocator { .. } => {
                Some("Use a git locator like 'git+file:///abs/path/to/repo'.")
            }
            Self::LocatorContainsCredentials { .. } => Some(
                "Remove credentials from the URL and use a git credential helper instead.",
            ),
            Self::LocatorContainsQueryOrFragment { .. } => Some(
                "Remove the query/fragment from the URL; use a plain git+https:// or git+file:// locator.",
            ),
            Self::Offline { .. } => Some(
                "Re-run with --online / set PX_ONLINE=1, or prefetch the snapshot while online.",
            ),
            Self::UnsupportedLocator { .. } => Some("Use a git+file:// or git+https:// locator."),
            Self::InvalidSubdir { .. } => Some("Use a relative subdir path (no '..')."),
            Self::MissingSubdir { .. } => Some("Check the subdir exists at the pinned commit."),
            Self::GitFetchFailed { .. } => {
                Some("Check the commit exists and the repository is accessible.")
            }
            Self::GitArchiveFailed { .. } => {
                Some("Check the commit exists and the repository is accessible.")
            }
            Self::GitInvocationFailed { .. } => Some("Install git and ensure it is on PATH."),
        }
    }

    #[must_use]
    fn details(&self) -> Value {
        let mut details = json!({
            "code": self.code(),
            "reason": self.reason(),
        });
        if let Value::Object(map) = &mut details {
            if let Some(hint) = self.hint() {
                map.insert("hint".into(), json!(hint));
            }
            match self {
                Self::InvalidSpec { spec } => {
                    map.insert("spec".into(), json!(spec));
                }
                Self::InvalidCommit { commit } => {
                    map.insert("commit".into(), json!(commit));
                }
                Self::InvalidLocator { locator }
                | Self::LocatorContainsCredentials { locator }
                | Self::LocatorContainsQueryOrFragment { locator }
                | Self::UnsupportedLocator { locator } => {
                    map.insert("locator".into(), json!(locator));
                }
                Self::Offline { locator, commit } => {
                    map.insert("locator".into(), json!(locator));
                    map.insert("commit".into(), json!(commit));
                }
                Self::InvalidSubdir { subdir } => {
                    map.insert("subdir".into(), json!(subdir));
                }
                Self::MissingSubdir { subdir, commit } => {
                    map.insert("subdir".into(), json!(subdir));
                    map.insert("commit".into(), json!(commit));
                }
                Self::GitFetchFailed {
                    locator,
                    commit,
                    stderr,
                } => {
                    map.insert("locator".into(), json!(locator));
                    map.insert("commit".into(), json!(commit));
                    map.insert("stderr".into(), json!(stderr));
                }
                Self::GitArchiveFailed {
                    locator,
                    commit,
                    stderr,
                } => {
                    map.insert("locator".into(), json!(locator));
                    map.insert("commit".into(), json!(commit));
                    map.insert("stderr".into(), json!(stderr));
                }
                Self::GitInvocationFailed { error } => {
                    map.insert("error".into(), json!(error));
                }
            }
        }
        details
    }
}

pub(super) fn repo_snapshot_user_error(issue: RepoSnapshotIssue) -> anyhow::Error {
    crate::InstallUserError::new(issue.to_string(), issue.details()).into()
}

pub(super) fn redact_repo_locator(locator: &str) -> String {
    let locator = locator.trim();
    if let Some(transport) = locator.strip_prefix("git+") {
        if let Ok(mut url) = Url::parse(transport) {
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.set_query(None);
            url.set_fragment(None);
            return format!("git+{}", url);
        }
    }

    let mut redacted = locator.to_string();
    if let Some(pos) = redacted.find('#') {
        redacted.truncate(pos);
    }
    if let Some(pos) = redacted.find('?') {
        redacted.truncate(pos);
    }
    if let Some(scheme_pos) = redacted.find("://") {
        let after_scheme = scheme_pos + 3;
        if let Some(at_rel) = redacted[after_scheme..].find('@') {
            let at_pos = after_scheme + at_rel;
            let next_slash = redacted[after_scheme..]
                .find('/')
                .map(|idx| after_scheme + idx);
            if next_slash.map(|slash| at_pos < slash).unwrap_or(true) {
                redacted.replace_range(after_scheme..at_pos, "***");
            }
        }
    }

    redacted
}
