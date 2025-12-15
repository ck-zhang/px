//! Objects table helpers (insert/check/touch).

use super::super::super::*;

impl ContentAddressableStore {
    pub(in crate::core::store::cas) fn ensure_index_entry(
        &self,
        oid: &str,
        kind: ObjectKind,
        size: u64,
    ) -> Result<()> {
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

    pub(in crate::core::store::cas) fn object_info_with_conn(
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

    pub(in crate::core::store::cas) fn touch_object(
        &self,
        conn: &mut Connection,
        oid: &str,
        now: u64,
    ) -> Result<()> {
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

    pub(in crate::core::store::cas::store_impl) fn assert_object_known(
        &self,
        conn: &Connection,
        oid: &str,
    ) -> Result<()> {
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
}
