use super::*;

fn normalize_archive_path(path: &Path) -> Result<String> {
    let normalized = path.to_string_lossy().replace('\\', "/");
    if normalized.starts_with('/') {
        return Err(anyhow!(
            "archive entries must be relative (got {})",
            normalized
        ));
    }
    if normalized.is_empty() {
        return Err(anyhow!("archive entry path is empty"));
    }
    Ok(normalized)
}

pub(super) fn archive_dir_canonical_to_writer<W: Write>(root: &Path, writer: W) -> Result<W> {
    use std::ffi::OsStr;

    let encoder = GzBuilder::new()
        .mtime(0)
        .write(writer, Compression::default());
    let mut builder = tar::Builder::new(encoder);
    builder.follow_symlinks(false);
    for entry in walkdir::WalkDir::new(root)
        .sort_by(|a, b| a.path().cmp(b.path()))
        .into_iter()
        .filter_entry(|entry| entry.file_name() != OsStr::new(".git"))
    {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                debug!(%err, root=%root.display(), "skipping path during archive walk");
                continue;
            }
        };
        let path = entry.path();
        if path == root {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .context("failed to relativize path")?;
        let rel_path = normalize_archive_path(rel)?;
        // Use symlink_metadata so we never follow links while capturing the tree.
        let metadata = match fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == ErrorKind::PermissionDenied => {
                debug!(path=%path.display(), "skipping unreadable path during archive");
                continue;
            }
            Err(err) => return Err(err.into()),
        };
        let file_type = metadata.file_type();
        let mut header = Header::new_gnu();
        header.set_mtime(0);
        header.set_uid(0);
        header.set_gid(0);
        let _ = header.set_username("");
        let _ = header.set_groupname("");
        if file_type.is_dir() {
            header.set_entry_type(tar::EntryType::Directory);
            header.set_mode(0o755);
            header.set_size(0);
            builder.append_data(&mut header, Path::new(&rel_path), std::io::empty())?;
        } else if file_type.is_file() {
            header.set_entry_type(tar::EntryType::Regular);
            header.set_mode(if is_executable(&metadata) {
                0o755
            } else {
                0o644
            });
            header.set_size(metadata.len());
            let file = match File::open(path) {
                Ok(file) => file,
                Err(err) if err.kind() == ErrorKind::PermissionDenied => {
                    debug!(path=%path.display(), "skipping unreadable file during archive");
                    continue;
                }
                Err(err) => return Err(err.into()),
            };
            builder.append_data(&mut header, Path::new(&rel_path), file)?;
        } else if file_type.is_symlink() {
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_mode(0o777);
            header.set_size(0);
            let mut target = match fs::read_link(path) {
                Ok(link) => link,
                Err(err) if err.kind() == ErrorKind::PermissionDenied => {
                    debug!(
                        path = %path.display(),
                        "skipping unreadable symlink during archive"
                    );
                    continue;
                }
                Err(err) => {
                    return Err(err).with_context(|| {
                        format!("failed to read symlink target {}", path.display())
                    })
                }
            };
            if target.is_absolute() {
                if let Ok(relative) = target.strip_prefix(root) {
                    target = relative.to_path_buf();
                } else {
                    target = target
                        .file_name()
                        .map(PathBuf::from)
                        .unwrap_or_else(|| PathBuf::from("target"));
                }
            }
            let target_path = normalize_archive_path(&target)?;
            if let Err(err) = header.set_link_name(Path::new(&target_path)) {
                let Some(basename) = target.file_name().and_then(|n| n.to_str()) else {
                    debug!(
                        path = %path.display(),
                        target = %target_path,
                        %err,
                        "skipping symlink with invalid target during archive"
                    );
                    continue;
                };
                if let Err(err) = header.set_link_name(basename) {
                    debug!(
                        path = %path.display(),
                        target = %target_path,
                        %err,
                        "skipping symlink with long target during archive"
                    );
                    continue;
                }
                debug!(
                    path = %path.display(),
                    target = %target_path,
                    fallback = basename,
                    "shortened symlink target during archive"
                );
            }
            builder.append_data(&mut header, Path::new(&rel_path), std::io::empty())?;
        } else {
            continue;
        }
    }
    builder.finish()?;
    let encoder = builder.into_inner()?;
    Ok(encoder.finish()?)
}

/// Produce a deterministic, gzip-compressed tar archive for a directory tree.
pub fn archive_dir_canonical(root: &Path) -> Result<Vec<u8>> {
    archive_dir_canonical_to_writer(root, Vec::new())
}

/// Archive a subset of paths under a shared root, skipping unreadable entries and any entry
/// rejected by `filter_entry`.
pub fn archive_selected_filtered<F>(root: &Path, paths: &[PathBuf], mut filter_entry: F) -> Result<Vec<u8>>
where
    F: FnMut(&walkdir::DirEntry) -> bool,
{
    use std::ffi::OsStr;

    let encoder = GzBuilder::new()
        .mtime(0)
        .write(Vec::new(), Compression::default());
    let mut builder = tar::Builder::new(encoder);
    builder.follow_symlinks(false);
    let mut seen = HashSet::new();

    for base in paths {
        if !base.starts_with(root) {
            debug!(
                path = %base.display(),
                root = %root.display(),
                "skipping path outside archive root"
            );
            continue;
        }
        if !base.exists() {
            continue;
        }
        for entry in walkdir::WalkDir::new(base)
            .sort_by(|a, b| a.path().cmp(b.path()))
            .into_iter()
            .filter_entry(|entry| entry.file_name() != OsStr::new(".git") && filter_entry(entry))
        {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    debug!(%err, root=%root.display(), "skipping path during archive walk");
                    continue;
                }
            };
            let path = entry.path();
            if !seen.insert(path.to_path_buf()) {
                continue;
            }
            if path == root {
                continue;
            }
            let rel = match path.strip_prefix(root) {
                Ok(rel) => rel,
                Err(_) => continue,
            };
            let rel_path = normalize_archive_path(rel)?;
            let metadata = match fs::symlink_metadata(path) {
                Ok(meta) => meta,
                Err(err) if err.kind() == ErrorKind::PermissionDenied => {
                    debug!(path=%path.display(), "skipping unreadable path during archive");
                    continue;
                }
                Err(err) => return Err(err.into()),
            };
            let file_type = metadata.file_type();
            let mut header = Header::new_gnu();
            header.set_mtime(0);
            header.set_uid(0);
            header.set_gid(0);
            let _ = header.set_username("");
            let _ = header.set_groupname("");
            if file_type.is_dir() {
                header.set_entry_type(tar::EntryType::Directory);
                header.set_mode(0o755);
                header.set_size(0);
                builder.append_data(&mut header, Path::new(&rel_path), std::io::empty())?;
            } else if file_type.is_file() {
                header.set_entry_type(tar::EntryType::Regular);
                header.set_mode(if is_executable(&metadata) {
                    0o755
                } else {
                    0o644
                });
                header.set_size(metadata.len());
                let file = match File::open(path) {
                    Ok(file) => file,
                    Err(err) if err.kind() == ErrorKind::PermissionDenied => {
                        debug!(path=%path.display(), "skipping unreadable file during archive");
                        continue;
                    }
                    Err(err) => return Err(err.into()),
                };
                builder.append_data(&mut header, Path::new(&rel_path), file)?;
            } else if file_type.is_symlink() {
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_mode(0o777);
                header.set_size(0);
                let mut target = match fs::read_link(path) {
                    Ok(link) => link,
                    Err(err) if err.kind() == ErrorKind::PermissionDenied => {
                        debug!(
                            path = %path.display(),
                            "skipping unreadable symlink during archive"
                        );
                        continue;
                    }
                    Err(err) => {
                        return Err(err).with_context(|| {
                            format!("failed to read symlink target {}", path.display())
                        })
                    }
                };
                if target.is_absolute() {
                    if let Ok(relative) = target.strip_prefix(root) {
                        target = relative.to_path_buf();
                    } else {
                        target = target
                            .file_name()
                            .map(PathBuf::from)
                            .unwrap_or_else(|| PathBuf::from("target"));
                    }
                }
                let target_str = normalize_archive_path(&target)?;
                if let Err(err) = header.set_link_name(&target_str) {
                    let Some(basename) = target.file_name().and_then(|n| n.to_str()) else {
                        debug!(
                            path = %path.display(),
                            target = %target_str,
                            %err,
                            "skipping symlink with invalid target during archive"
                        );
                        continue;
                    };
                    if let Err(err) = header.set_link_name(basename) {
                        debug!(
                            path = %path.display(),
                            target = %target_str,
                            %err,
                            "skipping symlink with long target during archive"
                        );
                        continue;
                    }
                    debug!(
                        path = %path.display(),
                        target = %target_str,
                        fallback = basename,
                        "shortened symlink target during archive"
                    );
                }
                builder.append_data(&mut header, Path::new(&rel_path), std::io::empty())?;
            }
        }
    }
    builder.finish()?;
    let encoder = builder.into_inner()?;
    Ok(encoder.finish()?)
}

#[cfg(unix)]
fn is_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &fs::Metadata) -> bool {
    false
}
