use super::*;

/// User-facing issues while creating or materializing a `repo-snapshot` CAS object.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
enum RepoSnapshotIssue {
    #[error("repo-snapshot spec must be '<locator>@<commit>' (got '{spec}')")]
    InvalidSpec { spec: String },
    #[error("repo-snapshot commit must be a pinned full hex SHA (got '{commit}')")]
    InvalidCommit { commit: String },
    #[error("repo-snapshot locator must be a git URL like 'git+file:///abs/path/to/repo' (got '{locator}')")]
    InvalidLocator { locator: String },
    #[error("repo-snapshot locator must not include credentials (got '{locator}')")]
    LocatorContainsCredentials { locator: String },
    #[error("repo-snapshot locator must not include a query or fragment (got '{locator}')")]
    LocatorContainsQueryOrFragment { locator: String },
    #[error("repo-snapshot requires PX_ONLINE=1 to fetch '{locator}@{commit}' (offline mode)")]
    Offline { locator: String, commit: String },
    #[error("unsupported repo-snapshot locator '{locator}'")]
    UnsupportedLocator { locator: String },
    #[error("repo-snapshot subdir must be a relative path without '..' (got '{subdir}')")]
    InvalidSubdir { subdir: String },
    #[error("repo-snapshot subdir '{subdir}' does not exist at commit '{commit}'")]
    MissingSubdir { subdir: String, commit: String },
    #[error("git fetch failed for '{locator}@{commit}': {stderr}")]
    GitFetchFailed {
        locator: String,
        commit: String,
        stderr: String,
    },
    #[error("git archive failed for '{locator}@{commit}': {stderr}")]
    GitArchiveFailed {
        locator: String,
        commit: String,
        stderr: String,
    },
    #[error("git is required to create a repo-snapshot, but failed to invoke it: {error}")]
    GitInvocationFailed { error: String },
}

impl RepoSnapshotIssue {
    #[must_use]
    fn code(&self) -> &'static str {
        match self {
            Self::InvalidSpec { .. }
            | Self::InvalidCommit { .. }
            | Self::InvalidLocator { .. }
            | Self::LocatorContainsCredentials { .. }
            | Self::LocatorContainsQueryOrFragment { .. }
            | Self::InvalidSubdir { .. }
            | Self::MissingSubdir { .. } => "PX720",
            Self::Offline { .. } => "PX721",
            Self::UnsupportedLocator { .. } => "PX722",
            Self::GitFetchFailed { .. }
            | Self::GitArchiveFailed { .. }
            | Self::GitInvocationFailed { .. } => "PX723",
        }
    }

    #[must_use]
    fn reason(&self) -> &'static str {
        match self {
            Self::InvalidSpec { .. } => "invalid_repo_snapshot_spec",
            Self::InvalidCommit { .. } => "invalid_repo_snapshot_commit",
            Self::InvalidLocator { .. } => "invalid_repo_snapshot_locator",
            Self::LocatorContainsCredentials { .. } => "invalid_repo_snapshot_locator",
            Self::LocatorContainsQueryOrFragment { .. } => "invalid_repo_snapshot_locator",
            Self::Offline { .. } => "repo_snapshot_offline",
            Self::UnsupportedLocator { .. } => "unsupported_repo_snapshot_locator",
            Self::InvalidSubdir { .. } => "invalid_repo_snapshot_subdir",
            Self::MissingSubdir { .. } => "missing_repo_snapshot_subdir",
            Self::GitFetchFailed { .. } => "repo_snapshot_git_fetch_failed",
            Self::GitArchiveFailed { .. } => "repo_snapshot_git_archive_failed",
            Self::GitInvocationFailed { .. } => "repo_snapshot_git_unavailable",
        }
    }

    #[must_use]
    fn hint(&self) -> Option<&'static str> {
        match self {
            Self::InvalidSpec { .. } => Some("Use 'git+file:///abs/path/to/repo@<full_sha>'"),
            Self::InvalidCommit { .. } => Some("Use a full commit SHA (no branches/tags)."),
            Self::InvalidLocator { .. } => Some("Use a git locator like 'git+file:///abs/path/to/repo'."),
            Self::LocatorContainsCredentials { .. } => Some("Remove credentials from the URL and use a git credential helper instead."),
            Self::LocatorContainsQueryOrFragment { .. } => Some("Remove the query/fragment from the URL; use a plain git+https:// or git+file:// locator."),
            Self::Offline { .. } => Some("Re-run with --online / set PX_ONLINE=1, or prefetch the snapshot while online."),
            Self::UnsupportedLocator { .. } => Some("Use a git+file:// or git+https:// locator."),
            Self::InvalidSubdir { .. } => Some("Use a relative subdir path (no '..')."),
            Self::MissingSubdir { .. } => Some("Check the subdir exists at the pinned commit."),
            Self::GitFetchFailed { .. } => Some("Check the commit exists and the repository is accessible."),
            Self::GitArchiveFailed { .. } => Some("Check the commit exists and the repository is accessible."),
            Self::GitInvocationFailed { .. } => Some("Install git and ensure it is on PATH."),
        }
    }

    #[must_use]
    fn details(&self) -> Value {
        let mut details = json!({
            "code": self.code(),
            "reason": self.reason(),
        });
        if let Value::Object(map) = &mut details {
            if let Some(hint) = self.hint() {
                map.insert("hint".into(), json!(hint));
            }
            match self {
                Self::InvalidSpec { spec } => {
                    map.insert("spec".into(), json!(spec));
                }
                Self::InvalidCommit { commit } => {
                    map.insert("commit".into(), json!(commit));
                }
                Self::InvalidLocator { locator }
                | Self::LocatorContainsCredentials { locator }
                | Self::LocatorContainsQueryOrFragment { locator }
                | Self::UnsupportedLocator { locator } => {
                    map.insert("locator".into(), json!(locator));
                }
                Self::Offline { locator, commit } => {
                    map.insert("locator".into(), json!(locator));
                    map.insert("commit".into(), json!(commit));
                }
                Self::InvalidSubdir { subdir } => {
                    map.insert("subdir".into(), json!(subdir));
                }
                Self::MissingSubdir { subdir, commit } => {
                    map.insert("subdir".into(), json!(subdir));
                    map.insert("commit".into(), json!(commit));
                }
                Self::GitFetchFailed {
                    locator,
                    commit,
                    stderr,
                } => {
                    map.insert("locator".into(), json!(locator));
                    map.insert("commit".into(), json!(commit));
                    map.insert("stderr".into(), json!(stderr));
                }
                Self::GitArchiveFailed {
                    locator,
                    commit,
                    stderr,
                } => {
                    map.insert("locator".into(), json!(locator));
                    map.insert("commit".into(), json!(commit));
                    map.insert("stderr".into(), json!(stderr));
                }
                Self::GitInvocationFailed { error } => {
                    map.insert("error".into(), json!(error));
                }
            }
        }
        details
    }
}

/// Specification for ensuring a `repo-snapshot` object exists in the CAS.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoSnapshotSpec {
    /// Canonical repository locator (e.g. `git+file:///abs/path/to/repo`).
    pub locator: String,
    /// Pinned commit identifier (full SHA-1/hex expected).
    pub commit: String,
    /// Optional subdirectory root within the repository.
    pub subdir: Option<PathBuf>,
}

impl RepoSnapshotSpec {
    /// Parse a commit-pinned locator of the form `git+file:///abs/path/to/repo@<sha>`.
    ///
    /// # Errors
    ///
    /// Returns an error when the locator is malformed or the commit is not a
    /// pinned full hex SHA.
    pub fn parse(locator_with_commit: &str) -> Result<Self> {
        let locator_with_commit = locator_with_commit.trim();
        let Some((locator, commit)) = locator_with_commit.rsplit_once('@') else {
            return Err(repo_snapshot_user_error(RepoSnapshotIssue::InvalidSpec {
                spec: redact_repo_locator(locator_with_commit),
            }));
        };
        let commit = normalize_commit_sha(commit).map_err(repo_snapshot_user_error)?;
        Ok(Self {
            locator: locator.to_string(),
            commit,
            subdir: None,
        })
    }
}

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
                let transport = resolved
                    .header
                    .locator
                    .strip_prefix("git+")
                    .unwrap_or(&resolved.header.locator);
                debug!(
                    locator = %resolved.header.locator,
                    commit = %resolved.header.commit,
                    "repo-snapshot creating snapshot via git fetch+archive"
                );
                let temp =
                    tempdir().context("failed to create temp directory for repo snapshot")?;
                let git_root = temp.path().join("git");
                fs::create_dir_all(&git_root)?;
                let checkout_root = temp.path().join("repo");
                fs::create_dir_all(&checkout_root)?;

                let output = Command::new("git")
                    .arg("-C")
                    .arg(&git_root)
                    .arg("init")
                    .arg("--quiet")
                    .env("GIT_TERMINAL_PROMPT", "0")
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

                let output = Command::new("git")
                    .arg("-C")
                    .arg(&git_root)
                    .arg("remote")
                    .arg("add")
                    .arg("origin")
                    .arg(transport)
                    .env("GIT_TERMINAL_PROMPT", "0")
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

                let output = Command::new("git")
                    .arg("-C")
                    .arg(&git_root)
                    .arg("fetch")
                    .arg("--no-tags")
                    .arg("--depth=1")
                    .arg("origin")
                    .arg(&resolved.header.commit)
                    .env("GIT_TERMINAL_PROMPT", "0")
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
                    .arg(&git_root)
                    .arg("archive")
                    .arg("--format=tar")
                    .arg("--output")
                    .arg(&tar_path)
                    .arg(&resolved.header.commit)
                    .env("GIT_TERMINAL_PROMPT", "0")
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

    /// Materialize a stored repository snapshot into `dst`.
    ///
    /// The caller is responsible for choosing an appropriate destination path.
    pub fn materialize_repo_snapshot(&self, oid: &str, dst: &Path) -> Result<()> {
        if dst.exists() {
            debug!(%oid, dst = %dst.display(), "repo-snapshot materialize hit");
            return Ok(());
        }
        debug!(%oid, dst = %dst.display(), "repo-snapshot materializing");

        let _lock = self.acquire_lock(oid)?;
        if dst.exists() {
            debug!(%oid, dst = %dst.display(), "repo-snapshot materialize hit");
            return Ok(());
        }
        self.ensure_object_present_in_index(oid)?;
        let object_path = self.object_path(oid);
        if !object_path.exists() {
            return Err(StoreError::MissingObject {
                oid: oid.to_string(),
            }
            .into());
        }
        self.verify_existing(oid, &object_path)?;
        let mut conn = self.connection()?;
        let now = timestamp_secs();
        self.touch_object(&mut conn, oid, now)?;
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = dst.with_extension("partial");
        if tmp.exists() {
            let _ = fs::remove_dir_all(&tmp);
        }
        fs::create_dir_all(&tmp)?;

        struct PayloadReader<R: Read> {
            inner: R,
            buf: [u8; 8192],
            buf_pos: usize,
            buf_len: usize,
            state: PayloadState,
            payload_match: usize,
            kind_match: usize,
            kind_found: bool,
        }

        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        enum PayloadState {
            Seek,
            Payload,
            Done,
        }

        impl<R: Read> PayloadReader<R> {
            const PAYLOAD_NEEDLE: &'static [u8] = b"\"payload\":\"";
            const KIND_NEEDLE: &'static [u8] = b"\"kind\":\"repo-snapshot\"";

            fn new(inner: R) -> Self {
                Self {
                    inner,
                    buf: [0u8; 8192],
                    buf_pos: 0,
                    buf_len: 0,
                    state: PayloadState::Seek,
                    payload_match: 0,
                    kind_match: 0,
                    kind_found: false,
                }
            }

            fn fill_buf(&mut self) -> std::io::Result<()> {
                if self.buf_pos < self.buf_len {
                    return Ok(());
                }
                self.buf_len = self.inner.read(&mut self.buf)?;
                self.buf_pos = 0;
                Ok(())
            }
        }

        impl<R: Read> Read for PayloadReader<R> {
            fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
                if out.is_empty() {
                    return Ok(0);
                }
                let mut written = 0;
                loop {
                    match self.state {
                        PayloadState::Done => return Ok(written),
                        PayloadState::Seek | PayloadState::Payload => {}
                    }

                    self.fill_buf()?;
                    if self.buf_len == 0 {
                        return if written > 0 {
                            Ok(written)
                        } else if self.state == PayloadState::Done {
                            Ok(0)
                        } else if self.state == PayloadState::Seek {
                            Err(std::io::Error::new(
                                ErrorKind::InvalidData,
                                "repo-snapshot payload not found",
                            ))
                        } else {
                            Err(std::io::Error::new(
                                ErrorKind::InvalidData,
                                "repo-snapshot payload is unterminated",
                            ))
                        };
                    }

                    while self.buf_pos < self.buf_len && written < out.len() {
                        let byte = self.buf[self.buf_pos];
                        self.buf_pos += 1;
                        match self.state {
                            PayloadState::Seek => {
                                let expected_kind = Self::KIND_NEEDLE[self.kind_match];
                                if byte == expected_kind {
                                    self.kind_match += 1;
                                    if self.kind_match == Self::KIND_NEEDLE.len() {
                                        self.kind_found = true;
                                        self.kind_match = 0;
                                    }
                                } else {
                                    self.kind_match =
                                        if byte == Self::KIND_NEEDLE[0] { 1 } else { 0 };
                                }

                                let expected_payload = Self::PAYLOAD_NEEDLE[self.payload_match];
                                if byte == expected_payload {
                                    self.payload_match += 1;
                                    if self.payload_match == Self::PAYLOAD_NEEDLE.len() {
                                        if !self.kind_found {
                                            return Err(std::io::Error::new(
                                                ErrorKind::InvalidData,
                                                "repo-snapshot kind mismatch while parsing payload",
                                            ));
                                        }
                                        self.state = PayloadState::Payload;
                                        self.payload_match = 0;
                                        break;
                                    }
                                } else {
                                    self.payload_match = if byte == Self::PAYLOAD_NEEDLE[0] {
                                        1
                                    } else {
                                        0
                                    };
                                }
                            }
                            PayloadState::Payload => {
                                if byte == b'"' {
                                    self.state = PayloadState::Done;
                                    break;
                                }
                                out[written] = byte;
                                written += 1;
                            }
                            PayloadState::Done => {}
                        }
                    }

                    if written >= out.len() {
                        return Ok(written);
                    }
                }
            }
        }

        let file = File::open(&object_path)?;
        let payload_reader = PayloadReader::new(std::io::BufReader::new(file));
        let decoded = base64::read::DecoderReader::new(payload_reader, &BASE64_STANDARD_NO_PAD);
        let decoder = GzDecoder::new(decoded);
        let mut tar = tar::Archive::new(decoder);
        if let Err(err) = tar.unpack(&tmp) {
            if err.kind() == ErrorKind::InvalidData {
                return Err(StoreError::DecodeFailure {
                    oid: oid.to_string(),
                    error: err.to_string(),
                }
                .into());
            }
            return Err(err.into());
        }
        fs::rename(&tmp, dst)?;
        make_read_only_recursive(dst)?;
        debug!(%oid, dst = %dst.display(), "repo-snapshot materialized");
        Ok(())
    }
}

/// Deterministic key for a commit-pinned repository snapshot.
#[must_use]
pub fn repo_snapshot_lookup_key(header: &RepoSnapshotHeader) -> String {
    format!(
        "{}|{}|{}",
        header.locator,
        header.commit,
        header.subdir.as_deref().unwrap_or("")
    )
}

/// Ensure a `repo-snapshot` exists in the global CAS.
pub fn ensure_repo_snapshot(spec: &RepoSnapshotSpec) -> Result<String> {
    global_store().ensure_repo_snapshot(spec)
}

/// Look up a `repo-snapshot` oid in the global CAS without producing it.
pub fn lookup_repo_snapshot_oid(spec: &RepoSnapshotSpec) -> Result<Option<String>> {
    global_store().lookup_repo_snapshot_oid(spec)
}

/// Materialize a `repo-snapshot` object from the global CAS into `dst`.
pub fn materialize_repo_snapshot(oid: &str, dst: &Path) -> Result<()> {
    global_store().materialize_repo_snapshot(oid, dst)
}

#[derive(Clone, Debug)]
struct ResolvedRepoSnapshotSpec {
    header: RepoSnapshotHeader,
    locator: ResolvedRepoLocator,
    subdir_rel: Option<PathBuf>,
}

#[derive(Clone, Debug)]
enum ResolvedRepoLocator {
    File {
        canonical: String,
        repo_path: PathBuf,
    },
    Remote {
        canonical: String,
    },
}

fn px_online_enabled() -> bool {
    match env::var("PX_ONLINE") {
        Ok(value) => {
            let lowered = value.to_ascii_lowercase();
            !matches!(lowered.as_str(), "0" | "false" | "no" | "off" | "")
        }
        Err(_) => true,
    }
}

fn repo_snapshot_user_error(issue: RepoSnapshotIssue) -> anyhow::Error {
    crate::InstallUserError::new(issue.to_string(), issue.details()).into()
}

fn normalize_commit_sha(commit: &str) -> std::result::Result<String, RepoSnapshotIssue> {
    let commit = commit.trim();
    let normalized = commit.to_ascii_lowercase();
    let len = normalized.len();
    let is_hex = normalized.chars().all(|c| c.is_ascii_hexdigit());
    if !(is_hex && (len == 40 || len == 64)) {
        return Err(RepoSnapshotIssue::InvalidCommit {
            commit: commit.to_string(),
        });
    }
    Ok(normalized)
}

fn normalize_subdir(
    subdir: Option<&Path>,
) -> std::result::Result<(Option<String>, Option<PathBuf>), RepoSnapshotIssue> {
    let Some(subdir) = subdir else {
        return Ok((None, None));
    };
    if subdir.as_os_str().is_empty() {
        return Ok((None, None));
    }
    if subdir.is_absolute() {
        return Err(RepoSnapshotIssue::InvalidSubdir {
            subdir: subdir.display().to_string(),
        });
    }

    let mut rel = PathBuf::new();
    for component in subdir.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::Normal(part) => rel.push(part),
            _ => {
                return Err(RepoSnapshotIssue::InvalidSubdir {
                    subdir: subdir.display().to_string(),
                })
            }
        }
    }
    if rel.as_os_str().is_empty() {
        return Ok((None, None));
    }
    let rel_str = rel.to_string_lossy().replace('\\', "/");
    Ok((Some(rel_str), Some(rel)))
}

fn normalize_absolute_path_lexical(path: &Path) -> Result<PathBuf, RepoSnapshotIssue> {
    use std::path::Component;

    if !path.is_absolute() {
        return Err(RepoSnapshotIssue::InvalidLocator {
            locator: redact_repo_locator(&format!("git+file://{}", path.display())),
        });
    }

    let mut prefix = None;
    let mut has_root = false;
    let mut parts: Vec<std::ffi::OsString> = Vec::new();
    for component in path.components() {
        match component {
            Component::Prefix(p) => prefix = Some(p),
            Component::RootDir => has_root = true,
            Component::CurDir => {}
            Component::Normal(part) => parts.push(part.to_os_string()),
            Component::ParentDir => {
                if parts.pop().is_none() {
                    return Err(RepoSnapshotIssue::InvalidLocator {
                        locator: redact_repo_locator(&format!("git+file://{}", path.display())),
                    });
                }
            }
        }
    }

    let mut normalized = PathBuf::new();
    if let Some(prefix) = prefix {
        normalized.push(prefix.as_os_str());
    }
    if has_root {
        normalized.push(std::path::MAIN_SEPARATOR_STR);
    }
    for part in parts {
        normalized.push(part);
    }

    if !normalized.is_absolute() {
        return Err(RepoSnapshotIssue::InvalidLocator {
            locator: redact_repo_locator(&format!("git+file://{}", path.display())),
        });
    }

    Ok(normalized)
}

fn redact_repo_locator(locator: &str) -> String {
    let locator = locator.trim();
    if let Some(transport) = locator.strip_prefix("git+") {
        if let Ok(mut url) = Url::parse(transport) {
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.set_query(None);
            url.set_fragment(None);
            return format!("git+{}", url);
        }
    }

    let mut redacted = locator.to_string();
    if let Some(pos) = redacted.find('#') {
        redacted.truncate(pos);
    }
    if let Some(pos) = redacted.find('?') {
        redacted.truncate(pos);
    }
    if let Some(scheme_pos) = redacted.find("://") {
        let after_scheme = scheme_pos + 3;
        if let Some(at_rel) = redacted[after_scheme..].find('@') {
            let at_pos = after_scheme + at_rel;
            let next_slash = redacted[after_scheme..]
                .find('/')
                .map(|idx| after_scheme + idx);
            if next_slash.map(|slash| at_pos < slash).unwrap_or(true) {
                redacted.replace_range(after_scheme..at_pos, "***");
            }
        }
    }

    redacted
}

fn resolve_repo_locator(
    locator: &str,
) -> std::result::Result<ResolvedRepoLocator, RepoSnapshotIssue> {
    let locator = locator.trim();
    if !locator.starts_with("git+") {
        return Err(RepoSnapshotIssue::InvalidLocator {
            locator: redact_repo_locator(locator),
        });
    }
    let transport = &locator["git+".len()..];
    let url = Url::parse(transport).map_err(|_| RepoSnapshotIssue::InvalidLocator {
        locator: redact_repo_locator(locator),
    })?;
    if url.username() != "" || url.password().is_some() {
        let mut redacted = url.clone();
        let _ = redacted.set_username("");
        let _ = redacted.set_password(None);
        redacted.set_query(None);
        redacted.set_fragment(None);
        return Err(RepoSnapshotIssue::LocatorContainsCredentials {
            locator: format!("git+{}", redacted),
        });
    }
    if url.query().is_some() || url.fragment().is_some() {
        let mut redacted = url.clone();
        let _ = redacted.set_username("");
        let _ = redacted.set_password(None);
        redacted.set_query(None);
        redacted.set_fragment(None);
        return Err(RepoSnapshotIssue::LocatorContainsQueryOrFragment {
            locator: format!("git+{}", redacted),
        });
    }
    match url.scheme() {
        "file" => {
            let repo_path = url
                .to_file_path()
                .map_err(|_| RepoSnapshotIssue::InvalidLocator {
                    locator: redact_repo_locator(locator),
                })?;
            let repo_path = if repo_path.is_absolute() {
                repo_path
            } else {
                return Err(RepoSnapshotIssue::InvalidLocator {
                    locator: redact_repo_locator(locator),
                });
            };
            let repo_path = normalize_absolute_path_lexical(&repo_path)?;
            let canonical_url =
                Url::from_file_path(&repo_path).map_err(|_| RepoSnapshotIssue::InvalidLocator {
                    locator: redact_repo_locator(locator),
                })?;
            Ok(ResolvedRepoLocator::File {
                canonical: format!("git+{}", canonical_url),
                repo_path,
            })
        }
        "http" | "https" => Ok(ResolvedRepoLocator::Remote {
            canonical: format!("git+{}", url),
        }),
        _ => Err(RepoSnapshotIssue::UnsupportedLocator {
            locator: locator.to_string(),
        }),
    }
}

fn resolve_repo_snapshot_spec(spec: &RepoSnapshotSpec) -> Result<ResolvedRepoSnapshotSpec> {
    let commit = normalize_commit_sha(&spec.commit).map_err(repo_snapshot_user_error)?;
    let locator = resolve_repo_locator(&spec.locator).map_err(repo_snapshot_user_error)?;
    let canonical_locator = match &locator {
        ResolvedRepoLocator::File { canonical, .. }
        | ResolvedRepoLocator::Remote { canonical, .. } => canonical.clone(),
    };
    let (subdir_str, subdir_rel) =
        normalize_subdir(spec.subdir.as_deref()).map_err(repo_snapshot_user_error)?;
    Ok(ResolvedRepoSnapshotSpec {
        header: RepoSnapshotHeader {
            locator: canonical_locator,
            commit,
            subdir: subdir_str,
        },
        locator,
        subdir_rel,
    })
}
