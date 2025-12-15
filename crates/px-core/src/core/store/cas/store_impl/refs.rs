use super::super::*;

impl ContentAddressableStore {
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
}
