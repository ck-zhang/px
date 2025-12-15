//! SQLite connection + transaction helpers.

use super::super::super::*;

impl ContentAddressableStore {
    pub(in crate::core::store::cas) fn connection(&self) -> Result<Connection> {
        let conn = self.connection_raw()?;
        conn.busy_timeout(Duration::from_secs(10))
            .context("failed to set busy timeout for CAS index")?;
        Ok(conn)
    }

    pub(in crate::core::store::cas::store_impl) fn with_immediate_tx<T, F>(&self, f: F) -> Result<T>
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

    pub(super) fn connection_raw(&self) -> Result<Connection> {
        let path = self.index_path();
        let conn = Connection::open(&path)
            .with_context(|| format!("failed to open CAS index at {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .context("failed to enable WAL for CAS index")?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .context("failed to enable foreign keys for CAS index")?;
        Ok(conn)
    }
}
