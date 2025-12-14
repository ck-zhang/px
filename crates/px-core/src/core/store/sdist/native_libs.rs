use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use tracing::debug;
use walkdir::WalkDir;

pub(super) fn copy_native_libs(env_root: &Path, dist_path: &Path) -> Result<()> {
    let mut env_lib_index: HashMap<String, PathBuf> = HashMap::new();
    for dir in ["lib", "lib64"] {
        let lib_root = env_root.join(dir);
        if !lib_root.exists() {
            continue;
        }
        for entry in WalkDir::new(&lib_root) {
            let entry = entry?;
            if entry.file_type().is_file() && is_shared_lib(entry.path()) {
                if let Some(name) = entry
                    .path()
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(str::to_string)
                {
                    env_lib_index
                        .entry(name)
                        .or_insert(entry.path().to_path_buf());
                }
            }
        }
    }
    debug!(
        env_root = %env_root.display(),
        lib_count = env_lib_index.len(),
        "collecting native libs from builder environment"
    );

    let mut queue = VecDeque::new();
    for entry in WalkDir::new(dist_path) {
        let entry = entry?;
        if entry.file_type().is_file() && is_shared_lib(entry.path()) {
            queue.push_back(entry.path().to_path_buf());
        }
    }
    debug!(
        dist = %dist_path.display(),
        seeds = queue.len(),
        "scanning built wheel for native dependencies"
    );

    let mut seen = HashSet::new();
    let mut deps_to_copy: HashSet<PathBuf> = HashSet::new();

    while let Some(target) = queue.pop_front() {
        if !seen.insert(target.clone()) {
            continue;
        }
        let deps = ldd_dependencies(&target, env_root)?;
        for dep in deps {
            let resolved = if dep.starts_with(env_root) {
                Some(dep)
            } else {
                dep.file_name()
                    .and_then(|name| name.to_str())
                    .and_then(|name| env_lib_index.get(name).cloned())
                    .or_else(|| Some(dep.clone()))
            };
            let Some(dep_path) = resolved else {
                continue;
            };
            if should_skip_native_dep(&dep_path) {
                continue;
            }
            if deps_to_copy.insert(dep_path.clone()) {
                queue.push_back(dep_path);
            }
        }
    }

    debug!(
        deps = deps_to_copy.len(),
        "copying resolved native libraries into wheel dist"
    );
    let sys_libs_root = dist_path.join("sys-libs");
    for dep in deps_to_copy {
        if should_skip_native_dep(&dep) {
            continue;
        }
        let rel = if dep.starts_with(env_root) {
            dep.strip_prefix(env_root)
                .unwrap_or(dep.as_path())
                .to_path_buf()
        } else {
            sys_libs_root.join(
                dep.file_name()
                    .unwrap_or_else(|| std::ffi::OsStr::new("unknown")),
            )
        };
        let dest = dist_path.join(rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&dep, &dest).with_context(|| {
            format!(
                "failed to copy native lib {} to {}",
                dep.display(),
                dest.display()
            )
        })?;
    }
    Ok(())
}

fn is_shared_lib(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| {
            let lower = name.to_ascii_lowercase();
            lower.contains(".so") || lower.ends_with(".dylib") || lower.ends_with(".dll")
        })
        .unwrap_or(false)
}

fn should_skip_native_dep(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| {
            let lower = name.to_ascii_lowercase();
            lower.starts_with("ld-linux") || lower.starts_with("libc.")
        })
        .unwrap_or(false)
}

fn ldd_dependencies(target: &Path, env_root: &Path) -> Result<Vec<PathBuf>> {
    let mut cmd = Command::new("ldd");
    cmd.arg(target);
    let mut search_paths = Vec::new();
    for dir in ["lib", "lib64"] {
        let candidate = env_root.join(dir);
        if candidate.exists() {
            search_paths.push(candidate);
        }
    }
    if !search_paths.is_empty() {
        let joined = search_paths
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(":");
        cmd.env("LD_LIBRARY_PATH", joined);
    }
    let output = cmd
        .output()
        .with_context(|| format!("failed to run ldd on {}", target.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("ldd failed for {}: {stderr}", target.display());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut deps = Vec::new();
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut path_candidate: Option<PathBuf> = None;
        if let Some((_, rest)) = trimmed.split_once("=>") {
            let value = rest.trim();
            if value.starts_with("not found") || value == "not found" {
                continue;
            }
            let path_part = value.split_whitespace().next().unwrap_or("");
            if path_part.is_empty() || path_part == "not" {
                continue;
            }
            if path_part.starts_with('/') {
                path_candidate = Some(PathBuf::from(path_part));
            }
        } else if let Some(first) = trimmed.split_whitespace().next() {
            if first.starts_with('/') {
                path_candidate = Some(PathBuf::from(first));
            }
        }
        if let Some(path) = path_candidate {
            deps.push(path);
        }
    }
    Ok(deps)
}
