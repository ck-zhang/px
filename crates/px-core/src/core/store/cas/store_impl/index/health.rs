//! Index validation/integrity checks + rebuild trigger.

use super::super::super::*;

impl ContentAddressableStore {
    pub(in crate::core::store::cas) fn ensure_index_health(
        &self,
        force_integrity: bool,
    ) -> Result<()> {
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
}
