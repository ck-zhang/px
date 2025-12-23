use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Context, Result};

/// Best-effort recursive chmod for paths that may have been hardened read-only.
#[cfg(unix)]
pub(crate) fn make_writable_recursive(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = fs::symlink_metadata(path) else {
        return;
    };
    if meta.file_type().is_symlink() {
        return;
    }
    let mode = if meta.is_dir() { 0o755 } else { 0o644 };
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
    if meta.is_dir() {
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                make_writable_recursive(&entry.path());
            }
        }
    }
}

#[cfg(not(unix))]
pub(crate) fn make_writable_recursive(path: &Path) {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return;
    };
    if meta.file_type().is_symlink() {
        return;
    }
    let mut perms = meta.permissions();
    if perms.readonly() {
        perms.set_readonly(false);
        let _ = fs::set_permissions(path, perms);
    }
    if meta.is_dir() {
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                make_writable_recursive(&entry.path());
            }
        }
    }
}

pub(crate) fn remove_dir_all_writable(path: &Path) -> Result<()> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("failed to stat {}", path.display())),
    };
    if meta.file_type().is_symlink() {
        fs::remove_file(path)
            .with_context(|| format!("failed to remove symlink {}", path.display()))?;
        return Ok(());
    }
    make_writable_recursive(path);
    fs::remove_dir_all(path).with_context(|| format!("failed to remove {}", path.display()))?;
    Ok(())
}

pub(crate) struct PxTempDir {
    inner: Option<tempfile::TempDir>,
    path: PathBuf,
}

impl PxTempDir {
    pub(crate) fn new_in(root: &Path, prefix: &str) -> Result<Self> {
        fs::create_dir_all(root).with_context(|| format!("failed to create {}", root.display()))?;
        prune_stale_tempdirs(root, prefix, Duration::from_secs(24 * 60 * 60));
        let dir = tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in(root)
            .with_context(|| format!("failed to create temp dir under {}", root.display()))?;
        let path = dir.path().to_path_buf();
        Ok(Self {
            inner: Some(dir),
            path,
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for PxTempDir {
    fn drop(&mut self) {
        let Some(dir) = self.inner.take() else {
            return;
        };
        let path = dir.keep();
        let _ = remove_dir_all_writable(&path);
    }
}

fn prune_stale_tempdirs(root: &Path, prefix: &str, max_age: Duration) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(prefix) {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let Some(modified) = meta.modified().ok() else {
            continue;
        };
        let age = now.duration_since(modified).unwrap_or_default();
        if age < max_age {
            continue;
        }
        let _ = remove_dir_all_writable(&entry.path());
    }
}

fn remove_path_for_replace(path: &Path) -> Result<()> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("failed to stat {}", path.display())),
    };
    let file_type = meta.file_type();

    if file_type.is_symlink() {
        fs::remove_file(path)
            .or_else(|err| {
                if err.kind() == std::io::ErrorKind::NotFound {
                    return Ok(());
                }
                fs::remove_dir(path).or_else(|dir_err| {
                    if dir_err.kind() == std::io::ErrorKind::NotFound {
                        Ok(())
                    } else {
                        Err(dir_err)
                    }
                })
            })
            .with_context(|| format!("failed to remove symlink {}", path.display()))?;
        return Ok(());
    }

    if file_type.is_dir() {
        // Prefer `remove_dir` so directory symlinks/junctions are removed without
        // recursively deleting their targets.
        if fs::remove_dir(path).is_ok() {
            return Ok(());
        }
        fs::remove_dir_all(path)
            .with_context(|| format!("failed to remove dir {}", path.display()))?;
        return Ok(());
    }

    fs::remove_file(path).with_context(|| format!("failed to remove file {}", path.display()))?;
    Ok(())
}

/// Replace `link` with a directory link pointing at `target`.
///
/// On Unix this is a symlink. On Windows this prefers a directory symlink, then falls back to a
/// directory junction (`mklink /J`) when symlinks are unavailable.
pub(crate) fn replace_dir_link(target: &Path, link: &Path) -> Result<()> {
    if !target.exists() {
        return Err(anyhow!(
            "cannot create env link; target does not exist: {}",
            target.display()
        ));
    }
    if let Some(parent) = link.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    #[cfg(windows)]
    fn is_permission_denied(err: &anyhow::Error) -> bool {
        err.chain().any(|cause| {
            cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io_err| io_err.kind() == std::io::ErrorKind::PermissionDenied)
        })
    }

    #[cfg(windows)]
    fn link_resolves_to_target(target: &Path, link: &Path) -> bool {
        let Ok(target) = fs::canonicalize(target) else {
            return false;
        };
        let Ok(link) = fs::canonicalize(link) else {
            return false;
        };
        link == target
    }

    #[cfg(windows)]
    {
        const RETRIES_MS: &[u64] = &[0, 2, 5, 10, 20, 50, 100];
        let mut last_error = None;
        for delay_ms in RETRIES_MS {
            if *delay_ms > 0 {
                std::thread::sleep(Duration::from_millis(*delay_ms));
            }
            match remove_path_for_replace(link) {
                Ok(()) => {
                    last_error = None;
                    break;
                }
                Err(err) if is_permission_denied(&err) => {
                    if link_resolves_to_target(target, link) {
                        return Ok(());
                    }
                    last_error = Some(err);
                    continue;
                }
                Err(err) => return Err(err),
            }
        }
        if let Some(err) = last_error {
            if link_resolves_to_target(target, link) {
                return Ok(());
            }
            return Err(err);
        }
    }

    #[cfg(not(windows))]
    remove_path_for_replace(link)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        match symlink(target, link) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "failed to create symlink {} -> {}",
                        link.display(),
                        target.display()
                    )
                });
            }
        }
        Ok(())
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::symlink_dir;
        match symlink_dir(target, link) {
            Ok(()) => return Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
            Err(_) => {
                if fs::symlink_metadata(link).is_ok() {
                    return Ok(());
                }
            }
        }

        let link_str = link
            .to_str()
            .ok_or_else(|| anyhow!("non-utf8 link path {}", link.display()))?;
        let target_str = target
            .to_str()
            .ok_or_else(|| anyhow!("non-utf8 target path {}", target.display()))?;

        if link_str.contains('"') || target_str.contains('"') {
            return Err(anyhow!(
                "cannot create Windows junction for paths containing quotes: {} -> {}",
                link.display(),
                target.display()
            ));
        }

        let cmdline = format!(r#"mklink /J "{link_str}" "{target_str}""#);
        let output = std::process::Command::new("cmd")
            .args(["/C", &cmdline])
            .output()
            .with_context(|| "failed to invoke cmd.exe for mklink")?;
        if output.status.success() {
            return Ok(());
        }
        if fs::symlink_metadata(link).is_ok() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(anyhow!(
            "mklink /J failed (exit {:?}): {}{}",
            output.status.code(),
            stdout.trim(),
            if stderr.trim().is_empty() {
                String::new()
            } else if stdout.trim().is_empty() {
                stderr.trim().to_string()
            } else {
                format!("\n{}", stderr.trim())
            }
        ));
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = target;
        let _ = link;
        Err(anyhow!(
            "directory links are not supported on this platform"
        ))
    }
}

/// Best-effort guess of the Python install root from a python executable path.
///
/// On Unix this is typically `<root>/bin/python`. On Windows (and some portable layouts) the
/// executable can live directly under the install root.
pub(crate) fn python_install_root(python_exe: &Path) -> Option<PathBuf> {
    let parent = python_exe.parent()?;

    // If the executable lives directly under a directory that looks like a Python install, use
    // that directory as the root.
    for marker in ["Lib", "lib", "DLLs", "include"] {
        if parent.join(marker).exists() {
            return Some(parent.to_path_buf());
        }
    }

    // Common Unix layout: <root>/bin/python
    if parent
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("bin"))
    {
        return parent.parent().map(|p| p.to_path_buf());
    }

    Some(parent.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    #[test]
    fn px_tempdir_cleans_read_only_children() {
        let root = tempfile::tempdir().expect("root tempdir");
        let dir = PxTempDir::new_in(root.path(), "px-test-temp-").expect("create px tempdir");
        let path = dir.path().to_path_buf();
        let nested = dir.path().join("nested");
        std::fs::create_dir_all(&nested).expect("nested dir");
        std::fs::write(nested.join("file.txt"), b"hello").expect("write file");

        crate::store::cas::make_read_only_recursive(&path).expect("harden perms");
        drop(dir);
        assert!(
            !path.exists(),
            "temp dir should be deleted even when read-only"
        );
    }

    #[test]
    fn replace_dir_link_tolerates_concurrent_replace() {
        let temp = tempfile::tempdir().expect("tempdir");
        let target = temp.path().join("target");
        std::fs::create_dir_all(&target).expect("target dir");
        let link = temp.path().join("link");

        replace_dir_link(&target, &link).expect("initial link");

        for _ in 0..25 {
            let barrier = Arc::new(Barrier::new(3));
            let target_a = target.clone();
            let link_a = link.clone();
            let barrier_a = barrier.clone();
            let a = std::thread::spawn(move || {
                barrier_a.wait();
                replace_dir_link(&target_a, &link_a)
            });

            let target_b = target.clone();
            let link_b = link.clone();
            let barrier_b = barrier.clone();
            let b = std::thread::spawn(move || {
                barrier_b.wait();
                replace_dir_link(&target_b, &link_b)
            });

            barrier.wait();
            a.join().expect("thread a").expect("replace a");
            b.join().expect("thread b").expect("replace b");
        }
    }
}
