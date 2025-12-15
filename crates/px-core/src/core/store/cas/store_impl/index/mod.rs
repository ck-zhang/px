//! CAS index management.
//!
//! The CAS index is a SQLite database used to track CAS objects and their owners. The code is
//! split by responsibility (connection/meta/health/rebuild/objects) to keep changes reviewable.

use super::super::*;

mod connection;
mod health;
mod meta;
mod objects;
mod permissions;
mod rebuild;
mod schema;

impl ContentAddressableStore {
    pub(in crate::core::store::cas) fn ensure_layout(&self) -> Result<()> {
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

    fn index_path(&self) -> PathBuf {
        self.root.join(INDEX_FILENAME)
    }
}
