use std::env;
use std::path::{Path, PathBuf};

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
        path: base.join("px").join("store"),
        source,
    })
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
            let metadata = entry.metadata()?;
            if metadata.is_dir() {
                stack.push(entry_path);
            } else if metadata.is_file() {
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
            let metadata = entry.metadata()?;
            if metadata.is_dir() {
                stack.push(entry_path.clone());
                if entry_path != path {
                    walk.dirs.push(entry_path);
                }
            } else if metadata.is_file() {
                let size = metadata.len();
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
        return Ok((home.join(".cache"), "HOME/.cache"));
    }

    let fallback = PathBuf::from("/tmp/px-cache");
    Ok((fallback, "default (/tmp/px-cache)"))
}

#[cfg(target_os = "windows")]
fn resolve_windows_cache_base() -> Result<(PathBuf, &'static str)> {
    if let Some(local_app_data) = env::var_os("LOCALAPPDATA") {
        let path = PathBuf::from(local_app_data);
        return Ok((path, "LOCALAPPDATA"));
    }

    if let Some(home) = env::var_os("USERPROFILE") {
        let path = PathBuf::from(home).join("AppData").join("Local");
        return Ok((path, "USERPROFILE/AppData/Local"));
    }

    if let Some(home) = home_dir() {
        return Ok((home.join("AppData").join("Local"), "HOME/AppData/Local"));
    }

    let fallback = PathBuf::from("C:\\px-cache");
    Ok((fallback, "default (C:\\px-cache)"))
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
