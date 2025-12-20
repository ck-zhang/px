use super::super::*;

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

    /// Return metadata about an object if present in the index.
    pub fn object_info(&self, oid: &str) -> Result<Option<ObjectInfo>> {
        self.ensure_layout()?;
        let mut conn = self.connection()?;
        if let Some(info) = self.object_info_with_conn(&conn, oid)? {
            return Ok(Some(info));
        }
        self.repair_object_index_from_disk(&mut conn, oid)
    }

    pub(in super::super) fn ensure_object_present_in_index(&self, oid: &str) -> Result<()> {
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

    pub(in super::super) fn verify_existing(&self, oid: &str, path: &Path) -> Result<()> {
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

    pub(in super::super) fn verify_bytes(&self, oid: &str, bytes: &[u8]) -> Result<()> {
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

    pub(in super::super) fn object_path(&self, oid: &str) -> PathBuf {
        let shard = oid.get(0..2).unwrap_or("xx");
        self.root.join(OBJECTS_DIR).join(shard).join(oid)
    }

    pub(in super::super) fn lock_path(&self, oid: &str) -> PathBuf {
        let filename = if !oid.is_empty()
            && oid.bytes().all(|b| {
                matches!(
                    b,
                    b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z' | b'.' | b'_' | b'-'
                )
            }) {
            oid.to_string()
        } else {
            hex::encode(Sha256::digest(oid.as_bytes()))
        };
        self.root.join(LOCKS_DIR).join(format!("{filename}.lock"))
    }

    pub(in super::super) fn tmp_path(&self, oid: &str) -> PathBuf {
        self.root.join(TMP_DIR).join(format!("{oid}.partial"))
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
