use std::fs;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::Result;
use serde_json::json;
use tar::Archive;
use tempfile::TempDir;

use crate::core::runtime::facade::compute_lock_hash_bytes;
use crate::{CommandContext, ExecutionOutcome};
use px_domain::api::detect_lock_drift;

pub(crate) fn validate_lock_for_ref(
    snapshot: &px_domain::api::ProjectSnapshot,
    lock: &px_domain::api::LockSnapshot,
    contents: &str,
    git_ref: &str,
    lock_rel: &Path,
    marker_env: Option<&pep508_rs::MarkerEnvironment>,
) -> Result<String, ExecutionOutcome> {
    if lock.manifest_fingerprint.is_none() {
        return Err(ExecutionOutcome::user_error(
            "lockfile at git ref is missing a manifest fingerprint",
            json!({
                "git_ref": git_ref,
                "lock_path": lock_rel.display().to_string(),
                "reason": "lock_missing_fingerprint_at_ref",
            }),
        ));
    }
    let drift = detect_lock_drift(snapshot, lock, marker_env);
    let manifest_match = lock
        .manifest_fingerprint
        .as_deref()
        .is_some_and(|fp| fp == snapshot.manifest_fingerprint);
    if !drift.is_empty() || !manifest_match {
        let mut details = json!({
            "git_ref": git_ref,
            "lock_path": lock_rel.display().to_string(),
            "reason": "lock_drift_at_ref",
            "manifest_fingerprint": snapshot.manifest_fingerprint,
            "lock_fingerprint": lock.manifest_fingerprint,
        });
        if !drift.is_empty() {
            details["drift"] = json!(drift);
        }
        return Err(ExecutionOutcome::user_error(
            "px lockfile is out of sync with the manifest at that git ref",
            details,
        ));
    }

    Ok(lock
        .lock_id
        .clone()
        .unwrap_or_else(|| compute_lock_hash_bytes(contents.as_bytes())))
}

pub(crate) fn git_repo_root() -> Result<PathBuf, ExecutionOutcome> {
    let output = Command::new("git")
        .arg("rev-parse")
        .arg("--show-toplevel")
        .output();
    match output {
        Ok(output) if output.status.success() => {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            Ok(PathBuf::from(path))
        }
        Ok(output) => Err(ExecutionOutcome::user_error(
            "px --at requires a git repository",
            json!({
                "reason": "not_a_git_repo",
                "stderr": String::from_utf8_lossy(&output.stderr),
            }),
        )),
        Err(err) => Err(ExecutionOutcome::failure(
            "failed to invoke git",
            json!({ "error": err.to_string() }),
        )),
    }
}

pub(crate) fn materialize_ref_tree(
    repo_root: &Path,
    git_ref: &str,
) -> Result<TempDir, ExecutionOutcome> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("archive")
        .arg(git_ref)
        .output();
    match output {
        Ok(output) if output.status.success() => {
            let temp = tempfile::tempdir().map_err(|err| {
                ExecutionOutcome::failure(
                    "failed to create temp directory for git-ref execution",
                    json!({ "error": err.to_string() }),
                )
            })?;
            Archive::new(Cursor::new(output.stdout))
                .unpack(temp.path())
                .map_err(|err| {
                    ExecutionOutcome::failure(
                        "failed to extract git ref for --at execution",
                        json!({
                            "git_ref": git_ref,
                            "error": err.to_string(),
                        }),
                    )
                })?;
            populate_submodules(repo_root, git_ref, temp.path())?;
            restore_lfs_pointers(repo_root, git_ref, temp.path())?;
            Ok(temp)
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let reason = if stderr.contains("not a valid object name")
                || stderr.contains("unknown revision")
                || stderr.contains("bad revision")
            {
                "invalid_git_ref"
            } else {
                "archive_failed"
            };
            Err(ExecutionOutcome::user_error(
                format!("failed to read files from git ref {git_ref}"),
                json!({
                    "git_ref": git_ref,
                    "stderr": stderr,
                    "reason": reason,
                }),
            ))
        }
        Err(err) => Err(ExecutionOutcome::failure(
            "failed to invoke git",
            json!({ "error": err.to_string() }),
        )),
    }
}

fn populate_submodules(
    repo_root: &Path,
    git_ref: &str,
    dest_root: &Path,
) -> Result<(), ExecutionOutcome> {
    let submodules = list_submodules(repo_root, git_ref)?;
    if submodules.is_empty() {
        return Ok(());
    }
    let mut missing = Vec::new();
    for (path, sha) in submodules {
        let dest = dest_root.join(&path);
        let worktree_path = repo_root.join(&path);
        let mut reason = None;
        if !worktree_path.exists() {
            reason = Some("submodule not checked out in working tree".to_string());
        } else if !worktree_path.is_dir() {
            reason = Some("submodule path is not a directory in working tree".to_string());
        }
        if reason.is_none() {
            let commit = Command::new("git")
                .arg("-C")
                .arg(&worktree_path)
                .arg("rev-parse")
                .arg("HEAD")
                .output();
            match commit {
                Ok(output) if output.status.success() => {
                    let found = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    if found != sha {
                        reason = Some(format!("submodule checked out at {found}, expected {sha}"));
                    }
                }
                Ok(output) => {
                    reason = Some(format!(
                        "failed to read submodule commit: {}",
                        String::from_utf8_lossy(&output.stderr)
                    ));
                }
                Err(err) => {
                    reason = Some(format!("failed to invoke git for submodule: {err}"));
                }
            }
        }
        if let Some(reason) = reason {
            missing.push(json!({ "path": path.display().to_string(), "reason": reason }));
            continue;
        }
        if let Err(err) = copy_tree(&worktree_path, &dest) {
            missing.push(json!({
                "path": path.display().to_string(),
                "reason": format!("failed to copy submodule: {err}"),
            }));
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(ExecutionOutcome::user_error(
            "submodules from the requested git ref are not available",
            json!({
                "git_ref": git_ref,
                "missing_submodules": missing,
                "hint": "run `git submodule update --init --recursive` to populate them, then retry"
            }),
        ))
    }
}

pub(super) fn list_submodules(
    repo_root: &Path,
    git_ref: &str,
) -> Result<Vec<(PathBuf, String)>, ExecutionOutcome> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("ls-tree")
        .arg("-rz")
        .arg(git_ref)
        .output();
    let output = match output {
        Ok(output) => output,
        Err(err) => {
            return Err(ExecutionOutcome::failure(
                "failed to invoke git",
                json!({ "error": err.to_string() }),
            ))
        }
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ExecutionOutcome::user_error(
            "failed to list files at git ref",
            json!({
                "git_ref": git_ref,
                "stderr": stderr,
                "reason": "git_ls_tree_failed",
            }),
        ));
    }
    let mut results = Vec::new();
    for record in output.stdout.split(|b| *b == 0) {
        if record.is_empty() {
            continue;
        }
        let Ok(text) = std::str::from_utf8(record) else {
            continue;
        };
        let mut parts = text.splitn(2, '\t');
        let Some(meta) = parts.next() else {
            continue;
        };
        let Some(path) = parts.next() else {
            continue;
        };
        let mut fields = meta.split_whitespace();
        let _mode = fields.next();
        let Some(kind) = fields.next() else {
            continue;
        };
        let Some(sha) = fields.next() else {
            continue;
        };
        if kind == "commit" {
            results.push((PathBuf::from(path), sha.to_string()));
        }
    }
    Ok(results)
}

pub(super) fn restore_lfs_pointers(
    repo_root: &Path,
    git_ref: &str,
    root: &Path,
) -> Result<(), ExecutionOutcome> {
    let mut missing = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) => {
                missing.push(json!({
                    "path": dir.display().to_string(),
                    "reason": format!("failed to read directory: {err}"),
                }));
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(path);
                continue;
            }
            if !meta.is_file() {
                continue;
            }
            let Ok(bytes) = fs::read(&path) else {
                continue;
            };
            if !is_lfs_pointer(&bytes) {
                continue;
            }
            match smudge_lfs_pointer(repo_root, &bytes) {
                Ok(contents) => {
                    if let Err(err) = fs::write(&path, contents) {
                        missing.push(json!({
                            "path": path.display().to_string(),
                            "reason": format!("failed to write LFS content: {err}"),
                        }));
                    }
                }
                Err(err) => {
                    missing.push(json!({
                        "path": path.display().to_string(),
                        "reason": err,
                    }));
                }
            }
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(ExecutionOutcome::user_error(
            "git LFS content for the requested ref is unavailable",
            json!({
                "git_ref": git_ref,
                "missing_lfs_objects": missing,
                "hint": "ensure git LFS is installed and fetchable, then retry"
            }),
        ))
    }
}

fn smudge_lfs_pointer(repo_root: &Path, pointer: &[u8]) -> std::result::Result<Vec<u8>, String> {
    let mut cmd = if let Ok(path) = which::which("git-lfs") {
        let mut cmd = Command::new(path);
        cmd.current_dir(repo_root).arg("smudge");
        cmd
    } else {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(repo_root).arg("lfs").arg("smudge");
        cmd
    };
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("failed to invoke git-lfs smudge: {err}"))?;
    if let Some(stdin) = child.stdin.as_mut() {
        if let Err(err) = stdin.write_all(pointer) {
            return Err(format!("failed to write LFS pointer to smudge: {err}"));
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|err| format!("failed to read git-lfs smudge output: {err}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(output.stdout)
}

pub(super) fn is_lfs_pointer(bytes: &[u8]) -> bool {
    let prefix = b"version https://git-lfs.github.com/spec/v1";
    bytes.starts_with(prefix)
}

pub(super) fn copy_tree(src: &Path, dest: &Path) -> anyhow::Result<()> {
    if dest.exists() {
        fs::remove_dir_all(dest).ok();
    }
    for entry in walkdir::WalkDir::new(src) {
        let entry = entry?;
        let rel = match entry.path().strip_prefix(src) {
            Ok(rel) => rel,
            Err(_) => continue,
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        if rel.components().any(|c| c.as_os_str() == ".git") {
            // Skip .git contents to avoid nested repository metadata.
            continue;
        }
        let target = dest.join(rel);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)?;
            continue;
        }
        if entry.file_type().is_symlink() {
            let link = fs::read_link(entry.path())?;
            create_symlink(&link, &target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(entry.path(), &target)?;
    }
    Ok(())
}

#[cfg(unix)]
fn create_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn create_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    use std::os::windows::fs as win_fs;
    let metadata = fs::metadata(target)?;
    if metadata.is_dir() {
        win_fs::symlink_dir(target, link)
    } else {
        win_fs::symlink_file(target, link)
    }
}

pub(super) fn manifest_has_px(doc: &toml_edit::DocumentMut) -> bool {
    doc.get("tool")
        .and_then(toml_edit::Item::as_table)
        .and_then(|tool| tool.get("px"))
        .is_some()
}

fn sanitize_ref_for_path(git_ref: &str) -> String {
    git_ref
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

pub(super) struct EnvVarGuard {
    key: &'static str,
    previous: Option<String>,
}

impl EnvVarGuard {
    pub(super) fn set(key: &'static str, value: String) -> Self {
        let previous = std::env::var(key).ok();
        std::env::set_var(key, value);
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(prev) => std::env::set_var(self.key, prev),
            None => std::env::remove_var(self.key),
        }
    }
}

pub(super) fn commit_stdlib_guard(ctx: &CommandContext, git_ref: &str) -> Option<EnvVarGuard> {
    if std::env::var("PX_STDLIB_STAGING_ROOT").is_ok() {
        return None;
    }
    let root = ctx
        .cache()
        .path
        .join("stdlib-tests")
        .join(sanitize_ref_for_path(git_ref));
    Some(EnvVarGuard::set(
        "PX_STDLIB_STAGING_ROOT",
        root.display().to_string(),
    ))
}
