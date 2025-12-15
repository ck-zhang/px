use super::super::*;

impl ContentAddressableStore {
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
                        if !self.kind_found {
                            if byte == Self::KIND_NEEDLE[self.kind_match] {
                                self.kind_match += 1;
                                if self.kind_match == Self::KIND_NEEDLE.len() {
                                    self.kind_found = true;
                                }
                            } else {
                                self.kind_match = 0;
                            }
                        }

                        match self.state {
                            PayloadState::Seek => {
                                if byte == Self::PAYLOAD_NEEDLE[self.payload_match] {
                                    self.payload_match += 1;
                                    if self.payload_match == Self::PAYLOAD_NEEDLE.len() {
                                        self.state = PayloadState::Payload;
                                        break;
                                    }
                                } else {
                                    self.payload_match = 0;
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
