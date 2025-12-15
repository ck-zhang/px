use super::super::RepoSnapshotHeader;

/// Deterministic key for a commit-pinned repository snapshot.
#[must_use]
pub fn repo_snapshot_lookup_key(header: &RepoSnapshotHeader) -> String {
    format!(
        "{}|{}|{}",
        header.locator,
        header.commit,
        header.subdir.as_deref().unwrap_or("")
    )
}
