use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use anyhow::Result;
use serde_json::json;
use sha2::{Digest, Sha256};
use tar::{Builder, EntryType, Header, HeaderMode};
use tempfile::NamedTempFile;

use crate::core::sandbox::sandbox_error;
use crate::InstallUserError;

use super::super::LayerTar;

pub(super) fn append_path<W: Write>(
    builder: &mut Builder<W>,
    archive_path: &Path,
    path: &Path,
) -> Result<(), InstallUserError> {
    let metadata = fs::symlink_metadata(path).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to read source metadata for sandbox layer",
            json!({ "path": path.display().to_string(), "error": err.to_string() }),
        )
    })?;
    if metadata.is_dir() {
        builder.append_dir(archive_path, path).map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to stage directory for sandbox layer",
                json!({ "path": path.display().to_string(), "error": err.to_string() }),
            )
        })?;
    } else if metadata.file_type().is_symlink() {
        let target = fs::read_link(path).map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to read symlink target for sandbox layer",
                json!({ "path": path.display().to_string(), "error": err.to_string() }),
            )
        })?;
        let mut header = Header::new_gnu();
        header.set_metadata_in_mode(&metadata, HeaderMode::Deterministic);
        header.set_entry_type(EntryType::Symlink);
        header.set_size(0);
        builder
            .append_link(&mut header, archive_path, &target)
            .map_err(|err| {
                sandbox_error(
                    "PX903",
                    "failed to add symlink to sandbox layer",
                    json!({ "path": path.display().to_string(), "error": err.to_string() }),
                )
            })?;
    } else {
        let mut file = File::open(path).map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to read source file for sandbox layer",
                json!({ "path": path.display().to_string(), "error": err.to_string() }),
            )
        })?;
        builder
            .append_file(archive_path, &mut file)
            .map_err(|err| {
                sandbox_error(
                    "PX903",
                    "failed to add file to sandbox layer",
                    json!({ "path": path.display().to_string(), "error": err.to_string() }),
                )
            })?;
    }
    Ok(())
}

pub(super) struct HashingWriter<W> {
    inner: W,
    hasher: Sha256,
    bytes_written: u64,
}

impl<W> HashingWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            bytes_written: 0,
        }
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.hasher.update(&buf[..written]);
        self.bytes_written = self
            .bytes_written
            .saturating_add(written.try_into().unwrap_or(u64::MAX));
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

pub(super) fn layer_tar_builder(
    blobs: &Path,
) -> Result<Builder<HashingWriter<NamedTempFile>>, InstallUserError> {
    fs::create_dir_all(blobs).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to prepare layer directory",
            json!({ "path": blobs.display().to_string(), "error": err.to_string() }),
        )
    })?;
    let file = NamedTempFile::new_in(blobs).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to create sandbox layer",
            json!({ "path": blobs.display().to_string(), "error": err.to_string() }),
        )
    })?;
    Ok(Builder::new(HashingWriter::new(file)))
}

pub(super) fn finalize_layer(
    builder: Builder<HashingWriter<NamedTempFile>>,
    blobs: &Path,
) -> Result<LayerTar, InstallUserError> {
    let writer = builder.into_inner().map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to finalize sandbox layer",
            json!({ "error": err.to_string() }),
        )
    })?;
    let HashingWriter {
        inner: temp,
        hasher,
        bytes_written: size,
    } = writer;
    let digest = format!("{:x}", hasher.finalize());
    let layer_path = blobs.join(&digest);
    if !layer_path.exists() {
        match temp.persist_noclobber(&layer_path) {
            Ok(_) => {}
            Err(err) => {
                if err.error.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(sandbox_error(
                        "PX903",
                        "failed to write sandbox layer",
                        json!({
                            "path": layer_path.display().to_string(),
                            "error": err.error.to_string(),
                        }),
                    ));
                }
            }
        }
    }
    Ok(LayerTar {
        digest,
        size,
        path: layer_path,
    })
}
