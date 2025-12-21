use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

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
