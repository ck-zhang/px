use super::*;

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
        crate::core::runtime::default_envs_root()?,
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
fn archive_selected_filtered_is_deterministic() -> Result<()> {
    let temp = tempdir()?;
    let root = temp.path().join("tree");
    let nested = root.join("nested");
    fs::create_dir_all(&nested)?;
    fs::write(root.join("a.txt"), b"hello")?;
    fs::write(nested.join("b.txt"), b"there")?;

    let selection = vec![root.join("a.txt"), nested.clone()];
    let first = archive_selected_filtered(&root, &selection, |_| true)?;
    thread::sleep(Duration::from_secs(1));
    let second = archive_selected_filtered(&root, &selection, |_| true)?;

    assert_eq!(
        first, second,
        "selected archive should be stable across runs"
    );
    Ok(())
}
