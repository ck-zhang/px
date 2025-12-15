use super::*;

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
