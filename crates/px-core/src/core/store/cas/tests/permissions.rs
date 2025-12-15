use super::*;

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
