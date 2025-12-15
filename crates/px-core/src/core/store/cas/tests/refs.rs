use super::*;

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
