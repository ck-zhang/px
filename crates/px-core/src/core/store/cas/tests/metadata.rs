use super::*;

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
