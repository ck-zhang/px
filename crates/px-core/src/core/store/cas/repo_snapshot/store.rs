use super::super::*;
use super::errors::{repo_snapshot_user_error, RepoSnapshotIssue};
use super::keys::repo_snapshot_lookup_key;
use super::resolve::{px_online_enabled, resolve_repo_snapshot_spec, ResolvedRepoLocator};
use super::RepoSnapshotSpec;

impl ContentAddressableStore {
    /// Ensure a deterministic, commit-pinned repository snapshot exists in the CAS.
    ///
    /// # Errors
    ///
    /// Returns an error if the repository cannot be accessed, the commit SHA is
    /// invalid, the snapshot cannot be produced deterministically, or the store
    /// is offline for a network-backed locator.
    pub fn ensure_repo_snapshot(&self, spec: &RepoSnapshotSpec) -> Result<String> {
        let resolved = resolve_repo_snapshot_spec(spec)?;
        let key = repo_snapshot_lookup_key(&resolved.header);
        if let Some(oid) = self.lookup_key(ObjectKind::RepoSnapshot, &key)? {
            debug!(
                oid = %oid,
                locator = %resolved.header.locator,
                commit = %resolved.header.commit,
                subdir = resolved.header.subdir.as_deref().unwrap_or(""),
                "repo-snapshot cache hit"
            );
            return Ok(oid);
        }
        debug!(
            locator = %resolved.header.locator,
            commit = %resolved.header.commit,
            subdir = resolved.header.subdir.as_deref().unwrap_or(""),
            "repo-snapshot cache miss"
        );

        match &resolved.locator {
            ResolvedRepoLocator::File { repo_path, .. } => {
                debug!(
                    repo_path = %repo_path.display(),
                    locator = %resolved.header.locator,
                    commit = %resolved.header.commit,
                    "repo-snapshot creating snapshot via git archive"
                );
                let temp =
                    tempdir().context("failed to create temp directory for repo snapshot")?;
                let checkout_root = temp.path().join("repo");
                fs::create_dir_all(&checkout_root)?;

                let tar_path = temp.path().join("repo.tar");
                let output = Command::new("git")
                    .arg("-C")
                    .arg(repo_path)
                    .arg("archive")
                    .arg("--format=tar")
                    .arg("--output")
                    .arg(&tar_path)
                    .arg(&resolved.header.commit)
                    .output()
                    .map_err(|err| {
                        repo_snapshot_user_error(RepoSnapshotIssue::GitInvocationFailed {
                            error: err.to_string(),
                        })
                    })?;
                if !output.status.success() {
                    return Err(repo_snapshot_user_error(
                        RepoSnapshotIssue::GitArchiveFailed {
                            locator: resolved.header.locator.clone(),
                            commit: resolved.header.commit.clone(),
                            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
                        },
                    ));
                }

                let file = File::open(&tar_path).with_context(|| {
                    format!("failed to open git archive {}", tar_path.display())
                })?;
                let mut archive = Archive::new(file);
                archive
                    .unpack(&checkout_root)
                    .context("failed to unpack git archive for repo snapshot")?;

                let snapshot_root = match resolved.subdir_rel.as_ref() {
                    Some(subdir) => checkout_root.join(subdir),
                    None => checkout_root,
                };
                if !snapshot_root.exists() {
                    return Err(repo_snapshot_user_error(RepoSnapshotIssue::MissingSubdir {
                        subdir: resolved
                            .header
                            .subdir
                            .clone()
                            .unwrap_or_else(|| "<missing>".to_string()),
                        commit: resolved.header.commit.clone(),
                    }));
                }

                let stored = self
                    .store_repo_snapshot_streaming(&resolved.header, &snapshot_root)
                    .map_err(store_write_error)?;
                self.record_key(ObjectKind::RepoSnapshot, &key, &stored.oid)
                    .map_err(store_write_error)?;
                debug!(
                    oid = %stored.oid,
                    locator = %resolved.header.locator,
                    commit = %resolved.header.commit,
                    subdir = resolved.header.subdir.as_deref().unwrap_or(""),
                    "repo-snapshot stored"
                );
                Ok(stored.oid)
            }
            ResolvedRepoLocator::Remote { .. } => {
                if !px_online_enabled() {
                    debug!(
                        locator = %resolved.header.locator,
                        commit = %resolved.header.commit,
                        "repo-snapshot blocked by offline mode"
                    );
                    return Err(repo_snapshot_user_error(RepoSnapshotIssue::Offline {
                        locator: resolved.header.locator.clone(),
                        commit: resolved.header.commit.clone(),
                    }));
                }
                debug!(
                    locator = %resolved.header.locator,
                    commit = %resolved.header.commit,
                    "repo-snapshot creating snapshot via git fetch"
                );
                let temp =
                    tempdir().context("failed to create temp directory for repo snapshot")?;
                let checkout_root = temp.path().join("repo");
                fs::create_dir_all(&checkout_root)?;

                let remote_url = resolved.header.locator.strip_prefix("git+").unwrap_or("");
                let init = Command::new("git")
                    .arg("-C")
                    .arg(&checkout_root)
                    .arg("init")
                    .arg("--quiet")
                    .output()
                    .map_err(|err| {
                        repo_snapshot_user_error(RepoSnapshotIssue::GitInvocationFailed {
                            error: err.to_string(),
                        })
                    })?;
                if !init.status.success() {
                    return Err(repo_snapshot_user_error(
                        RepoSnapshotIssue::GitFetchFailed {
                            locator: resolved.header.locator.clone(),
                            commit: resolved.header.commit.clone(),
                            stderr: String::from_utf8_lossy(&init.stderr).trim().to_string(),
                        },
                    ));
                }

                let output = Command::new("git")
                    .arg("-C")
                    .arg(&checkout_root)
                    .arg("fetch")
                    .arg("--quiet")
                    .arg("--depth")
                    .arg("1")
                    .arg(remote_url)
                    .arg(&resolved.header.commit)
                    .output()
                    .map_err(|err| {
                        repo_snapshot_user_error(RepoSnapshotIssue::GitInvocationFailed {
                            error: err.to_string(),
                        })
                    })?;
                if !output.status.success() {
                    return Err(repo_snapshot_user_error(
                        RepoSnapshotIssue::GitFetchFailed {
                            locator: resolved.header.locator.clone(),
                            commit: resolved.header.commit.clone(),
                            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
                        },
                    ));
                }

                let tar_path = temp.path().join("repo.tar");
                let output = Command::new("git")
                    .arg("-C")
                    .arg(&checkout_root)
                    .arg("archive")
                    .arg("--format=tar")
                    .arg("--output")
                    .arg(&tar_path)
                    .arg("FETCH_HEAD")
                    .output()
                    .map_err(|err| {
                        repo_snapshot_user_error(RepoSnapshotIssue::GitInvocationFailed {
                            error: err.to_string(),
                        })
                    })?;
                if !output.status.success() {
                    return Err(repo_snapshot_user_error(
                        RepoSnapshotIssue::GitArchiveFailed {
                            locator: resolved.header.locator.clone(),
                            commit: resolved.header.commit.clone(),
                            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
                        },
                    ));
                }

                let file = File::open(&tar_path).with_context(|| {
                    format!("failed to open git archive {}", tar_path.display())
                })?;
                let mut archive = Archive::new(file);
                archive
                    .unpack(&checkout_root)
                    .context("failed to unpack git archive for repo snapshot")?;

                let snapshot_root = match resolved.subdir_rel.as_ref() {
                    Some(subdir) => checkout_root.join(subdir),
                    None => checkout_root,
                };
                if !snapshot_root.exists() {
                    return Err(repo_snapshot_user_error(RepoSnapshotIssue::MissingSubdir {
                        subdir: resolved
                            .header
                            .subdir
                            .clone()
                            .unwrap_or_else(|| "<missing>".to_string()),
                        commit: resolved.header.commit.clone(),
                    }));
                }

                let stored = self
                    .store_repo_snapshot_streaming(&resolved.header, &snapshot_root)
                    .map_err(store_write_error)?;
                self.record_key(ObjectKind::RepoSnapshot, &key, &stored.oid)
                    .map_err(store_write_error)?;
                debug!(
                    oid = %stored.oid,
                    locator = %resolved.header.locator,
                    commit = %resolved.header.commit,
                    subdir = resolved.header.subdir.as_deref().unwrap_or(""),
                    "repo-snapshot stored"
                );
                Ok(stored.oid)
            }
        }
    }

    fn store_repo_snapshot_streaming(
        &self,
        header: &RepoSnapshotHeader,
        snapshot_root: &Path,
    ) -> Result<StoredObject> {
        struct HashingFileWriter {
            file: File,
            hasher: Sha256,
            bytes_written: u64,
        }

        impl HashingFileWriter {
            fn new(file: File) -> Self {
                Self {
                    file,
                    hasher: Sha256::new(),
                    bytes_written: 0,
                }
            }
        }

        impl Write for HashingFileWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                let written = self.file.write(buf)?;
                if written > 0 {
                    self.hasher.update(&buf[..written]);
                    self.bytes_written = self.bytes_written.saturating_add(written as u64);
                }
                Ok(written)
            }

            fn flush(&mut self) -> std::io::Result<()> {
                self.file.flush()
            }
        }

        use base64::write::EncoderWriter;

        self.ensure_layout()?;

        let mut header_value =
            serde_json::to_value(header).context("failed to encode repo-snapshot header")?;
        sort_json_value(&mut header_value);
        let header_json =
            serde_json::to_vec(&header_value).context("failed to encode repo-snapshot header")?;

        let tmp_dir = self.root.join(TMP_DIR);
        fs::create_dir_all(&tmp_dir)?;
        let tmp_file = tempfile::Builder::new()
            .prefix("repo-snapshot-")
            .suffix(".partial")
            .tempfile_in(&tmp_dir)
            .context("failed to create temp file for repo-snapshot")?;
        let (tmp_file, tmp_path) = tmp_file.keep().map_err(|err| anyhow!(err.error))?;

        let mut writer = HashingFileWriter::new(tmp_file);
        writer
            .write_all(b"{\"header\":")
            .context("failed to write repo-snapshot object header")?;
        writer
            .write_all(&header_json)
            .context("failed to write repo-snapshot object header")?;
        writer
            .write_all(b",\"kind\":\"repo-snapshot\",\"payload\":\"")
            .context("failed to write repo-snapshot object header")?;

        let b64 = EncoderWriter::new(writer, &BASE64_STANDARD_NO_PAD);
        let mut b64 = archive_dir_canonical_to_writer(snapshot_root, b64)
            .context("failed to archive repo-snapshot tree")?;
        let mut writer = b64.finish().context("failed to finalize base64 encoding")?;
        writer
            .write_all(b"\",\"payload_kind\":\"repo-snapshot\"}")
            .context("failed to finalize repo-snapshot object")?;
        writer.file.sync_all().with_context(|| {
            format!(
                "failed to flush temp repo-snapshot object {}",
                tmp_path.display()
            )
        })?;
        let HashingFileWriter {
            file,
            hasher,
            bytes_written,
        } = writer;
        drop(file);
        let size = bytes_written;
        let oid = hex::encode(hasher.finalize());

        fsync_dir(&tmp_dir).ok();

        let _lock = self.acquire_lock(&oid)?;
        let object_path = self.object_path(&oid);
        if object_path.exists() {
            self.verify_existing(&oid, &object_path)?;
            self.ensure_index_entry(&oid, ObjectKind::RepoSnapshot, size)?;
            let _ = fs::remove_file(&tmp_path);
            fsync_dir(&tmp_dir).ok();
            debug!(%oid, kind=%ObjectKind::RepoSnapshot.as_str(), "cas hit");
            return Ok(StoredObject {
                oid,
                path: object_path,
                size,
                kind: ObjectKind::RepoSnapshot,
            });
        }

        if let Some(parent) = object_path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create object directory {}", parent.display())
            })?;
        }
        fs::rename(&tmp_path, &object_path).with_context(|| {
            format!(
                "failed to move repo-snapshot object into place ({} -> {})",
                tmp_path.display(),
                object_path.display()
            )
        })?;
        if let Some(parent) = object_path.parent() {
            fsync_dir(parent).ok();
        }
        make_read_only_recursive(&object_path)?;
        self.verify_existing(&oid, &object_path)?;
        self.ensure_index_entry(&oid, ObjectKind::RepoSnapshot, size)?;
        debug!(%oid, kind=%ObjectKind::RepoSnapshot.as_str(), "cas store");
        Ok(StoredObject {
            oid,
            path: object_path,
            size,
            kind: ObjectKind::RepoSnapshot,
        })
    }

    /// Look up a `repo-snapshot` oid in the CAS without producing it.
    ///
    /// This is useful for strict offline consumers that want to fail unless the
    /// snapshot is already cached.
    pub fn lookup_repo_snapshot_oid(&self, spec: &RepoSnapshotSpec) -> Result<Option<String>> {
        let resolved = resolve_repo_snapshot_spec(spec)?;
        let key = repo_snapshot_lookup_key(&resolved.header);
        self.lookup_key(ObjectKind::RepoSnapshot, &key)
    }

    /// Resolve a repo snapshot spec into its canonical header without creating it.
    pub fn resolve_repo_snapshot_header(
        &self,
        spec: &RepoSnapshotSpec,
    ) -> Result<RepoSnapshotHeader> {
        let resolved = resolve_repo_snapshot_spec(spec)?;
        Ok(resolved.header)
    }
}
