use std::{
    borrow::Cow,
    collections::HashSet,
    env,
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Read, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Context, Result};
use base64::prelude::{Engine as _, BASE64_STANDARD_NO_PAD};
use flate2::{read::GzDecoder, Compression, GzBuilder};
use fs4::FileExt;
use rand::{seq::IteratorRandom, thread_rng};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::OnceLock;
use tar::{Archive, Header};
#[cfg(test)]
use tempfile::tempdir;
use tracing::{debug, warn};
const OBJECTS_DIR: &str = "objects";
const LOCKS_DIR: &str = "locks";
const TMP_DIR: &str = "tmp";
const INDEX_FILENAME: &str = "index.sqlite";
const CAS_FORMAT_VERSION: u32 = 2;
const SCHEMA_VERSION: u32 = 1;
const META_KEY_CAS_FORMAT_VERSION: &str = "cas_format_version";
const META_KEY_SCHEMA_VERSION: &str = "schema_version";
const META_KEY_CREATED_BY: &str = "created_by_px_version";
const META_KEY_LAST_USED: &str = "last_used_px_version";
const PX_VERSION: &str = env!("CARGO_PKG_VERSION");
pub(crate) const MATERIALIZED_PKG_BUILDS_DIR: &str = "pkg-builds";
pub(crate) const MATERIALIZED_RUNTIMES_DIR: &str = "runtimes";
pub(crate) const MATERIALIZED_REPO_SNAPSHOTS_DIR: &str = "repo-snapshots";
mod archive;
mod doctor;
mod gc;
mod keys;
mod repo_snapshot;
mod store_impl;

pub use archive::{archive_dir_canonical, archive_selected_filtered};
pub use gc::run_gc_with_env_policy;
pub use keys::{pkg_build_lookup_key, source_lookup_key};
pub use repo_snapshot::{
    ensure_repo_snapshot, lookup_repo_snapshot_oid, materialize_repo_snapshot,
    repo_snapshot_lookup_key, RepoSnapshotSpec,
};

use archive::archive_dir_canonical_to_writer;

/// Errors surfaced by the CAS.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum StoreError {
    #[error("[PX800] CAS object {oid} is missing")]
    MissingObject { oid: String },
    #[error("[PX800] CAS object {oid} digest mismatch (expected {expected}, found {actual})")]
    DigestMismatch {
        oid: String,
        expected: String,
        actual: String,
    },
    #[error("[PX800] CAS object {oid} metadata mismatch (expected {expected:?}, found {found:?})")]
    KindMismatch {
        oid: String,
        expected: ObjectKind,
        found: ObjectKind,
    },
    #[error("[PX800] CAS object {oid} size mismatch (expected {expected}, found {found})")]
    SizeMismatch {
        oid: String,
        expected: u64,
        found: u64,
    },
    #[error("[PX800] CAS object {oid} decode failed: {error}")]
    DecodeFailure { oid: String, error: String },
    #[error("[PX811] CAS index is corrupt: {0}")]
    IndexCorrupt(String),
    #[error("[PX812] CAS metadata is missing required key '{0}'")]
    MissingMeta(String),
    #[error(
        "[PX812] CAS format/schema incompatible for {key}: expected {expected}, found {found}"
    )]
    IncompatibleFormat {
        key: String,
        expected: String,
        found: String,
    },
    #[error("[PX810] CAS store write failed: {0}")]
    StoreWriteFailure(String),
    #[error("[PX800] Unknown object kind '{0}'")]
    UnknownKind(String),
    #[error("[PX800] Unknown owner type '{0}'")]
    UnknownOwnerType(String),
}

impl StoreError {
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::MissingObject { .. }
            | Self::DigestMismatch { .. }
            | Self::KindMismatch { .. }
            | Self::SizeMismatch { .. }
            | Self::DecodeFailure { .. }
            | Self::UnknownKind(_)
            | Self::UnknownOwnerType(_) => {
                crate::core::tooling::diagnostics::cas::MISSING_OR_CORRUPT
            }
            Self::StoreWriteFailure(_) => {
                crate::core::tooling::diagnostics::cas::STORE_WRITE_FAILURE
            }
            Self::IndexCorrupt(_) => crate::core::tooling::diagnostics::cas::INDEX_CORRUPT,
            Self::MissingMeta(_) | Self::IncompatibleFormat { .. } => {
                crate::core::tooling::diagnostics::cas::FORMAT_INCOMPATIBLE
            }
        }
    }
}

/// Object kinds defined by the CAS design.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ObjectKind {
    Source,
    PkgBuild,
    Runtime,
    RepoSnapshot,
    Profile,
    Meta,
}

impl ObjectKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::PkgBuild => "pkg-build",
            Self::Runtime => "runtime",
            Self::RepoSnapshot => "repo-snapshot",
            Self::Profile => "profile",
            Self::Meta => "meta",
        }
    }
}

impl TryFrom<&str> for ObjectKind {
    type Error = StoreError;

    fn try_from(value: &str) -> std::result::Result<Self, Self::Error> {
        match value {
            "source" => Ok(Self::Source),
            "pkg-build" => Ok(Self::PkgBuild),
            "runtime" => Ok(Self::Runtime),
            "repo-snapshot" => Ok(Self::RepoSnapshot),
            "profile" => Ok(Self::Profile),
            "meta" => Ok(Self::Meta),
            other => Err(StoreError::UnknownKind(other.to_string())),
        }
    }
}

/// High-level owner categories that keep objects live.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OwnerType {
    ProjectEnv,
    WorkspaceEnv,
    ToolEnv,
    Runtime,
    Profile,
}

impl OwnerType {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProjectEnv => "project-env",
            Self::WorkspaceEnv => "workspace-env",
            Self::ToolEnv => "tool-env",
            Self::Runtime => "runtime",
            Self::Profile => "profile",
        }
    }
}

impl TryFrom<&str> for OwnerType {
    type Error = StoreError;

    fn try_from(value: &str) -> std::result::Result<Self, Self::Error> {
        match value {
            "project-env" => Ok(Self::ProjectEnv),
            "workspace-env" => Ok(Self::WorkspaceEnv),
            "tool-env" => Ok(Self::ToolEnv),
            "runtime" => Ok(Self::Runtime),
            "profile" => Ok(Self::Profile),
            other => Err(StoreError::UnknownOwnerType(other.to_string())),
        }
    }
}

/// Concrete owner identifier.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerId {
    pub owner_type: OwnerType,
    pub owner_id: String,
}

/// Small set of metadata persisted alongside an object.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectInfo {
    pub oid: String,
    pub kind: ObjectKind,
    pub size: u64,
    pub created_at: u64,
    pub last_accessed: u64,
}

/// A stored object on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredObject {
    pub oid: String,
    pub path: PathBuf,
    pub size: u64,
    pub kind: ObjectKind,
}

/// Canonical source metadata baked into the payload header.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceHeader {
    pub name: String,
    pub version: String,
    pub filename: String,
    pub index_url: String,
    pub sha256: String,
}

/// Canonical pkg-build metadata baked into the payload header.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PkgBuildHeader {
    pub source_oid: String,
    pub runtime_abi: String,
    #[serde(default = "default_builder_id")]
    pub builder_id: String,
    pub build_options_hash: String,
}

fn default_builder_id() -> String {
    "legacy-builder-v0".to_string()
}

/// Canonical runtime metadata baked into the payload header.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeHeader {
    pub version: String,
    pub abi: String,
    pub platform: String,
    pub build_config_hash: String,
    #[serde(default = "default_runtime_exe_path")]
    pub exe_path: String,
}

fn default_runtime_exe_path() -> String {
    "bin/python".to_string()
}

/// Canonical repo-snapshot metadata baked into the payload header.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoSnapshotHeader {
    /// Canonical repository locator (e.g. `git+file:///abs/path/to/repo`).
    pub locator: String,
    /// Pinned commit identifier (full SHA-1/hex expected).
    pub commit: String,
    /// Optional subdirectory root within the repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subdir: Option<String>,
}

/// Canonical profile metadata baked into the payload header.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfilePackage {
    pub name: String,
    pub version: String,
    pub pkg_build_oid: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileHeader {
    pub runtime_oid: String,
    pub packages: Vec<ProfilePackage>,
    pub sys_path_order: Vec<String>,
    pub env_vars: BTreeMap<String, serde_json::Value>,
}

/// Object payloads understood by the CAS, with canonical headers for each kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObjectPayload<'a> {
    Source {
        header: SourceHeader,
        bytes: Cow<'a, [u8]>,
    },
    PkgBuild {
        header: PkgBuildHeader,
        /// Canonical, normalized filesystem archive (e.g., tarball).
        archive: Cow<'a, [u8]>,
    },
    Runtime {
        header: RuntimeHeader,
        /// Canonical, normalized runtime archive (e.g., tarball).
        archive: Cow<'a, [u8]>,
    },
    RepoSnapshot {
        header: RepoSnapshotHeader,
        /// Canonical, normalized repository snapshot archive (e.g., tarball).
        archive: Cow<'a, [u8]>,
    },
    Profile {
        header: ProfileHeader,
    },
    Meta {
        bytes: Cow<'a, [u8]>,
    },
}

/// Hydrated object loaded from the store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadedObject {
    Source {
        header: SourceHeader,
        bytes: Vec<u8>,
        oid: String,
    },
    PkgBuild {
        header: PkgBuildHeader,
        archive: Vec<u8>,
        oid: String,
    },
    Runtime {
        header: RuntimeHeader,
        archive: Vec<u8>,
        oid: String,
    },
    RepoSnapshot {
        header: RepoSnapshotHeader,
        archive: Vec<u8>,
        oid: String,
    },
    Profile {
        header: ProfileHeader,
        oid: String,
    },
    Meta {
        bytes: Vec<u8>,
        oid: String,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GcSummary {
    pub scanned: usize,
    pub reclaimed: usize,
    pub reclaimed_bytes: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DoctorSummary {
    pub partials_removed: u64,
    pub objects_removed: usize,
    pub missing_objects: usize,
    pub corrupt_objects: usize,
    pub refs_pruned: usize,
    pub keys_pruned: usize,
    pub locked_skipped: usize,
}

#[derive(Debug, Default)]
struct StoreHealth {
    permissions_checked: AtomicBool,
    index_validated: AtomicBool,
}

/// Content-addressable store responsible for persisting immutable objects and
/// tracking their owners.
#[derive(Clone)]
pub struct ContentAddressableStore {
    root: PathBuf,
    envs_root: PathBuf,
    root_is_default: bool,
    health: Arc<StoreHealth>,
}

impl std::fmt::Debug for ContentAddressableStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContentAddressableStore")
            .field("root", &self.root)
            .field("envs_root", &self.envs_root)
            .field("root_is_default", &self.root_is_default)
            .field(
                "health_checked",
                &self.health.permissions_checked.load(Ordering::Relaxed),
            )
            .finish()
    }
}

/// Deterministic, local-root fingerprint used when deriving owner ids.
pub(crate) fn root_fingerprint(root: &Path) -> Result<String> {
    let canonical = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    Ok(hex::encode(Sha256::digest(
        canonical.display().to_string().as_bytes(),
    )))
}

fn default_tools_root_path() -> Result<PathBuf> {
    if let Some(dir) = env::var_os("PX_TOOLS_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let home = dirs_next::home_dir().context("failed to resolve HOME for tools")?;
    Ok(home.join(".px").join("tools"))
}

fn store_write_error(err: anyhow::Error) -> anyhow::Error {
    if err.is::<StoreError>() {
        err
    } else {
        StoreError::StoreWriteFailure(err.to_string()).into()
    }
}

fn remove_write_permissions(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    let mut perms = metadata.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = perms.mode();
        let new_mode = mode & !0o222;
        if mode != new_mode {
            perms.set_mode(new_mode);
            fs::set_permissions(path, perms)?;
        }
    }
    #[cfg(not(unix))]
    {
        if !perms.readonly() {
            perms.set_readonly(true);
            fs::set_permissions(path, perms)?;
        }
    }
    Ok(())
}

pub(crate) fn make_read_only_recursive(path: &Path) -> Result<()> {
    remove_write_permissions(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_dir() {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            make_read_only_recursive(&entry.path())?;
        }
    }
    Ok(())
}

fn fsync_dir(dir: &Path) -> Result<()> {
    let file = File::open(dir)?;
    file.sync_all()?;
    Ok(())
}

fn timestamp_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn file_modified_secs(path: &Path) -> Option<u64> {
    fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

fn default_root() -> Result<PathBuf> {
    if let Some(path) = env::var_os("PX_STORE_PATH") {
        return Ok(PathBuf::from(path));
    }
    let home = dirs_next::home_dir().context("failed to resolve HOME for CAS")?;
    Ok(home.join(".px").join("store"))
}

fn default_envs_root_path() -> Result<PathBuf> {
    if let Some(path) = env::var_os("PX_ENVS_PATH") {
        return Ok(PathBuf::from(path));
    }
    let home = dirs_next::home_dir().context("failed to resolve HOME for envs")?;
    Ok(home.join(".px").join("envs"))
}

fn canonical_bytes(payload: &ObjectPayload<'_>) -> Result<Vec<u8>> {
    let canonical = match payload {
        ObjectPayload::Source { header, bytes } => CanonicalObject {
            kind: ObjectKind::Source,
            data: CanonicalData::Source {
                header: header.clone(),
                payload: bytes.to_vec(),
            },
        },
        ObjectPayload::PkgBuild { header, archive } => CanonicalObject {
            kind: ObjectKind::PkgBuild,
            data: CanonicalData::PkgBuild {
                header: header.clone(),
                payload: archive.to_vec(),
            },
        },
        ObjectPayload::Runtime { header, archive } => CanonicalObject {
            kind: ObjectKind::Runtime,
            data: CanonicalData::Runtime {
                header: header.clone(),
                payload: archive.to_vec(),
            },
        },
        ObjectPayload::RepoSnapshot { header, archive } => CanonicalObject {
            kind: ObjectKind::RepoSnapshot,
            data: CanonicalData::RepoSnapshot {
                header: header.clone(),
                payload: archive.to_vec(),
            },
        },
        ObjectPayload::Profile { header } => CanonicalObject {
            kind: ObjectKind::Profile,
            data: CanonicalData::Profile {
                header: {
                    let mut clone = header.clone();
                    clone.packages.sort_by(|a, b| {
                        a.name
                            .cmp(&b.name)
                            .then(a.version.cmp(&b.version))
                            .then(a.pkg_build_oid.cmp(&b.pkg_build_oid))
                    });
                    clone
                },
            },
        },
        ObjectPayload::Meta { bytes } => CanonicalObject {
            kind: ObjectKind::Meta,
            data: CanonicalData::Meta {
                payload: bytes.to_vec(),
            },
        },
    };
    let mut value =
        serde_json::to_value(&canonical).context("failed to encode canonical CAS payload")?;
    sort_json_value(&mut value);
    serde_json::to_vec(&value).context("failed to encode canonical CAS payload")
}

fn canonical_kind(bytes: &[u8]) -> Result<ObjectKind> {
    let canonical: CanonicalObject = serde_json::from_slice(bytes)
        .context("failed to decode canonical payload for kind detection")?;
    Ok(canonical.kind)
}

fn canonical_kind_from_path(oid: &str, path: &Path) -> Result<ObjectKind> {
    const NEEDLE: &[u8] = b"\"kind\":\"";

    let mut file = File::open(path)
        .with_context(|| format!("failed to open CAS object at {}", path.display()))?;
    let mut buf = [0u8; 32 * 1024];
    let mut needle_pos = 0usize;
    let mut reading_kind = false;
    let mut kind_bytes: Vec<u8> = Vec::new();

    loop {
        let read = file
            .read(&mut buf)
            .with_context(|| format!("failed to read CAS object at {}", path.display()))?;
        if read == 0 {
            break;
        }

        for &byte in &buf[..read] {
            if !reading_kind {
                if byte == NEEDLE[needle_pos] {
                    needle_pos += 1;
                    if needle_pos == NEEDLE.len() {
                        reading_kind = true;
                    }
                } else {
                    needle_pos = if byte == NEEDLE[0] { 1 } else { 0 };
                }
                continue;
            }

            if byte == b'"' {
                let kind =
                    std::str::from_utf8(&kind_bytes).map_err(|err| StoreError::DecodeFailure {
                        oid: oid.to_string(),
                        error: err.to_string(),
                    })?;
                return Ok(ObjectKind::try_from(kind)?);
            }

            kind_bytes.push(byte);
            if kind_bytes.len() > 64 {
                return Err(StoreError::DecodeFailure {
                    oid: oid.to_string(),
                    error: "CAS object kind is too long".to_string(),
                }
                .into());
            }
        }
    }

    Err(StoreError::DecodeFailure {
        oid: oid.to_string(),
        error: "CAS object kind not found".to_string(),
    }
    .into())
}

fn sort_json_value(value: &mut Value) {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<_> = std::mem::take(map).into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            for (_, v) in entries.iter_mut() {
                sort_json_value(v);
            }
            map.extend(entries);
        }
        Value::Array(items) => {
            for item in items {
                sort_json_value(item);
            }
        }
        _ => {}
    }
}

static GLOBAL_STORE: OnceLock<ContentAddressableStore> = OnceLock::new();

/// Access the global CAS rooted at the default location.
pub fn global_store() -> &'static ContentAddressableStore {
    #[cfg(test)]
    ensure_test_store_env();
    GLOBAL_STORE.get_or_init(|| {
        let _timing = crate::tooling::timings::TimingGuard::new("global_store_init");
        let store = ContentAddressableStore::new(None).expect("CAS initialization");
        // Best-effort cleanup of leftover partials to keep the store tidy.
        let _ = store.sweep_partials();
        store
    })
}

#[cfg(test)]
fn ensure_test_store_env() {
    static TEST_ROOT: OnceLock<tempfile::TempDir> = OnceLock::new();
    let root_dir = TEST_ROOT
        .get_or_init(|| tempdir().expect("temp store root"))
        .path()
        .to_path_buf();
    let store = root_dir.join("store");
    let envs = root_dir.join("envs");
    std::env::set_var("PX_STORE_PATH", &store);
    std::env::set_var("PX_ENVS_PATH", &envs);
}

#[cfg(test)]
fn set_last_accessed(conn: &Connection, oid: &str, ts: i64) -> Result<()> {
    conn.execute(
        "UPDATE objects SET last_accessed=?1 WHERE oid=?2",
        params![ts, oid],
    )?;
    Ok(())
}

#[cfg(test)]
fn set_created_at(conn: &Connection, oid: &str, ts: i64) -> Result<()> {
    conn.execute(
        "UPDATE objects SET created_at=?1 WHERE oid=?2",
        params![ts, oid],
    )?;
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CanonicalObject {
    kind: ObjectKind,
    #[serde(flatten)]
    data: CanonicalData,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "payload_kind", rename_all = "kebab-case")]
enum CanonicalData {
    Source {
        header: SourceHeader,
        #[serde(with = "base64_bytes")]
        payload: Vec<u8>,
    },
    PkgBuild {
        header: PkgBuildHeader,
        #[serde(with = "base64_bytes")]
        payload: Vec<u8>,
    },
    Runtime {
        header: RuntimeHeader,
        #[serde(with = "base64_bytes")]
        payload: Vec<u8>,
    },
    RepoSnapshot {
        header: RepoSnapshotHeader,
        #[serde(with = "base64_bytes")]
        payload: Vec<u8>,
    },
    Profile {
        header: ProfileHeader,
    },
    Meta {
        #[serde(with = "base64_bytes")]
        payload: Vec<u8>,
    },
}

impl LoadedObject {
    #[must_use]
    pub fn kind(&self) -> ObjectKind {
        match self {
            Self::Source { .. } => ObjectKind::Source,
            Self::PkgBuild { .. } => ObjectKind::PkgBuild,
            Self::Runtime { .. } => ObjectKind::Runtime,
            Self::RepoSnapshot { .. } => ObjectKind::RepoSnapshot,
            Self::Profile { .. } => ObjectKind::Profile,
            Self::Meta { .. } => ObjectKind::Meta,
        }
    }
}

impl ObjectPayload<'_> {
    #[must_use]
    pub fn kind(&self) -> ObjectKind {
        match self {
            Self::Source { .. } => ObjectKind::Source,
            Self::PkgBuild { .. } => ObjectKind::PkgBuild,
            Self::Runtime { .. } => ObjectKind::Runtime,
            Self::RepoSnapshot { .. } => ObjectKind::RepoSnapshot,
            Self::Profile { .. } => ObjectKind::Profile,
            Self::Meta { .. } => ObjectKind::Meta,
        }
    }
}

mod base64_bytes {
    use super::*;
    use serde::de::Error;

    pub fn serialize<S: serde::Serializer>(
        bytes: &Vec<u8>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let encoded = BASE64_STANDARD_NO_PAD.encode(bytes);
        serializer.serialize_str(&encoded)
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(deserializer)?;
        BASE64_STANDARD_NO_PAD
            .decode(s.as_bytes())
            .map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests;
