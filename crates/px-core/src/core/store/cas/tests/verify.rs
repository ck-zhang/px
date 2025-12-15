use super::*;

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
