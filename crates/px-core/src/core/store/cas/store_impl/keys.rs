use super::super::*;

impl ContentAddressableStore {
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
}
