use super::*;

impl ContentAddressableStore {
    /// Sweep and delete any `.partial` files left over from failed writes.
    pub fn sweep_partials(&self) -> Result<u64> {
        self.ensure_layout()?;
        let mut removed = 0;
        let tmp_dir = self.root.join(TMP_DIR);
        if !tmp_dir.exists() {
            return Ok(removed);
        }
        for entry in fs::read_dir(&tmp_dir)? {
            let entry = entry?;
            if entry.file_type()?.is_file()
                && entry.file_name().to_string_lossy().contains(".partial")
            {
                fs::remove_file(entry.path())?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Verify the integrity of a subset of objects by recomputing their oids.
    pub fn verify_sample(&self, sample: usize) -> Result<Vec<String>> {
        self.ensure_layout()?;
        let conn = self.connection()?;
        let mut stmt = conn.prepare("SELECT oid FROM objects")?;
        let all = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut rng = thread_rng();
        let picked = all
            .iter()
            .cloned()
            .choose_multiple(&mut rng, sample.min(all.len()));

        let mut failures = Vec::new();
        for oid in picked {
            match self.load(&oid) {
                Ok(_) => {}
                Err(err) => failures.push(format!("{oid}: {err}")),
            }
        }
        Ok(failures)
    }

    /// Best-effort self-healing pass that removes corrupt/missing objects and stale metadata.
    pub fn doctor(&self) -> Result<DoctorSummary> {
        self.ensure_layout()?;
        let partials_removed = self.sweep_partials()?;
        let mut summary = DoctorSummary {
            partials_removed,
            ..DoctorSummary::default()
        };

        let mut conn = self.connection()?;
        let mut stmt = conn.prepare("SELECT oid, kind FROM objects")?;
        let objects = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);

        for (oid, kind_str) in objects {
            let kind = match ObjectKind::try_from(kind_str.as_str()) {
                Ok(kind) => kind,
                Err(_) => {
                    if let Some(_lock) = self.try_lock_for_gc(&oid)? {
                        let (refs_pruned, keys_pruned) =
                            self.purge_object(&mut conn, &oid, None)?;
                        summary.refs_pruned += refs_pruned;
                        summary.keys_pruned += keys_pruned;
                        summary.corrupt_objects += 1;
                        summary.objects_removed += 1;
                    } else {
                        summary.locked_skipped += 1;
                    }
                    continue;
                }
            };
            let path = self.object_path(&oid);
            let issue = if !path.exists() {
                Some(false)
            } else {
                match fs::read(&path) {
                    Ok(bytes) => {
                        if self.verify_bytes(&oid, &bytes).is_err()
                            || canonical_kind(&bytes).ok() != Some(kind)
                        {
                            Some(true)
                        } else {
                            None
                        }
                    }
                    Err(_) => Some(true),
                }
            };

            if let Some(is_corrupt) = issue {
                if let Some(_lock) = self.try_lock_for_gc(&oid)? {
                    let (refs_pruned, keys_pruned) =
                        self.purge_object(&mut conn, &oid, Some(&path))?;
                    summary.refs_pruned += refs_pruned;
                    summary.keys_pruned += keys_pruned;
                    summary.objects_removed += 1;
                    if is_corrupt {
                        summary.corrupt_objects += 1;
                    } else {
                        summary.missing_objects += 1;
                    }
                } else {
                    summary.locked_skipped += 1;
                }
            }
        }

        let (orphan_removed, _) = self.sweep_orphaned_objects(&conn, timestamp_secs())?;
        summary.objects_removed += orphan_removed;

        let (refs_pruned, keys_pruned) = self.prune_orphans(&mut conn)?;
        summary.refs_pruned += refs_pruned;
        summary.keys_pruned += keys_pruned;
        Ok(summary)
    }

    fn purge_object(
        &self,
        conn: &mut Connection,
        oid: &str,
        path_hint: Option<&Path>,
    ) -> Result<(usize, usize)> {
        let tx = conn.transaction()?;
        let refs_count: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM refs WHERE oid = ?1",
                params![oid],
                |row| row.get(0),
            )
            .unwrap_or(0);
        let keys_removed = tx.execute("DELETE FROM keys WHERE oid = ?1", params![oid])?;
        let deleted = tx.execute("DELETE FROM objects WHERE oid = ?1", params![oid])?;
        tx.commit()?;

        if deleted > 0 {
            let path = path_hint
                .map(PathBuf::from)
                .unwrap_or_else(|| self.object_path(oid));
            if path.exists() {
                let _ = fs::remove_file(&path);
                if let Some(parent) = path.parent() {
                    fsync_dir(parent).ok();
                }
            }
            self.remove_materialized(oid)?;
            let tmp = self.tmp_path(oid);
            if tmp.exists() {
                let _ = fs::remove_file(tmp);
            }
        }

        Ok((refs_count as usize, keys_removed))
    }

    fn prune_orphans(&self, conn: &mut Connection) -> Result<(usize, usize)> {
        let refs_pruned = conn.execute(
            "DELETE FROM refs WHERE oid NOT IN (SELECT oid FROM objects)",
            [],
        )?;
        let keys_pruned = conn.execute(
            "DELETE FROM keys WHERE oid NOT IN (SELECT oid FROM objects)",
            [],
        )?;
        Ok((refs_pruned, keys_pruned))
    }
}
