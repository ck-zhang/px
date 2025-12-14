use std::{fs, path::Path};

use anyhow::Result;
use walkdir::WalkDir;

#[cfg(unix)]
pub(super) fn make_writable_recursive(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = fs::metadata(path) {
        let mode = if meta.is_dir() { 0o755 } else { 0o644 };
        let _ = fs::set_permissions(path, PermissionsExt::from_mode(mode));
        if meta.is_dir() {
            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries.flatten() {
                    make_writable_recursive(&entry.path());
                }
            }
        }
    }
}

#[cfg(not(unix))]
pub(super) fn make_writable_recursive(_path: &Path) {}

pub(crate) fn copy_tree(src: &Path, dest: &Path) -> Result<()> {
    for entry in WalkDir::new(src) {
        let entry = entry?;
        let path = entry.path();
        if path == src {
            continue;
        }
        let rel = path.strip_prefix(src).unwrap_or(path);
        let target = dest.join(rel);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(path, &target)?;
        } else if entry.file_type().is_symlink() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            let link_target = fs::read_link(path)?;
            let _ = fs::remove_file(&target);
            #[cfg(unix)]
            {
                use std::os::unix::fs::symlink;
                symlink(&link_target, &target)?;
            }
            #[cfg(not(unix))]
            {
                if link_target.is_file() {
                    let _ = fs::copy(&link_target, &target)?;
                }
            }
        }
    }
    Ok(())
}
