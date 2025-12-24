use super::super::types::{SandboxBase, SandboxDefinition, SandboxImageManifest, SBX_VERSION};
use super::*;
use crate::core::system_deps::resolve_system_deps;
use serde_json;
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::fs::File;
use std::path::Path;
use tar::Archive;
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

    let blobs = temp.path().join("blobs");
    let env_layer = write_env_layer_tar(&env_root, Some(&runtime_root), &blobs)?;

    let file = File::open(env_layer.path)?;
    let mut archive = Archive::new(file);
    let mut saw_hello = false;
    let mut saw_static = false;
    for entry in archive.entries()? {
        let entry = entry?;
        let path = entry.path()?.into_owned();
        if path == Path::new("px/runtime/lib/python3.12/hello.py") {
            saw_hello = true;
        }
        if path == Path::new("px/runtime/lib/python3.12/config-3.12-test/libpython3.12.a") {
            saw_static = true;
        }
    }

    assert!(saw_hello, "runtime file should be present in layer tar");
    assert!(!saw_static, "static lib should be excluded from layer tar");
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
