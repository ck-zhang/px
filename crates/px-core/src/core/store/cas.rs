use std::{
    borrow::Cow,
    collections::HashSet,
    env,
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Context, Result};
use base64::prelude::{Engine as _, BASE64_STANDARD_NO_PAD};
use flate2::{write::GzEncoder, Compression};
use fs4::FileExt;
use rand::{seq::IteratorRandom, thread_rng};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::OnceLock;
use tar::Header;
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
    pub build_options_hash: String,
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
    health: Arc<StoreHealth>,
}

impl std::fmt::Debug for ContentAddressableStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContentAddressableStore")
            .field("root", &self.root)
            .field("envs_root", &self.envs_root)
            .field(
                "health_checked",
                &self.health.permissions_checked.load(Ordering::Relaxed),
            )
            .finish()
    }
}

impl ContentAddressableStore {
    /// Initialize a store at the provided root, or the default `~/.px/store`
    /// when `None` is supplied.
    ///
    /// # Errors
    ///
    /// Returns an error if the root cannot be created or the index schema
    /// cannot be initialized.
    pub fn new(root: Option<PathBuf>) -> Result<Self> {
        let root = match root {
            Some(path) => path,
            None => default_root()?,
        };
        let envs_root = default_envs_root_path()?;
        let store = Self {
            root,
            envs_root,
            health: Arc::default(),
        };
        store.ensure_layout()?;
        Ok(store)
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[cfg(test)]
    #[must_use]
    pub fn envs_root(&self) -> &Path {
        &self.envs_root
    }

    /// Compute the oid for a given object payload using sha256 over the
    /// canonical encoding.
    pub fn compute_oid(payload: &ObjectPayload<'_>) -> Result<String> {
        let canonical = canonical_bytes(payload)?;
        Ok(hex::encode(Sha256::digest(canonical)))
    }

    /// Store a typed payload, returning the materialized object metadata. If
    /// the object already exists, its integrity is checked before returning.
    pub fn store(&self, payload: &ObjectPayload<'_>) -> Result<StoredObject> {
        self.ensure_layout()?;
        let canonical = canonical_bytes(payload)?;
        let oid = hex::encode(Sha256::digest(&canonical));
        let _lock = self.acquire_lock(&oid)?;
        let tmp = self.tmp_path(&oid);
        if tmp.exists() {
            let _ = fs::remove_file(&tmp);
        }
        let object_path = self.object_path(&oid);

        if object_path.exists() {
            self.verify_existing(&oid, &object_path)?;
            self.ensure_index_entry(&oid, payload.kind(), canonical.len() as u64)?;
            if let ObjectPayload::Runtime { header, .. } = payload {
                let _ = self.write_runtime_manifest(&oid, header);
            }
            debug!(%oid, kind=%payload.kind().as_str(), "cas hit");
            return Ok(StoredObject {
                oid,
                path: object_path,
                size: canonical.len() as u64,
                kind: payload.kind(),
            });
        }

        self.write_new_object(&oid, &canonical, &object_path)
            .map_err(store_write_error)?;
        self.ensure_index_entry(&oid, payload.kind(), canonical.len() as u64)
            .map_err(store_write_error)?;
        if let ObjectPayload::Runtime { header, .. } = payload {
            let _ = self.write_runtime_manifest(&oid, header);
        }
        debug!(%oid, kind=%payload.kind().as_str(), "cas store");
        Ok(StoredObject {
            oid,
            path: object_path,
            size: canonical.len() as u64,
            kind: payload.kind(),
        })
    }

    /// Load a typed object, verifying its digest and returning the structured
    /// metadata/payload.
    pub fn load(&self, oid: &str) -> Result<LoadedObject> {
        self.ensure_layout()?;
        let mut conn = self.connection()?;
        let info = match self.object_info_with_conn(&conn, oid)? {
            Some(info) => info,
            None => self
                .repair_object_index_from_disk(&mut conn, oid)?
                .ok_or_else(|| StoreError::MissingObject {
                    oid: oid.to_string(),
                })?,
        };

        let object_path = self.object_path(oid);
        if !object_path.exists() {
            return Err(StoreError::MissingObject {
                oid: oid.to_string(),
            }
            .into());
        }

        let bytes = fs::read(&object_path)
            .with_context(|| format!("failed to read CAS object at {}", object_path.display()))?;
        self.verify_bytes(oid, &bytes)?;
        let now = timestamp_secs();
        self.touch_object(&mut conn, oid, now)?;
        let loaded = self.decode_object(oid, &bytes)?;
        if let LoadedObject::Runtime { header, .. } = &loaded {
            let _ = self.write_runtime_manifest(oid, header);
        }

        if loaded.kind() != info.kind {
            return Err(StoreError::KindMismatch {
                oid: oid.to_string(),
                expected: info.kind,
                found: loaded.kind(),
            }
            .into());
        }

        debug!(%oid, kind=?loaded.kind().as_str(), "cas hit");
        Ok(loaded)
    }

    /// Attach an owner reference to an oid, preventing it from being reclaimed.
    pub fn add_ref(&self, owner: &OwnerId, oid: &str) -> Result<()> {
        self.ensure_layout()?;
        self.ensure_object_present_in_index(oid)?;
        self.with_immediate_tx(|tx| {
            self.assert_object_known(tx, oid)?;
            tx.execute(
                "INSERT OR IGNORE INTO refs(owner_type, owner_id, oid) VALUES (?1, ?2, ?3)",
                params![owner.owner_type.as_str(), owner.owner_id, oid],
            )?;
            Ok(())
        })
    }

    /// Remove an owner reference; returns whether a row was deleted.
    pub fn remove_ref(&self, owner: &OwnerId, oid: &str) -> Result<bool> {
        self.ensure_layout()?;
        self.with_immediate_tx(|tx| {
            let deleted = tx.execute(
                "DELETE FROM refs WHERE owner_type=?1 AND owner_id=?2 AND oid=?3",
                params![owner.owner_type.as_str(), owner.owner_id, oid],
            )?;
            Ok(deleted > 0)
        })
    }

    /// Remove all references owned by a specific owner (across all oids).
    pub fn remove_owner_refs(&self, owner: &OwnerId) -> Result<u64> {
        self.ensure_layout()?;
        self.with_immediate_tx(|tx| {
            let removed = tx.execute(
                "DELETE FROM refs WHERE owner_type=?1 AND owner_id=?2",
                params![owner.owner_type.as_str(), owner.owner_id],
            )?;
            Ok(removed as u64)
        })
    }

    /// List all owners referencing a given oid.
    pub fn refs_for(&self, oid: &str) -> Result<Vec<OwnerId>> {
        self.ensure_layout()?;
        let conn = self.connection()?;
        let mut stmt = conn.prepare("SELECT owner_type, owner_id FROM refs WHERE oid = ?1")?;
        let mut rows = stmt.query(params![oid])?;
        let mut owners = Vec::new();
        while let Some(row) = rows.next()? {
            let owner_type: String = row.get(0)?;
            let owner_id: String = row.get(1)?;
            owners.push(OwnerId {
                owner_type: OwnerType::try_from(owner_type.as_str())?,
                owner_id,
            });
        }
        Ok(owners)
    }

    /// Return metadata about an object if present in the index.
    pub fn object_info(&self, oid: &str) -> Result<Option<ObjectInfo>> {
        self.ensure_layout()?;
        let mut conn = self.connection()?;
        if let Some(info) = self.object_info_with_conn(&conn, oid)? {
            return Ok(Some(info));
        }
        self.repair_object_index_from_disk(&mut conn, oid)
    }

    fn ensure_object_present_in_index(&self, oid: &str) -> Result<()> {
        let mut conn = self.connection()?;
        if self.object_info_with_conn(&conn, oid)?.is_some() {
            return Ok(());
        }
        if self
            .repair_object_index_from_disk(&mut conn, oid)?
            .is_some()
        {
            return Ok(());
        }
        Err(StoreError::MissingObject {
            oid: oid.to_string(),
        }
        .into())
    }

    fn repair_object_index_from_disk(
        &self,
        conn: &mut Connection,
        oid: &str,
    ) -> Result<Option<ObjectInfo>> {
        let path = self.object_path(oid);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&path)
            .with_context(|| format!("failed to read CAS object at {}", path.display()))?;
        self.verify_bytes(oid, &bytes)?;
        let kind = canonical_kind(&bytes)?;
        let size = bytes.len() as u64;
        let created_at = file_modified_secs(&path).unwrap_or_else(timestamp_secs);
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("failed to start CAS index repair transaction")?;
        let existing = tx
            .query_row(
                "SELECT kind, size, created_at, last_accessed FROM objects WHERE oid = ?1",
                params![oid],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?;
        let info = if let Some((kind_str, stored_size, stored_created, stored_accessed)) = existing
        {
            let stored_kind = ObjectKind::try_from(kind_str.as_str())?;
            if stored_kind != kind {
                return Err(StoreError::KindMismatch {
                    oid: oid.to_string(),
                    expected: kind,
                    found: stored_kind,
                }
                .into());
            }
            if stored_size as u64 != size {
                return Err(StoreError::SizeMismatch {
                    oid: oid.to_string(),
                    expected: size,
                    found: stored_size as u64,
                }
                .into());
            }
            ObjectInfo {
                oid: oid.to_string(),
                kind,
                size,
                created_at: stored_created as u64,
                last_accessed: stored_accessed as u64,
            }
        } else {
            tx.execute(
                "INSERT INTO objects(oid, kind, size, created_at, last_accessed) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    oid,
                    kind.as_str(),
                    size as i64,
                    created_at as i64,
                    created_at as i64
                ],
            )?;
            ObjectInfo {
                oid: oid.to_string(),
                kind,
                size,
                created_at,
                last_accessed: created_at,
            }
        };
        tx.commit()?;
        Ok(Some(info))
    }

    /// Look up an object oid by a deterministic key; drops stale rows if the object is missing.
    pub fn lookup_key(&self, kind: ObjectKind, key: &str) -> Result<Option<String>> {
        self.ensure_layout()?;
        let conn = self.connection()?;
        let mut stmt =
            conn.prepare("SELECT oid FROM keys WHERE kind = ?1 AND lookup_key = ?2 LIMIT 1")?;
        let mut rows = stmt.query(params![kind.as_str(), key])?;
        if let Some(row) = rows.next()? {
            let oid: String = row.get(0)?;
            if let Some(info) = self.object_info_with_conn(&conn, &oid)? {
                let path = self.object_path(&oid);
                if path.exists() {
                    if let Err(err) = self.verify_existing(&oid, &path) {
                        warn!(%oid, %key, %err, "CAS object failed integrity during lookup");
                    } else {
                        return Ok(Some(oid));
                    }
                }
                // Clean up stale mapping and dead index rows.
                let _ = conn.execute("DELETE FROM objects WHERE oid = ?1", params![info.oid]);
            }
            let _ = conn.execute(
                "DELETE FROM keys WHERE kind = ?1 AND lookup_key = ?2",
                params![kind.as_str(), key],
            );
        }
        Ok(None)
    }

    /// Record or update a deterministic lookup key for an object.
    pub fn record_key(&self, kind: ObjectKind, key: &str, oid: &str) -> Result<()> {
        self.ensure_layout()?;
        let conn = self.connection()?;
        conn.execute(
            "INSERT INTO keys(kind, lookup_key, oid) VALUES(?1, ?2, ?3) \
             ON CONFLICT(lookup_key) DO UPDATE SET oid=excluded.oid",
            params![kind.as_str(), key, oid],
        )?;
        Ok(())
    }

    /// List object identifiers, optionally filtered by kind and prefix.
    pub fn list(&self, kind: Option<ObjectKind>, prefix: Option<&str>) -> Result<Vec<String>> {
        self.ensure_layout()?;
        let conn = self.connection()?;
        let (query, params): (String, Vec<String>) = match (kind, prefix) {
            (Some(kind), Some(prefix)) => (
                "SELECT oid FROM objects WHERE kind = ?1 AND oid LIKE ?2 ORDER BY oid ASC"
                    .to_string(),
                vec![kind.as_str().to_string(), format!("{prefix}%")],
            ),
            (Some(kind), None) => (
                "SELECT oid FROM objects WHERE kind = ?1 ORDER BY oid ASC".to_string(),
                vec![kind.as_str().to_string()],
            ),
            (None, Some(prefix)) => (
                "SELECT oid FROM objects WHERE oid LIKE ?1 ORDER BY oid ASC".to_string(),
                vec![format!("{prefix}%")],
            ),
            (None, None) => (
                "SELECT oid FROM objects ORDER BY oid ASC".to_string(),
                vec![],
            ),
        };

        let mut stmt = conn.prepare(&query)?;
        let mut rows = stmt.query(rusqlite::params_from_iter(params.iter()))?;

        let mut results = Vec::new();
        while let Some(row) = rows.next()? {
            results.push(row.get::<_, String>(0)?);
        }
        Ok(results)
    }

    /// Perform a mark-and-sweep GC. Objects without any references and older
    /// than the supplied grace window are deleted from disk and the index.
    pub fn garbage_collect(&self, grace: Duration) -> Result<GcSummary> {
        self.ensure_layout()?;
        self.ensure_index_health(true).map_err(|err| {
            warn!(%err, "skipping cas gc because index is unhealthy");
            err
        })?;
        let mut conn = self.connection()?;
        let live = self.live_set(&conn)?;
        let cutoff = timestamp_secs().saturating_sub(grace.as_secs());
        let mut summary = GcSummary::default();

        let mut stmt = conn.prepare("SELECT oid, kind, size, created_at FROM objects")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)? as u64,
                    row.get::<_, i64>(3)? as u64,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        drop(stmt);

        for (oid, kind_str, size, created_at) in rows {
            ObjectKind::try_from(kind_str.as_str())?;
            summary.scanned += 1;

            if live.contains(&oid) || created_at > cutoff {
                continue;
            }

            let Some(_lock) = self.try_lock_for_gc(&oid)? else {
                // Another process is using the object; skip it for now.
                continue;
            };

            if self.delete_if_unreferenced(&mut conn, &oid)? {
                summary.reclaimed += 1;
                summary.reclaimed_bytes += size;
            }
        }

        let (orphans, orphan_bytes) = self.sweep_orphaned_objects(&conn, cutoff)?;
        summary.reclaimed += orphans;
        summary.reclaimed_bytes += orphan_bytes;
        summary.scanned += orphans;

        debug!(
            scanned = summary.scanned,
            reclaimed = summary.reclaimed,
            reclaimed_bytes = summary.reclaimed_bytes,
            "cas gc sweep complete"
        );

        Ok(summary)
    }

    /// Enforce a soft size cap by reclaiming oldest unreferenced objects after an initial GC.
    pub fn garbage_collect_with_limit(&self, grace: Duration, max_bytes: u64) -> Result<GcSummary> {
        let mut summary = self.garbage_collect(grace)?;
        let mut conn = self.connection()?;
        let total: i64 =
            conn.query_row("SELECT COALESCE(SUM(size), 0) FROM objects", [], |row| {
                row.get(0)
            })?;
        if total as u64 <= max_bytes {
            return Ok(summary);
        }

        let cutoff = timestamp_secs().saturating_sub(grace.as_secs());
        let mut stmt = conn.prepare(
            "SELECT o.oid, o.size, o.last_accessed, o.created_at FROM objects o \
             LEFT JOIN refs r ON r.oid = o.oid \
             WHERE r.oid IS NULL AND o.created_at <= ?1 \
             ORDER BY o.last_accessed ASC",
        )?;
        let rows = stmt
            .query_map(params![cutoff as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)? as u64,
                    row.get::<_, i64>(2)? as u64,
                    row.get::<_, i64>(3)? as u64,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        drop(stmt);

        let mut current = total as u64;
        let mut locked = 0usize;
        for (oid, size, _, _) in rows {
            if current <= max_bytes {
                break;
            }
            let Some(_lock) = self.try_lock_for_gc(&oid)? else {
                locked += 1;
                continue;
            };
            if self.delete_if_unreferenced(&mut conn, &oid)? {
                current = current.saturating_sub(size);
                summary.reclaimed += 1;
                summary.reclaimed_bytes += size;
            }
        }
        if current > max_bytes {
            warn!(
                remaining_bytes = current,
                limit_bytes = max_bytes,
                grace_secs = grace.as_secs(),
                locked_objects = locked,
                "cas size cap respected grace window but could not reach limit"
            );
        } else {
            debug!(
                reclaimed = summary.reclaimed,
                reclaimed_bytes = summary.reclaimed_bytes,
                limit_bytes = max_bytes,
                locked_objects = locked,
                "cas size cap enforcement complete"
            );
        }
        Ok(summary)
    }

    fn sweep_orphaned_objects(&self, conn: &Connection, cutoff: u64) -> Result<(usize, u64)> {
        let objects_root = self.root.join(OBJECTS_DIR);
        if !objects_root.exists() {
            return Ok((0, 0));
        }
        let mut reclaimed = 0usize;
        let mut reclaimed_bytes = 0u64;
        for entry in walkdir::WalkDir::new(&objects_root)
            .min_depth(2)
            .max_depth(2)
        {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path().to_path_buf();
            let Some(file_name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if self.object_info_with_conn(conn, file_name)?.is_some() {
                continue;
            }
            let modified = file_modified_secs(&path).unwrap_or(0);
            if modified > cutoff {
                continue;
            }
            let Some(_lock) = self.try_lock_for_gc(file_name)? else {
                continue;
            };
            let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            // Treat digest mismatches as corrupt and remove them too.
            let _ = self.verify_existing(file_name, &path);
            let _ = fs::remove_file(&path);
            if let Some(parent) = path.parent() {
                fsync_dir(parent).ok();
            }
            let _ = conn.execute("DELETE FROM keys WHERE oid = ?1", params![file_name]);
            let _ = conn.execute("DELETE FROM refs WHERE oid = ?1", params![file_name]);
            let _ = conn.execute("DELETE FROM objects WHERE oid = ?1", params![file_name]);
            self.remove_materialized(file_name)?;
            reclaimed += 1;
            reclaimed_bytes = reclaimed_bytes.saturating_add(size);
        }
        Ok((reclaimed, reclaimed_bytes))
    }

    /// Sweep and delete any `.partial` files left over from failed writes.
    pub fn sweep_partials(&self) -> Result<u64> {
        self.ensure_layout()?;
        let mut removed = 0;
        let tmp_dir = self.root.join(TMP_DIR);
        if !tmp_dir.exists() {
            return Ok(removed);
        }
        for entry in fs::read_dir(&tmp_dir)? {
            let entry = entry?;
            if entry.file_type()?.is_file()
                && entry.file_name().to_string_lossy().contains(".partial")
            {
                fs::remove_file(entry.path())?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Verify the integrity of a subset of objects by recomputing their oids.
    pub fn verify_sample(&self, sample: usize) -> Result<Vec<String>> {
        self.ensure_layout()?;
        let conn = self.connection()?;
        let mut stmt = conn.prepare("SELECT oid FROM objects")?;
        let all = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut rng = thread_rng();
        let picked = all
            .iter()
            .cloned()
            .choose_multiple(&mut rng, sample.min(all.len()));

        let mut failures = Vec::new();
        for oid in picked {
            match self.load(&oid) {
                Ok(_) => {}
                Err(err) => failures.push(format!("{oid}: {err}")),
            }
        }
        Ok(failures)
    }

    /// Best-effort self-healing pass that removes corrupt/missing objects and stale metadata.
    pub fn doctor(&self) -> Result<DoctorSummary> {
        self.ensure_layout()?;
        let partials_removed = self.sweep_partials()?;
        let mut summary = DoctorSummary {
            partials_removed,
            ..DoctorSummary::default()
        };

        let mut conn = self.connection()?;
        let mut stmt = conn.prepare("SELECT oid, kind FROM objects")?;
        let objects = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);

        for (oid, kind_str) in objects {
            let kind = match ObjectKind::try_from(kind_str.as_str()) {
                Ok(kind) => kind,
                Err(_) => {
                    if let Some(_lock) = self.try_lock_for_gc(&oid)? {
                        let (refs_pruned, keys_pruned) =
                            self.purge_object(&mut conn, &oid, None)?;
                        summary.refs_pruned += refs_pruned;
                        summary.keys_pruned += keys_pruned;
                        summary.corrupt_objects += 1;
                        summary.objects_removed += 1;
                    } else {
                        summary.locked_skipped += 1;
                    }
                    continue;
                }
            };
            let path = self.object_path(&oid);
            let issue = if !path.exists() {
                Some(false)
            } else {
                match fs::read(&path) {
                    Ok(bytes) => {
                        if self.verify_bytes(&oid, &bytes).is_err() {
                            Some(true)
                        } else if canonical_kind(&bytes).ok() != Some(kind) {
                            Some(true)
                        } else {
                            None
                        }
                    }
                    Err(_) => Some(true),
                }
            };

            if let Some(is_corrupt) = issue {
                if let Some(_lock) = self.try_lock_for_gc(&oid)? {
                    let (refs_pruned, keys_pruned) =
                        self.purge_object(&mut conn, &oid, Some(&path))?;
                    summary.refs_pruned += refs_pruned;
                    summary.keys_pruned += keys_pruned;
                    summary.objects_removed += 1;
                    if is_corrupt {
                        summary.corrupt_objects += 1;
                    } else {
                        summary.missing_objects += 1;
                    }
                } else {
                    summary.locked_skipped += 1;
                }
            }
        }

        let (orphan_removed, _) = self.sweep_orphaned_objects(&conn, timestamp_secs())?;
        summary.objects_removed += orphan_removed;

        let (refs_pruned, keys_pruned) = self.prune_orphans(&mut conn)?;
        summary.refs_pruned += refs_pruned;
        summary.keys_pruned += keys_pruned;
        Ok(summary)
    }

    fn ensure_layout(&self) -> Result<()> {
        for dir in [OBJECTS_DIR, LOCKS_DIR, TMP_DIR] {
            fs::create_dir_all(self.root.join(dir)).with_context(|| {
                format!(
                    "failed to ensure CAS directory {}",
                    self.root.join(dir).display()
                )
            })?;
        }
        self.ensure_index_health(false)?;
        let mut conn = self.connection_raw()?;
        self.init_schema(&conn)?;
        self.ensure_meta(&mut conn)?;
        self.ensure_store_permissions();
        Ok(())
    }

    fn connection(&self) -> Result<Connection> {
        let conn = self.connection_raw()?;
        conn.busy_timeout(Duration::from_secs(10))
            .context("failed to set busy timeout for CAS index")?;
        Ok(conn)
    }

    fn with_immediate_tx<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&rusqlite::Transaction<'_>) -> Result<T>,
    {
        let mut conn = self.connection()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("failed to start CAS index transaction")?;
        let result = f(&tx)?;
        tx.commit()?;
        Ok(result)
    }

    fn connection_raw(&self) -> Result<Connection> {
        let path = self.index_path();
        let conn = Connection::open(&path)
            .with_context(|| format!("failed to open CAS index at {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", &"WAL")
            .context("failed to enable WAL for CAS index")?;
        conn.pragma_update(None, "foreign_keys", &"ON")
            .context("failed to enable foreign keys for CAS index")?;
        Ok(conn)
    }

    fn init_schema(&self, conn: &Connection) -> Result<()> {
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS objects (
                oid TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                size INTEGER NOT NULL,
                created_at INTEGER NOT NULL,
                last_accessed INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS refs (
                owner_type TEXT NOT NULL,
                owner_id TEXT NOT NULL,
                oid TEXT NOT NULL,
                PRIMARY KEY(owner_type, owner_id, oid),
                FOREIGN KEY(oid) REFERENCES objects(oid) ON DELETE CASCADE
            );
            CREATE TABLE IF NOT EXISTS keys (
                kind TEXT NOT NULL,
                lookup_key TEXT PRIMARY KEY,
                oid TEXT NOT NULL
            );
            "#,
        )
        .context("failed to initialize CAS index schema")?;
        Ok(())
    }

    fn ensure_meta(&self, conn: &mut Connection) -> Result<()> {
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("failed to start CAS meta transaction")?;
        tx.execute(
            "INSERT OR IGNORE INTO meta(key, value) VALUES (?1, ?2)",
            params![META_KEY_CAS_FORMAT_VERSION, CAS_FORMAT_VERSION.to_string()],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO meta(key, value) VALUES (?1, ?2)",
            params![META_KEY_SCHEMA_VERSION, SCHEMA_VERSION.to_string()],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO meta(key, value) VALUES (?1, ?2)",
            params![META_KEY_CREATED_BY, PX_VERSION],
        )?;
        tx.commit()?;

        self.enforce_meta_version(conn, META_KEY_CAS_FORMAT_VERSION, CAS_FORMAT_VERSION)?;
        self.enforce_meta_version(conn, META_KEY_SCHEMA_VERSION, SCHEMA_VERSION)?;
        self.record_last_used_px_version(conn)?;
        Ok(())
    }

    fn ensure_index_health(&self, force_integrity: bool) -> Result<()> {
        let already_validated = self.health.index_validated.load(Ordering::SeqCst);
        let index_missing = !self.index_path().exists();
        if !force_integrity && already_validated && !index_missing {
            return Ok(());
        }

        let require_integrity = force_integrity || !already_validated || index_missing;
        match self.validate_index(require_integrity) {
            Ok(()) => {
                self.health.index_validated.store(true, Ordering::SeqCst);
                Ok(())
            }
            Err(err) => {
                if matches!(
                    err.downcast_ref::<StoreError>(),
                    Some(StoreError::MissingMeta(_)) | Some(StoreError::IncompatibleFormat { .. })
                ) {
                    // Do not auto-repair on format/schema incompatibility; surface PX812 so the
                    // caller can migrate or clear the store per the spec.
                    return Err(err);
                }
                debug!(
                    root = %self.root.display(),
                    error = %err,
                    "cas index unhealthy; rebuilding from store"
                );
                self.rebuild_index_from_store().map_err(|rebuild_err| {
                    debug!(
                        root = %self.root.display(),
                        error = %rebuild_err,
                        "cas index rebuild failed"
                    );
                    rebuild_err
                })?;
                self.health.index_validated.store(true, Ordering::SeqCst);
                Ok(())
            }
        }
    }

    fn validate_index(&self, integrity_check: bool) -> Result<()> {
        let path = self.index_path();
        if !path.exists() {
            return Err(StoreError::IndexCorrupt("index.sqlite missing".to_string()).into());
        }
        let conn = self.connection_raw()?;
        if integrity_check {
            self.run_integrity_check(&conn)?;
        }
        self.assert_expected_tables(&conn)?;
        self.enforce_meta_version(&conn, META_KEY_CAS_FORMAT_VERSION, CAS_FORMAT_VERSION)?;
        self.enforce_meta_version(&conn, META_KEY_SCHEMA_VERSION, SCHEMA_VERSION)?;
        self.require_meta_presence(&conn, META_KEY_CREATED_BY)?;
        self.require_meta_presence(&conn, META_KEY_LAST_USED)?;
        Ok(())
    }

    fn run_integrity_check(&self, conn: &Connection) -> Result<()> {
        let mut stmt = conn.prepare("PRAGMA integrity_check")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let result: String = row.get(0)?;
            if result.to_ascii_lowercase() != "ok" {
                return Err(StoreError::IndexCorrupt(result).into());
            }
        }
        Ok(())
    }

    fn assert_expected_tables(&self, conn: &Connection) -> Result<()> {
        let mut stmt = conn.prepare(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name IN ('meta', 'objects', 'refs', 'keys')",
        )?;
        let found = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<HashSet<_>>>()?;
        let missing: Vec<&str> = ["meta", "objects", "refs", "keys"]
            .iter()
            .copied()
            .filter(|name| !found.contains(&name.to_string()))
            .collect();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(StoreError::IndexCorrupt(format!("missing tables: {}", missing.join(", "))).into())
        }
    }

    fn rebuild_index_from_store(&self) -> Result<()> {
        let index_path = self.index_path();
        let temp_index = index_path.with_extension("rebuild");
        if temp_index.exists() {
            let _ = fs::remove_file(&temp_index);
        }
        if let Some(parent) = temp_index.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut conn = Connection::open(&temp_index).with_context(|| {
            format!(
                "failed to open temporary CAS index at {}",
                temp_index.display()
            )
        })?;
        conn.pragma_update(None, "journal_mode", &"WAL")
            .context("failed to enable WAL for rebuilt CAS index")?;
        conn.pragma_update(None, "foreign_keys", &"ON")
            .context("failed to enable foreign keys for rebuilt CAS index")?;
        self.init_schema(&conn)?;
        self.ensure_meta(&mut conn)?;

        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("failed to start CAS index rebuild transaction")?;
        let objects = self.populate_objects_from_store(&tx)?;
        self.populate_refs_from_runtimes(&tx, &objects)?;
        self.populate_refs_from_envs(&tx, &objects)?;
        self.populate_refs_from_state_files(&tx, &objects)?;
        self.populate_refs_from_tools(&tx, &objects)?;
        tx.commit()?;
        self.record_last_used_px_version(&mut conn)?;
        conn.close().ok();

        if index_path.exists() {
            let _ = fs::remove_file(&index_path);
        }
        fs::rename(&temp_index, &index_path).with_context(|| {
            format!(
                "failed to move rebuilt CAS index into place ({} -> {})",
                temp_index.display(),
                index_path.display()
            )
        })?;
        if let Some(parent) = index_path.parent() {
            fsync_dir(parent).ok();
        }
        debug!(
            root = %self.root.display(),
            "cas index reconstructed from store"
        );
        Ok(())
    }

    fn populate_objects_from_store(
        &self,
        tx: &rusqlite::Transaction<'_>,
    ) -> Result<HashSet<String>> {
        let objects_root = self.root.join(OBJECTS_DIR);
        let mut inserted = HashSet::new();
        if !objects_root.exists() {
            return Ok(inserted);
        }
        let now = timestamp_secs();
        for entry in walkdir::WalkDir::new(&objects_root)
            .min_depth(2)
            .max_depth(2)
        {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    warn!(%err, "failed to walk CAS objects during index rebuild");
                    continue;
                }
            };
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path().to_path_buf();
            let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(err) => {
                    warn!(path=%path.display(), %err, "failed to read CAS object during rebuild");
                    continue;
                }
            };
            if let Err(err) = self.verify_bytes(file_name, &bytes) {
                warn!(
                    path = %path.display(),
                    %err,
                    "skipping corrupt CAS object during index rebuild"
                );
                continue;
            }
            let kind = match canonical_kind(&bytes) {
                Ok(kind) => kind,
                Err(err) => {
                    warn!(
                        path = %path.display(),
                        %err,
                        "skipping CAS object with unreadable header during rebuild"
                    );
                    continue;
                }
            };
            let created_at = file_modified_secs(&path).unwrap_or(now);
            tx.execute(
                "INSERT OR REPLACE INTO objects(oid, kind, size, created_at, last_accessed) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    file_name,
                    kind.as_str(),
                    bytes.len() as i64,
                    created_at as i64,
                    created_at as i64
                ],
            )?;
            inserted.insert(file_name.to_string());
        }
        Ok(inserted)
    }

    fn populate_refs_from_runtimes(
        &self,
        tx: &rusqlite::Transaction<'_>,
        known_objects: &HashSet<String>,
    ) -> Result<()> {
        let runtimes_root = self.root.join(MATERIALIZED_RUNTIMES_DIR);
        if !runtimes_root.exists() {
            return Ok(());
        }

        #[derive(Deserialize)]
        struct RuntimeManifest {
            runtime_oid: String,
            version: String,
            platform: String,
            #[serde(default)]
            owner_id: Option<String>,
        }

        for entry in fs::read_dir(&runtimes_root)? {
            let Ok(entry) = entry else { continue };
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let manifest_path = entry.path().join("manifest.json");
            if !manifest_path.is_file() {
                continue;
            }
            let Ok(contents) = fs::read_to_string(&manifest_path) else {
                debug!(
                    path = %manifest_path.display(),
                    "failed to read runtime manifest during index rebuild"
                );
                continue;
            };
            let Ok(manifest) = serde_json::from_str::<RuntimeManifest>(&contents) else {
                debug!(
                    path = %manifest_path.display(),
                    "failed to parse runtime manifest during index rebuild"
                );
                continue;
            };
            let owner_id = manifest
                .owner_id
                .unwrap_or_else(|| format!("runtime:{}:{}", manifest.version, manifest.platform));
            self.insert_ref_if_known(
                tx,
                OwnerType::Runtime,
                &owner_id,
                &manifest.runtime_oid,
                known_objects,
            )?;
        }
        Ok(())
    }

    fn populate_refs_from_envs(
        &self,
        tx: &rusqlite::Transaction<'_>,
        known_objects: &HashSet<String>,
    ) -> Result<()> {
        let envs_root = self.envs_root.clone();
        if !envs_root.exists() {
            return Ok(());
        }

        #[derive(Deserialize)]
        struct EnvManifest {
            profile_oid: String,
            runtime_oid: String,
            #[serde(default)]
            packages: Vec<ProfilePackage>,
        }

        for entry in fs::read_dir(&envs_root)? {
            let Ok(entry) = entry else { continue };
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let manifest_path = entry.path().join("manifest.json");
            if !manifest_path.is_file() {
                continue;
            }
            let Ok(contents) = fs::read_to_string(&manifest_path) else {
                debug!(
                    path = %manifest_path.display(),
                    "failed to read env manifest during index rebuild"
                );
                continue;
            };
            let Ok(manifest) = serde_json::from_str::<EnvManifest>(&contents) else {
                debug!(
                    path = %manifest_path.display(),
                    "failed to parse env manifest during index rebuild"
                );
                continue;
            };
            self.insert_ref_if_known(
                tx,
                OwnerType::Profile,
                &manifest.profile_oid,
                &manifest.profile_oid,
                known_objects,
            )?;
            self.insert_ref_if_known(
                tx,
                OwnerType::Profile,
                &manifest.profile_oid,
                &manifest.runtime_oid,
                known_objects,
            )?;
            for pkg in manifest.packages {
                self.insert_ref_if_known(
                    tx,
                    OwnerType::Profile,
                    &manifest.profile_oid,
                    &pkg.pkg_build_oid,
                    known_objects,
                )?;
            }
        }
        Ok(())
    }

    fn populate_refs_from_tools(
        &self,
        tx: &rusqlite::Transaction<'_>,
        known_objects: &HashSet<String>,
    ) -> Result<()> {
        let tools_root = default_tools_root_path()?;
        if !tools_root.exists() {
            return Ok(());
        }
        let mut seen = HashSet::new();
        for entry in fs::read_dir(&tools_root)? {
            let Ok(entry) = entry else { continue };
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let tool_name = entry.file_name().to_string_lossy().to_string();
            let state_path = entry.path().join(".px").join("state.json");
            if !state_path.is_file() {
                continue;
            }
            if !seen.insert(state_path.clone()) {
                continue;
            }
            let contents = match fs::read_to_string(&state_path) {
                Ok(contents) => contents,
                Err(err) => {
                    debug!(
                        path = %state_path.display(),
                        %err,
                        "failed to read tool state during index rebuild"
                    );
                    continue;
                }
            };
            let state: RebuildState = match serde_json::from_str(&contents) {
                Ok(state) => state,
                Err(err) => {
                    debug!(
                        path = %state_path.display(),
                        %err,
                        "failed to parse tool state during index rebuild"
                    );
                    continue;
                }
            };
            let Some(env) = state.current_env else {
                continue;
            };
            let Some(profile_oid) = env.profile_oid.filter(|oid| !oid.is_empty()) else {
                continue;
            };
            if !known_objects.contains(&profile_oid) {
                debug!(
                    path = %state_path.display(),
                    profile_oid,
                    "tool state referenced missing CAS object during index rebuild"
                );
                continue;
            }
            let lock_id = env.lock_id.trim();
            if lock_id.is_empty() {
                continue;
            }
            let runtime_version = state
                .runtime
                .as_ref()
                .and_then(|r| (!r.version.is_empty()).then(|| r.version.as_str()))
                .or_else(|| {
                    env.python
                        .as_ref()
                        .and_then(|py| (!py.version.is_empty()).then(|| py.version.as_str()))
                });
            let Some(runtime_version) = runtime_version else {
                continue;
            };
            let owner_id = tool_owner_id(&tool_name, lock_id, runtime_version)
                .unwrap_or_else(|_| format!("tool-env:{tool_name}:{lock_id}:{runtime_version}"));
            self.insert_ref_if_known(
                tx,
                OwnerType::ToolEnv,
                &owner_id,
                &profile_oid,
                known_objects,
            )?;
        }
        Ok(())
    }

    fn populate_refs_from_state_files(
        &self,
        tx: &rusqlite::Transaction<'_>,
        known_objects: &HashSet<String>,
    ) -> Result<()> {
        let mut seen = HashSet::new();
        for (kind, path) in state_files_to_scan()? {
            if !seen.insert(path.clone()) {
                continue;
            }
            let Some(root) = path.parent().and_then(|p| p.parent()) else {
                continue;
            };
            let contents = match fs::read_to_string(&path) {
                Ok(contents) => contents,
                Err(err) => {
                    debug!(path = %path.display(), %err, "failed to read state file during index rebuild");
                    continue;
                }
            };
            let state: RebuildState = match serde_json::from_str(&contents) {
                Ok(state) => state,
                Err(err) => {
                    debug!(path = %path.display(), %err, "failed to parse state file during index rebuild");
                    continue;
                }
            };
            let Some(env) = state.current_env else {
                continue;
            };
            let Some(profile_oid) = env.profile_oid.filter(|oid| !oid.is_empty()) else {
                continue;
            };
            if !known_objects.contains(&profile_oid) {
                debug!(
                    path = %path.display(),
                    profile_oid,
                    "state file referenced missing CAS object during index rebuild"
                );
                continue;
            }
            let lock_id = env.lock_id.trim();
            if lock_id.is_empty() {
                continue;
            }
            let runtime_version = state
                .runtime
                .as_ref()
                .and_then(|r| (!r.version.is_empty()).then(|| r.version.as_str()))
                .or_else(|| {
                    env.python
                        .as_ref()
                        .and_then(|py| (!py.version.is_empty()).then(|| py.version.as_str()))
                });
            let Some(runtime_version) = runtime_version else {
                continue;
            };
            let owner_id = match owner_id_from_state(kind, root, lock_id, runtime_version) {
                Ok(id) => id,
                Err(err) => {
                    debug!(
                        path = %path.display(),
                        %err,
                        "failed to derive owner id from state during index rebuild"
                    );
                    continue;
                }
            };
            let owner_type = match kind {
                StateFileKind::Project => OwnerType::ProjectEnv,
                StateFileKind::Workspace => OwnerType::WorkspaceEnv,
            };
            self.insert_ref_if_known(tx, owner_type, &owner_id, &profile_oid, known_objects)?;
        }
        Ok(())
    }

    fn insert_ref_if_known(
        &self,
        tx: &rusqlite::Transaction<'_>,
        owner_type: OwnerType,
        owner_id: &str,
        oid: &str,
        known_objects: &HashSet<String>,
    ) -> Result<()> {
        if known_objects.contains(oid) {
            tx.execute(
                "INSERT OR IGNORE INTO refs(owner_type, owner_id, oid) VALUES (?1, ?2, ?3)",
                params![owner_type.as_str(), owner_id, oid],
            )?;
        } else {
            debug!(
                owner_type = owner_type.as_str(),
                owner_id, oid, "env manifest referenced missing CAS object during index rebuild"
            );
        }
        Ok(())
    }

    fn ensure_store_permissions(&self) {
        if self.health.permissions_checked.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Err(err) = self.harden_store_permissions() {
            warn!(
                root = %self.root.display(),
                %err,
                "failed to harden CAS store permissions; write protections may be incomplete"
            );
        }
    }

    fn harden_store_permissions(&self) -> Result<()> {
        let objects_root = self.root.join(OBJECTS_DIR);
        if objects_root.exists() {
            for entry in walkdir::WalkDir::new(&objects_root)
                .min_depth(2)
                .max_depth(2)
            {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(err) => {
                        warn!(%err, "failed to walk CAS objects during permission hardening");
                        continue;
                    }
                };
                if !entry.file_type().is_file() {
                    continue;
                }
                if let Err(err) = make_read_only_recursive(entry.path()) {
                    warn!(
                        path = %entry.path().display(),
                        %err,
                        "failed to harden CAS object permissions"
                    );
                }
            }
        }

        for dir in [MATERIALIZED_PKG_BUILDS_DIR, MATERIALIZED_RUNTIMES_DIR] {
            let root = self.root.join(dir);
            if !root.exists() {
                continue;
            }
            for entry in fs::read_dir(&root)? {
                let Ok(entry) = entry else { continue };
                if let Err(err) = make_read_only_recursive(&entry.path()) {
                    warn!(
                        path = %entry.path().display(),
                        %err,
                        "failed to harden materialized CAS directory permissions"
                    );
                }
            }
        }
        Ok(())
    }

    fn meta_value(&self, conn: &Connection, key: &str) -> Result<Option<String>> {
        conn.query_row(
            "SELECT value FROM meta WHERE key = ?1",
            params![key],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(Into::into)
    }

    fn enforce_meta_version(&self, conn: &Connection, key: &str, expected: u32) -> Result<()> {
        let value = self
            .meta_value(conn, key)?
            .ok_or_else(|| StoreError::MissingMeta(key.to_string()))?;
        let parsed = value
            .parse::<u32>()
            .map_err(|_| StoreError::IncompatibleFormat {
                key: key.to_string(),
                expected: expected.to_string(),
                found: value.clone(),
            })?;
        if parsed != expected {
            return Err(StoreError::IncompatibleFormat {
                key: key.to_string(),
                expected: expected.to_string(),
                found: value,
            }
            .into());
        }
        Ok(())
    }

    fn require_meta_presence(&self, conn: &Connection, key: &str) -> Result<()> {
        self.meta_value(conn, key)?
            .ok_or_else(|| StoreError::MissingMeta(key.to_string()))?;
        Ok(())
    }

    fn record_last_used_px_version(&self, conn: &mut Connection) -> Result<()> {
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![META_KEY_LAST_USED, PX_VERSION],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn live_set(&self, conn: &Connection) -> Result<HashSet<String>> {
        let mut stmt = conn.prepare("SELECT DISTINCT oid FROM refs")?;
        let mut rows = stmt.query([])?;
        let mut set = HashSet::new();
        while let Some(row) = rows.next()? {
            let oid: String = row.get(0)?;
            set.insert(oid);
        }
        Ok(set)
    }

    fn delete_if_unreferenced(&self, conn: &mut Connection, oid: &str) -> Result<bool> {
        // Remove the index row only if no refs exist at deletion time to avoid racing
        // with concurrent ref creation.
        let tx = conn.transaction()?;
        let deleted = tx.execute(
            "DELETE FROM objects \
             WHERE oid = ?1 \
             AND NOT EXISTS (SELECT 1 FROM refs WHERE refs.oid = ?1)",
            params![oid],
        )?;
        tx.commit()?;

        if deleted == 0 {
            return Ok(false);
        }

        let path = self.object_path(oid);
        if path.exists() {
            fs::remove_file(&path)
                .with_context(|| format!("failed to delete CAS object {}", path.display()))?;
            if let Some(parent) = path.parent() {
                fsync_dir(parent).ok();
            }
        }

        self.remove_materialized(oid)?;

        // Clean up stale partial files to avoid future collisions.
        let tmp = self.tmp_path(oid);
        if tmp.exists() {
            let _ = fs::remove_file(tmp);
        }
        let _ = conn.execute("DELETE FROM keys WHERE oid = ?1", params![oid]);
        Ok(true)
    }

    fn purge_object(
        &self,
        conn: &mut Connection,
        oid: &str,
        path_hint: Option<&Path>,
    ) -> Result<(usize, usize)> {
        let tx = conn.transaction()?;
        let refs_count: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM refs WHERE oid = ?1",
                params![oid],
                |row| row.get(0),
            )
            .unwrap_or(0);
        let keys_removed = tx.execute("DELETE FROM keys WHERE oid = ?1", params![oid])?;
        let deleted = tx.execute("DELETE FROM objects WHERE oid = ?1", params![oid])?;
        tx.commit()?;

        if deleted > 0 {
            let path = path_hint
                .map(PathBuf::from)
                .unwrap_or_else(|| self.object_path(oid));
            if path.exists() {
                let _ = fs::remove_file(&path);
                if let Some(parent) = path.parent() {
                    fsync_dir(parent).ok();
                }
            }
            self.remove_materialized(oid)?;
            let tmp = self.tmp_path(oid);
            if tmp.exists() {
                let _ = fs::remove_file(tmp);
            }
        }

        Ok((refs_count as usize, keys_removed as usize))
    }

    fn prune_orphans(&self, conn: &mut Connection) -> Result<(usize, usize)> {
        let refs_pruned = conn.execute(
            "DELETE FROM refs WHERE oid NOT IN (SELECT oid FROM objects)",
            [],
        )?;
        let keys_pruned = conn.execute(
            "DELETE FROM keys WHERE oid NOT IN (SELECT oid FROM objects)",
            [],
        )?;
        Ok((refs_pruned as usize, keys_pruned as usize))
    }

    fn runtime_manifest_path(&self, oid: &str) -> PathBuf {
        self.root
            .join(MATERIALIZED_RUNTIMES_DIR)
            .join(oid)
            .join("manifest.json")
    }

    pub(crate) fn write_runtime_manifest(&self, oid: &str, header: &RuntimeHeader) -> Result<()> {
        let path = self.runtime_manifest_path(oid);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let manifest = json!({
            "runtime_oid": oid,
            "version": header.version,
            "platform": header.platform,
            "owner_id": format!("runtime:{}:{}", header.version, header.platform),
        });
        fs::write(&path, serde_json::to_string_pretty(&manifest)?)
            .with_context(|| format!("failed to write runtime manifest {}", path.display()))?;
        Ok(())
    }

    fn remove_materialized(&self, oid: &str) -> Result<()> {
        for dir in [MATERIALIZED_PKG_BUILDS_DIR, MATERIALIZED_RUNTIMES_DIR] {
            let path = self.root.join(dir).join(oid);
            if path.exists() {
                fs::remove_dir_all(&path).with_context(|| {
                    format!(
                        "failed to remove materialized CAS directory {}",
                        path.display()
                    )
                })?;
            }
            let partial = path.with_extension("partial");
            if partial.exists() {
                let _ = fs::remove_dir_all(&partial);
            }
        }
        Ok(())
    }

    /// Remove a materialized environment projection for the given profile oid.
    pub(crate) fn remove_env_materialization(&self, profile_oid: &str) -> Result<()> {
        let env_root = self.envs_root.join(profile_oid);
        for path in [
            env_root.clone(),
            env_root.with_extension("partial"),
            env_root.with_extension("backup"),
        ] {
            if path.exists() {
                fs::remove_dir_all(&path).with_context(|| {
                    format!("failed to remove env materialization {}", path.display())
                })?;
            }
        }
        Ok(())
    }

    fn verify_existing(&self, oid: &str, path: &Path) -> Result<()> {
        let bytes = fs::read(path)
            .with_context(|| format!("failed to read existing CAS object {}", path.display()))?;
        self.verify_bytes(oid, &bytes)
    }

    fn verify_bytes(&self, oid: &str, bytes: &[u8]) -> Result<()> {
        let actual = hex::encode(Sha256::digest(bytes));
        if actual != oid {
            return Err(StoreError::DigestMismatch {
                oid: oid.to_string(),
                expected: oid.to_string(),
                actual,
            }
            .into());
        }
        Ok(())
    }

    fn write_new_object(&self, oid: &str, canonical_bytes: &[u8], dest: &Path) -> Result<()> {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create object directory {}", parent.display())
            })?;
        }

        let tmp = self.tmp_path(oid);
        if let Some(parent) = tmp.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create temp directory {}", parent.display()))?;
        }
        if tmp.exists() {
            let _ = fs::remove_file(&tmp);
        }

        {
            let mut file = File::create(&tmp)
                .with_context(|| format!("failed to create temp CAS object {}", tmp.display()))?;
            file.write_all(canonical_bytes)
                .with_context(|| format!("failed to write temp CAS object {}", tmp.display()))?;
            file.sync_all()
                .with_context(|| format!("failed to flush temp CAS object {}", tmp.display()))?;
        }

        if let Some(parent) = tmp.parent() {
            fsync_dir(parent).ok();
        }

        fs::rename(&tmp, dest).with_context(|| {
            format!(
                "failed to move CAS object into place ({} -> {})",
                tmp.display(),
                dest.display()
            )
        })?;

        if let Some(parent) = dest.parent() {
            fsync_dir(parent).ok();
        }

        make_read_only_recursive(dest)?;

        self.verify_existing(oid, dest)
    }

    fn ensure_index_entry(&self, oid: &str, kind: ObjectKind, size: u64) -> Result<()> {
        let now = timestamp_secs() as i64;
        self.with_immediate_tx(|tx| {
            match self.object_info_with_conn(tx, oid)? {
                Some(info) => {
                    if info.kind != kind {
                        return Err(StoreError::KindMismatch {
                            oid: oid.to_string(),
                            expected: kind,
                            found: info.kind,
                        }
                        .into());
                    }
                    if info.size != size {
                        return Err(StoreError::SizeMismatch {
                            oid: oid.to_string(),
                            expected: size,
                            found: info.size,
                        }
                        .into());
                    }
                    self.touch_object_tx(tx, oid, now as u64)?;
                }
                None => {
                    tx.execute(
                        "INSERT INTO objects(oid, kind, size, created_at, last_accessed) \
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![oid, kind.as_str(), size as i64, now, now],
                    )?;
                }
            }
            Ok(())
        })
    }

    fn object_info_with_conn(&self, conn: &Connection, oid: &str) -> Result<Option<ObjectInfo>> {
        let mut stmt = conn.prepare(
            "SELECT oid, kind, size, created_at, last_accessed FROM objects WHERE oid = ?1",
        )?;
        let mut rows = stmt.query(params![oid])?;
        if let Some(row) = rows.next()? {
            let kind_str: String = row.get(1)?;
            let kind = ObjectKind::try_from(kind_str.as_str())?;
            Ok(Some(ObjectInfo {
                oid: row.get(0)?,
                kind,
                size: row.get::<_, i64>(2)? as u64,
                created_at: row.get::<_, i64>(3)? as u64,
                last_accessed: row.get::<_, i64>(4)? as u64,
            }))
        } else {
            Ok(None)
        }
    }

    fn touch_object(&self, conn: &mut Connection, oid: &str, now: u64) -> Result<()> {
        conn.execute(
            "UPDATE objects SET last_accessed=?1 WHERE oid=?2",
            params![now as i64, oid],
        )?;
        Ok(())
    }

    fn touch_object_tx(&self, tx: &rusqlite::Transaction<'_>, oid: &str, now: u64) -> Result<()> {
        tx.execute(
            "UPDATE objects SET last_accessed=?1 WHERE oid=?2",
            params![now as i64, oid],
        )?;
        Ok(())
    }

    fn assert_object_known(&self, conn: &Connection, oid: &str) -> Result<()> {
        let exists = conn
            .query_row(
                "SELECT 1 FROM objects WHERE oid=?1 LIMIT 1",
                params![oid],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if exists {
            Ok(())
        } else {
            Err(StoreError::MissingObject {
                oid: oid.to_string(),
            }
            .into())
        }
    }

    pub(crate) fn acquire_lock(&self, oid: &str) -> Result<File> {
        let path = self.lock_path(oid);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create CAS lock directory {}", parent.display())
            })?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("failed to open CAS lock {}", path.display()))?;
        file.lock_exclusive()
            .with_context(|| format!("failed to lock {}", path.display()))?;
        Ok(file)
    }

    fn try_lock_for_gc(&self, oid: &str) -> Result<Option<File>> {
        let path = self.lock_path(oid);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(file)),
            Err(err) if err.kind() == ErrorKind::WouldBlock => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    fn object_path(&self, oid: &str) -> PathBuf {
        let shard = oid.get(0..2).unwrap_or("xx");
        self.root.join(OBJECTS_DIR).join(shard).join(oid)
    }

    fn lock_path(&self, oid: &str) -> PathBuf {
        self.root.join(LOCKS_DIR).join(format!("{oid}.lock"))
    }

    fn tmp_path(&self, oid: &str) -> PathBuf {
        self.root.join(TMP_DIR).join(format!("{oid}.partial"))
    }

    fn index_path(&self) -> PathBuf {
        self.root.join(INDEX_FILENAME)
    }

    fn decode_object(&self, oid: &str, canonical_bytes: &[u8]) -> Result<LoadedObject> {
        let canonical: CanonicalObject = serde_json::from_slice(canonical_bytes)
            .context("failed to decode canonical CAS payload")?;
        match canonical.data {
            CanonicalData::Source { header, payload } => Ok(LoadedObject::Source {
                header,
                bytes: payload,
                oid: oid.to_string(),
            }),
            CanonicalData::PkgBuild { header, payload } => Ok(LoadedObject::PkgBuild {
                header,
                archive: payload,
                oid: oid.to_string(),
            }),
            CanonicalData::Runtime { header, payload } => Ok(LoadedObject::Runtime {
                header,
                archive: payload,
                oid: oid.to_string(),
            }),
            CanonicalData::Profile { header } => Ok(LoadedObject::Profile {
                header,
                oid: oid.to_string(),
            }),
            CanonicalData::Meta { payload } => Ok(LoadedObject::Meta {
                bytes: payload,
                oid: oid.to_string(),
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum StateFileKind {
    Project,
    Workspace,
}

#[derive(Debug, Deserialize)]
struct RebuildState {
    #[serde(default)]
    current_env: Option<RebuildStoredEnv>,
    #[serde(default)]
    runtime: Option<RebuildStoredRuntime>,
}

#[derive(Debug, Deserialize)]
struct RebuildStoredEnv {
    #[serde(default, alias = "lock_hash")]
    lock_id: String,
    #[serde(default)]
    profile_oid: Option<String>,
    #[serde(default)]
    python: Option<RebuildStoredPython>,
}

#[derive(Debug, Deserialize)]
struct RebuildStoredRuntime {
    #[serde(default)]
    version: String,
}

#[derive(Debug, Deserialize)]
struct RebuildStoredPython {
    #[serde(default)]
    version: String,
}

fn state_files_to_scan() -> Result<Vec<(StateFileKind, PathBuf)>> {
    let mut results = Vec::new();
    let Ok(cwd) = env::current_dir() else {
        return Ok(results);
    };
    for ancestor in cwd.ancestors() {
        let px_dir = ancestor.join(".px");
        let project = px_dir.join("state.json");
        if project.is_file() {
            results.push((StateFileKind::Project, project));
        }
        let workspace = px_dir.join("workspace-state.json");
        if workspace.is_file() {
            results.push((StateFileKind::Workspace, workspace));
        }
    }
    Ok(results)
}

fn owner_id_from_state(
    kind: StateFileKind,
    root: &Path,
    lock_id: &str,
    runtime_version: &str,
) -> Result<String> {
    let fingerprint = root_fingerprint(root)?;
    let prefix = match kind {
        StateFileKind::Project => "project-env",
        StateFileKind::Workspace => "workspace-env",
    };
    Ok(format!(
        "{prefix}:{fingerprint}:{lock_id}:{runtime_version}"
    ))
}

fn root_fingerprint(root: &Path) -> Result<String> {
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

fn tool_owner_id(tool: &str, lock_id: &str, runtime: &str) -> Result<String> {
    Ok(format!(
        "tool-env:{}:{}:{}",
        tool.to_ascii_lowercase(),
        lock_id,
        runtime
    ))
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

fn normalize_archive_path(path: &Path) -> Result<String> {
    let normalized = path.to_string_lossy().replace('\\', "/");
    if normalized.starts_with('/') {
        return Err(anyhow!(
            "archive entries must be relative (got {})",
            normalized
        ));
    }
    if normalized.is_empty() {
        return Err(anyhow!("archive entry path is empty"));
    }
    Ok(normalized)
}

/// Produce a deterministic, gzip-compressed tar archive for a directory tree.
pub fn archive_dir_canonical(root: &Path) -> Result<Vec<u8>> {
    let mut builder = tar::Builder::new(Vec::new());
    builder.follow_symlinks(false);
    for entry in walkdir::WalkDir::new(root)
        .sort_by(|a, b| a.path().cmp(b.path()))
        .into_iter()
    {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                debug!(%err, root=%root.display(), "skipping path during archive walk");
                continue;
            }
        };
        let path = entry.path();
        if path == root {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .context("failed to relativize path")?;
        let rel_path = normalize_archive_path(rel)?;
        // Use symlink_metadata so we never follow links while capturing the tree.
        let metadata = match fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == ErrorKind::PermissionDenied => {
                debug!(path=%path.display(), "skipping unreadable path during archive");
                continue;
            }
            Err(err) => return Err(err.into()),
        };
        let file_type = metadata.file_type();
        let mut header = Header::new_gnu();
        header.set_mtime(0);
        header.set_uid(0);
        header.set_gid(0);
        if file_type.is_dir() {
            header.set_entry_type(tar::EntryType::Directory);
            header.set_mode(0o755);
            header.set_size(0);
            builder.append_data(&mut header, Path::new(&rel_path), std::io::empty())?;
        } else if file_type.is_file() {
            header.set_entry_type(tar::EntryType::Regular);
            header.set_mode(if is_executable(&metadata) {
                0o755
            } else {
                0o644
            });
            header.set_size(metadata.len());
            let file = match File::open(path) {
                Ok(file) => file,
                Err(err) if err.kind() == ErrorKind::PermissionDenied => {
                    debug!(path=%path.display(), "skipping unreadable file during archive");
                    continue;
                }
                Err(err) => return Err(err.into()),
            };
            builder.append_data(&mut header, Path::new(&rel_path), file)?;
        } else if file_type.is_symlink() {
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_mode(0o777);
            header.set_size(0);
            let mut target = match fs::read_link(path) {
                Ok(link) => link,
                Err(err) if err.kind() == ErrorKind::PermissionDenied => {
                    debug!(
                        path = %path.display(),
                        "skipping unreadable symlink during archive"
                    );
                    continue;
                }
                Err(err) => {
                    return Err(err).with_context(|| {
                        format!("failed to read symlink target {}", path.display())
                    })
                }
            };
            if target.is_absolute() {
                if let Ok(relative) = target.strip_prefix(root) {
                    target = relative.to_path_buf();
                } else {
                    target = target
                        .file_name()
                        .map(PathBuf::from)
                        .unwrap_or_else(|| PathBuf::from("target"));
                }
            }
            let target_path = normalize_archive_path(&target)?;
            if let Err(err) = header.set_link_name(Path::new(&target_path)) {
                let Some(basename) = target.file_name().and_then(|n| n.to_str()) else {
                    debug!(
                        path = %path.display(),
                        target = %target_path,
                        %err,
                        "skipping symlink with invalid target during archive"
                    );
                    continue;
                };
                if let Err(err) = header.set_link_name(basename) {
                    debug!(
                        path = %path.display(),
                        target = %target_path,
                        %err,
                        "skipping symlink with long target during archive"
                    );
                    continue;
                }
                debug!(
                    path = %path.display(),
                    target = %target_path,
                    fallback = basename,
                    "shortened symlink target during archive"
                );
            }
            builder.append_data(&mut header, Path::new(&rel_path), std::io::empty())?;
        } else {
            continue;
        }
    }
    builder.finish()?;
    let tar_bytes = builder.into_inner()?;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&tar_bytes)?;
    Ok(encoder.finish()?)
}

/// Archive a subset of paths under a shared root, skipping unreadable entries.
pub fn archive_selected(root: &Path, paths: &[PathBuf]) -> Result<Vec<u8>> {
    let mut builder = tar::Builder::new(Vec::new());
    builder.follow_symlinks(false);
    let mut seen = HashSet::new();

    for base in paths {
        if !base.starts_with(root) {
            debug!(
                path = %base.display(),
                root = %root.display(),
                "skipping path outside archive root"
            );
            continue;
        }
        if !base.exists() {
            continue;
        }
        for entry in walkdir::WalkDir::new(base)
            .sort_by(|a, b| a.path().cmp(b.path()))
            .into_iter()
        {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    debug!(%err, root=%root.display(), "skipping path during archive walk");
                    continue;
                }
            };
            let path = entry.path();
            if !seen.insert(path.to_path_buf()) {
                continue;
            }
            if path == root {
                continue;
            }
            let rel = match path.strip_prefix(root) {
                Ok(rel) => rel,
                Err(_) => continue,
            };
            let rel_path = normalize_archive_path(rel)?;
            let metadata = match fs::symlink_metadata(path) {
                Ok(meta) => meta,
                Err(err) if err.kind() == ErrorKind::PermissionDenied => {
                    debug!(path=%path.display(), "skipping unreadable path during archive");
                    continue;
                }
                Err(err) => return Err(err.into()),
            };
            let file_type = metadata.file_type();
            let mut header = Header::new_gnu();
            header.set_mtime(0);
            header.set_uid(0);
            header.set_gid(0);
            if file_type.is_dir() {
                header.set_entry_type(tar::EntryType::Directory);
                header.set_mode(0o755);
                header.set_size(0);
                builder.append_data(&mut header, Path::new(&rel_path), std::io::empty())?;
            } else if file_type.is_file() {
                header.set_entry_type(tar::EntryType::Regular);
                header.set_mode(if is_executable(&metadata) {
                    0o755
                } else {
                    0o644
                });
                header.set_size(metadata.len());
                let file = match File::open(path) {
                    Ok(file) => file,
                    Err(err) if err.kind() == ErrorKind::PermissionDenied => {
                        debug!(path=%path.display(), "skipping unreadable file during archive");
                        continue;
                    }
                    Err(err) => return Err(err.into()),
                };
                builder.append_data(&mut header, Path::new(&rel_path), file)?;
            } else if file_type.is_symlink() {
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_mode(0o777);
                header.set_size(0);
                let mut target = match fs::read_link(path) {
                    Ok(link) => link,
                    Err(err) if err.kind() == ErrorKind::PermissionDenied => {
                        debug!(
                            path = %path.display(),
                            "skipping unreadable symlink during archive"
                        );
                        continue;
                    }
                    Err(err) => {
                        return Err(err).with_context(|| {
                            format!("failed to read symlink target {}", path.display())
                        })
                    }
                };
                if target.is_absolute() {
                    if let Ok(relative) = target.strip_prefix(root) {
                        target = relative.to_path_buf();
                    } else {
                        target = target
                            .file_name()
                            .map(PathBuf::from)
                            .unwrap_or_else(|| PathBuf::from("target"));
                    }
                }
                let target_str = normalize_archive_path(&target)?;
                if let Err(err) = header.set_link_name(&target_str) {
                    let Some(basename) = target.file_name().and_then(|n| n.to_str()) else {
                        debug!(
                            path = %path.display(),
                            target = %target_str,
                            %err,
                            "skipping symlink with invalid target during archive"
                        );
                        continue;
                    };
                    if let Err(err) = header.set_link_name(basename) {
                        debug!(
                            path = %path.display(),
                            target = %target_str,
                            %err,
                            "skipping symlink with long target during archive"
                        );
                        continue;
                    }
                    debug!(
                        path = %path.display(),
                        target = %target_str,
                        fallback = basename,
                        "shortened symlink target during archive"
                    );
                }
                builder.append_data(&mut header, Path::new(&rel_path), std::io::empty())?;
            }
        }
    }

    let tar_bytes = builder.into_inner()?;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&tar_bytes)?;
    Ok(encoder.finish()?)
}

fn is_executable(metadata: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        return metadata.permissions().mode() & 0o111 != 0;
    }
    #[cfg(not(unix))]
    {
        false
    }
}

static GLOBAL_STORE: OnceLock<ContentAddressableStore> = OnceLock::new();

/// Access the global CAS rooted at the default location.
pub fn global_store() -> &'static ContentAddressableStore {
    #[cfg(test)]
    ensure_test_store_env();
    GLOBAL_STORE.get_or_init(|| {
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
/// Run GC with environment-driven policy. Returns `Ok(None)` when disabled via
/// `PX_CAS_GC_DISABLE=1`.
pub fn run_gc_with_env_policy(store: &ContentAddressableStore) -> Result<Option<GcSummary>> {
    if env::var("PX_CAS_GC_DISABLE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
    {
        return Ok(None);
    }
    let grace_secs = env::var("PX_CAS_GC_GRACE_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(86_400);
    let max_bytes = env::var("PX_CAS_MAX_BYTES")
        .ok()
        .and_then(|v| v.parse::<u64>().ok());
    let grace = Duration::from_secs(grace_secs);
    let summary = match max_bytes {
        Some(limit) => store.garbage_collect_with_limit(grace, limit)?,
        None => store.garbage_collect(grace)?,
    };
    Ok(Some(summary))
}

/// Deterministic key for a source download request.
#[must_use]
pub fn source_lookup_key(header: &SourceHeader) -> String {
    format!(
        "{}|{}|{}|{}|{}",
        header.name.to_ascii_lowercase(),
        header.version,
        header.filename,
        header.index_url,
        header.sha256
    )
}

/// Deterministic key for a pkg-build.
#[must_use]
pub fn pkg_build_lookup_key(header: &PkgBuildHeader) -> String {
    format!(
        "{}|{}|{}",
        header.source_oid, header.runtime_abi, header.build_options_hash
    )
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
mod tests {
    use super::*;
    use anyhow::bail;
    #[cfg(unix)]
    use flate2::read::GzDecoder;
    use serde_json::json;
    use std::collections::BTreeMap;
    #[cfg(unix)]
    use std::io::Read;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    #[cfg(unix)]
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::{Mutex, OnceLock};
    use std::thread;
    use tempfile::tempdir;

    static CURRENT_DIR_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    fn new_store() -> Result<(tempfile::TempDir, ContentAddressableStore)> {
        let temp = tempdir()?;
        let root = temp.path().join("store");
        let envs_root = temp.path().join("envs");
        std::env::set_var("PX_ENVS_PATH", &envs_root);
        let store = ContentAddressableStore::new(Some(root))?;
        Ok((temp, store))
    }

    #[cfg(unix)]
    fn make_writable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = fs::metadata(path) {
            let mut perms = metadata.permissions();
            perms.set_mode(perms.mode() | 0o200);
            let _ = fs::set_permissions(path, perms);
        }
    }

    #[cfg(not(unix))]
    fn make_writable(path: &Path) {
        if let Ok(metadata) = fs::metadata(path) {
            let mut perms = metadata.permissions();
            perms.set_readonly(false);
            let _ = fs::set_permissions(path, perms);
        }
    }

    fn demo_source_payload() -> ObjectPayload<'static> {
        ObjectPayload::Source {
            header: SourceHeader {
                name: "demo".to_string(),
                version: "1.0.0".to_string(),
                filename: "demo-1.0.0.whl".to_string(),
                index_url: "https://example.invalid/simple/".to_string(),
                sha256: "deadbeef".to_string(),
            },
            bytes: Cow::Owned(b"demo-payload".to_vec()),
        }
    }

    #[test]
    fn creates_layout_and_schema() -> Result<()> {
        let (_temp, store) = new_store()?;
        let root = store.root().to_path_buf();
        for dir in [OBJECTS_DIR, LOCKS_DIR, TMP_DIR] {
            assert!(
                root.join(dir).is_dir(),
                "expected {} directory to exist",
                dir
            );
        }
        assert!(
            root.join(INDEX_FILENAME).is_file(),
            "expected index.sqlite to exist"
        );
        Ok(())
    }

    #[test]
    fn records_and_validates_meta_versions() -> Result<()> {
        let (_temp, store) = new_store()?;
        let conn = store.connection()?;
        let fmt: String = conn.query_row(
            "SELECT value FROM meta WHERE key = ?1",
            rusqlite::params![META_KEY_CAS_FORMAT_VERSION],
            |row| row.get(0),
        )?;
        assert_eq!(fmt, CAS_FORMAT_VERSION.to_string());
        let schema: String = conn.query_row(
            "SELECT value FROM meta WHERE key = ?1",
            rusqlite::params![META_KEY_SCHEMA_VERSION],
            |row| row.get(0),
        )?;
        assert_eq!(schema, SCHEMA_VERSION.to_string());
        let created_by: String = conn.query_row(
            "SELECT value FROM meta WHERE key = ?1",
            rusqlite::params![META_KEY_CREATED_BY],
            |row| row.get(0),
        )?;
        assert_eq!(created_by, PX_VERSION);

        conn.execute(
            "UPDATE meta SET value='0.0.0' WHERE key=?1",
            rusqlite::params![META_KEY_LAST_USED],
        )?;
        drop(conn);

        // Trigger layout validation and ensure last_used is refreshed.
        let _ = store.list(None, None)?;
        let conn = store.connection()?;
        let last_used: String = conn.query_row(
            "SELECT value FROM meta WHERE key = ?1",
            rusqlite::params![META_KEY_LAST_USED],
            |row| row.get(0),
        )?;
        assert_eq!(last_used, PX_VERSION);

        conn.execute(
            "UPDATE meta SET value = '999' WHERE key = ?1",
            rusqlite::params![META_KEY_SCHEMA_VERSION],
        )?;
        drop(conn);
        let err = store.list(None, None).unwrap_err();
        let store_err = err
            .downcast_ref::<StoreError>()
            .expect("should produce StoreError");
        assert!(
            matches!(
                store_err,
                StoreError::IncompatibleFormat { key, .. }
                if key == META_KEY_SCHEMA_VERSION
            ),
            "schema mismatch should be surfaced"
        );
        Ok(())
    }

    #[test]
    fn stores_and_validates_integrity() -> Result<()> {
        let (_temp, store) = new_store()?;
        let payload = demo_source_payload();
        let stored = store.store(&payload)?;
        assert!(stored.path.exists());

        let loaded = store.load(&stored.oid)?;
        match loaded {
            LoadedObject::Source { bytes, header, .. } => {
                assert_eq!(bytes, b"demo-payload");
                assert_eq!(header.name, "demo");
            }
            _ => bail!("expected source object"),
        }

        let info = store.object_info(&stored.oid)?.expect("metadata present");
        let previous_access = info.last_accessed;
        thread::sleep(Duration::from_millis(5));
        let _ = store.load(&stored.oid)?;
        let updated = store.object_info(&stored.oid)?.expect("metadata present");
        assert!(
            updated.last_accessed >= previous_access,
            "last_accessed should advance after reads"
        );

        make_writable(&stored.path);
        fs::write(&stored.path, b"corrupt")?;
        let err = store.load(&stored.oid).unwrap_err();
        let store_err = err
            .downcast_ref::<StoreError>()
            .expect("should produce StoreError");
        assert!(
            matches!(store_err, StoreError::DigestMismatch { .. }),
            "expected digest mismatch when data is corrupted"
        );
        Ok(())
    }

    #[test]
    fn rebuild_restores_env_owner_refs_from_state() -> Result<()> {
        let _dir_guard = CURRENT_DIR_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap();
        let (temp, store) = new_store()?;
        let runtime_oid = "runtime-oid".to_string();
        let profile_payload = ObjectPayload::Profile {
            header: ProfileHeader {
                runtime_oid: runtime_oid.clone(),
                packages: Vec::new(),
                sys_path_order: Vec::new(),
                env_vars: BTreeMap::new(),
            },
        };
        let stored_profile = store.store(&profile_payload)?;
        let profile_oid = stored_profile.oid.clone();

        let project_root = temp.path().join("project");
        let px_dir = project_root.join(".px");
        fs::create_dir_all(&px_dir)?;
        let state_path = px_dir.join("state.json");
        let env_site = project_root.join("env-site");
        let state_body = json!({
            "current_env": {
                "id": "env",
                "lock_id": "lock-123",
                "platform": "linux",
                "site_packages": env_site.display().to_string(),
                "profile_oid": profile_oid,
                "python": { "path": "python", "version": "3.11.0" }
            },
            "runtime": {
                "path": "python",
                "version": "3.11.0",
                "platform": "linux"
            }
        });
        fs::write(&state_path, serde_json::to_string_pretty(&state_body)?)?;

        let index_path = store.root().join(INDEX_FILENAME);
        fs::remove_file(&index_path)?;

        let cwd = env::current_dir()?;
        env::set_current_dir(&project_root)?;
        let _ = store.list(None, None)?;
        env::set_current_dir(cwd)?;

        let refs = store.refs_for(&profile_oid)?;
        let expected_owner =
            owner_id_from_state(StateFileKind::Project, &project_root, "lock-123", "3.11.0")?;
        assert!(
            refs.iter()
                .any(|owner| owner.owner_type == OwnerType::ProjectEnv
                    && owner.owner_id == expected_owner),
            "expected reconstructed project-env owner reference"
        );
        Ok(())
    }

    #[test]
    fn rebuild_restores_tool_owner_refs_from_state() -> Result<()> {
        let (temp, store) = new_store()?;
        let tools_dir = temp.path().join("tools");
        std::env::set_var("PX_TOOLS_DIR", &tools_dir);

        let profile_payload = ObjectPayload::Profile {
            header: ProfileHeader {
                runtime_oid: "runtime-oid".to_string(),
                packages: Vec::new(),
                sys_path_order: Vec::new(),
                env_vars: BTreeMap::new(),
            },
        };
        let stored_profile = store.store(&profile_payload)?;
        let profile_oid = stored_profile.oid.clone();

        let tool_root = tools_dir.join("demo-tool");
        fs::create_dir_all(tool_root.join(".px"))?;
        let state_body = json!({
            "current_env": {
                "id": "env",
                "lock_id": "lock-abc",
                "site_packages": "/tmp/site",
                "profile_oid": profile_oid,
                "python": { "path": "python", "version": "3.11.0" }
            },
            "runtime": {
                "path": "python",
                "version": "3.11.0",
                "platform": "linux"
            }
        });
        fs::write(
            tool_root.join(".px").join("state.json"),
            serde_json::to_string_pretty(&state_body)?,
        )?;

        let index_path = store.root().join(INDEX_FILENAME);
        fs::remove_file(&index_path)?;

        let _ = store.list(None, None)?;
        let refs = store.refs_for(&profile_oid)?;
        let expected_owner = tool_owner_id("demo-tool", "lock-abc", "3.11.0")?;
        assert!(
            refs.iter()
                .any(|owner| owner.owner_type == OwnerType::ToolEnv
                    && owner.owner_id == expected_owner),
            "expected reconstructed tool-env owner reference"
        );
        Ok(())
    }

    #[test]
    fn rebuild_restores_runtime_owner_refs_from_manifest() -> Result<()> {
        let (_temp, store) = new_store()?;
        let runtime_payload = ObjectPayload::Runtime {
            header: RuntimeHeader {
                version: "3.11.0".to_string(),
                abi: "cp311".to_string(),
                platform: "linux".to_string(),
                build_config_hash: "abc".to_string(),
                exe_path: "bin/python".to_string(),
            },
            archive: Cow::Owned(b"runtime".to_vec()),
        };
        let runtime = store.store(&runtime_payload)?;
        let manifest_dir = store
            .root()
            .join(MATERIALIZED_RUNTIMES_DIR)
            .join(&runtime.oid);
        fs::create_dir_all(&manifest_dir)?;
        let manifest_path = manifest_dir.join("manifest.json");
        fs::write(
            &manifest_path,
            serde_json::to_string_pretty(&json!({
                "runtime_oid": runtime.oid,
                "version": "3.11.0",
                "platform": "linux",
                "owner_id": "runtime:3.11.0:linux"
            }))?,
        )?;

        let index_path = store.root().join(INDEX_FILENAME);
        fs::remove_file(&index_path)?;

        let _ = store.list(None, None)?;
        let refs = store.refs_for(&runtime.oid)?;
        let expected_owner = "runtime:3.11.0:linux".to_string();
        assert!(
            refs.iter()
                .any(|owner| owner.owner_type == OwnerType::Runtime
                    && owner.owner_id == expected_owner),
            "expected reconstructed runtime owner reference"
        );
        Ok(())
    }

    #[test]
    fn remove_env_materialization_cleans_all_variants() -> Result<()> {
        let (_temp, store) = new_store()?;
        let env_root = store.envs_root().join("profile-123");
        fs::create_dir_all(&env_root)?;
        fs::create_dir_all(env_root.with_extension("partial"))?;
        fs::create_dir_all(env_root.with_extension("backup"))?;

        store.remove_env_materialization("profile-123")?;

        assert!(
            !env_root.exists(),
            "env materialization should be removed when requested"
        );
        assert!(
            !env_root.with_extension("partial").exists(),
            "partial env materialization should be removed"
        );
        assert!(
            !env_root.with_extension("backup").exists(),
            "backup env materialization should be removed"
        );
        Ok(())
    }

    #[test]
    fn load_repairs_missing_index_entry_from_disk() -> Result<()> {
        let (_temp, store) = new_store()?;
        let stored = store.store(&demo_source_payload())?;

        let conn = store.connection()?;
        conn.execute("DELETE FROM objects WHERE oid = ?1", params![stored.oid])?;

        let loaded = store.load(&stored.oid)?;
        assert!(
            matches!(loaded, LoadedObject::Source { .. }),
            "load should succeed even when index entry is missing"
        );
        let repaired = store.object_info(&stored.oid)?;
        assert!(
            repaired.is_some(),
            "index metadata should be recreated during load"
        );
        Ok(())
    }

    #[test]
    fn add_ref_repairs_missing_index_entry() -> Result<()> {
        let (_temp, store) = new_store()?;
        let stored = store.store(&demo_source_payload())?;

        let conn = store.connection()?;
        conn.execute("DELETE FROM objects WHERE oid = ?1", params![stored.oid])?;

        let owner = OwnerId {
            owner_type: OwnerType::Profile,
            owner_id: "profile:demo".to_string(),
        };
        store.add_ref(&owner, &stored.oid)?;

        let owners = store.refs_for(&stored.oid)?;
        assert!(
            owners
                .iter()
                .any(|o| o.owner_id == owner.owner_id && o.owner_type == owner.owner_type),
            "reference should be recorded after repairing index"
        );
        let repaired = store.object_info(&stored.oid)?;
        assert!(repaired.is_some(), "object info should be restored");
        Ok(())
    }

    #[test]
    fn runtime_manifest_recreated_on_load() -> Result<()> {
        let (_temp, store) = new_store()?;
        let runtime_payload = ObjectPayload::Runtime {
            header: RuntimeHeader {
                version: "3.11.0".to_string(),
                abi: "cp311".to_string(),
                platform: "linux".to_string(),
                build_config_hash: "abc".to_string(),
                exe_path: "bin/python".to_string(),
            },
            archive: Cow::Owned(b"runtime".to_vec()),
        };
        let runtime = store.store(&runtime_payload)?;
        let manifest_path = store
            .root()
            .join(MATERIALIZED_RUNTIMES_DIR)
            .join(&runtime.oid)
            .join("manifest.json");
        assert!(
            manifest_path.exists(),
            "runtime manifest should be written during store"
        );
        fs::remove_file(&manifest_path)?;

        let _ = store.load(&runtime.oid)?;
        assert!(
            manifest_path.exists(),
            "loading a runtime should recreate its manifest for reconstruction"
        );
        Ok(())
    }

    #[test]
    fn objects_are_made_read_only() -> Result<()> {
        let (_temp, store) = new_store()?;
        let stored = store.store(&demo_source_payload())?;
        let metadata = fs::metadata(&stored.path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                metadata.permissions().mode() & 0o222,
                0,
                "write bits should be stripped from CAS objects"
            );
        }
        let write_attempt = OpenOptions::new().write(true).open(&stored.path);
        assert!(
            write_attempt.is_err(),
            "objects should not be writable after creation"
        );
        Ok(())
    }

    #[test]
    fn permission_health_check_hardens_existing_objects() -> Result<()> {
        let (temp, store) = new_store()?;
        let stored = store.store(&demo_source_payload())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&stored.path)?.permissions();
            perms.set_mode(perms.mode() | 0o200);
            fs::set_permissions(&stored.path, perms)?;
        }
        #[cfg(not(unix))]
        {
            let mut perms = fs::metadata(&stored.path)?.permissions();
            perms.set_readonly(false);
            fs::set_permissions(&stored.path, perms)?;
        }

        let root = store.root().to_path_buf();
        let _rehydrated = ContentAddressableStore::new(Some(root))?;
        let metadata = fs::metadata(&stored.path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                metadata.permissions().mode() & 0o222,
                0,
                "health check should strip write bits"
            );
        }
        let write_attempt = OpenOptions::new().write(true).open(&stored.path);
        assert!(
            write_attempt.is_err(),
            "objects should be hardened against writes after health check"
        );
        drop(temp);
        Ok(())
    }

    #[test]
    fn references_block_gc_until_removed() -> Result<()> {
        let (_temp, store) = new_store()?;
        let stored = store.store(&ObjectPayload::Profile {
            header: ProfileHeader {
                runtime_oid: "runtime-oid".to_string(),
                packages: vec![],
                sys_path_order: vec![],
                env_vars: BTreeMap::new(),
            },
        })?;
        let owner = OwnerId {
            owner_type: OwnerType::ProjectEnv,
            owner_id: "proj-123".to_string(),
        };
        store.add_ref(&owner, &stored.oid)?;

        let summary = store.garbage_collect(Duration::from_secs(0))?;
        assert_eq!(summary.reclaimed, 0, "live reference should prevent GC");
        assert!(stored.path.exists());

        assert!(store.remove_ref(&owner, &stored.oid)?);
        let summary = store.garbage_collect(Duration::from_secs(0))?;
        assert_eq!(summary.reclaimed, 1, "object should be reclaimed");
        assert!(!stored.path.exists());
        Ok(())
    }

    #[test]
    fn stale_partials_are_cleaned_on_store() -> Result<()> {
        let (_temp, store) = new_store()?;
        let stored = store.store(&ObjectPayload::Meta {
            bytes: Cow::Owned(b"meta".to_vec()),
        })?;
        let tmp = store.tmp_path(&stored.oid);
        if let Some(parent) = tmp.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&tmp, b"junk")?;

        let again = store.store(&ObjectPayload::Meta {
            bytes: Cow::Owned(b"meta".to_vec()),
        })?;
        assert_eq!(again.oid, stored.oid);
        assert!(
            !tmp.exists(),
            "partial file should be removed after storing object"
        );
        let loaded = store.load(&stored.oid)?;
        match loaded {
            LoadedObject::Meta { bytes, .. } => assert_eq!(bytes, b"meta"),
            other => bail!("unexpected object {other:?}"),
        }
        Ok(())
    }

    #[test]
    fn refs_are_deduplicated() -> Result<()> {
        let (_temp, store) = new_store()?;
        let stored = store.store(&ObjectPayload::Runtime {
            header: RuntimeHeader {
                version: "3.11.0".to_string(),
                abi: "cp311".to_string(),
                platform: "x86_64-manylinux".to_string(),
                build_config_hash: "abc".to_string(),
                exe_path: "bin/python".to_string(),
            },
            archive: Cow::Owned(b"runtime".to_vec()),
        })?;
        let owner = OwnerId {
            owner_type: OwnerType::Runtime,
            owner_id: "python-3.11.0".to_string(),
        };
        store.add_ref(&owner, &stored.oid)?;
        store.add_ref(&owner, &stored.oid)?;
        let refs = store.refs_for(&stored.oid)?;
        assert_eq!(refs.len(), 1, "refs should be deduplicated");
        Ok(())
    }

    #[test]
    fn owner_refs_can_be_pruned_for_orphan_profiles() -> Result<()> {
        let (_temp, store) = new_store()?;
        let profile = store.store(&ObjectPayload::Profile {
            header: ProfileHeader {
                runtime_oid: "runtime-oid".to_string(),
                packages: vec![],
                sys_path_order: vec![],
                env_vars: BTreeMap::new(),
            },
        })?;
        let pkg = store.store(&ObjectPayload::Meta {
            bytes: Cow::Owned(b"pkg".to_vec()),
        })?;

        let profile_owner = OwnerId {
            owner_type: OwnerType::Profile,
            owner_id: profile.oid.clone(),
        };
        let env_owner = OwnerId {
            owner_type: OwnerType::ProjectEnv,
            owner_id: "proj-123".to_string(),
        };
        store.add_ref(&profile_owner, &pkg.oid)?;
        store.add_ref(&env_owner, &profile.oid)?;

        // Simulate staleness so GC can sweep immediately.
        let conn = store.connection()?;
        set_last_accessed(&conn, &profile.oid, 0)?;
        set_last_accessed(&conn, &pkg.oid, 0)?;

        assert!(store.remove_ref(&env_owner, &profile.oid)?);
        assert!(store.refs_for(&profile.oid)?.is_empty());
        let removed = store.remove_owner_refs(&profile_owner)?;
        assert_eq!(removed, 1, "profile-owned refs should be dropped");

        let summary = store.garbage_collect(Duration::from_secs(0))?;
        assert_eq!(summary.reclaimed, 2, "profile and pkg should be reclaimed");
        assert!(!store.object_path(&profile.oid).exists());
        assert!(!store.object_path(&pkg.oid).exists());
        Ok(())
    }

    #[test]
    fn concurrent_store_is_safe() -> Result<()> {
        let (_temp, store) = new_store()?;
        let payload = demo_source_payload();
        let store = Arc::new(store);
        let mut handles = Vec::new();
        for _ in 0..4 {
            let store = Arc::clone(&store);
            let payload = payload.clone();
            handles.push(std::thread::spawn(move || store.store(&payload)));
        }
        let mut oids = Vec::new();
        for handle in handles {
            let stored = handle.join().expect("thread join")?;
            oids.push(stored.oid);
        }
        oids.dedup();
        assert_eq!(oids.len(), 1, "concurrent stores should deduplicate");
        Ok(())
    }

    #[test]
    fn verify_sample_reports_corruption() -> Result<()> {
        let (_temp, store) = new_store()?;
        let payload = demo_source_payload();
        let stored = store.store(&payload)?;
        make_writable(&stored.path);
        fs::write(&stored.path, b"bad")?;
        let failures = store.verify_sample(1)?;
        assert_eq!(failures.len(), 1, "corruption should be detected");
        Ok(())
    }

    #[test]
    fn canonical_archives_are_stable() -> Result<()> {
        let temp = tempdir()?;
        let dir_a = temp.path().join("a");
        let dir_b = temp.path().join("b");
        fs::create_dir_all(&dir_a)?;
        fs::create_dir_all(&dir_b)?;
        fs::write(dir_a.join("file.txt"), b"hello")?;
        fs::write(dir_b.join("file.txt"), b"hello")?;
        filetime::set_file_mtime(
            dir_a.join("file.txt"),
            filetime::FileTime::from_unix_time(100, 0),
        )?;
        filetime::set_file_mtime(
            dir_b.join("file.txt"),
            filetime::FileTime::from_unix_time(500, 0),
        )?;

        let archive_a = archive_dir_canonical(&dir_a)?;
        let archive_b = archive_dir_canonical(&dir_b)?;
        assert_eq!(
            archive_a, archive_b,
            "canonical archives should ignore mtimes"
        );

        let payload_a = ObjectPayload::PkgBuild {
            header: PkgBuildHeader {
                source_oid: "src".into(),
                runtime_abi: "abi".into(),
                build_options_hash: "opts".into(),
            },
            archive: Cow::Owned(archive_a),
        };
        let payload_b = ObjectPayload::PkgBuild {
            header: PkgBuildHeader {
                source_oid: "src".into(),
                runtime_abi: "abi".into(),
                build_options_hash: "opts".into(),
            },
            archive: Cow::Owned(archive_b),
        };
        assert_eq!(
            ContentAddressableStore::compute_oid(&payload_a)?,
            ContentAddressableStore::compute_oid(&payload_b)?,
            "canonical encodings should produce the same oid"
        );
        Ok(())
    }

    #[test]
    fn canonical_encoding_sorts_keys() -> Result<()> {
        let bytes = canonical_bytes(&demo_source_payload())?;
        let value: Value = serde_json::from_slice(&bytes)?;
        let object = value
            .as_object()
            .expect("canonical payload should be an object");
        let keys: Vec<_> = object.keys().cloned().collect();
        assert_eq!(
            keys,
            vec![
                "header".to_string(),
                "kind".to_string(),
                "payload".to_string(),
                "payload_kind".to_string()
            ],
            "top-level keys should be lexicographically ordered"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn canonical_archives_capture_symlinks() -> Result<()> {
        let temp = tempdir()?;
        let root = temp.path().join("root");
        fs::create_dir_all(&root)?;
        let target = root.join("data.txt");
        fs::write(&target, b"payload")?;
        let link = root.join("alias.txt");
        symlink(&target, &link)?;

        let archive = archive_dir_canonical(&root)?;
        let decoder = GzDecoder::new(&archive[..]);
        let mut archive = tar::Archive::new(decoder);
        let mut seen = 0;
        for entry in archive.entries()? {
            let mut entry = entry?;
            if entry.path()? == Path::new("alias.txt") {
                seen += 1;
                assert!(entry.header().entry_type().is_symlink());
                let target_path = entry
                    .link_name()?
                    .expect("symlink should have a target")
                    .into_owned();
                assert_eq!(target_path, Path::new("data.txt"));
                let mut body = String::new();
                let _ = entry.read_to_string(&mut body)?;
                assert!(body.is_empty(), "symlink should not carry file bytes");
            }
        }
        assert_eq!(seen, 1, "symlink entry should be captured once");
        Ok(())
    }

    #[test]
    fn gc_respects_size_limit() -> Result<()> {
        let (_temp, store) = new_store()?;
        let small = store.store(&ObjectPayload::Meta {
            bytes: Cow::Owned(vec![0u8; 8]),
        })?;
        let big = store.store(&ObjectPayload::Meta {
            bytes: Cow::Owned(vec![0u8; 1024]),
        })?;
        let small_size = store.object_info(&small.oid)?.unwrap().size;
        let limit = small_size + 1;

        // Make big very old and small in the future to preserve it.
        let conn = store.connection()?;
        let now = timestamp_secs() as i64;
        set_last_accessed(&conn, &big.oid, 0)?;
        set_last_accessed(&conn, &small.oid, now + 1_000)?;
        set_created_at(&conn, &big.oid, 0)?;
        set_created_at(&conn, &small.oid, now + 1_000)?;

        let summary = store.garbage_collect_with_limit(Duration::from_secs(0), limit)?;
        assert_eq!(
            summary.reclaimed, 1,
            "one object should be reclaimed respecting size ordering"
        );
        assert!(
            !store.object_path(&big.oid).exists(),
            "largest, oldest object should be reclaimed"
        );
        assert!(
            store.object_path(&small.oid).exists(),
            "small object should remain under limit"
        );
        Ok(())
    }

    #[test]
    fn gc_size_limit_respects_grace_window() -> Result<()> {
        let (_temp, store) = new_store()?;
        let old = store.store(&ObjectPayload::Meta {
            bytes: Cow::Owned(vec![1u8; 16]),
        })?;
        let fresh = store.store(&ObjectPayload::Meta {
            bytes: Cow::Owned(vec![2u8; 32]),
        })?;

        let conn = store.connection()?;
        set_last_accessed(&conn, &old.oid, 0)?;
        set_created_at(&conn, &old.oid, 0)?;

        let limit = 1; // force pressure well below the remaining object size
        let summary = store.garbage_collect_with_limit(Duration::from_secs(120), limit)?;
        assert!(
            summary.reclaimed >= 1,
            "stale objects should still be reclaimed under size pressure"
        );
        assert!(
            !store.object_path(&old.oid).exists(),
            "old object should be collected"
        );
        assert!(
            store.object_path(&fresh.oid).exists(),
            "recent object should be protected by grace period even if size cap unmet"
        );
        Ok(())
    }

    #[test]
    fn gc_removes_orphaned_on_disk_objects() -> Result<()> {
        let (_temp, store) = new_store()?;
        let stored = store.store(&demo_source_payload())?;
        let path = store.object_path(&stored.oid);
        assert!(path.exists(), "object should exist on disk");

        let conn = store.connection()?;
        conn.execute(
            "DELETE FROM objects WHERE oid = ?1",
            rusqlite::params![&stored.oid],
        )?;
        drop(conn);

        let summary = store.garbage_collect(Duration::from_secs(0))?;
        assert!(
            summary.reclaimed >= 1,
            "orphaned object should be reclaimed"
        );
        assert!(!path.exists(), "orphaned file should be removed");
        Ok(())
    }

    #[test]
    fn gc_removes_materialized_directories() -> Result<()> {
        let (_temp, store) = new_store()?;
        let stored = store.store(&ObjectPayload::Meta {
            bytes: Cow::Owned(b"meta".to_vec()),
        })?;

        let pkg_dir = store
            .root()
            .join(MATERIALIZED_PKG_BUILDS_DIR)
            .join(&stored.oid);
        fs::create_dir_all(&pkg_dir)?;
        fs::write(pkg_dir.join("payload"), b"pkg")?;
        let runtime_dir = store
            .root()
            .join(MATERIALIZED_RUNTIMES_DIR)
            .join(&stored.oid);
        fs::create_dir_all(&runtime_dir)?;
        fs::write(runtime_dir.join("python"), b"py")?;

        let conn = store.connection()?;
        set_last_accessed(&conn, &stored.oid, 0)?;

        let summary = store.garbage_collect(Duration::from_secs(0))?;
        assert_eq!(summary.reclaimed, 1, "object should be reclaimed");
        assert!(
            !pkg_dir.exists(),
            "pkg-build materialization should be removed with object"
        );
        assert!(
            !runtime_dir.exists(),
            "runtime materialization should be removed with object"
        );
        Ok(())
    }

    #[test]
    fn gc_rebuilds_index_before_running() -> Result<()> {
        let (temp, store) = new_store()?;
        let runtime = store.store(&ObjectPayload::Runtime {
            header: RuntimeHeader {
                version: "3.11.0".to_string(),
                abi: "cp311".to_string(),
                platform: "x86_64-manylinux".to_string(),
                build_config_hash: "abc".to_string(),
                exe_path: "bin/python".to_string(),
            },
            archive: Cow::Owned(b"runtime".to_vec()),
        })?;
        let profile_header = ProfileHeader {
            runtime_oid: runtime.oid.clone(),
            packages: vec![],
            sys_path_order: vec![],
            env_vars: BTreeMap::new(),
        };
        let profile = store.store(&ObjectPayload::Profile {
            header: profile_header.clone(),
        })?;
        let garbage = store.store(&ObjectPayload::Meta {
            bytes: Cow::Owned(b"stale".to_vec()),
        })?;
        filetime::set_file_mtime(
            store.object_path(&garbage.oid),
            filetime::FileTime::from_unix_time(0, 0),
        )?;

        let env_root = temp.path().join("envs").join(&profile.oid);
        fs::create_dir_all(&env_root)?;
        fs::write(
            env_root.join("manifest.json"),
            serde_json::to_string_pretty(&json!({
                "profile_oid": profile.oid,
                "runtime_oid": runtime.oid,
                "packages": profile_header.packages,
            }))?,
        )?;

        fs::remove_file(store.root().join(INDEX_FILENAME))?;
        let summary = store.garbage_collect(Duration::from_secs(0))?;
        assert!(
            store.object_path(&profile.oid).exists() && store.object_path(&runtime.oid).exists(),
            "referenced objects should survive GC after index rebuild"
        );
        assert!(
            !store.object_path(&garbage.oid).exists(),
            "unreferenced objects should be reclaimed after rebuild"
        );
        let refs = store.refs_for(&profile.oid)?;
        assert!(
            refs.iter().any(
                |owner| owner.owner_type == OwnerType::Profile && owner.owner_id == profile.oid
            ),
            "profile refs should be reconstructed from manifests"
        );
        assert!(
            summary.reclaimed >= 1,
            "garbage should be collected after rebuild"
        );
        Ok(())
    }

    #[test]
    fn lookup_keys_roundtrip_and_cleanup() -> Result<()> {
        let (_temp, store) = new_store()?;
        let header = SourceHeader {
            name: "demo".to_string(),
            version: "1.0.0".to_string(),
            filename: "demo-1.0.0.whl".to_string(),
            index_url: "https://example.invalid/simple/".to_string(),
            sha256: "deadbeef".to_string(),
        };
        let payload = ObjectPayload::Source {
            header: header.clone(),
            bytes: Cow::Owned(b"payload".to_vec()),
        };
        let stored = store.store(&payload)?;
        let key = source_lookup_key(&header);
        store.record_key(ObjectKind::Source, &key, &stored.oid)?;
        let found = store.lookup_key(ObjectKind::Source, &key)?;
        assert_eq!(found.as_deref(), Some(stored.oid.as_str()));

        fs::remove_file(&stored.path)?;
        let missing = store.lookup_key(ObjectKind::Source, &key)?;
        assert!(missing.is_none(), "stale mapping should be purged");
        Ok(())
    }

    #[test]
    fn list_filters_by_kind_and_prefix() -> Result<()> {
        let (_temp, store) = new_store()?;
        let source = store.store(&demo_source_payload())?;
        let meta = store.store(&ObjectPayload::Meta {
            bytes: Cow::Owned(b"meta".to_vec()),
        })?;

        let all = store.list(None, None)?;
        assert_eq!(all.len(), 2);
        assert!(
            all.windows(2).all(|w| w[0] <= w[1]),
            "list should be sorted"
        );

        let sources = store.list(Some(ObjectKind::Source), None)?;
        assert_eq!(sources, vec![source.oid.clone()]);

        let prefix = &meta.oid[..4.min(meta.oid.len())];
        let prefixed = store.list(None, Some(prefix))?;
        assert!(
            prefixed.iter().all(|oid| oid.starts_with(prefix)),
            "prefix filter should be applied"
        );
        Ok(())
    }

    #[test]
    fn profile_packages_are_canonicalized() -> Result<()> {
        let (_temp, store) = new_store()?;
        let pkg_a = ProfilePackage {
            name: "B".to_string(),
            version: "2.0.0".to_string(),
            pkg_build_oid: "pkg-b".to_string(),
        };
        let pkg_b = ProfilePackage {
            name: "A".to_string(),
            version: "1.0.0".to_string(),
            pkg_build_oid: "pkg-a".to_string(),
        };
        let header_unsorted = ProfileHeader {
            runtime_oid: "runtime".to_string(),
            packages: vec![pkg_a.clone(), pkg_b.clone()],
            sys_path_order: vec!["pkg-a".to_string(), "pkg-b".to_string()],
            env_vars: BTreeMap::new(),
        };
        let header_sorted = ProfileHeader {
            runtime_oid: "runtime".to_string(),
            packages: vec![pkg_b.clone(), pkg_a.clone()],
            sys_path_order: vec!["pkg-a".to_string(), "pkg-b".to_string()],
            env_vars: BTreeMap::new(),
        };

        let oid_unsorted = ContentAddressableStore::compute_oid(&ObjectPayload::Profile {
            header: header_unsorted.clone(),
        })?;
        let oid_sorted = ContentAddressableStore::compute_oid(&ObjectPayload::Profile {
            header: header_sorted.clone(),
        })?;
        assert_eq!(
            oid_unsorted, oid_sorted,
            "package order should be canonical"
        );

        let stored = store.store(&ObjectPayload::Profile {
            header: header_unsorted,
        })?;
        let loaded = store.load(&stored.oid)?;
        match loaded {
            LoadedObject::Profile { header, .. } => {
                let names: Vec<_> = header.packages.iter().map(|p| p.name.as_str()).collect();
                assert_eq!(
                    names,
                    vec!["A", "B"],
                    "packages should be sorted canonically"
                );
            }
            other => bail!("unexpected object {other:?}"),
        }

        let stored_again = store.store(&ObjectPayload::Profile {
            header: header_sorted,
        })?;
        assert_eq!(
            stored.oid, stored_again.oid,
            "dedupe should respect canonical order"
        );
        Ok(())
    }

    #[test]
    fn profile_env_vars_round_trip() -> Result<()> {
        let (_temp, store) = new_store()?;
        let mut env_vars = BTreeMap::new();
        env_vars.insert("FOO".to_string(), json!("bar"));
        env_vars.insert("COUNT".to_string(), json!(1));
        let header = ProfileHeader {
            runtime_oid: "runtime".to_string(),
            packages: vec![],
            sys_path_order: vec![],
            env_vars: env_vars.clone(),
        };
        let stored = store.store(&ObjectPayload::Profile { header })?;
        let loaded = store.load(&stored.oid)?;
        match loaded {
            LoadedObject::Profile { header, .. } => {
                assert_eq!(
                    header.env_vars, env_vars,
                    "env vars should persist in profile"
                );
            }
            other => bail!("unexpected object {other:?}"),
        }
        Ok(())
    }

    #[test]
    fn reports_kind_mismatch_from_index() -> Result<()> {
        let (_temp, store) = new_store()?;
        let stored = store.store(&demo_source_payload())?;
        let conn = store.connection()?;
        conn.execute(
            "UPDATE objects SET kind = 'meta' WHERE oid = ?1",
            rusqlite::params![&stored.oid],
        )?;
        let err = store.load(&stored.oid).unwrap_err();
        let store_err = err
            .downcast_ref::<StoreError>()
            .expect("should produce StoreError");
        assert!(
            matches!(
                store_err,
                StoreError::KindMismatch {
                    expected: ObjectKind::Meta,
                    found: ObjectKind::Source,
                    ..
                }
            ),
            "load should detect kind mismatch between index and object"
        );
        Ok(())
    }

    #[test]
    fn reports_size_mismatch_from_index() -> Result<()> {
        let (_temp, store) = new_store()?;
        let payload = demo_source_payload();
        let stored = store.store(&payload)?;
        let conn = store.connection()?;
        conn.execute(
            "UPDATE objects SET size = size + 1 WHERE oid = ?1",
            rusqlite::params![&stored.oid],
        )?;
        let err = store.store(&payload).unwrap_err();
        let store_err = err
            .downcast_ref::<StoreError>()
            .expect("should produce StoreError");
        assert!(
            matches!(store_err, StoreError::SizeMismatch { .. }),
            "size mismatch should be surfaced during store"
        );
        Ok(())
    }

    #[test]
    fn add_ref_to_missing_object_errors() -> Result<()> {
        let (_temp, store) = new_store()?;
        let owner = OwnerId {
            owner_type: OwnerType::ProjectEnv,
            owner_id: "proj".to_string(),
        };
        let err = store.add_ref(&owner, "deadbeef").unwrap_err();
        let store_err = err
            .downcast_ref::<StoreError>()
            .expect("should produce StoreError");
        assert!(matches!(store_err, StoreError::MissingObject { .. }));
        let refs = store.refs_for("deadbeef")?;
        assert!(
            refs.is_empty(),
            "no refs should be recorded for missing object"
        );
        Ok(())
    }

    #[test]
    fn stores_large_payloads() -> Result<()> {
        let (_temp, store) = new_store()?;
        let big = vec![42u8; 2 * 1024 * 1024 + 123];
        let payload = ObjectPayload::Meta {
            bytes: Cow::Owned(big.clone()),
        };
        let stored = store.store(&payload)?;
        let loaded = store.load(&stored.oid)?;
        match loaded {
            LoadedObject::Meta { bytes, .. } => assert_eq!(bytes, big),
            other => bail!("unexpected object {other:?}"),
        }
        Ok(())
    }

    #[test]
    fn doctor_removes_missing_objects_and_metadata() -> Result<()> {
        let (_temp, store) = new_store()?;
        let stored = store.store(&ObjectPayload::Meta {
            bytes: Cow::Owned(b"meta".to_vec()),
        })?;
        let owner = OwnerId {
            owner_type: OwnerType::ProjectEnv,
            owner_id: "proj".to_string(),
        };
        store.add_ref(&owner, &stored.oid)?;
        store.record_key(ObjectKind::Meta, "demo-key", &stored.oid)?;

        fs::remove_file(&stored.path)?;
        let summary = store.doctor()?;
        assert_eq!(
            summary.missing_objects, 1,
            "missing object should be flagged"
        );
        assert_eq!(
            summary.objects_removed, 1,
            "missing object should be purged"
        );
        assert!(
            store.object_info(&stored.oid)?.is_none(),
            "missing object should be removed from the index"
        );
        assert!(
            store.lookup_key(ObjectKind::Meta, "demo-key")?.is_none(),
            "stale lookup keys should be pruned"
        );
        assert!(
            store.refs_for(&stored.oid)?.is_empty(),
            "dangling refs should be cleaned"
        );
        Ok(())
    }

    #[test]
    fn doctor_purges_corrupt_objects_and_materializations() -> Result<()> {
        let (_temp, store) = new_store()?;
        let stored = store.store(&ObjectPayload::Meta {
            bytes: Cow::Owned(b"meta".to_vec()),
        })?;
        let pkg_dir = store
            .root()
            .join(MATERIALIZED_PKG_BUILDS_DIR)
            .join(&stored.oid);
        fs::create_dir_all(&pkg_dir)?;
        fs::write(pkg_dir.join("payload"), b"pkg")?;
        make_writable(&stored.path);
        fs::write(&stored.path, b"corrupt")?;

        let summary = store.doctor()?;
        assert_eq!(
            summary.corrupt_objects, 1,
            "corrupt object should be reported"
        );
        assert!(
            !pkg_dir.exists(),
            "materialized directories should be removed for purged objects"
        );
        assert!(
            store.object_info(&stored.oid)?.is_none(),
            "corrupt object should be removed"
        );
        Ok(())
    }

    #[test]
    fn doctor_removes_partials_and_counts_them() -> Result<()> {
        let (_temp, store) = new_store()?;
        let stored = store.store(&ObjectPayload::Meta {
            bytes: Cow::Owned(b"meta".to_vec()),
        })?;
        let tmp = store.tmp_path(&stored.oid);
        if let Some(parent) = tmp.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&tmp, b"junk")?;

        let summary = store.doctor()?;
        assert!(
            summary.partials_removed >= 1,
            "doctor should clean partials"
        );
        assert!(!tmp.exists(), "partial file should be removed");
        Ok(())
    }

    #[test]
    fn doctor_skips_locked_objects_until_retried() -> Result<()> {
        let (_temp, store) = new_store()?;
        let stored = store.store(&ObjectPayload::Meta {
            bytes: Cow::Owned(b"meta".to_vec()),
        })?;
        // Simulate a missing object while holding the lock to force a skip.
        fs::remove_file(&stored.path)?;
        let lock = store.acquire_lock(&stored.oid)?;
        let summary = store.doctor()?;
        assert_eq!(
            summary.locked_skipped, 1,
            "doctor should skip locked objects"
        );
        assert!(
            store.object_info(&stored.oid)?.is_some(),
            "locked object should remain indexed"
        );
        drop(lock);
        let summary = store.doctor()?;
        assert_eq!(
            summary.missing_objects, 1,
            "doctor should clean missing object after lock release"
        );
        assert!(
            store.object_info(&stored.oid)?.is_none(),
            "missing object should be removed after retry"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn canonical_archives_rewrite_symlinks_outside_root() -> Result<()> {
        let temp = tempdir()?;
        let root = temp.path().join("root");
        fs::create_dir_all(&root)?;
        let outside = temp.path().join("outside.txt");
        fs::write(&outside, b"payload")?;
        let link = root.join("alias.txt");
        symlink(&outside, &link)?;

        let archive = archive_dir_canonical(&root)?;
        let decoder = GzDecoder::new(&archive[..]);
        let mut archive = tar::Archive::new(decoder);
        let mut seen = false;
        for entry in archive.entries()? {
            let entry = entry?;
            if entry.path()? == Path::new("alias.txt") {
                seen = true;
                let target = entry
                    .link_name()?
                    .expect("symlink should have target")
                    .into_owned();
                assert_eq!(
                    target,
                    Path::new("outside.txt"),
                    "absolute targets should be rewritten to a relative basename"
                );
            }
        }
        assert!(seen, "symlink entry should be captured");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn canonical_archives_rewrite_absolute_symlinks() -> Result<()> {
        let temp = tempdir()?;
        let root = temp.path().join("root");
        fs::create_dir_all(root.join("dir"))?;
        let target = root.join("dir").join("data.txt");
        fs::write(&target, b"payload")?;
        let abs_target = fs::canonicalize(&target)?;
        let link = root.join("alias.txt");
        symlink(&abs_target, &link)?;

        let archive = archive_dir_canonical(&root)?;
        let decoder = GzDecoder::new(&archive[..]);
        let mut archive = tar::Archive::new(decoder);
        let mut seen = 0;
        for entry in archive.entries()? {
            let entry = entry?;
            if entry.path()? == Path::new("alias.txt") {
                seen += 1;
                let target = entry
                    .link_name()?
                    .expect("symlink should have a target")
                    .into_owned();
                assert_eq!(
                    target,
                    Path::new("dir").join("data.txt"),
                    "absolute targets within root should be relativized"
                );
            }
        }
        assert_eq!(seen, 1, "symlink entry should be captured once");
        Ok(())
    }
}
