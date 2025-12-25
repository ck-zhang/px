use super::super::types::{SandboxBase, SandboxDefinition, SandboxImageManifest, SBX_VERSION};
use super::*;
use crate::core::system_deps::resolve_system_deps;
use serde_json;
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::fs::File;
use std::path::Path;
use tar::{Archive, EntryType};
use tempfile::tempdir;

#[test]
fn oci_builder_writes_layer_with_sources() -> Result<()> {
    let temp = tempdir()?;
    let source = temp.path().join("app");
    fs::create_dir_all(&source)?;
    fs::write(source.join("app.py"), b"print('hi')\n")?;
    let env_root = temp.path().join("env");
    fs::create_dir_all(env_root.join("bin"))?;
    fs::write(
        env_root.join("bin").join("python"),
        b"#!/usr/bin/env python\n",
    )?;
    let oci_root = temp.path().join("oci");

    let mut caps = BTreeSet::new();
    caps.insert("postgres".to_string());
    let system_deps = resolve_system_deps(&caps, None);
    let definition = SandboxDefinition {
        base_os_oid: "base".into(),
        capabilities: caps.clone(),
        system_deps: system_deps.clone(),
        profile_oid: "profile".into(),
        sbx_version: SBX_VERSION,
    };
    let artifacts = SandboxArtifacts {
        base: SandboxBase {
            name: "demo".into(),
            base_os_oid: "base".into(),
            supported_capabilities: caps.clone(),
        },
        definition: definition.clone(),
        manifest: SandboxImageManifest {
            sbx_id: definition.sbx_id(),
            base_os_oid: "base".into(),
            profile_oid: "profile".into(),
            capabilities: caps,
            system_deps,
            image_digest: String::new(),
            base_layer_digest: None,
            env_layer_digest: None,
            system_layer_digest: None,
            created_at: String::new(),
            px_version: "test".into(),
            sbx_version: SBX_VERSION,
        },
        env_root: env_root.clone(),
    };

    let blobs = oci_root.join("blobs").join("sha256");
    let env_layer = write_env_layer_tar(&env_root, None, &blobs)?;
    let app_layer = layers::write_app_layer_tar(&source, &blobs)?;
    build_oci_image(
        &artifacts,
        &oci_root,
        vec![env_layer, app_layer],
        Some("demo:latest"),
        Path::new("/app"),
        Some("/app"),
    )?;
    let index_path = oci_root.join("index.json");
    assert!(index_path.exists(), "index.json missing");
    let index: serde_json::Value = serde_json::from_slice(&fs::read(&index_path)?)?;
    let manifest_digest = index["manifests"][0]["digest"]
        .as_str()
        .unwrap()
        .trim_start_matches("sha256:")
        .to_string();
    let manifest_path = oci_root.join("blobs").join("sha256").join(&manifest_digest);
    let manifest: serde_json::Value = serde_json::from_slice(&fs::read(&manifest_path)?)?;
    let layers = manifest["layers"].as_array().expect("layers array");
    let mut found = false;
    for layer in layers {
        let digest = layer["digest"]
            .as_str()
            .unwrap()
            .trim_start_matches("sha256:");
        let layer_path = oci_root.join("blobs").join("sha256").join(digest);
        let file = File::open(layer_path)?;
        let mut archive = Archive::new(file);
        for entry in archive.entries()? {
            let entry = entry?;
            if entry.path()? == Path::new("app/app.py") {
                found = true;
                break;
            }
        }
        if found {
            break;
        }
    }
    assert!(found, "app layer should include app/app.py");
    Ok(())
}

#[test]
fn env_layer_skips_runtime_static_libs() -> Result<()> {
    let temp = tempdir()?;
    let env_root = temp.path().join("env");
    fs::create_dir_all(&env_root)?;
    fs::write(
        env_root.join("pyvenv.cfg"),
        b"home = /does/not/matter\nversion = 3.12.0\n",
    )?;

    let runtime_root = temp.path().join("runtime");
    let lib_root = runtime_root.join("lib").join("python3.12");
    fs::create_dir_all(&lib_root)?;
    fs::write(lib_root.join("hello.py"), b"print('hi')\n")?;
    let config_root = lib_root.join("config-3.12-test");
    fs::create_dir_all(&config_root)?;
    fs::write(config_root.join("libpython3.12.a"), vec![0u8; 16])?;
    let pip_root = lib_root.join("site-packages").join("pip");
    fs::create_dir_all(&pip_root)?;
    fs::write(pip_root.join("__init__.py"), b"")?;
    let test_root = lib_root.join("test");
    fs::create_dir_all(&test_root)?;
    fs::write(test_root.join("dummy.py"), b"")?;
    let pycache_root = lib_root.join("__pycache__");
    fs::create_dir_all(&pycache_root)?;
    fs::write(pycache_root.join("dummy.cpython-312.pyc"), vec![0u8; 4])?;
    fs::write(lib_root.join("orphan.pyc"), vec![0u8; 4])?;

    let blobs = temp.path().join("blobs");
    let env_layer = write_env_layer_tar(&env_root, Some(&runtime_root), &blobs)?;

    let file = File::open(env_layer.path)?;
    let mut archive = Archive::new(file);
    let mut saw_hello = false;
    let mut saw_static = false;
    let mut saw_site_packages = false;
    let mut saw_test = false;
    let mut saw_pycache = false;
    let mut saw_pyc = false;
    for entry in archive.entries()? {
        let entry = entry?;
        let path = entry.path()?.into_owned();
        if path == Path::new("px/runtime/lib/python3.12/hello.py") {
            saw_hello = true;
        }
        if path == Path::new("px/runtime/lib/python3.12/config-3.12-test/libpython3.12.a") {
            saw_static = true;
        }
        if path == Path::new("px/runtime/lib/python3.12/site-packages/pip/__init__.py") {
            saw_site_packages = true;
        }
        if path == Path::new("px/runtime/lib/python3.12/test/dummy.py") {
            saw_test = true;
        }
        if path == Path::new("px/runtime/lib/python3.12/__pycache__/dummy.cpython-312.pyc") {
            saw_pycache = true;
        }
        if path == Path::new("px/runtime/lib/python3.12/orphan.pyc") {
            saw_pyc = true;
        }
    }

    assert!(saw_hello, "runtime file should be present in layer tar");
    assert!(!saw_static, "static lib should be excluded from layer tar");
    assert!(
        !saw_site_packages,
        "runtime site-packages should be excluded from layer tar"
    );
    assert!(!saw_test, "runtime stdlib tests should be excluded from layer tar");
    assert!(
        !saw_pycache,
        "runtime __pycache__ should be excluded from layer tar"
    );
    assert!(!saw_pyc, "runtime .pyc files should be excluded from layer tar");
    Ok(())
}

#[test]
fn env_layer_stages_runtime_python_aliases() -> Result<()> {
    let temp = tempdir()?;
    let env_root = temp.path().join("env");
    fs::create_dir_all(&env_root)?;
    fs::write(
        env_root.join("pyvenv.cfg"),
        b"home = /does/not/matter\nversion = 3.12.0\n",
    )?;

    let runtime_root = temp.path().join("runtime");
    let runtime_bin = runtime_root.join("bin");
    fs::create_dir_all(&runtime_bin)?;
    let interpreter = runtime_bin.join("python3.12");
    fs::write(&interpreter, b"#!/bin/sh\nexit 0\n")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&interpreter)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&interpreter, perms)?;
    }

    let blobs = temp.path().join("blobs");
    let env_layer = write_env_layer_tar(&env_root, Some(&runtime_root), &blobs)?;

    let file = File::open(env_layer.path)?;
    let mut archive = Archive::new(file);
    let mut saw_python = None::<String>;
    let mut saw_python3 = None::<String>;
    let mut saw_python312 = false;
    for entry in archive.entries()? {
        let entry = entry?;
        let path = entry.path()?.into_owned();
        if path == Path::new("px/runtime/bin/python3.12") {
            saw_python312 = true;
        }
        if path == Path::new("px/runtime/bin/python") {
            assert_eq!(
                entry.header().entry_type(),
                EntryType::Symlink,
                "px/runtime/bin/python should be a symlink"
            );
            let target = entry
                .link_name()?
                .map(|path| path.to_string_lossy().to_string())
                .unwrap_or_default();
            saw_python = Some(target);
        }
        if path == Path::new("px/runtime/bin/python3") {
            assert_eq!(
                entry.header().entry_type(),
                EntryType::Symlink,
                "px/runtime/bin/python3 should be a symlink"
            );
            let target = entry
                .link_name()?
                .map(|path| path.to_string_lossy().to_string())
                .unwrap_or_default();
            saw_python3 = Some(target);
        }
    }

    assert!(saw_python312, "runtime interpreter should be present in layer tar");
    assert_eq!(
        saw_python.as_deref(),
        Some("python3.12"),
        "sandbox runtime should provide /px/runtime/bin/python symlink"
    );
    assert_eq!(
        saw_python3.as_deref(),
        Some("python3.12"),
        "sandbox runtime should provide /px/runtime/bin/python3 symlink"
    );
    Ok(())
}

#[test]
fn default_tag_uses_manifest_version_when_available() -> Result<()> {
    let temp = tempdir()?;
    let manifest = temp.path().join("pyproject.toml");
    fs::write(
        &manifest,
        r#"[project]
name = "demo"
version = "1.2.3"
requires-python = ">=3.11"
"#,
    )?;
    let tag = defaults::default_tag("demo", &manifest, "profile");
    assert_eq!(tag, "px.local/demo:1.2.3");
    Ok(())
}

#[test]
fn default_tag_falls_back_to_profile_when_version_missing() {
    let temp = tempdir().unwrap();
    let manifest = temp.path().join("pyproject.toml");
    fs::write(
        &manifest,
        r#"[project]
name = "demo"
requires-python = ">=3.11"
"#,
    )
    .unwrap();
    let tag = defaults::default_tag("demo", &manifest, "profile-oid-1234567890");
    assert!(
        tag.starts_with("px.local/demo:profile-oid"),
        "fallback tag should use profile oid"
    );
}

#[test]
fn registry_auth_requires_username_and_password() {
    let prev_user = env::var("PX_REGISTRY_USERNAME").ok();
    let prev_pass = env::var("PX_REGISTRY_PASSWORD").ok();
    env::set_var("PX_REGISTRY_USERNAME", "demo");
    env::remove_var("PX_REGISTRY_PASSWORD");
    let err = super::oci::registry_auth_from_env().expect_err("missing password should error");
    let reason = err
        .details
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    assert_eq!(reason, "registry_auth_missing");
    if let Some(val) = prev_user {
        env::set_var("PX_REGISTRY_USERNAME", val);
    } else {
        env::remove_var("PX_REGISTRY_USERNAME");
    }
    if let Some(val) = prev_pass {
        env::set_var("PX_REGISTRY_PASSWORD", val);
    } else {
        env::remove_var("PX_REGISTRY_PASSWORD");
    }
}
