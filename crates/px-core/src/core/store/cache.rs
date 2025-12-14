use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use dirs_next::home_dir;

#[derive(Debug, Clone)]
pub struct CacheLocation {
    pub path: PathBuf,
    pub source: &'static str,
}

#[derive(Debug, Clone)]
pub struct CacheUsage {
    pub exists: bool,
    pub total_entries: u64,
    pub total_size_bytes: u64,
}

#[derive(Debug, Clone, Default)]
pub struct CacheWalk {
    pub exists: bool,
    pub files: Vec<CacheEntry>,
    pub dirs: Vec<PathBuf>,
    pub total_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct CacheEntry {
    pub path: PathBuf,
    pub size: u64,
}

#[derive(Debug, Clone)]
pub struct CachePruneError {
    pub path: PathBuf,
    pub error: String,
}

#[derive(Debug, Clone, Default)]
pub struct CachePruneResult {
    pub candidate_entries: u64,
    pub candidate_size_bytes: u64,
    pub deleted_entries: u64,
    pub deleted_size_bytes: u64,
    pub deleted_dirs: u64,
    pub errors: Vec<CachePruneError>,
}

/// Determine the root directory for the on-disk cache.
///
/// # Errors
///
/// Returns an error if the configured directory cannot be resolved or created.
pub fn resolve_cache_store_path() -> Result<CacheLocation> {
    if let Some(override_path) = env::var_os("PX_CACHE_PATH") {
        let path = absolutize(PathBuf::from(override_path))?;
        return Ok(CacheLocation {
            path,
            source: "PX_CACHE_PATH",
        });
    }

    #[cfg(target_os = "windows")]
    let (base, source) = resolve_windows_cache_base()?;
    #[cfg(not(target_os = "windows"))]
    let (base, source) = resolve_unix_cache_base()?;

    Ok(CacheLocation {
        path: base.join("cache"),
        source,
    })
}

#[must_use]
pub fn pyc_cache_prefix(cache_root: &Path, profile_oid: &str) -> PathBuf {
    cache_root.join("pyc").join(profile_oid)
}

const PYC_CACHE_MARKER: &str = ".px-last-used";

#[derive(Clone, Copy)]
struct PycCachePolicy {
    max_profiles: usize,
    target_profiles: usize,
    max_age: Duration,
}

const DEFAULT_PYC_CACHE_POLICY: PycCachePolicy = PycCachePolicy {
    // Upper bound on the number of per-profile cache directories kept under cache_root/pyc/.
    // Older directories are removed (LRU) once this threshold is exceeded.
    max_profiles: 256,
    // Target count after pruning. This provides some hysteresis so we don't prune on every run.
    target_profiles: 192,
    // Opportunistically delete cache directories that haven't been used recently.
    max_age: Duration::from_secs(30 * 24 * 60 * 60),
};

/// Ensure the per-profile Python bytecode cache prefix exists on disk.
///
/// # Errors
///
/// Returns an error if the directory cannot be created.
pub fn ensure_pyc_cache_prefix(cache_root: &Path, profile_oid: &str) -> Result<PathBuf> {
    let prefix = pyc_cache_prefix(cache_root, profile_oid);
    fs::create_dir_all(&prefix)
        .with_context(|| format!("failed to create pyc cache directory {}", prefix.display()))?;
    touch_pyc_cache_marker(&prefix)?;
    let pyc_root = cache_root.join("pyc");
    let _ = prune_pyc_cache_root(&pyc_root, Some(profile_oid), DEFAULT_PYC_CACHE_POLICY);
    Ok(prefix)
}

fn touch_pyc_cache_marker(prefix: &Path) -> Result<()> {
    let marker = prefix.join(PYC_CACHE_MARKER);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    fs::write(&marker, format!("{now}\n").as_bytes())
        .with_context(|| format!("failed to write pyc cache marker {}", marker.display()))?;
    Ok(())
}

fn prune_pyc_cache_root(
    pyc_root: &Path,
    active_profile_oid: Option<&str>,
    policy: PycCachePolicy,
) -> Result<()> {
    if !pyc_root.exists() {
        return Ok(());
    }
    if policy.target_profiles > policy.max_profiles {
        return Ok(());
    }

    let now = SystemTime::now();
    let mut entries = Vec::new();
    let read_dir = fs::read_dir(pyc_root)
        .with_context(|| format!("failed to list pyc cache directory {}", pyc_root.display()))?;
    for entry in read_dir {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() || !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(oid) = name.to_str() else {
            continue;
        };
        if oid.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let last_used = last_used_time(&path).unwrap_or(now);
        entries.push((oid.to_string(), path, last_used));
    }

    for (oid, path, last_used) in entries.iter() {
        if active_profile_oid.is_some_and(|active| active == oid) {
            continue;
        }
        if now
            .duration_since(*last_used)
            .unwrap_or_default()
            .saturating_sub(policy.max_age)
            .as_secs()
            == 0
        {
            continue;
        }
        let _ = fs::remove_dir_all(path);
    }

    let mut remaining = Vec::new();
    for entry in fs::read_dir(pyc_root)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() || !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(oid) = name.to_str() else {
            continue;
        };
        if oid.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let last_used = last_used_time(&path).unwrap_or(now);
        remaining.push((oid.to_string(), path, last_used));
    }

    if remaining.len() <= policy.max_profiles {
        return Ok(());
    }
    remaining.sort_by(|a, b| a.2.cmp(&b.2));

    let mut count = remaining.len();
    for (oid, path, _) in remaining {
        if count <= policy.target_profiles {
            break;
        }
        if active_profile_oid.is_some_and(|active| active == oid) {
            continue;
        }
        if fs::remove_dir_all(&path).is_ok() {
            count = count.saturating_sub(1);
        }
    }
    Ok(())
}

fn last_used_time(path: &Path) -> Option<SystemTime> {
    let marker = path.join(PYC_CACHE_MARKER);
    if let Ok(meta) = fs::metadata(&marker) {
        if let Ok(modified) = meta.modified() {
            return Some(modified);
        }
    }
    fs::metadata(path).ok()?.modified().ok()
}

/// Compute aggregate statistics for every file under the cache path.
///
/// # Errors
///
/// Returns an error if the directory tree cannot be traversed.
pub fn compute_cache_usage(path: &Path) -> Result<CacheUsage> {
    if !path.exists() {
        return Ok(CacheUsage {
            exists: false,
            total_entries: 0,
            total_size_bytes: 0,
        });
    }

    let mut total_entries = 0u64;
    let mut total_size_bytes = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let entry_path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                stack.push(entry_path);
            } else if file_type.is_file() {
                let metadata = entry.metadata()?;
                total_entries += 1;
                total_size_bytes += metadata.len();
            }
        }
    }

    Ok(CacheUsage {
        exists: true,
        total_entries,
        total_size_bytes,
    })
}

/// Gather every entry under the cache directory.
///
/// # Errors
///
/// Returns an error if reading the directory tree fails at any point.
pub fn collect_cache_walk(path: &Path) -> Result<CacheWalk> {
    if !path.exists() {
        return Ok(CacheWalk::default());
    }

    let mut walk = CacheWalk {
        exists: true,
        ..CacheWalk::default()
    };
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let entry_path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                stack.push(entry_path.clone());
                if entry_path != path {
                    walk.dirs.push(entry_path);
                }
            } else if file_type.is_file() {
                let size = entry.metadata()?.len();
                walk.total_bytes += size;
                walk.files.push(CacheEntry {
                    path: entry_path,
                    size,
                });
            }
        }
    }

    walk.files.sort_by(|a, b| a.path.cmp(&b.path));
    walk.dirs.sort();
    Ok(walk)
}

#[must_use]
pub fn prune_cache_entries(walk: &CacheWalk) -> CachePruneResult {
    let mut result = CachePruneResult {
        candidate_entries: walk.files.len() as u64,
        candidate_size_bytes: walk.total_bytes,
        ..CachePruneResult::default()
    };

    for entry in &walk.files {
        match std::fs::remove_file(&entry.path) {
            Ok(()) => {
                result.deleted_entries += 1;
                result.deleted_size_bytes += entry.size;
            }
            Err(err) => result.errors.push(CachePruneError {
                path: entry.path.clone(),
                error: err.to_string(),
            }),
        }
    }

    for dir in walk.dirs.iter().rev() {
        match std::fs::remove_dir(dir) {
            Ok(()) => {
                result.deleted_dirs += 1;
            }
            Err(err) => result.errors.push(CachePruneError {
                path: dir.clone(),
                error: err.to_string(),
            }),
        }
    }

    result
}

#[cfg(not(target_os = "windows"))]
fn resolve_unix_cache_base() -> Result<(PathBuf, &'static str)> {
    if let Some(home) = home_dir() {
        return Ok((home.join(".px"), "HOME/.px"));
    }

    let fallback = PathBuf::from("/tmp/px");
    Ok((fallback, "default (/tmp/px)"))
}

#[cfg(target_os = "windows")]
fn resolve_windows_cache_base() -> Result<(PathBuf, &'static str)> {
    if let Some(local_app_data) = env::var_os("LOCALAPPDATA") {
        let path = PathBuf::from(local_app_data);
        return Ok((path.join("px"), "LOCALAPPDATA/px"));
    }

    if let Some(home) = env::var_os("USERPROFILE") {
        let path = PathBuf::from(home).join("AppData").join("Local").join("px");
        return Ok((path, "USERPROFILE/AppData/Local/px"));
    }

    if let Some(home) = home_dir() {
        return Ok((
            home.join("AppData").join("Local").join("px"),
            "HOME/AppData/Local/px",
        ));
    }

    let fallback = PathBuf::from("C:\\px");
    Ok((fallback, "default (C:\\px)"))
}

fn absolutize(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()
            .context("failed to resolve PX_CACHE_PATH")?
            .join(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use filetime::{set_file_mtime, FileTime};
    use tempfile::tempdir;

    fn seed_pyc_dir(root: &Path, oid: &str, last_used_secs_ago: u64) -> Result<PathBuf> {
        fs::create_dir_all(root)?;
        let dir = root.join(oid);
        fs::create_dir_all(&dir)?;
        let marker = dir.join(PYC_CACHE_MARKER);
        fs::write(&marker, b"")?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let target = now.saturating_sub(last_used_secs_ago as i64);
        set_file_mtime(&marker, FileTime::from_unix_time(target, 0))?;
        Ok(dir)
    }

    #[test]
    fn pyc_cache_prunes_oldest_when_over_profile_limit() -> Result<()> {
        let temp = tempdir()?;
        let pyc_root = temp.path().join("pyc");
        seed_pyc_dir(&pyc_root, "a", 300)?;
        seed_pyc_dir(&pyc_root, "b", 200)?;
        seed_pyc_dir(&pyc_root, "c", 100)?;

        prune_pyc_cache_root(
            &pyc_root,
            Some("c"),
            PycCachePolicy {
                max_profiles: 2,
                target_profiles: 2,
                max_age: Duration::from_secs(365 * 24 * 60 * 60),
            },
        )?;

        assert!(
            !pyc_root.join("a").exists(),
            "expected oldest cache removed"
        );
        assert!(pyc_root.join("b").exists(), "expected newer cache retained");
        assert!(
            pyc_root.join("c").exists(),
            "expected active cache retained"
        );
        Ok(())
    }

    #[test]
    fn pyc_cache_prunes_entries_older_than_max_age() -> Result<()> {
        let temp = tempdir()?;
        let pyc_root = temp.path().join("pyc");
        seed_pyc_dir(&pyc_root, "old", 1_000)?;
        seed_pyc_dir(&pyc_root, "new", 10)?;

        prune_pyc_cache_root(
            &pyc_root,
            Some("new"),
            PycCachePolicy {
                max_profiles: 50,
                target_profiles: 50,
                max_age: Duration::from_secs(100),
            },
        )?;

        assert!(!pyc_root.join("old").exists(), "expected old cache removed");
        assert!(
            pyc_root.join("new").exists(),
            "expected recent cache retained"
        );
        Ok(())
    }

    #[test]
    fn pyc_cache_never_removes_active_profile_dir() -> Result<()> {
        let temp = tempdir()?;
        let pyc_root = temp.path().join("pyc");
        seed_pyc_dir(&pyc_root, "active", 10_000)?;

        prune_pyc_cache_root(
            &pyc_root,
            Some("active"),
            PycCachePolicy {
                max_profiles: 1,
                target_profiles: 1,
                max_age: Duration::from_secs(1),
            },
        )?;

        assert!(
            pyc_root.join("active").exists(),
            "active cache should never be pruned"
        );
        Ok(())
    }
}
