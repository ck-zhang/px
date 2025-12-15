//! Repo-snapshot CAS object support.
//!
//! Mapping note (for reviewers):
//! - Old: `core/store/cas/repo_snapshot.rs`
//! - New:
//!   - public entrypoints + re-exports: `repo_snapshot/mod.rs`
//!   - issue codes/reasons + redaction helpers: `repo_snapshot/errors.rs`
//!   - `RepoSnapshotSpec` parsing: `repo_snapshot/spec.rs`
//!   - locator/spec normalization: `repo_snapshot/resolve.rs`
//!   - deterministic lookup key: `repo_snapshot/keys.rs`
//!   - global-store wrappers: `repo_snapshot/global.rs`
//!   - CAS store operations: `repo_snapshot/store.rs`
//!   - materialization (decode/unpack): `repo_snapshot/materialize.rs`

mod errors;
mod global;
mod keys;
mod materialize;
mod resolve;
mod spec;
mod store;

pub use global::{ensure_repo_snapshot, lookup_repo_snapshot_oid, materialize_repo_snapshot};
pub use keys::repo_snapshot_lookup_key;
pub use spec::RepoSnapshotSpec;
