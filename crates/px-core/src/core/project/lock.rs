use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fs4::FileExt;

#[derive(Debug)]
pub(crate) struct ProjectLock {
    _file: File,
}

impl ProjectLock {
    pub(crate) fn try_acquire(root: &Path) -> Result<Option<Self>> {
        let path = project_lock_path(root);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .with_context(|| format!("failed to open {}", path.display()))?;

        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(err) if err.kind() == ErrorKind::WouldBlock => Ok(None),
            #[cfg(windows)]
            Err(err) if matches!(err.raw_os_error(), Some(32 | 33)) => Ok(None),
            Err(err) => Err(err.into()),
        }
    }
}

fn project_lock_path(root: &Path) -> PathBuf {
    root.join(".px").join("project.lock")
}
