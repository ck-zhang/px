use super::*;

impl ContentAddressableStore {
    /// Perform a mark-and-sweep GC. Objects without any references and older
    /// than the supplied grace window are deleted from disk and the index.
    pub fn garbage_collect(&self, grace: Duration) -> Result<GcSummary> {
        self.ensure_layout()?;
        self.ensure_index_health(true).map_err(|err| {
            warn!(%err, "skipping cas gc because index is unhealthy");
            err
        })?;
        let mut conn = self.connection()?;
        let live = self.live_set(&conn)?;
        let cutoff = timestamp_secs().saturating_sub(grace.as_secs());
        let mut summary = GcSummary::default();

        let mut stmt = conn.prepare("SELECT oid, kind, size, created_at FROM objects")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)? as u64,
                    row.get::<_, i64>(3)? as u64,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        drop(stmt);

        for (oid, kind_str, size, created_at) in rows {
            ObjectKind::try_from(kind_str.as_str())?;
            summary.scanned += 1;

            if live.contains(&oid) || created_at > cutoff {
                continue;
            }

            let Some(_lock) = self.try_lock_for_gc(&oid)? else {
                // Another process is using the object; skip it for now.
                continue;
            };

            if self.delete_if_unreferenced(&mut conn, &oid)? {
                summary.reclaimed += 1;
                summary.reclaimed_bytes += size;
            }
        }

        let (orphans, orphan_bytes) = self.sweep_orphaned_objects(&conn, cutoff)?;
        summary.reclaimed += orphans;
        summary.reclaimed_bytes += orphan_bytes;
        summary.scanned += orphans;

        debug!(
            scanned = summary.scanned,
            reclaimed = summary.reclaimed,
            reclaimed_bytes = summary.reclaimed_bytes,
            "cas gc sweep complete"
        );

        Ok(summary)
    }

    /// Enforce a soft size cap by reclaiming oldest unreferenced objects after an initial GC.
    pub fn garbage_collect_with_limit(&self, grace: Duration, max_bytes: u64) -> Result<GcSummary> {
        let mut summary = self.garbage_collect(grace)?;
        let mut conn = self.connection()?;
        let total: i64 =
            conn.query_row("SELECT COALESCE(SUM(size), 0) FROM objects", [], |row| {
                row.get(0)
            })?;
        if total as u64 <= max_bytes {
            return Ok(summary);
        }

        let cutoff = timestamp_secs().saturating_sub(grace.as_secs());
        let mut stmt = conn.prepare(
            "SELECT o.oid, o.size, o.last_accessed, o.created_at FROM objects o \
             LEFT JOIN refs r ON r.oid = o.oid \
             WHERE r.oid IS NULL AND o.created_at <= ?1 \
             ORDER BY o.last_accessed ASC",
        )?;
        let rows = stmt
            .query_map(params![cutoff as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)? as u64,
                    row.get::<_, i64>(2)? as u64,
                    row.get::<_, i64>(3)? as u64,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        drop(stmt);

        let mut current = total as u64;
        let mut locked = 0usize;
        for (oid, size, _, _) in rows {
            if current <= max_bytes {
                break;
            }
            let Some(_lock) = self.try_lock_for_gc(&oid)? else {
                locked += 1;
                continue;
            };
            if self.delete_if_unreferenced(&mut conn, &oid)? {
                current = current.saturating_sub(size);
                summary.reclaimed += 1;
                summary.reclaimed_bytes += size;
            }
        }
        if current > max_bytes {
            warn!(
                remaining_bytes = current,
                limit_bytes = max_bytes,
                grace_secs = grace.as_secs(),
                locked_objects = locked,
                "cas size cap respected grace window but could not reach limit"
            );
        } else {
            debug!(
                reclaimed = summary.reclaimed,
                reclaimed_bytes = summary.reclaimed_bytes,
                limit_bytes = max_bytes,
                locked_objects = locked,
                "cas size cap enforcement complete"
            );
        }
        Ok(summary)
    }

    pub(super) fn sweep_orphaned_objects(
        &self,
        conn: &Connection,
        cutoff: u64,
    ) -> Result<(usize, u64)> {
        let objects_root = self.root.join(OBJECTS_DIR);
        if !objects_root.exists() {
            return Ok((0, 0));
        }
        let mut reclaimed = 0usize;
        let mut reclaimed_bytes = 0u64;
        for entry in walkdir::WalkDir::new(&objects_root)
            .min_depth(2)
            .max_depth(2)
        {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path().to_path_buf();
            let Some(file_name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if self.object_info_with_conn(conn, file_name)?.is_some() {
                continue;
            }
            let modified = file_modified_secs(&path).unwrap_or(0);
            if modified > cutoff {
                continue;
            }
            let Some(_lock) = self.try_lock_for_gc(file_name)? else {
                continue;
            };
            let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            // Treat digest mismatches as corrupt and remove them too.
            let _ = self.verify_existing(file_name, &path);
            let _ = fs::remove_file(&path);
            if let Some(parent) = path.parent() {
                fsync_dir(parent).ok();
            }
            let _ = conn.execute("DELETE FROM keys WHERE oid = ?1", params![file_name]);
            let _ = conn.execute("DELETE FROM refs WHERE oid = ?1", params![file_name]);
            let _ = conn.execute("DELETE FROM objects WHERE oid = ?1", params![file_name]);
            self.remove_materialized(file_name)?;
            reclaimed += 1;
            reclaimed_bytes = reclaimed_bytes.saturating_add(size);
        }
        Ok((reclaimed, reclaimed_bytes))
    }

    fn live_set(&self, conn: &Connection) -> Result<HashSet<String>> {
        let mut stmt = conn.prepare("SELECT DISTINCT oid FROM refs")?;
        let mut rows = stmt.query([])?;
        let mut set = HashSet::new();
        while let Some(row) = rows.next()? {
            let oid: String = row.get(0)?;
            set.insert(oid);
        }
        Ok(set)
    }

    fn delete_if_unreferenced(&self, conn: &mut Connection, oid: &str) -> Result<bool> {
        // Remove the index row only if no refs exist at deletion time to avoid racing
        // with concurrent ref creation.
        let tx = conn.transaction()?;
        let deleted = tx.execute(
            "DELETE FROM objects \
             WHERE oid = ?1 \
             AND NOT EXISTS (SELECT 1 FROM refs WHERE refs.oid = ?1)",
            params![oid],
        )?;
        tx.commit()?;

        if deleted == 0 {
            return Ok(false);
        }

        let path = self.object_path(oid);
        if path.exists() {
            fs::remove_file(&path)
                .with_context(|| format!("failed to delete CAS object {}", path.display()))?;
            if let Some(parent) = path.parent() {
                fsync_dir(parent).ok();
            }
        }

        self.remove_materialized(oid)?;

        // Clean up stale partial files to avoid future collisions.
        let tmp = self.tmp_path(oid);
        if tmp.exists() {
            let _ = fs::remove_file(tmp);
        }
        let _ = conn.execute("DELETE FROM keys WHERE oid = ?1", params![oid]);
        Ok(true)
    }

    pub(super) fn remove_materialized(&self, oid: &str) -> Result<()> {
        for dir in [
            MATERIALIZED_PKG_BUILDS_DIR,
            MATERIALIZED_RUNTIMES_DIR,
            MATERIALIZED_REPO_SNAPSHOTS_DIR,
        ] {
            let path = self.root.join(dir).join(oid);
            if path.exists() {
                fs::remove_dir_all(&path).with_context(|| {
                    format!(
                        "failed to remove materialized CAS directory {}",
                        path.display()
                    )
                })?;
            }
            let partial = path.with_extension("partial");
            if partial.exists() {
                let _ = fs::remove_dir_all(&partial);
            }
        }
        Ok(())
    }

    pub(super) fn try_lock_for_gc(&self, oid: &str) -> Result<Option<File>> {
        let path = self.lock_path(oid);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(&path)?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(file)),
            Err(err) if err.kind() == ErrorKind::WouldBlock => Ok(None),
            Err(err) => Err(err.into()),
        }
    }
}

/// Run GC with environment-driven policy. Returns `Ok(None)` when disabled via
/// `PX_CAS_GC_DISABLE=1`.
pub fn run_gc_with_env_policy(store: &ContentAddressableStore) -> Result<Option<GcSummary>> {
    if env::var("PX_CAS_GC_DISABLE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
    {
        return Ok(None);
    }
    let grace_secs = env::var("PX_CAS_GC_GRACE_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(86_400);
    let max_bytes = env::var("PX_CAS_MAX_BYTES")
        .ok()
        .and_then(|v| v.parse::<u64>().ok());
    let grace = Duration::from_secs(grace_secs);
    let summary = match max_bytes {
        Some(limit) => store.garbage_collect_with_limit(grace, limit)?,
        None => store.garbage_collect(grace)?,
    };
    Ok(Some(summary))
}
