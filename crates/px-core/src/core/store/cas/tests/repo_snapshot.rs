use super::*;
use serial_test::serial;

#[test]
#[serial]
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
#[serial]
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
#[serial]
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
#[serial]
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
#[serial]
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
#[serial]
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
#[serial]
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
#[serial]
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
#[serial]
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
#[serial]
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
#[serial]
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
#[serial]
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
#[serial]
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
