//! Index schema initialization (SQLite DDL).

use super::super::super::*;

impl ContentAddressableStore {
    pub(super) fn init_schema(&self, conn: &Connection) -> Result<()> {
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
}
