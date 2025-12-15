//! Index rebuild by scanning on-disk store objects and known owner state files.

use super::super::super::*;

impl ContentAddressableStore {
    pub(super) fn rebuild_index_from_store(&self) -> Result<()> {
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
}
