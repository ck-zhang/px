//! CAS unit tests.
//!
//! Mapping note (for reviewers):
//! - Old: `core/store/cas/tests.rs`
//! - New: `core/store/cas/tests/` (split by topic)

use super::*;
use anyhow::bail;
#[cfg(unix)]
use flate2::read::GzDecoder;
use serde_json::json;
use std::collections::BTreeMap;
use std::env;
#[cfg(unix)]
use std::io::Read;
use std::ops::Deref;
#[cfg(unix)]
use std::os::unix::fs::symlink;
#[cfg(unix)]
use std::path::Path;
use std::sync::Arc;
use std::sync::{Mutex, OnceLock};
use std::thread;
use tempfile::tempdir;
use url::Url;

static CURRENT_DIR_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

struct EnvStoreTemp {
    inner: tempfile::TempDir,
    prev_envs: Option<String>,
}

impl Deref for EnvStoreTemp {
    type Target = tempfile::TempDir;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl Drop for EnvStoreTemp {
    fn drop(&mut self) {
        if let Some(prev) = &self.prev_envs {
            std::env::set_var("PX_ENVS_PATH", prev);
        } else {
            std::env::remove_var("PX_ENVS_PATH");
        }
    }
}

fn new_store() -> Result<(EnvStoreTemp, ContentAddressableStore)> {
    let temp = tempdir()?;
    let root = temp.path().join("store");
    let envs_root = temp.path().join("envs");
    let prev_envs = std::env::var("PX_ENVS_PATH").ok();
    std::env::set_var("PX_ENVS_PATH", &envs_root);
    let store = ContentAddressableStore::new(Some(root))?;
    Ok((
        EnvStoreTemp {
            inner: temp,
            prev_envs,
        },
        store,
    ))
}

struct EnvVarGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = env::var_os(key);
        env::set_var(key, value);
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match self.previous.as_ref() {
            Some(value) => env::set_var(self.key, value),
            None => env::remove_var(self.key),
        }
    }
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

fn git(repo: &std::path::Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()?;
    if !output.status.success() {
        bail!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn git_ok(repo: &std::path::Path, args: &[&str]) -> Result<()> {
    let _ = git(repo, args)?;
    Ok(())
}

fn init_git_repo(repo: &std::path::Path) -> Result<()> {
    git_ok(repo, &["init"])?;
    git_ok(repo, &["config", "user.email", "px-test@example.invalid"])?;
    git_ok(repo, &["config", "user.name", "px test"])?;
    Ok(())
}

#[cfg(unix)]
fn make_writable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(metadata) = fs::metadata(path) {
        let mut perms = metadata.permissions();
        perms.set_mode(perms.mode() | 0o200);
        let _ = fs::set_permissions(path, perms);
    }
}

#[cfg(not(unix))]
fn make_writable(path: &Path) {
    if let Ok(metadata) = fs::metadata(path) {
        let mut perms = metadata.permissions();
        perms.set_readonly(false);
        let _ = fs::set_permissions(path, perms);
    }
}

fn demo_source_payload() -> ObjectPayload<'static> {
    ObjectPayload::Source {
        header: SourceHeader {
            name: "demo".to_string(),
            version: "1.0.0".to_string(),
            filename: "demo-1.0.0.whl".to_string(),
            index_url: "https://example.invalid/simple/".to_string(),
            sha256: "deadbeef".to_string(),
        },
        bytes: Cow::Owned(b"demo-payload".to_vec()),
    }
}

mod basics;
mod doctor;
mod gc;
mod index;
mod metadata;
mod permissions;
mod refs;
mod repo_snapshot;
mod verify;
