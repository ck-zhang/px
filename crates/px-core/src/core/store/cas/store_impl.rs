// Core CAS store operations (moved out of mod.rs for readability).
use super::*;

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

    pub(super) fn ensure_object_present_in_index(&self, oid: &str) -> Result<()> {
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
        self.verify_existing(oid, &path)?;
        let kind = canonical_kind_from_path(oid, &path)?;
        let size = fs::metadata(&path)
            .with_context(|| format!("failed to stat CAS object at {}", path.display()))?
            .len();
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

    pub(super) fn ensure_layout(&self) -> Result<()> {
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

    pub(super) fn connection(&self) -> Result<Connection> {
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
        conn.pragma_update(None, "journal_mode", "WAL")
            .context("failed to enable WAL for CAS index")?;
        conn.pragma_update(None, "foreign_keys", "ON")
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

    pub(super) fn ensure_index_health(&self, force_integrity: bool) -> Result<()> {
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
            if !result.eq_ignore_ascii_case("ok") {
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
            .filter(|name| !found.contains(*name))
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
        conn.pragma_update(None, "journal_mode", "WAL")
            .context("failed to enable WAL for rebuilt CAS index")?;
        conn.pragma_update(None, "foreign_keys", "ON")
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
                .and_then(|r| (!r.version.is_empty()).then_some(r.version.as_str()))
                .or_else(|| {
                    env.python
                        .as_ref()
                        .and_then(|py| (!py.version.is_empty()).then_some(py.version.as_str()))
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
                .and_then(|r| (!r.version.is_empty()).then_some(r.version.as_str()))
                .or_else(|| {
                    env.python
                        .as_ref()
                        .and_then(|py| (!py.version.is_empty()).then_some(py.version.as_str()))
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

        for dir in [
            MATERIALIZED_PKG_BUILDS_DIR,
            MATERIALIZED_RUNTIMES_DIR,
            MATERIALIZED_REPO_SNAPSHOTS_DIR,
        ] {
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

    pub(super) fn verify_existing(&self, oid: &str, path: &Path) -> Result<()> {
        let mut file = File::open(path)
            .with_context(|| format!("failed to open existing CAS object {}", path.display()))?;
        let mut hasher = Sha256::new();
        let mut buf = [0u8; 32 * 1024];
        loop {
            let read = file.read(&mut buf).with_context(|| {
                format!("failed to read existing CAS object {}", path.display())
            })?;
            if read == 0 {
                break;
            }
            hasher.update(&buf[..read]);
        }
        let actual = hex::encode(hasher.finalize());
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

    pub(super) fn verify_bytes(&self, oid: &str, bytes: &[u8]) -> Result<()> {
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

    pub(super) fn ensure_index_entry(&self, oid: &str, kind: ObjectKind, size: u64) -> Result<()> {
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

    pub(super) fn object_info_with_conn(
        &self,
        conn: &Connection,
        oid: &str,
    ) -> Result<Option<ObjectInfo>> {
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

    pub(super) fn touch_object(&self, conn: &mut Connection, oid: &str, now: u64) -> Result<()> {
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
            .truncate(true)
            .open(&path)
            .with_context(|| format!("failed to open CAS lock {}", path.display()))?;
        file.lock_exclusive()
            .with_context(|| format!("failed to lock {}", path.display()))?;
        Ok(file)
    }

    pub(super) fn object_path(&self, oid: &str) -> PathBuf {
        let shard = oid.get(0..2).unwrap_or("xx");
        self.root.join(OBJECTS_DIR).join(shard).join(oid)
    }

    pub(super) fn lock_path(&self, oid: &str) -> PathBuf {
        self.root.join(LOCKS_DIR).join(format!("{oid}.lock"))
    }

    pub(super) fn tmp_path(&self, oid: &str) -> PathBuf {
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
            CanonicalData::RepoSnapshot { header, payload } => Ok(LoadedObject::RepoSnapshot {
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
