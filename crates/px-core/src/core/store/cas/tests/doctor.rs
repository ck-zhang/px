use super::*;

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
