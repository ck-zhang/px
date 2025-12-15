//! Meta table initialization + version enforcement.

use super::super::super::*;

impl ContentAddressableStore {
    pub(super) fn ensure_meta(&self, conn: &mut Connection) -> Result<()> {
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

    fn meta_value(&self, conn: &Connection, key: &str) -> Result<Option<String>> {
        conn.query_row(
            "SELECT value FROM meta WHERE key = ?1",
            params![key],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(Into::into)
    }

    pub(super) fn enforce_meta_version(
        &self,
        conn: &Connection,
        key: &str,
        expected: u32,
    ) -> Result<()> {
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

    pub(super) fn require_meta_presence(&self, conn: &Connection, key: &str) -> Result<()> {
        self.meta_value(conn, key)?
            .ok_or_else(|| StoreError::MissingMeta(key.to_string()))?;
        Ok(())
    }

    pub(super) fn record_last_used_px_version(&self, conn: &mut Connection) -> Result<()> {
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![META_KEY_LAST_USED, PX_VERSION],
        )?;
        tx.commit()?;
        Ok(())
    }
}
