use std::{borrow::Cow, collections::BTreeMap, env, fs, path::PathBuf};

#[cfg(unix)]
use std::process::Command;
#[cfg(unix)]
use std::sync::Arc;

use anyhow::Result;
use flate2::read::GzDecoder;
#[cfg(unix)]
use px_domain::api::LockSnapshot;
use px_domain::api::{DependencyGroupSource, PxOptions};
use serde_json::Value;
use tar::Archive;
use tempfile::tempdir;

use crate::core::runtime::facade::RuntimeMetadata;
#[cfg(unix)]
use crate::{api::GlobalOptions, api::SystemEffects, CommandContext};
use crate::store::cas::{
    archive_dir_canonical, global_store, ObjectPayload, PkgBuildHeader, ProfileHeader,
    ProfilePackage, RuntimeHeader, MATERIALIZED_PKG_BUILDS_DIR,
};
use crate::ManifestSnapshot;

#[cfg(not(windows))]
use super::write_python_shim;
use super::{materialize, profile, runtime};

#[test]
fn runtime_archive_captures_full_tree() -> Result<()> {
    let temp = tempdir()?;
    let root = temp.path().join("runtime");
    let bin = root.join("bin");
    let lib = root.join("lib/python3.11");
    let include = root.join("include");
    fs::create_dir_all(&bin)?;
    fs::create_dir_all(&lib)?;
    fs::create_dir_all(&include)?;
    fs::write(bin.join("python"), b"#!python")?;
    fs::write(lib.join("stdlib.py"), b"# stdlib")?;
    fs::write(include.join("Python.h"), b"// header")?;

    let runtime_meta = RuntimeMetadata {
        path: bin.join("python").display().to_string(),
        version: "3.11.0".to_string(),
        platform: "linux".to_string(),
    };
    let archive = runtime::runtime_archive(&runtime_meta)?;
    let decoder = GzDecoder::new(&archive[..]);
    let mut tar = Archive::new(decoder);
    let mut seen = Vec::new();
    for entry in tar.entries()? {
        let entry = entry?;
        seen.push(entry.path()?.into_owned());
    }
    assert!(
        seen.contains(&PathBuf::from("bin/python")),
        "interpreter should be captured"
    );
    assert!(
        seen.contains(&PathBuf::from("lib/python3.11/stdlib.py")),
        "stdlib should be captured"
    );
    assert!(
        seen.contains(&PathBuf::from("include/Python.h")),
        "headers should be captured"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn runtime_archive_ignores_scripts_dir_from_probe() -> Result<()> {
    let temp = tempdir()?;
    let root = temp.path().join("runtime");
    let bin = root.join("bin");
    let lib = root.join("lib/python3.11");
    let include = root.join("include");
    let tools = root.join("tools");
    fs::create_dir_all(&bin)?;
    fs::create_dir_all(&lib)?;
    fs::create_dir_all(&include)?;
    fs::create_dir_all(&tools)?;

    let python = bin.join("python");
    let payload = serde_json::json!({
        "executable": python.display().to_string(),
        "stdlib": lib.display().to_string(),
        "platstdlib": lib.display().to_string(),
        "include": include.display().to_string(),
        "scripts": tools.display().to_string(),
    });
    let shim = format!("#!/bin/sh\ncat <<'JSON'\n{}\nJSON\n", payload.to_string());
    fs::write(&python, shim)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&python, fs::Permissions::from_mode(0o755))?;
    }

    fs::write(lib.join("stdlib.py"), b"# stdlib")?;
    fs::write(include.join("Python.h"), b"// header")?;
    fs::write(tools.join("junk"), b"junk")?;

    let runtime_meta = RuntimeMetadata {
        path: python.display().to_string(),
        version: "3.11.0".to_string(),
        platform: "linux".to_string(),
    };
    let archive = runtime::runtime_archive(&runtime_meta)?;
    let decoder = GzDecoder::new(&archive[..]);
    let mut tar = Archive::new(decoder);
    let mut seen = Vec::new();
    for entry in tar.entries()? {
        let entry = entry?;
        seen.push(entry.path()?.into_owned());
    }
    assert!(
        seen.contains(&PathBuf::from("bin/python")),
        "interpreter should be captured"
    );
    assert!(
        !seen.contains(&PathBuf::from("tools/junk")),
        "runtime archive should not include sysconfig scripts dir"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn ensure_profile_manifest_reuses_cached_runtime_archive() -> Result<()> {
    let temp = tempdir()?;

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let previous = env::var_os(key);
            env::set_var(key, value);
            Self { key, previous }
        }

        fn remove(key: &'static str) -> Self {
            let previous = env::var_os(key);
            env::remove_var(key);
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(prev) = self.previous.take() {
                env::set_var(self.key, prev);
            } else {
                env::remove_var(self.key);
            }
        }
    }

    let runtime_root = temp.path().join("runtime");
    let bin = runtime_root.join("bin");
    let lib = runtime_root.join("lib/python3.11");
    let include = runtime_root.join("include");
    fs::create_dir_all(&bin)?;
    fs::create_dir_all(&lib)?;
    fs::create_dir_all(&include)?;
    fs::write(lib.join("stdlib.py"), b"# stdlib")?;
    fs::write(include.join("Python.h"), b"// header")?;

    let python = bin.join("python");
    let payload = serde_json::json!({
        "python": ["cp311", "py311", "py3"],
        "abi": ["cp311", "abi3", "none"],
        "platform": ["linux_x86_64", "any"],
        "tags": [],
        "executable": python.display().to_string(),
        "stdlib": lib.display().to_string(),
        "platstdlib": lib.display().to_string(),
        "include": include.display().to_string(),
    });
    let shim = format!("#!/bin/sh\ncat <<'JSON'\n{}\nJSON\n", payload.to_string());
    fs::write(&python, shim)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&python, fs::Permissions::from_mode(0o755))?;
    }

    let project_root = temp.path().join("project");
    fs::create_dir_all(&project_root)?;
    let manifest_path = project_root.join("pyproject.toml");
    fs::write(
        &manifest_path,
        "[project]\nname = \"demo\"\nversion = \"0.1.0\"\nrequires-python = \">=3.11\"\n",
    )?;

    let cache_root = temp.path().join("cache");
    let _cache_guard = EnvVarGuard::set("PX_CACHE_PATH", &cache_root);
    let _host_only_guard = EnvVarGuard::remove("PX_RUNTIME_HOST_ONLY");

    let global = GlobalOptions {
        quiet: false,
        verbose: 0,
        trace: false,
        debug: false,
        json: false,
    };
    let ctx = CommandContext::new(&global, Arc::new(SystemEffects::new()))?;

    let lock = LockSnapshot {
        version: 1,
        project_name: Some("demo".to_string()),
        python_requirement: Some(">=3.11".to_string()),
        manifest_fingerprint: Some("fp".to_string()),
        lock_id: Some("id".to_string()),
        dependencies: Vec::new(),
        mode: Some("p0-pinned".to_string()),
        resolved: Vec::new(),
        graph: None,
        workspace: None,
    };
    let snapshot = ManifestSnapshot {
        root: project_root.clone(),
        manifest_path: manifest_path.clone(),
        lock_path: project_root.join("px.lock"),
        name: "demo".to_string(),
        python_requirement: ">=3.11".to_string(),
        dependencies: Vec::new(),
        dependency_groups: Vec::new(),
        declared_dependency_groups: Vec::new(),
        dependency_group_source: DependencyGroupSource::None,
        group_dependencies: Vec::new(),
        requirements: Vec::new(),
        python_override: None,
        px_options: PxOptions::default(),
        manifest_fingerprint: "fp".to_string(),
    };
    let runtime_meta = RuntimeMetadata {
        path: python.display().to_string(),
        version: "3.11.0".to_string(),
        platform: "linux".to_string(),
    };
    let env_owner = crate::store::cas::OwnerId {
        owner_type: crate::store::cas::OwnerType::ProjectEnv,
        owner_id: "demo".to_string(),
    };

    let first = profile::ensure_profile_manifest(&ctx, &snapshot, &lock, &runtime_meta, &env_owner)?;

    struct PermissionGuard {
        path: PathBuf,
        previous: fs::Permissions,
    }

    impl Drop for PermissionGuard {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.path, self.previous.clone());
        }
    }

    use std::os::unix::fs::PermissionsExt;
    let lib_meta = fs::metadata(&lib)?;
    let include_meta = fs::metadata(&include)?;
    let _lib_guard = PermissionGuard {
        path: lib.clone(),
        previous: lib_meta.permissions(),
    };
    let _include_guard = PermissionGuard {
        path: include.clone(),
        previous: include_meta.permissions(),
    };
    fs::set_permissions(&lib, fs::Permissions::from_mode(0o000))?;
    fs::set_permissions(&include, fs::Permissions::from_mode(0o000))?;

    let second = profile::ensure_profile_manifest(&ctx, &snapshot, &lock, &runtime_meta, &env_owner)?;

    assert_eq!(
        first.header.runtime_oid, second.header.runtime_oid,
        "cached runtime oid should be reused"
    );
    Ok(())
}

#[cfg(not(windows))]
#[test]
fn python_shim_carries_runtime_site_and_home() -> Result<()> {
    let temp = tempdir()?;
    let runtime_root = temp.path().join("runtime");
    let bin_dir = runtime_root.join("bin");
    fs::create_dir_all(&bin_dir)?;
    let runtime = bin_dir.join("python");
    fs::write(&runtime, b"#!/bin/false")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o755))?;
    }

    let env_root = temp.path().join("env");
    let env_bin = env_root.join("bin");
    let site = env_root.join("lib/python3.11/site-packages");
    fs::create_dir_all(&site)?;

    write_python_shim(&env_bin, &runtime, &site, &BTreeMap::new())?;
    let shim = fs::read_to_string(env_bin.join("python"))?;
    assert!(
        shim.contains(&runtime_root.display().to_string()),
        "PYTHONHOME should include runtime root"
    );
    let runtime_site = runtime_root.join("lib/python3.11/site-packages");
    assert!(
        shim.contains(&runtime_site.display().to_string()),
        "PYTHONPATH should include runtime site-packages"
    );
    assert!(
        shim.contains(&site.display().to_string()),
        "PYTHONPATH should include env site-packages"
    );
    Ok(())
}

#[test]
fn env_bin_entries_link_to_store_materialization() -> Result<()> {
    let store = global_store();
    let temp = tempdir()?;

    // Minimal runtime executable.
    let runtime_root = temp.path().join("runtime");
    let runtime_bin = runtime_root.join("bin");
    fs::create_dir_all(&runtime_bin)?;
    let runtime_exe = runtime_bin.join("python");
    fs::write(&runtime_exe, b"#!/bin/false")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&runtime_exe, fs::Permissions::from_mode(0o755))?;
    }

    // CAS pkg-build with a bin script.
    let pkg_root = temp.path().join("pkg");
    let pkg_bin = pkg_root.join("bin");
    let pkg_site = pkg_root.join("site-packages");
    fs::create_dir_all(&pkg_bin)?;
    fs::create_dir_all(&pkg_site)?;
    let script = pkg_bin.join("demo");
    fs::write(&script, b"#!/bin/echo demo")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755))?;
    }

    let pkg_archive = archive_dir_canonical(&pkg_root)?;
    let pkg_obj = store.store(&ObjectPayload::PkgBuild {
        header: PkgBuildHeader {
            source_oid: "src".into(),
            runtime_abi: "abi".into(),
            builder_id: "builder".into(),
            build_options_hash: "opts".into(),
        },
        archive: Cow::Owned(pkg_archive),
    })?;

    // Runtime object to back the profile.
    let runtime_header = RuntimeHeader {
        version: "3.11.0".to_string(),
        abi: "cp311".to_string(),
        platform: "linux".to_string(),
        build_config_hash: "abc".to_string(),
        exe_path: "bin/python".to_string(),
    };
    let runtime_obj = store.store(&ObjectPayload::Runtime {
        header: runtime_header.clone(),
        archive: Cow::Owned(archive_dir_canonical(&runtime_root)?),
    })?;

    let profile_header = ProfileHeader {
        runtime_oid: runtime_obj.oid.clone(),
        packages: vec![ProfilePackage {
            name: "demo".to_string(),
            version: "1.0.0".to_string(),
            pkg_build_oid: pkg_obj.oid.clone(),
        }],
        sys_path_order: Vec::new(),
        env_vars: BTreeMap::new(),
    };
    let profile_obj = store.store(&ObjectPayload::Profile {
        header: profile_header.clone(),
    })?;

    let snapshot = ManifestSnapshot {
        root: temp.path().to_path_buf(),
        manifest_path: temp.path().join("pyproject.toml"),
        lock_path: temp.path().join("px.lock"),
        name: "demo".to_string(),
        python_requirement: ">=3.11".to_string(),
        dependencies: Vec::new(),
        dependency_groups: Vec::new(),
        declared_dependency_groups: Vec::new(),
        dependency_group_source: DependencyGroupSource::None,
        group_dependencies: Vec::new(),
        requirements: Vec::new(),
        python_override: None,
        px_options: PxOptions::default(),
        manifest_fingerprint: "fp".to_string(),
    };

    let env_root = materialize::materialize_profile_env(
        &snapshot,
        &RuntimeMetadata {
            path: runtime_exe.display().to_string(),
            version: "3.11.0".to_string(),
            platform: "linux".to_string(),
        },
        &profile_header,
        &profile_obj.oid,
        &runtime_exe,
    )?;

    let env_bin = env_root.join("bin").join("demo");
    let store_bin = store
        .root()
        .join(MATERIALIZED_PKG_BUILDS_DIR)
        .join(&pkg_obj.oid)
        .join("bin")
        .join("demo");

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = fs::symlink_metadata(&env_bin)?;
        if meta.file_type().is_symlink() {
            let target = fs::read_link(&env_bin)?;
            assert_eq!(target, store_bin, "bin entry should be a symlink into CAS");
        } else {
            let src_meta = fs::metadata(&store_bin)?;
            let dest_meta = fs::metadata(&env_bin)?;
            assert_eq!(
                src_meta.ino(),
                dest_meta.ino(),
                "bin entry should be hard-linked to CAS materialization"
            );
            assert_eq!(
                src_meta.dev(),
                dest_meta.dev(),
                "bin entry should share the same device"
            );
        }
    }
    #[cfg(not(unix))]
    {
        assert_eq!(
            fs::metadata(&env_bin)?.len(),
            fs::metadata(&store_bin)?.len(),
            "bin entry should point to CAS materialization"
        );
    }
    Ok(())
}

#[test]
fn python_bin_entries_are_rewritten_to_env_python() -> Result<()> {
    let store = global_store();
    let temp = tempdir()?;

    // Minimal runtime executable.
    let runtime_root = temp.path().join("runtime");
    let runtime_bin = runtime_root.join("bin");
    fs::create_dir_all(&runtime_bin)?;
    let runtime_exe = runtime_bin.join("python");
    fs::write(&runtime_exe, b"#!/bin/false")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&runtime_exe, fs::Permissions::from_mode(0o755))?;
    }

    // CAS pkg-build with a python shebang.
    let pkg_root = temp.path().join("pkg");
    let pkg_bin = pkg_root.join("bin");
    let pkg_site = pkg_root.join("site-packages");
    fs::create_dir_all(&pkg_bin)?;
    fs::create_dir_all(&pkg_site)?;
    let script = pkg_bin.join("demo");
    fs::write(&script, b"#!/usr/bin/env python3\nprint('hi from demo')\n")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755))?;
    }

    let pkg_archive = archive_dir_canonical(&pkg_root)?;
    let pkg_obj = store.store(&ObjectPayload::PkgBuild {
        header: PkgBuildHeader {
            source_oid: "src".into(),
            runtime_abi: "abi".into(),
            builder_id: "builder".into(),
            build_options_hash: "opts".into(),
        },
        archive: Cow::Owned(pkg_archive),
    })?;

    let runtime_header = RuntimeHeader {
        version: "3.11.0".to_string(),
        abi: "cp311".to_string(),
        platform: "linux".to_string(),
        build_config_hash: "abc".to_string(),
        exe_path: "bin/python".to_string(),
    };
    let runtime_obj = store.store(&ObjectPayload::Runtime {
        header: runtime_header.clone(),
        archive: Cow::Owned(archive_dir_canonical(&runtime_root)?),
    })?;

    let profile_header = ProfileHeader {
        runtime_oid: runtime_obj.oid.clone(),
        packages: vec![ProfilePackage {
            name: "demo".to_string(),
            version: "1.0.0".to_string(),
            pkg_build_oid: pkg_obj.oid.clone(),
        }],
        sys_path_order: Vec::new(),
        env_vars: BTreeMap::new(),
    };
    let profile_obj = store.store(&ObjectPayload::Profile {
        header: profile_header.clone(),
    })?;

    let snapshot = ManifestSnapshot {
        root: temp.path().to_path_buf(),
        manifest_path: temp.path().join("pyproject.toml"),
        lock_path: temp.path().join("px.lock"),
        name: "demo".to_string(),
        python_requirement: ">=3.11".to_string(),
        dependencies: Vec::new(),
        dependency_groups: Vec::new(),
        declared_dependency_groups: Vec::new(),
        dependency_group_source: DependencyGroupSource::None,
        group_dependencies: Vec::new(),
        requirements: Vec::new(),
        python_override: None,
        px_options: PxOptions::default(),
        manifest_fingerprint: "fp".to_string(),
    };

    let env_root = materialize::materialize_profile_env(
        &snapshot,
        &RuntimeMetadata {
            path: runtime_exe.display().to_string(),
            version: "3.11.0".to_string(),
            platform: "linux".to_string(),
        },
        &profile_header,
        &profile_obj.oid,
        &runtime_exe,
    )?;

    let env_script = env_root.join("bin").join("demo");
    let contents = fs::read_to_string(&env_script)?;
    let expected_shebang = if cfg!(windows) {
        format!("#!{}\n", runtime_exe.display())
    } else {
        format!("#!{}\n", env_root.join("bin").join("python").display())
    };
    assert!(
        contents.starts_with(&expected_shebang),
        "shebang should point at env python"
    );
    assert!(
        contents.contains("hi from demo"),
        "script body should be preserved"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert!(
            !fs::symlink_metadata(&env_script)?.file_type().is_symlink(),
            "python bin shims should be copied, not linked"
        );
        let mode = fs::metadata(&env_script)?.mode();
        assert!(
            mode & 0o111 != 0,
            "rewritten script should remain executable"
        );
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn python_shim_applies_profile_env_vars() -> Result<()> {
    let temp = tempdir()?;
    let runtime_root = temp.path().join("runtime");
    let bin_dir = runtime_root.join("bin");
    fs::create_dir_all(&bin_dir)?;
    let runtime = bin_dir.join("python");
    fs::write(
        &runtime,
        "#!/usr/bin/env bash\nprintf \"%s\" \"$FOO_FROM_PROFILE\"\n",
    )?;
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o755))?;

    let env_root = temp.path().join("env");
    let env_bin = env_root.join("bin");
    let site = env_root.join("lib/python3.11/site-packages");
    fs::create_dir_all(&site)?;

    let mut env_vars = BTreeMap::new();
    env_vars.insert(
        "FOO_FROM_PROFILE".to_string(),
        Value::String("from_profile".to_string()),
    );
    write_python_shim(&env_bin, &runtime, &site, &env_vars)?;
    let shim = env_bin.join("python");
    let output = Command::new(&shim)
        .env("PX_CACHE_PATH", temp.path())
        .env("FOO_FROM_PROFILE", "ignored")
        .output()?;
    assert!(
        output.status.success(),
        "shim should run successfully: {:?}",
        output
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout, "from_profile",
        "profile env vars should override parent values"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn python_shim_preserves_existing_pythonpath() -> Result<()> {
    let temp = tempdir()?;
    let runtime_root = temp.path().join("runtime");
    let bin_dir = runtime_root.join("bin");
    fs::create_dir_all(&bin_dir)?;
    let runtime = bin_dir.join("python");
    fs::write(
        &runtime,
        "#!/usr/bin/env bash\nprintf \"%s\" \"$PYTHONPATH\"\n",
    )?;
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o755))?;

    let env_root = temp.path().join("env");
    let env_bin = env_root.join("bin");
    let site = env_root.join("lib/python3.11/site-packages");
    fs::create_dir_all(&site)?;

    write_python_shim(&env_bin, &runtime, &site, &BTreeMap::new())?;
    let shim = env_bin.join("python");
    let existing = "/tmp/custom";
    let runtime_site = runtime_root.join("lib/python3.11/site-packages");
    let expected = format!("{existing}:{}:{}", site.display(), runtime_site.display());

    let output = Command::new(&shim)
        .env("PX_CACHE_PATH", temp.path())
        .env("PYTHONPATH", existing)
        .output()?;
    assert!(
        output.status.success(),
        "shim should run successfully: {:?}",
        output
    );
    let value = String::from_utf8_lossy(&output.stdout);
    assert_eq!(value, expected);
    Ok(())
}

#[test]
fn profile_env_vars_merge_snapshot_and_env_override() -> Result<()> {
    let snapshot = px_domain::api::ProjectSnapshot {
        root: PathBuf::from("/tmp/demo"),
        manifest_path: PathBuf::from("/tmp/demo/pyproject.toml"),
        lock_path: PathBuf::from("/tmp/demo/px.lock"),
        name: "demo".to_string(),
        python_requirement: ">=3.11".to_string(),
        dependencies: vec![],
        dependency_groups: vec![],
        declared_dependency_groups: vec![],
        dependency_group_source: DependencyGroupSource::None,
        group_dependencies: vec![],
        requirements: vec![],
        python_override: None,
        px_options: px_domain::api::PxOptions {
            manage_command: None,
            plugin_imports: vec![],
            env_vars: BTreeMap::from([("FROM_SNAPSHOT".to_string(), "snap".to_string())]),
        },
        manifest_fingerprint: "fp".to_string(),
    };
    let prev_env = env::var("PX_PROFILE_ENV_VARS").ok();
    env::set_var(
        "PX_PROFILE_ENV_VARS",
        r#"{"FROM_ENV":"env","FROM_SNAPSHOT":"override"}"#,
    );
    let merged = profile::profile_env_vars(&snapshot)?;
    if let Some(val) = prev_env {
        env::set_var("PX_PROFILE_ENV_VARS", val);
    } else {
        env::remove_var("PX_PROFILE_ENV_VARS");
    }
    assert_eq!(
        merged.get("FROM_SNAPSHOT"),
        Some(&Value::String("override".to_string()))
    );
    assert_eq!(
        merged.get("FROM_ENV"),
        Some(&Value::String("env".to_string()))
    );
    Ok(())
}
