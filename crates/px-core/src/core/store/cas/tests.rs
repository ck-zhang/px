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

#[test]
fn default_roots_match_documented_layout() -> Result<()> {
    for key in [
        "PX_CACHE_PATH",
        "PX_STORE_PATH",
        "PX_ENVS_PATH",
        "PX_TOOLS_DIR",
        "PX_TOOL_STORE",
        "PX_SANDBOX_STORE",
        "PX_RUNTIME_REGISTRY",
    ] {
        if std::env::var_os(key).is_some() {
            eprintln!("skipping default_roots_match_documented_layout ({key} is set)");
            return Ok(());
        }
    }

    let home = dirs_next::home_dir().context("home directory not found")?;
    let px_root = home.join(".px");

    assert_eq!(default_root()?, px_root.join("store"));
    assert_eq!(default_envs_root_path()?, px_root.join("envs"));
    assert_eq!(default_tools_root_path()?, px_root.join("tools"));
    assert_eq!(
        crate::core::sandbox::default_store_root()?,
        px_root.join("sandbox")
    );
    assert_eq!(
        crate::store::resolve_cache_store_path()?.path,
        px_root.join("cache")
    );
    assert_eq!(
        crate::core::runtime::cas_env::default_envs_root()?,
        px_root.join("envs")
    );

    Ok(())
}

#[test]
fn creates_layout_and_schema() -> Result<()> {
    let (_temp, store) = new_store()?;
    let root = store.root().to_path_buf();
    for dir in [OBJECTS_DIR, LOCKS_DIR, TMP_DIR] {
        assert!(
            root.join(dir).is_dir(),
            "expected {} directory to exist",
            dir
        );
    }
    assert!(
        root.join(INDEX_FILENAME).is_file(),
        "expected index.sqlite to exist"
    );
    Ok(())
}

#[test]
fn records_and_validates_meta_versions() -> Result<()> {
    let (_temp, store) = new_store()?;
    let conn = store.connection()?;
    let fmt: String = conn.query_row(
        "SELECT value FROM meta WHERE key = ?1",
        rusqlite::params![META_KEY_CAS_FORMAT_VERSION],
        |row| row.get(0),
    )?;
    assert_eq!(fmt, CAS_FORMAT_VERSION.to_string());
    let schema: String = conn.query_row(
        "SELECT value FROM meta WHERE key = ?1",
        rusqlite::params![META_KEY_SCHEMA_VERSION],
        |row| row.get(0),
    )?;
    assert_eq!(schema, SCHEMA_VERSION.to_string());
    let created_by: String = conn.query_row(
        "SELECT value FROM meta WHERE key = ?1",
        rusqlite::params![META_KEY_CREATED_BY],
        |row| row.get(0),
    )?;
    assert_eq!(created_by, PX_VERSION);

    conn.execute(
        "UPDATE meta SET value='0.0.0' WHERE key=?1",
        rusqlite::params![META_KEY_LAST_USED],
    )?;
    drop(conn);

    // Trigger layout validation and ensure last_used is refreshed.
    let _ = store.list(None, None)?;
    let conn = store.connection()?;
    let last_used: String = conn.query_row(
        "SELECT value FROM meta WHERE key = ?1",
        rusqlite::params![META_KEY_LAST_USED],
        |row| row.get(0),
    )?;
    assert_eq!(last_used, PX_VERSION);

    conn.execute(
        "UPDATE meta SET value = '999' WHERE key = ?1",
        rusqlite::params![META_KEY_SCHEMA_VERSION],
    )?;
    drop(conn);
    let err = store.list(None, None).unwrap_err();
    let store_err = err
        .downcast_ref::<StoreError>()
        .expect("should produce StoreError");
    assert!(
        matches!(
            store_err,
            StoreError::IncompatibleFormat { key, .. }
            if key == META_KEY_SCHEMA_VERSION
        ),
        "schema mismatch should be surfaced"
    );
    Ok(())
}

#[test]
fn stores_and_validates_integrity() -> Result<()> {
    let (_temp, store) = new_store()?;
    let payload = demo_source_payload();
    let stored = store.store(&payload)?;
    assert!(stored.path.exists());

    let loaded = store.load(&stored.oid)?;
    match loaded {
        LoadedObject::Source { bytes, header, .. } => {
            assert_eq!(bytes, b"demo-payload");
            assert_eq!(header.name, "demo");
        }
        _ => bail!("expected source object"),
    }

    let info = store.object_info(&stored.oid)?.expect("metadata present");
    let previous_access = info.last_accessed;
    thread::sleep(Duration::from_millis(5));
    let _ = store.load(&stored.oid)?;
    let updated = store.object_info(&stored.oid)?.expect("metadata present");
    assert!(
        updated.last_accessed >= previous_access,
        "last_accessed should advance after reads"
    );

    make_writable(&stored.path);
    fs::write(&stored.path, b"corrupt")?;
    let err = store.load(&stored.oid).unwrap_err();
    let store_err = err
        .downcast_ref::<StoreError>()
        .expect("should produce StoreError");
    assert!(
        matches!(store_err, StoreError::DigestMismatch { .. }),
        "expected digest mismatch when data is corrupted"
    );
    Ok(())
}

#[test]
fn archive_dir_canonical_is_deterministic() -> Result<()> {
    let temp = tempdir()?;
    let root = temp.path().join("tree");
    fs::create_dir_all(&root)?;
    fs::write(root.join("a.txt"), b"hello")?;

    let first = archive_dir_canonical(&root)?;
    thread::sleep(Duration::from_secs(1));
    let second = archive_dir_canonical(&root)?;

    assert_eq!(
        first, second,
        "canonical archive should be stable across runs"
    );
    Ok(())
}

#[test]
fn archive_selected_is_deterministic() -> Result<()> {
    let temp = tempdir()?;
    let root = temp.path().join("tree");
    let nested = root.join("nested");
    fs::create_dir_all(&nested)?;
    fs::write(root.join("a.txt"), b"hello")?;
    fs::write(nested.join("b.txt"), b"there")?;

    let selection = vec![root.join("a.txt"), nested.clone()];
    let first = archive_selected(&root, &selection)?;
    thread::sleep(Duration::from_secs(1));
    let second = archive_selected(&root, &selection)?;

    assert_eq!(
        first, second,
        "selected archive should be stable across runs"
    );
    Ok(())
}

#[test]
fn repo_snapshot_oid_changes_with_commit() -> Result<()> {
    if !git_available() {
        eprintln!("skipping repo_snapshot_oid_changes_with_commit (git missing)");
        return Ok(());
    }

    let (_temp, store) = new_store()?;
    let repo_tmp = tempdir()?;
    let repo_root = repo_tmp.path().join("repo");
    fs::create_dir_all(&repo_root)?;
    init_git_repo(&repo_root)?;

    fs::write(repo_root.join("data.txt"), b"one")?;
    git_ok(&repo_root, &["add", "."])?;
    git_ok(&repo_root, &["commit", "-m", "c1"])?;
    let commit1 = git(&repo_root, &["rev-parse", "HEAD"])?;

    fs::write(repo_root.join("data.txt"), b"two")?;
    git_ok(&repo_root, &["add", "."])?;
    git_ok(&repo_root, &["commit", "-m", "c2"])?;
    let commit2 = git(&repo_root, &["rev-parse", "HEAD"])?;

    let locator = format!(
        "git+{}",
        Url::from_file_path(repo_root.canonicalize()?)
            .expect("file url")
            .as_str()
    );

    let spec1 = RepoSnapshotSpec::parse(&format!("{locator}@{commit1}"))?;
    let spec2 = RepoSnapshotSpec::parse(&format!("{locator}@{commit2}"))?;
    let oid1 = store.ensure_repo_snapshot(&spec1)?;
    let oid2 = store.ensure_repo_snapshot(&spec2)?;
    assert_ne!(oid1, oid2, "oid should change when commit changes");
    Ok(())
}

#[test]
fn repo_snapshot_ensure_is_stable_and_cached() -> Result<()> {
    if !git_available() {
        eprintln!("skipping repo_snapshot_ensure_is_stable_and_cached (git missing)");
        return Ok(());
    }

    let (_temp, store) = new_store()?;
    let repo_tmp = tempdir()?;
    let repo_root = repo_tmp.path().join("repo");
    fs::create_dir_all(&repo_root)?;
    init_git_repo(&repo_root)?;

    fs::write(repo_root.join("data.txt"), b"payload")?;
    git_ok(&repo_root, &["add", "."])?;
    git_ok(&repo_root, &["commit", "-m", "c1"])?;
    let commit = git(&repo_root, &["rev-parse", "HEAD"])?;

    let locator = format!(
        "git+{}",
        Url::from_file_path(repo_root.canonicalize()?)
            .expect("file url")
            .as_str()
    );
    let spec = RepoSnapshotSpec::parse(&format!("{locator}@{commit}"))?;

    let first = store.ensure_repo_snapshot(&spec)?;
    fs::remove_dir_all(&repo_root)?;
    let second = store.ensure_repo_snapshot(&spec)?;
    assert_eq!(first, second, "ensure should be a cache hit on rerun");
    Ok(())
}

#[test]
fn repo_snapshot_archive_metadata_is_normalized_and_excludes_git() -> Result<()> {
    if !git_available() {
        eprintln!(
            "skipping repo_snapshot_archive_metadata_is_normalized_and_excludes_git (git missing)"
        );
        return Ok(());
    }

    let (_temp, store) = new_store()?;
    let repo_tmp = tempdir()?;
    let repo_root = repo_tmp.path().join("repo");
    fs::create_dir_all(&repo_root)?;
    init_git_repo(&repo_root)?;

    fs::write(repo_root.join("data.txt"), b"payload")?;
    git_ok(&repo_root, &["add", "."])?;
    git_ok(&repo_root, &["commit", "-m", "c1"])?;
    let commit = git(&repo_root, &["rev-parse", "HEAD"])?;

    let locator = format!(
        "git+{}",
        Url::from_file_path(repo_root.canonicalize()?)
            .expect("file url")
            .as_str()
    );
    let spec = RepoSnapshotSpec::parse(&format!("{locator}@{commit}"))?;
    let oid = store.ensure_repo_snapshot(&spec)?;

    let loaded = store.load(&oid)?;
    let LoadedObject::RepoSnapshot { archive, .. } = loaded else {
        bail!("expected repo-snapshot object");
    };

    let decoder = flate2::read::GzDecoder::new(&archive[..]);
    let mut tar = tar::Archive::new(decoder);
    for entry in tar.entries()? {
        let entry = entry?;
        let path = entry.path()?.into_owned();
        assert!(
            !path.components().any(|c| c.as_os_str() == ".git"),
            "snapshot should exclude .git (saw {})",
            path.display()
        );
        let header = entry.header();
        assert_eq!(header.mtime()?, 0, "mtime should be normalized");
        assert_eq!(header.uid()?, 0, "uid should be normalized");
        assert_eq!(header.gid()?, 0, "gid should be normalized");
        if let Ok(Some(name)) = header.username() {
            assert!(name.is_empty(), "uname should be normalized");
        }
        if let Ok(Some(name)) = header.groupname() {
            assert!(name.is_empty(), "gname should be normalized");
        }
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn repo_snapshot_preserves_symlinks_and_exec_bits() -> Result<()> {
    if !git_available() {
        eprintln!("skipping repo_snapshot_preserves_symlinks_and_exec_bits (git missing)");
        return Ok(());
    }

    let (_temp, store) = new_store()?;
    let repo_tmp = tempdir()?;
    let repo_root = repo_tmp.path().join("repo");
    fs::create_dir_all(&repo_root)?;
    init_git_repo(&repo_root)?;

    let bin_dir = repo_root.join("bin");
    fs::create_dir_all(&bin_dir)?;
    let exe_path = bin_dir.join("run.sh");
    fs::write(&exe_path, b"#!/bin/sh\necho hi\n")?;
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&exe_path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&exe_path, perms)?;
    }
    symlink("bin/run.sh", repo_root.join("run-link"))?;
    git_ok(&repo_root, &["add", "."])?;
    git_ok(&repo_root, &["commit", "-m", "c1"])?;
    let commit = git(&repo_root, &["rev-parse", "HEAD"])?;

    let locator = format!(
        "git+{}",
        Url::from_file_path(repo_root.canonicalize()?)
            .expect("file url")
            .as_str()
    );
    let spec = RepoSnapshotSpec::parse(&format!("{locator}@{commit}"))?;
    let oid = store.ensure_repo_snapshot(&spec)?;

    let loaded = store.load(&oid)?;
    let LoadedObject::RepoSnapshot { archive, .. } = loaded else {
        bail!("expected repo-snapshot object");
    };

    let decoder = GzDecoder::new(&archive[..]);
    let mut tar = tar::Archive::new(decoder);
    let mut saw_exec = false;
    let mut saw_link = false;
    for entry in tar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        if path == std::path::Path::new("bin/run.sh") {
            saw_exec = true;
            assert!(entry.header().entry_type().is_file());
            assert_eq!(entry.header().mode()? & 0o777, 0o755);
        }
        if path == std::path::Path::new("run-link") {
            saw_link = true;
            assert!(entry.header().entry_type().is_symlink());
            let target = entry.link_name()?.expect("symlink target").into_owned();
            assert_eq!(target, std::path::Path::new("bin/run.sh"));
            let mut body = String::new();
            entry.read_to_string(&mut body)?;
            assert!(body.is_empty(), "symlink should not carry bytes");
        }
    }
    assert!(saw_exec, "expected executable file in snapshot");
    assert!(saw_link, "expected symlink in snapshot");
    Ok(())
}

#[test]
fn repo_snapshot_offline_fails_for_remote_when_missing() -> Result<()> {
    let (_temp, store) = new_store()?;
    let _offline = EnvVarGuard::set("PX_ONLINE", "0");

    let spec = RepoSnapshotSpec {
        locator: "git+https://example.invalid/repo".to_string(),
        commit: "0123456789abcdef0123456789abcdef01234567".to_string(),
        subdir: None,
    };
    let err = store.ensure_repo_snapshot(&spec).unwrap_err();
    let user = err
        .downcast_ref::<crate::InstallUserError>()
        .expect("expected InstallUserError");
    assert_eq!(
        user.details().get("code").and_then(Value::as_str),
        Some("PX721")
    );
    assert_eq!(
        user.details().get("reason").and_then(Value::as_str),
        Some("repo_snapshot_offline")
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn repo_snapshot_file_locator_normalization_is_fs_independent() -> Result<()> {
    use std::os::unix::fs::symlink;

    let (_temp, store) = new_store()?;
    let temp = tempdir()?;
    let real = temp.path().join("real");
    fs::create_dir_all(&real)?;
    let link = temp.path().join("link");
    if let Err(err) = symlink(&real, &link) {
        eprintln!("skipping repo_snapshot_file_locator_normalization_is_fs_independent ({err})");
        return Ok(());
    }

    let url = Url::from_file_path(&link).expect("file url");
    let locator = format!("git+{}", url);
    let spec = RepoSnapshotSpec {
        locator,
        commit: "0123456789abcdef0123456789abcdef01234567".to_string(),
        subdir: None,
    };
    let header1 = store.resolve_repo_snapshot_header(&spec)?;

    fs::remove_dir_all(&real)?;
    let header2 = store.resolve_repo_snapshot_header(&spec)?;

    assert_eq!(
        header1.locator, header2.locator,
        "canonical file locator must not depend on filesystem resolution"
    );
    Ok(())
}

#[test]
fn repo_snapshot_locator_with_credentials_is_rejected_and_redacted() -> Result<()> {
    let (_temp, store) = new_store()?;
    let spec = RepoSnapshotSpec {
        locator: "git+https://user:supersecret@example.invalid/repo.git".to_string(),
        commit: "0123456789abcdef0123456789abcdef01234567".to_string(),
        subdir: None,
    };
    let err = store.ensure_repo_snapshot(&spec).unwrap_err();
    let user = err
        .downcast_ref::<crate::InstallUserError>()
        .expect("expected InstallUserError");
    assert_eq!(
        user.details().get("code").and_then(Value::as_str),
        Some("PX720")
    );
    assert_eq!(
        user.details().get("reason").and_then(Value::as_str),
        Some("invalid_repo_snapshot_locator")
    );
    let locator = user
        .details()
        .get("locator")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        !locator.contains("supersecret"),
        "error details must not leak credentials"
    );
    assert!(
        !user.message().contains("supersecret"),
        "error message must not leak credentials"
    );
    Ok(())
}

#[test]
fn repo_snapshot_file_locator_normalizes_dot_segments() -> Result<()> {
    let (_temp, store) = new_store()?;
    let temp = tempdir()?;
    let root = temp.path().join("repo");
    fs::create_dir_all(&root)?;

    let dotted = temp.path().join("a").join("..").join("repo");
    let url = Url::from_file_path(&dotted).expect("file url");
    let spec = RepoSnapshotSpec {
        locator: format!("git+{}", url),
        commit: "0123456789abcdef0123456789abcdef01234567".to_string(),
        subdir: None,
    };
    let header = store.resolve_repo_snapshot_header(&spec)?;
    let expected = format!(
        "git+{}",
        Url::from_file_path(&root).expect("file url").as_str()
    );
    assert_eq!(header.locator, expected);
    Ok(())
}

#[test]
fn archive_dir_canonical_excludes_dot_git_dir() -> Result<()> {
    use flate2::read::GzDecoder;

    let temp = tempdir()?;
    let root = temp.path().join("tree");
    fs::create_dir_all(root.join(".git"))?;
    fs::write(root.join(".git").join("config"), "unsafe")?;
    fs::write(root.join("a.txt"), "hello")?;

    let archive = archive_dir_canonical(&root)?;
    let decoder = GzDecoder::new(&archive[..]);
    let mut tar = tar::Archive::new(decoder);
    for entry in tar.entries()? {
        let entry = entry?;
        let path = entry.path()?.into_owned();
        assert!(
            !path.components().any(|c| c.as_os_str() == ".git"),
            "archive should exclude .git (saw {})",
            path.display()
        );
    }
    Ok(())
}

#[test]
fn repo_snapshot_parse_reports_invalid_spec() -> Result<()> {
    let err = RepoSnapshotSpec::parse("git+file:///tmp/repo").unwrap_err();
    let user = err
        .downcast_ref::<crate::InstallUserError>()
        .expect("expected InstallUserError");
    assert_eq!(
        user.details().get("code").and_then(Value::as_str),
        Some("PX720")
    );
    assert_eq!(
        user.details().get("reason").and_then(Value::as_str),
        Some("invalid_repo_snapshot_spec")
    );
    Ok(())
}

#[test]
fn repo_snapshot_invalid_commit_is_user_error() -> Result<()> {
    let (_temp, store) = new_store()?;
    let spec = RepoSnapshotSpec {
        locator: "git+https://example.invalid/repo".to_string(),
        commit: "not-a-sha".to_string(),
        subdir: None,
    };
    let err = store.ensure_repo_snapshot(&spec).unwrap_err();
    let user = err
        .downcast_ref::<crate::InstallUserError>()
        .expect("expected InstallUserError");
    assert_eq!(
        user.details().get("code").and_then(Value::as_str),
        Some("PX720")
    );
    assert_eq!(
        user.details().get("reason").and_then(Value::as_str),
        Some("invalid_repo_snapshot_commit")
    );
    Ok(())
}

#[test]
fn repo_snapshot_cache_hit_does_not_require_online_or_git() -> Result<()> {
    let (_temp, store) = new_store()?;
    let temp = tempdir()?;
    let tree = temp.path().join("tree");
    fs::create_dir_all(&tree)?;
    fs::write(tree.join("data.txt"), b"payload")?;
    let archive = archive_dir_canonical(&tree)?;

    let header = RepoSnapshotHeader {
        locator: "git+https://example.invalid/repo".to_string(),
        commit: "0123456789abcdef0123456789abcdef01234567".to_string(),
        subdir: None,
    };
    let payload = ObjectPayload::RepoSnapshot {
        header: header.clone(),
        archive: Cow::Owned(archive),
    };
    let stored = store.store(&payload)?;
    store.record_key(
        ObjectKind::RepoSnapshot,
        &repo_snapshot_lookup_key(&header),
        &stored.oid,
    )?;

    let _offline = EnvVarGuard::set("PX_ONLINE", "0");
    let _no_git = EnvVarGuard::set("PATH", "");
    let spec = RepoSnapshotSpec {
        locator: header.locator.clone(),
        commit: header.commit.to_ascii_uppercase(),
        subdir: None,
    };
    let oid = store.ensure_repo_snapshot(&spec)?;
    assert_eq!(oid, stored.oid);
    Ok(())
}

#[test]
fn repo_snapshot_locator_normalization_is_stable() -> Result<()> {
    let (_temp, store) = new_store()?;
    let temp = tempdir()?;
    let repo_root = temp.path().join("repo");
    fs::create_dir_all(&repo_root)?;
    let locator = format!(
        "git+{}",
        Url::from_file_path(&repo_root).expect("file url").as_str()
    );
    let spec = RepoSnapshotSpec {
        locator,
        commit: "0123456789abcdef0123456789abcdef01234567".to_string(),
        subdir: None,
    };
    let first = store.resolve_repo_snapshot_header(&spec)?;
    let second = store.resolve_repo_snapshot_header(&spec)?;
    assert_eq!(first.locator, second.locator);
    Ok(())
}

#[test]
fn rebuild_restores_env_owner_refs_from_state() -> Result<()> {
    let _dir_guard = CURRENT_DIR_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap();
    let (temp, store) = new_store()?;
    let runtime_oid = "runtime-oid".to_string();
    let profile_payload = ObjectPayload::Profile {
        header: ProfileHeader {
            runtime_oid: runtime_oid.clone(),
            packages: Vec::new(),
            sys_path_order: Vec::new(),
            env_vars: BTreeMap::new(),
        },
    };
    let stored_profile = store.store(&profile_payload)?;
    let profile_oid = stored_profile.oid.clone();

    let project_root = temp.path().join("project");
    let px_dir = project_root.join(".px");
    fs::create_dir_all(&px_dir)?;
    let state_path = px_dir.join("state.json");
    let env_site = project_root.join("env-site");
    let state_body = json!({
        "current_env": {
            "id": "env",
            "lock_id": "lock-123",
            "platform": "linux",
            "site_packages": env_site.display().to_string(),
            "profile_oid": profile_oid,
            "python": { "path": "python", "version": "3.11.0" }
        },
        "runtime": {
            "path": "python",
            "version": "3.11.0",
            "platform": "linux"
        }
    });
    fs::write(&state_path, serde_json::to_string_pretty(&state_body)?)?;

    let index_path = store.root().join(INDEX_FILENAME);
    fs::remove_file(&index_path)?;

    let cwd = env::current_dir()?;
    env::set_current_dir(&project_root)?;
    let _ = store.list(None, None)?;
    env::set_current_dir(cwd)?;

    let refs = store.refs_for(&profile_oid)?;
    let expected_owner =
        owner_id_from_state(StateFileKind::Project, &project_root, "lock-123", "3.11.0")?;
    assert!(
        refs.iter()
            .any(|owner| owner.owner_type == OwnerType::ProjectEnv
                && owner.owner_id == expected_owner),
        "expected reconstructed project-env owner reference"
    );
    Ok(())
}

#[test]
fn rebuild_restores_tool_owner_refs_from_state() -> Result<()> {
    let (temp, store) = new_store()?;
    let tools_dir = temp.path().join("tools");
    std::env::set_var("PX_TOOLS_DIR", &tools_dir);

    let profile_payload = ObjectPayload::Profile {
        header: ProfileHeader {
            runtime_oid: "runtime-oid".to_string(),
            packages: Vec::new(),
            sys_path_order: Vec::new(),
            env_vars: BTreeMap::new(),
        },
    };
    let stored_profile = store.store(&profile_payload)?;
    let profile_oid = stored_profile.oid.clone();

    let tool_root = tools_dir.join("demo-tool");
    fs::create_dir_all(tool_root.join(".px"))?;
    let state_body = json!({
        "current_env": {
            "id": "env",
            "lock_id": "lock-abc",
            "site_packages": "/tmp/site",
            "profile_oid": profile_oid,
            "python": { "path": "python", "version": "3.11.0" }
        },
        "runtime": {
            "path": "python",
            "version": "3.11.0",
            "platform": "linux"
        }
    });
    fs::write(
        tool_root.join(".px").join("state.json"),
        serde_json::to_string_pretty(&state_body)?,
    )?;

    let index_path = store.root().join(INDEX_FILENAME);
    fs::remove_file(&index_path)?;

    let _ = store.list(None, None)?;
    let refs = store.refs_for(&profile_oid)?;
    let expected_owner = tool_owner_id("demo-tool", "lock-abc", "3.11.0")?;
    assert!(
        refs.iter()
            .any(|owner| owner.owner_type == OwnerType::ToolEnv
                && owner.owner_id == expected_owner),
        "expected reconstructed tool-env owner reference"
    );
    Ok(())
}

#[test]
fn rebuild_restores_runtime_owner_refs_from_manifest() -> Result<()> {
    let (_temp, store) = new_store()?;
    let runtime_payload = ObjectPayload::Runtime {
        header: RuntimeHeader {
            version: "3.11.0".to_string(),
            abi: "cp311".to_string(),
            platform: "linux".to_string(),
            build_config_hash: "abc".to_string(),
            exe_path: "bin/python".to_string(),
        },
        archive: Cow::Owned(b"runtime".to_vec()),
    };
    let runtime = store.store(&runtime_payload)?;
    let manifest_dir = store
        .root()
        .join(MATERIALIZED_RUNTIMES_DIR)
        .join(&runtime.oid);
    fs::create_dir_all(&manifest_dir)?;
    let manifest_path = manifest_dir.join("manifest.json");
    fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&json!({
            "runtime_oid": runtime.oid,
            "version": "3.11.0",
            "platform": "linux",
            "owner_id": "runtime:3.11.0:linux"
        }))?,
    )?;

    let index_path = store.root().join(INDEX_FILENAME);
    fs::remove_file(&index_path)?;

    let _ = store.list(None, None)?;
    let refs = store.refs_for(&runtime.oid)?;
    let expected_owner = "runtime:3.11.0:linux".to_string();
    assert!(
        refs.iter()
            .any(|owner| owner.owner_type == OwnerType::Runtime
                && owner.owner_id == expected_owner),
        "expected reconstructed runtime owner reference"
    );
    Ok(())
}

#[test]
fn remove_env_materialization_cleans_all_variants() -> Result<()> {
    let (_temp, store) = new_store()?;
    let env_root = store.envs_root().join("profile-123");
    fs::create_dir_all(&env_root)?;
    fs::create_dir_all(env_root.with_extension("partial"))?;
    fs::create_dir_all(env_root.with_extension("backup"))?;

    store.remove_env_materialization("profile-123")?;

    assert!(
        !env_root.exists(),
        "env materialization should be removed when requested"
    );
    assert!(
        !env_root.with_extension("partial").exists(),
        "partial env materialization should be removed"
    );
    assert!(
        !env_root.with_extension("backup").exists(),
        "backup env materialization should be removed"
    );
    Ok(())
}

#[test]
fn load_repairs_missing_index_entry_from_disk() -> Result<()> {
    let (_temp, store) = new_store()?;
    let stored = store.store(&demo_source_payload())?;

    let conn = store.connection()?;
    conn.execute("DELETE FROM objects WHERE oid = ?1", params![stored.oid])?;

    let loaded = store.load(&stored.oid)?;
    assert!(
        matches!(loaded, LoadedObject::Source { .. }),
        "load should succeed even when index entry is missing"
    );
    let repaired = store.object_info(&stored.oid)?;
    assert!(
        repaired.is_some(),
        "index metadata should be recreated during load"
    );
    Ok(())
}

#[test]
fn add_ref_repairs_missing_index_entry() -> Result<()> {
    let (_temp, store) = new_store()?;
    let stored = store.store(&demo_source_payload())?;

    let conn = store.connection()?;
    conn.execute("DELETE FROM objects WHERE oid = ?1", params![stored.oid])?;

    let owner = OwnerId {
        owner_type: OwnerType::Profile,
        owner_id: "profile:demo".to_string(),
    };
    store.add_ref(&owner, &stored.oid)?;

    let owners = store.refs_for(&stored.oid)?;
    assert!(
        owners
            .iter()
            .any(|o| o.owner_id == owner.owner_id && o.owner_type == owner.owner_type),
        "reference should be recorded after repairing index"
    );
    let repaired = store.object_info(&stored.oid)?;
    assert!(repaired.is_some(), "object info should be restored");
    Ok(())
}

#[test]
fn runtime_manifest_recreated_on_load() -> Result<()> {
    let (_temp, store) = new_store()?;
    let runtime_payload = ObjectPayload::Runtime {
        header: RuntimeHeader {
            version: "3.11.0".to_string(),
            abi: "cp311".to_string(),
            platform: "linux".to_string(),
            build_config_hash: "abc".to_string(),
            exe_path: "bin/python".to_string(),
        },
        archive: Cow::Owned(b"runtime".to_vec()),
    };
    let runtime = store.store(&runtime_payload)?;
    let manifest_path = store
        .root()
        .join(MATERIALIZED_RUNTIMES_DIR)
        .join(&runtime.oid)
        .join("manifest.json");
    assert!(
        manifest_path.exists(),
        "runtime manifest should be written during store"
    );
    fs::remove_file(&manifest_path)?;

    let _ = store.load(&runtime.oid)?;
    assert!(
        manifest_path.exists(),
        "loading a runtime should recreate its manifest for reconstruction"
    );
    Ok(())
}

#[test]
fn objects_are_made_read_only() -> Result<()> {
    let (_temp, store) = new_store()?;
    let stored = store.store(&demo_source_payload())?;
    let metadata = fs::metadata(&stored.path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            metadata.permissions().mode() & 0o222,
            0,
            "write bits should be stripped from CAS objects"
        );
    }
    let write_attempt = OpenOptions::new().write(true).open(&stored.path);
    assert!(
        write_attempt.is_err(),
        "objects should not be writable after creation"
    );
    Ok(())
}

#[test]
fn permission_health_check_hardens_existing_objects() -> Result<()> {
    let (temp, store) = new_store()?;
    let stored = store.store(&demo_source_payload())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&stored.path)?.permissions();
        perms.set_mode(perms.mode() | 0o200);
        fs::set_permissions(&stored.path, perms)?;
    }
    #[cfg(not(unix))]
    {
        let mut perms = fs::metadata(&stored.path)?.permissions();
        perms.set_readonly(false);
        fs::set_permissions(&stored.path, perms)?;
    }

    let root = store.root().to_path_buf();
    let _rehydrated = ContentAddressableStore::new(Some(root))?;
    let metadata = fs::metadata(&stored.path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            metadata.permissions().mode() & 0o222,
            0,
            "health check should strip write bits"
        );
    }
    let write_attempt = OpenOptions::new().write(true).open(&stored.path);
    assert!(
        write_attempt.is_err(),
        "objects should be hardened against writes after health check"
    );
    drop(temp);
    Ok(())
}

#[test]
fn references_block_gc_until_removed() -> Result<()> {
    let (_temp, store) = new_store()?;
    let stored = store.store(&ObjectPayload::Profile {
        header: ProfileHeader {
            runtime_oid: "runtime-oid".to_string(),
            packages: vec![],
            sys_path_order: vec![],
            env_vars: BTreeMap::new(),
        },
    })?;
    let owner = OwnerId {
        owner_type: OwnerType::ProjectEnv,
        owner_id: "proj-123".to_string(),
    };
    store.add_ref(&owner, &stored.oid)?;

    let summary = store.garbage_collect(Duration::from_secs(0))?;
    assert_eq!(summary.reclaimed, 0, "live reference should prevent GC");
    assert!(stored.path.exists());

    assert!(store.remove_ref(&owner, &stored.oid)?);
    let summary = store.garbage_collect(Duration::from_secs(0))?;
    assert_eq!(summary.reclaimed, 1, "object should be reclaimed");
    assert!(!stored.path.exists());
    Ok(())
}

#[test]
fn stale_partials_are_cleaned_on_store() -> Result<()> {
    let (_temp, store) = new_store()?;
    let stored = store.store(&ObjectPayload::Meta {
        bytes: Cow::Owned(b"meta".to_vec()),
    })?;
    let tmp = store.tmp_path(&stored.oid);
    if let Some(parent) = tmp.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&tmp, b"junk")?;

    let again = store.store(&ObjectPayload::Meta {
        bytes: Cow::Owned(b"meta".to_vec()),
    })?;
    assert_eq!(again.oid, stored.oid);
    assert!(
        !tmp.exists(),
        "partial file should be removed after storing object"
    );
    let loaded = store.load(&stored.oid)?;
    match loaded {
        LoadedObject::Meta { bytes, .. } => assert_eq!(bytes, b"meta"),
        other => bail!("unexpected object {other:?}"),
    }
    Ok(())
}

#[test]
fn refs_are_deduplicated() -> Result<()> {
    let (_temp, store) = new_store()?;
    let stored = store.store(&ObjectPayload::Runtime {
        header: RuntimeHeader {
            version: "3.11.0".to_string(),
            abi: "cp311".to_string(),
            platform: "x86_64-manylinux".to_string(),
            build_config_hash: "abc".to_string(),
            exe_path: "bin/python".to_string(),
        },
        archive: Cow::Owned(b"runtime".to_vec()),
    })?;
    let owner = OwnerId {
        owner_type: OwnerType::Runtime,
        owner_id: "python-3.11.0".to_string(),
    };
    store.add_ref(&owner, &stored.oid)?;
    store.add_ref(&owner, &stored.oid)?;
    let refs = store.refs_for(&stored.oid)?;
    assert_eq!(refs.len(), 1, "refs should be deduplicated");
    Ok(())
}

#[test]
fn owner_refs_can_be_pruned_for_orphan_profiles() -> Result<()> {
    let (_temp, store) = new_store()?;
    let profile = store.store(&ObjectPayload::Profile {
        header: ProfileHeader {
            runtime_oid: "runtime-oid".to_string(),
            packages: vec![],
            sys_path_order: vec![],
            env_vars: BTreeMap::new(),
        },
    })?;
    let pkg = store.store(&ObjectPayload::Meta {
        bytes: Cow::Owned(b"pkg".to_vec()),
    })?;

    let profile_owner = OwnerId {
        owner_type: OwnerType::Profile,
        owner_id: profile.oid.clone(),
    };
    let env_owner = OwnerId {
        owner_type: OwnerType::ProjectEnv,
        owner_id: "proj-123".to_string(),
    };
    store.add_ref(&profile_owner, &pkg.oid)?;
    store.add_ref(&env_owner, &profile.oid)?;

    // Simulate staleness so GC can sweep immediately.
    let conn = store.connection()?;
    set_last_accessed(&conn, &profile.oid, 0)?;
    set_last_accessed(&conn, &pkg.oid, 0)?;

    assert!(store.remove_ref(&env_owner, &profile.oid)?);
    assert!(store.refs_for(&profile.oid)?.is_empty());
    let removed = store.remove_owner_refs(&profile_owner)?;
    assert_eq!(removed, 1, "profile-owned refs should be dropped");

    let summary = store.garbage_collect(Duration::from_secs(0))?;
    assert_eq!(summary.reclaimed, 2, "profile and pkg should be reclaimed");
    assert!(!store.object_path(&profile.oid).exists());
    assert!(!store.object_path(&pkg.oid).exists());
    Ok(())
}

#[test]
fn concurrent_store_is_safe() -> Result<()> {
    let (_temp, store) = new_store()?;
    let payload = demo_source_payload();
    let store = Arc::new(store);
    let mut handles = Vec::new();
    for _ in 0..4 {
        let store = Arc::clone(&store);
        let payload = payload.clone();
        handles.push(std::thread::spawn(move || store.store(&payload)));
    }
    let mut oids = Vec::new();
    for handle in handles {
        let stored = handle.join().expect("thread join")?;
        oids.push(stored.oid);
    }
    oids.dedup();
    assert_eq!(oids.len(), 1, "concurrent stores should deduplicate");
    Ok(())
}

#[test]
fn verify_sample_reports_corruption() -> Result<()> {
    let (_temp, store) = new_store()?;
    let payload = demo_source_payload();
    let stored = store.store(&payload)?;
    make_writable(&stored.path);
    fs::write(&stored.path, b"bad")?;
    let failures = store.verify_sample(1)?;
    assert_eq!(failures.len(), 1, "corruption should be detected");
    Ok(())
}

#[test]
fn canonical_archives_are_stable() -> Result<()> {
    let temp = tempdir()?;
    let dir_a = temp.path().join("a");
    let dir_b = temp.path().join("b");
    fs::create_dir_all(&dir_a)?;
    fs::create_dir_all(&dir_b)?;
    fs::write(dir_a.join("file.txt"), b"hello")?;
    fs::write(dir_b.join("file.txt"), b"hello")?;
    filetime::set_file_mtime(
        dir_a.join("file.txt"),
        filetime::FileTime::from_unix_time(100, 0),
    )?;
    filetime::set_file_mtime(
        dir_b.join("file.txt"),
        filetime::FileTime::from_unix_time(500, 0),
    )?;

    let archive_a = archive_dir_canonical(&dir_a)?;
    let archive_b = archive_dir_canonical(&dir_b)?;
    assert_eq!(
        archive_a, archive_b,
        "canonical archives should ignore mtimes"
    );

    let payload_a = ObjectPayload::PkgBuild {
        header: PkgBuildHeader {
            source_oid: "src".into(),
            runtime_abi: "abi".into(),
            builder_id: "builder".into(),
            build_options_hash: "opts".into(),
        },
        archive: Cow::Owned(archive_a),
    };
    let payload_b = ObjectPayload::PkgBuild {
        header: PkgBuildHeader {
            source_oid: "src".into(),
            runtime_abi: "abi".into(),
            builder_id: "builder".into(),
            build_options_hash: "opts".into(),
        },
        archive: Cow::Owned(archive_b),
    };
    assert_eq!(
        ContentAddressableStore::compute_oid(&payload_a)?,
        ContentAddressableStore::compute_oid(&payload_b)?,
        "canonical encodings should produce the same oid"
    );
    Ok(())
}

#[test]
fn canonical_encoding_sorts_keys() -> Result<()> {
    let bytes = canonical_bytes(&demo_source_payload())?;
    let value: Value = serde_json::from_slice(&bytes)?;
    let object = value
        .as_object()
        .expect("canonical payload should be an object");
    let keys: Vec<_> = object.keys().cloned().collect();
    assert_eq!(
        keys,
        vec![
            "header".to_string(),
            "kind".to_string(),
            "payload".to_string(),
            "payload_kind".to_string()
        ],
        "top-level keys should be lexicographically ordered"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn canonical_archives_capture_symlinks() -> Result<()> {
    let temp = tempdir()?;
    let root = temp.path().join("root");
    fs::create_dir_all(&root)?;
    let target = root.join("data.txt");
    fs::write(&target, b"payload")?;
    let link = root.join("alias.txt");
    symlink(&target, &link)?;

    let archive = archive_dir_canonical(&root)?;
    let decoder = GzDecoder::new(&archive[..]);
    let mut archive = tar::Archive::new(decoder);
    let mut seen = 0;
    for entry in archive.entries()? {
        let mut entry = entry?;
        if entry.path()? == Path::new("alias.txt") {
            seen += 1;
            assert!(entry.header().entry_type().is_symlink());
            let target_path = entry
                .link_name()?
                .expect("symlink should have a target")
                .into_owned();
            assert_eq!(target_path, Path::new("data.txt"));
            let mut body = String::new();
            let _ = entry.read_to_string(&mut body)?;
            assert!(body.is_empty(), "symlink should not carry file bytes");
        }
    }
    assert_eq!(seen, 1, "symlink entry should be captured once");
    Ok(())
}

#[test]
fn gc_respects_size_limit() -> Result<()> {
    let (_temp, store) = new_store()?;
    let small = store.store(&ObjectPayload::Meta {
        bytes: Cow::Owned(vec![0u8; 8]),
    })?;
    let big = store.store(&ObjectPayload::Meta {
        bytes: Cow::Owned(vec![0u8; 1024]),
    })?;
    let small_size = store.object_info(&small.oid)?.unwrap().size;
    let limit = small_size + 1;

    // Make big very old and small in the future to preserve it.
    let conn = store.connection()?;
    let now = timestamp_secs() as i64;
    set_last_accessed(&conn, &big.oid, 0)?;
    set_last_accessed(&conn, &small.oid, now + 1_000)?;
    set_created_at(&conn, &big.oid, 0)?;
    set_created_at(&conn, &small.oid, now + 1_000)?;

    let summary = store.garbage_collect_with_limit(Duration::from_secs(0), limit)?;
    assert_eq!(
        summary.reclaimed, 1,
        "one object should be reclaimed respecting size ordering"
    );
    assert!(
        !store.object_path(&big.oid).exists(),
        "largest, oldest object should be reclaimed"
    );
    assert!(
        store.object_path(&small.oid).exists(),
        "small object should remain under limit"
    );
    Ok(())
}

#[test]
fn gc_size_limit_respects_grace_window() -> Result<()> {
    let (_temp, store) = new_store()?;
    let old = store.store(&ObjectPayload::Meta {
        bytes: Cow::Owned(vec![1u8; 16]),
    })?;
    let fresh = store.store(&ObjectPayload::Meta {
        bytes: Cow::Owned(vec![2u8; 32]),
    })?;

    let conn = store.connection()?;
    set_last_accessed(&conn, &old.oid, 0)?;
    set_created_at(&conn, &old.oid, 0)?;

    let limit = 1; // force pressure well below the remaining object size
    let summary = store.garbage_collect_with_limit(Duration::from_secs(120), limit)?;
    assert!(
        summary.reclaimed >= 1,
        "stale objects should still be reclaimed under size pressure"
    );
    assert!(
        !store.object_path(&old.oid).exists(),
        "old object should be collected"
    );
    assert!(
        store.object_path(&fresh.oid).exists(),
        "recent object should be protected by grace period even if size cap unmet"
    );
    Ok(())
}

#[test]
fn gc_removes_orphaned_on_disk_objects() -> Result<()> {
    let (_temp, store) = new_store()?;
    let stored = store.store(&demo_source_payload())?;
    let path = store.object_path(&stored.oid);
    assert!(path.exists(), "object should exist on disk");

    let conn = store.connection()?;
    conn.execute(
        "DELETE FROM objects WHERE oid = ?1",
        rusqlite::params![&stored.oid],
    )?;
    drop(conn);

    let summary = store.garbage_collect(Duration::from_secs(0))?;
    assert!(
        summary.reclaimed >= 1,
        "orphaned object should be reclaimed"
    );
    assert!(!path.exists(), "orphaned file should be removed");
    Ok(())
}

#[test]
fn gc_removes_materialized_directories() -> Result<()> {
    let (_temp, store) = new_store()?;
    let stored = store.store(&ObjectPayload::Meta {
        bytes: Cow::Owned(b"meta".to_vec()),
    })?;

    let pkg_dir = store
        .root()
        .join(MATERIALIZED_PKG_BUILDS_DIR)
        .join(&stored.oid);
    fs::create_dir_all(&pkg_dir)?;
    fs::write(pkg_dir.join("payload"), b"pkg")?;
    let runtime_dir = store
        .root()
        .join(MATERIALIZED_RUNTIMES_DIR)
        .join(&stored.oid);
    fs::create_dir_all(&runtime_dir)?;
    fs::write(runtime_dir.join("python"), b"py")?;

    let conn = store.connection()?;
    set_last_accessed(&conn, &stored.oid, 0)?;

    let summary = store.garbage_collect(Duration::from_secs(0))?;
    assert_eq!(summary.reclaimed, 1, "object should be reclaimed");
    assert!(
        !pkg_dir.exists(),
        "pkg-build materialization should be removed with object"
    );
    assert!(
        !runtime_dir.exists(),
        "runtime materialization should be removed with object"
    );
    Ok(())
}

#[test]
fn gc_rebuilds_index_before_running() -> Result<()> {
    let (temp, store) = new_store()?;
    let runtime = store.store(&ObjectPayload::Runtime {
        header: RuntimeHeader {
            version: "3.11.0".to_string(),
            abi: "cp311".to_string(),
            platform: "x86_64-manylinux".to_string(),
            build_config_hash: "abc".to_string(),
            exe_path: "bin/python".to_string(),
        },
        archive: Cow::Owned(b"runtime".to_vec()),
    })?;
    let profile_header = ProfileHeader {
        runtime_oid: runtime.oid.clone(),
        packages: vec![],
        sys_path_order: vec![],
        env_vars: BTreeMap::new(),
    };
    let profile = store.store(&ObjectPayload::Profile {
        header: profile_header.clone(),
    })?;
    let garbage = store.store(&ObjectPayload::Meta {
        bytes: Cow::Owned(b"stale".to_vec()),
    })?;
    filetime::set_file_mtime(
        store.object_path(&garbage.oid),
        filetime::FileTime::from_unix_time(0, 0),
    )?;

    let env_root = temp.path().join("envs").join(&profile.oid);
    fs::create_dir_all(&env_root)?;
    fs::write(
        env_root.join("manifest.json"),
        serde_json::to_string_pretty(&json!({
            "profile_oid": profile.oid,
            "runtime_oid": runtime.oid,
            "packages": profile_header.packages,
        }))?,
    )?;

    fs::remove_file(store.root().join(INDEX_FILENAME))?;
    let summary = store.garbage_collect(Duration::from_secs(0))?;
    assert!(
        store.object_path(&profile.oid).exists() && store.object_path(&runtime.oid).exists(),
        "referenced objects should survive GC after index rebuild"
    );
    assert!(
        !store.object_path(&garbage.oid).exists(),
        "unreferenced objects should be reclaimed after rebuild"
    );
    let refs = store.refs_for(&profile.oid)?;
    assert!(
        refs.iter()
            .any(|owner| owner.owner_type == OwnerType::Profile && owner.owner_id == profile.oid),
        "profile refs should be reconstructed from manifests"
    );
    assert!(
        summary.reclaimed >= 1,
        "garbage should be collected after rebuild"
    );
    Ok(())
}

#[test]
fn lookup_keys_roundtrip_and_cleanup() -> Result<()> {
    let (_temp, store) = new_store()?;
    let header = SourceHeader {
        name: "demo".to_string(),
        version: "1.0.0".to_string(),
        filename: "demo-1.0.0.whl".to_string(),
        index_url: "https://example.invalid/simple/".to_string(),
        sha256: "deadbeef".to_string(),
    };
    let payload = ObjectPayload::Source {
        header: header.clone(),
        bytes: Cow::Owned(b"payload".to_vec()),
    };
    let stored = store.store(&payload)?;
    let key = source_lookup_key(&header);
    store.record_key(ObjectKind::Source, &key, &stored.oid)?;
    let found = store.lookup_key(ObjectKind::Source, &key)?;
    assert_eq!(found.as_deref(), Some(stored.oid.as_str()));

    fs::remove_file(&stored.path)?;
    let missing = store.lookup_key(ObjectKind::Source, &key)?;
    assert!(missing.is_none(), "stale mapping should be purged");
    Ok(())
}

#[test]
fn list_filters_by_kind_and_prefix() -> Result<()> {
    let (_temp, store) = new_store()?;
    let source = store.store(&demo_source_payload())?;
    let meta = store.store(&ObjectPayload::Meta {
        bytes: Cow::Owned(b"meta".to_vec()),
    })?;

    let all = store.list(None, None)?;
    assert_eq!(all.len(), 2);
    assert!(
        all.windows(2).all(|w| w[0] <= w[1]),
        "list should be sorted"
    );

    let sources = store.list(Some(ObjectKind::Source), None)?;
    assert_eq!(sources, vec![source.oid.clone()]);

    let prefix = &meta.oid[..4.min(meta.oid.len())];
    let prefixed = store.list(None, Some(prefix))?;
    assert!(
        prefixed.iter().all(|oid| oid.starts_with(prefix)),
        "prefix filter should be applied"
    );
    Ok(())
}

#[test]
fn profile_packages_are_canonicalized() -> Result<()> {
    let (_temp, store) = new_store()?;
    let pkg_a = ProfilePackage {
        name: "B".to_string(),
        version: "2.0.0".to_string(),
        pkg_build_oid: "pkg-b".to_string(),
    };
    let pkg_b = ProfilePackage {
        name: "A".to_string(),
        version: "1.0.0".to_string(),
        pkg_build_oid: "pkg-a".to_string(),
    };
    let header_unsorted = ProfileHeader {
        runtime_oid: "runtime".to_string(),
        packages: vec![pkg_a.clone(), pkg_b.clone()],
        sys_path_order: vec!["pkg-a".to_string(), "pkg-b".to_string()],
        env_vars: BTreeMap::new(),
    };
    let header_sorted = ProfileHeader {
        runtime_oid: "runtime".to_string(),
        packages: vec![pkg_b.clone(), pkg_a.clone()],
        sys_path_order: vec!["pkg-a".to_string(), "pkg-b".to_string()],
        env_vars: BTreeMap::new(),
    };

    let oid_unsorted = ContentAddressableStore::compute_oid(&ObjectPayload::Profile {
        header: header_unsorted.clone(),
    })?;
    let oid_sorted = ContentAddressableStore::compute_oid(&ObjectPayload::Profile {
        header: header_sorted.clone(),
    })?;
    assert_eq!(
        oid_unsorted, oid_sorted,
        "package order should be canonical"
    );

    let stored = store.store(&ObjectPayload::Profile {
        header: header_unsorted,
    })?;
    let loaded = store.load(&stored.oid)?;
    match loaded {
        LoadedObject::Profile { header, .. } => {
            let names: Vec<_> = header.packages.iter().map(|p| p.name.as_str()).collect();
            assert_eq!(
                names,
                vec!["A", "B"],
                "packages should be sorted canonically"
            );
        }
        other => bail!("unexpected object {other:?}"),
    }

    let stored_again = store.store(&ObjectPayload::Profile {
        header: header_sorted,
    })?;
    assert_eq!(
        stored.oid, stored_again.oid,
        "dedupe should respect canonical order"
    );
    Ok(())
}

#[test]
fn profile_env_vars_round_trip() -> Result<()> {
    let (_temp, store) = new_store()?;
    let mut env_vars = BTreeMap::new();
    env_vars.insert("FOO".to_string(), json!("bar"));
    env_vars.insert("COUNT".to_string(), json!(1));
    let header = ProfileHeader {
        runtime_oid: "runtime".to_string(),
        packages: vec![],
        sys_path_order: vec![],
        env_vars: env_vars.clone(),
    };
    let stored = store.store(&ObjectPayload::Profile { header })?;
    let loaded = store.load(&stored.oid)?;
    match loaded {
        LoadedObject::Profile { header, .. } => {
            assert_eq!(
                header.env_vars, env_vars,
                "env vars should persist in profile"
            );
        }
        other => bail!("unexpected object {other:?}"),
    }
    Ok(())
}

#[test]
fn reports_kind_mismatch_from_index() -> Result<()> {
    let (_temp, store) = new_store()?;
    let stored = store.store(&demo_source_payload())?;
    let conn = store.connection()?;
    conn.execute(
        "UPDATE objects SET kind = 'meta' WHERE oid = ?1",
        rusqlite::params![&stored.oid],
    )?;
    let err = store.load(&stored.oid).unwrap_err();
    let store_err = err
        .downcast_ref::<StoreError>()
        .expect("should produce StoreError");
    assert!(
        matches!(
            store_err,
            StoreError::KindMismatch {
                expected: ObjectKind::Meta,
                found: ObjectKind::Source,
                ..
            }
        ),
        "load should detect kind mismatch between index and object"
    );
    Ok(())
}

#[test]
fn reports_size_mismatch_from_index() -> Result<()> {
    let (_temp, store) = new_store()?;
    let payload = demo_source_payload();
    let stored = store.store(&payload)?;
    let conn = store.connection()?;
    conn.execute(
        "UPDATE objects SET size = size + 1 WHERE oid = ?1",
        rusqlite::params![&stored.oid],
    )?;
    let err = store.store(&payload).unwrap_err();
    let store_err = err
        .downcast_ref::<StoreError>()
        .expect("should produce StoreError");
    assert!(
        matches!(store_err, StoreError::SizeMismatch { .. }),
        "size mismatch should be surfaced during store"
    );
    Ok(())
}

#[test]
fn add_ref_to_missing_object_errors() -> Result<()> {
    let (_temp, store) = new_store()?;
    let owner = OwnerId {
        owner_type: OwnerType::ProjectEnv,
        owner_id: "proj".to_string(),
    };
    let err = store.add_ref(&owner, "deadbeef").unwrap_err();
    let store_err = err
        .downcast_ref::<StoreError>()
        .expect("should produce StoreError");
    assert!(matches!(store_err, StoreError::MissingObject { .. }));
    let refs = store.refs_for("deadbeef")?;
    assert!(
        refs.is_empty(),
        "no refs should be recorded for missing object"
    );
    Ok(())
}

#[test]
fn stores_large_payloads() -> Result<()> {
    let (_temp, store) = new_store()?;
    let big = vec![42u8; 2 * 1024 * 1024 + 123];
    let payload = ObjectPayload::Meta {
        bytes: Cow::Owned(big.clone()),
    };
    let stored = store.store(&payload)?;
    let loaded = store.load(&stored.oid)?;
    match loaded {
        LoadedObject::Meta { bytes, .. } => assert_eq!(bytes, big),
        other => bail!("unexpected object {other:?}"),
    }
    Ok(())
}

#[test]
fn doctor_removes_missing_objects_and_metadata() -> Result<()> {
    let (_temp, store) = new_store()?;
    let stored = store.store(&ObjectPayload::Meta {
        bytes: Cow::Owned(b"meta".to_vec()),
    })?;
    let owner = OwnerId {
        owner_type: OwnerType::ProjectEnv,
        owner_id: "proj".to_string(),
    };
    store.add_ref(&owner, &stored.oid)?;
    store.record_key(ObjectKind::Meta, "demo-key", &stored.oid)?;

    fs::remove_file(&stored.path)?;
    let summary = store.doctor()?;
    assert_eq!(
        summary.missing_objects, 1,
        "missing object should be flagged"
    );
    assert_eq!(
        summary.objects_removed, 1,
        "missing object should be purged"
    );
    assert!(
        store.object_info(&stored.oid)?.is_none(),
        "missing object should be removed from the index"
    );
    assert!(
        store.lookup_key(ObjectKind::Meta, "demo-key")?.is_none(),
        "stale lookup keys should be pruned"
    );
    assert!(
        store.refs_for(&stored.oid)?.is_empty(),
        "dangling refs should be cleaned"
    );
    Ok(())
}

#[test]
fn doctor_purges_corrupt_objects_and_materializations() -> Result<()> {
    let (_temp, store) = new_store()?;
    let stored = store.store(&ObjectPayload::Meta {
        bytes: Cow::Owned(b"meta".to_vec()),
    })?;
    let pkg_dir = store
        .root()
        .join(MATERIALIZED_PKG_BUILDS_DIR)
        .join(&stored.oid);
    fs::create_dir_all(&pkg_dir)?;
    fs::write(pkg_dir.join("payload"), b"pkg")?;
    make_writable(&stored.path);
    fs::write(&stored.path, b"corrupt")?;

    let summary = store.doctor()?;
    assert_eq!(
        summary.corrupt_objects, 1,
        "corrupt object should be reported"
    );
    assert!(
        !pkg_dir.exists(),
        "materialized directories should be removed for purged objects"
    );
    assert!(
        store.object_info(&stored.oid)?.is_none(),
        "corrupt object should be removed"
    );
    Ok(())
}

#[test]
fn doctor_removes_partials_and_counts_them() -> Result<()> {
    let (_temp, store) = new_store()?;
    let stored = store.store(&ObjectPayload::Meta {
        bytes: Cow::Owned(b"meta".to_vec()),
    })?;
    let tmp = store.tmp_path(&stored.oid);
    if let Some(parent) = tmp.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&tmp, b"junk")?;

    let summary = store.doctor()?;
    assert!(
        summary.partials_removed >= 1,
        "doctor should clean partials"
    );
    assert!(!tmp.exists(), "partial file should be removed");
    Ok(())
}

#[test]
fn doctor_skips_locked_objects_until_retried() -> Result<()> {
    let (_temp, store) = new_store()?;
    let stored = store.store(&ObjectPayload::Meta {
        bytes: Cow::Owned(b"meta".to_vec()),
    })?;
    // Simulate a missing object while holding the lock to force a skip.
    fs::remove_file(&stored.path)?;
    let lock = store.acquire_lock(&stored.oid)?;
    let summary = store.doctor()?;
    assert_eq!(
        summary.locked_skipped, 1,
        "doctor should skip locked objects"
    );
    assert!(
        store.object_info(&stored.oid)?.is_some(),
        "locked object should remain indexed"
    );
    drop(lock);
    let summary = store.doctor()?;
    assert_eq!(
        summary.missing_objects, 1,
        "doctor should clean missing object after lock release"
    );
    assert!(
        store.object_info(&stored.oid)?.is_none(),
        "missing object should be removed after retry"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn canonical_archives_rewrite_symlinks_outside_root() -> Result<()> {
    let temp = tempdir()?;
    let root = temp.path().join("root");
    fs::create_dir_all(&root)?;
    let outside = temp.path().join("outside.txt");
    fs::write(&outside, b"payload")?;
    let link = root.join("alias.txt");
    symlink(&outside, &link)?;

    let archive = archive_dir_canonical(&root)?;
    let decoder = GzDecoder::new(&archive[..]);
    let mut archive = tar::Archive::new(decoder);
    let mut seen = false;
    for entry in archive.entries()? {
        let entry = entry?;
        if entry.path()? == Path::new("alias.txt") {
            seen = true;
            let target = entry
                .link_name()?
                .expect("symlink should have target")
                .into_owned();
            assert_eq!(
                target,
                Path::new("outside.txt"),
                "absolute targets should be rewritten to a relative basename"
            );
        }
    }
    assert!(seen, "symlink entry should be captured");
    Ok(())
}

#[cfg(unix)]
#[test]
fn canonical_archives_rewrite_absolute_symlinks() -> Result<()> {
    let temp = tempdir()?;
    let root = temp.path().join("root");
    fs::create_dir_all(root.join("dir"))?;
    let target = root.join("dir").join("data.txt");
    fs::write(&target, b"payload")?;
    let abs_target = fs::canonicalize(&target)?;
    let link = root.join("alias.txt");
    symlink(&abs_target, &link)?;

    let archive = archive_dir_canonical(&root)?;
    let decoder = GzDecoder::new(&archive[..]);
    let mut archive = tar::Archive::new(decoder);
    let mut seen = 0;
    for entry in archive.entries()? {
        let entry = entry?;
        if entry.path()? == Path::new("alias.txt") {
            seen += 1;
            let target = entry
                .link_name()?
                .expect("symlink should have a target")
                .into_owned();
            assert_eq!(
                target,
                Path::new("dir").join("data.txt"),
                "absolute targets within root should be relativized"
            );
        }
    }
    assert_eq!(seen, 1, "symlink entry should be captured once");
    Ok(())
}
