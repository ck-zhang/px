use super::*;

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
