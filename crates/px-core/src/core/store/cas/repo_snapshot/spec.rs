use std::path::PathBuf;

use anyhow::Result;

use super::errors::{redact_repo_locator, repo_snapshot_user_error, RepoSnapshotIssue};
use super::resolve::normalize_commit_sha;

/// Specification for ensuring a `repo-snapshot` object exists in the CAS.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoSnapshotSpec {
    /// Canonical repository locator (e.g. `git+file:///abs/path/to/repo`).
    pub locator: String,
    /// Pinned commit identifier (full SHA-1/hex expected).
    pub commit: String,
    /// Optional subdirectory root within the repository.
    pub subdir: Option<PathBuf>,
}

impl RepoSnapshotSpec {
    /// Parse a commit-pinned locator of the form `git+file:///abs/path/to/repo@<sha>`.
    ///
    /// # Errors
    ///
    /// Returns an error when the locator is malformed or the commit is not a
    /// pinned full hex SHA.
    pub fn parse(locator_with_commit: &str) -> Result<Self> {
        let locator_with_commit = locator_with_commit.trim();
        let Some((locator, commit)) = locator_with_commit.rsplit_once('@') else {
            return Err(repo_snapshot_user_error(RepoSnapshotIssue::InvalidSpec {
                spec: redact_repo_locator(locator_with_commit),
            }));
        };
        let commit = normalize_commit_sha(commit).map_err(repo_snapshot_user_error)?;
        Ok(Self {
            locator: locator.to_string(),
            commit,
            subdir: None,
        })
    }
}
