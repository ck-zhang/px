use std::path::Path;

use anyhow::Result;

use super::super::global_store;
use super::RepoSnapshotSpec;

/// Ensure a `repo-snapshot` exists in the global CAS.
pub fn ensure_repo_snapshot(spec: &RepoSnapshotSpec) -> Result<String> {
    global_store().ensure_repo_snapshot(spec)
}

/// Look up a `repo-snapshot` oid in the global CAS without producing it.
pub fn lookup_repo_snapshot_oid(spec: &RepoSnapshotSpec) -> Result<Option<String>> {
    global_store().lookup_repo_snapshot_oid(spec)
}

/// Materialize a `repo-snapshot` object from the global CAS into `dst`.
pub fn materialize_repo_snapshot(oid: &str, dst: &Path) -> Result<()> {
    global_store().materialize_repo_snapshot(oid, dst)
}
