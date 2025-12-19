use super::*;
use crate::api::{Config, SystemEffects};
use crate::core::config::settings::EnvSnapshot;
use crate::core::python::python_sys::{
    current_marker_environment, InterpreterSupportedTag, InterpreterTags,
};
use crate::core::runtime::artifacts::{parse_exact_pin, select_wheel};
use crate::core::runtime::effects::Effects;
use crate::core::runtime::materialize_project_site;
use crate::core::store::pypi::{PypiDigests, PypiFile};
use anyhow::Result;
use px_domain::api::marker_applies;
use px_domain::api::{
    DependencyGroupSource, LockSnapshot, LockedArtifact, LockedDependency, PxOptions,
};
use serial_test::serial;
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use tempfile::tempdir;

#[test]
fn config_respects_env_flags() {
    let snapshot = EnvSnapshot::testing(&[
        ("PX_ONLINE", "1"),
        ("PX_FORCE_SDIST", "1"),
        ("PX_TEST_FALLBACK_STD", "1"),
        ("PX_SKIP_TESTS", "1"),
    ]);
    let effects = SystemEffects::new();
    let config = Config::from_snapshot(&snapshot, effects.cache()).expect("config");
    assert!(config.network.online);
    assert!(config.resolver.force_sdist);
    assert!(config.test.fallback_builtin);
    assert_eq!(config.test.skip_tests_flag.as_deref(), Some("1"));
}

#[test]
fn network_online_default_true() {
    let snapshot = EnvSnapshot::testing(&[]);
    let effects = SystemEffects::new();
    let config = Config::from_snapshot(&snapshot, effects.cache()).expect("config");
    assert!(config.network.online);
}

#[test]
fn network_can_be_disabled_via_env() {
    let snapshot = EnvSnapshot::testing(&[("PX_ONLINE", "0")]);
    let effects = SystemEffects::new();
    let config = Config::from_snapshot(&snapshot, effects.cache()).expect("config");
    assert!(!config.network.online);
}

#[test]
fn resolver_enabled_by_default() {
    let snapshot = EnvSnapshot::testing(&[]);
    let effects = SystemEffects::new();
    let config = Config::from_snapshot(&snapshot, effects.cache()).expect("config");
    assert!(
        !config.resolver.force_sdist,
        "force_sdist should default to false"
    );
}

#[test]
#[serial]
fn base_env_exports_manage_command_alias() -> Result<()> {
    let temp = tempdir()?;
    let python =
        crate::python_sys::detect_interpreter().unwrap_or_else(|_| "/usr/bin/python3".to_string());
    let site_bin = temp.path().join("site-bin");
    fs::create_dir_all(&site_bin)?;
    let ctx = PythonContext {
        project_root: temp.path().to_path_buf(),
        state_root: temp.path().to_path_buf(),
        project_name: "demo".to_string(),
        python,
        pythonpath: String::new(),
        allowed_paths: vec![temp.path().to_path_buf()],
        site_bin: Some(site_bin.clone()),
        pep582_bin: Vec::new(),
        pyc_cache_prefix: None,
        px_options: PxOptions {
            manage_command: Some("self".to_string()),
            plugin_imports: Vec::new(),
            env_vars: BTreeMap::new(),
        },
    };
    // Seed existing PATH and proxy vars to verify overrides
    let original_path = std::env::var("PATH").ok();
    let original_proxy = std::env::var("HTTPS_PROXY").ok();
    std::env::set_var("PATH", "/usr/local/bin");
    std::env::set_var("HTTPS_PROXY", "http://proxy");
    let envs = ctx.base_env(&json!({}))?;
    if let Some(val) = original_path {
        std::env::set_var("PATH", val);
    }
    if let Some(val) = original_proxy {
        std::env::set_var("HTTPS_PROXY", val);
    } else {
        std::env::remove_var("HTTPS_PROXY");
    }
    assert!(envs
        .iter()
        .any(|(key, value)| key == "PYAPP_COMMAND_NAME" && value == "self"));
    let path_entry = envs
        .iter()
        .find(|(key, _)| key == "PATH")
        .map(|(_, value)| value.clone())
        .unwrap_or_default();
    // site/bin should be first
    assert!(
        path_entry.starts_with(&site_bin.display().to_string()),
        "PATH should be rebuilt with site/bin first"
    );
    let virtual_env = envs
        .iter()
        .find(|(key, _)| key == "VIRTUAL_ENV")
        .map(|(_, value)| value.clone())
        .unwrap_or_default();
    assert_eq!(
        virtual_env,
        site_bin
            .parent()
            .unwrap()
            .canonicalize()
            .unwrap()
            .display()
            .to_string(),
        "VIRTUAL_ENV should point to the site parent"
    );
    for key in [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "NO_PROXY",
        "no_proxy",
    ] {
        assert!(
            envs.iter().any(|(k, v)| k == key && v.is_empty()),
            "{key} should be cleared from the project env"
        );
    }
    Ok(())
}

#[test]
fn python_environment_markers_create_pyvenv_and_shims() -> Result<()> {
    let temp = tempdir()?;
    let site_dir = temp.path().join("env").join("site");
    fs::create_dir_all(site_dir.join("bin"))?;

    let runtime_dir = temp.path().join("runtime").join("bin");
    fs::create_dir_all(&runtime_dir)?;
    let runtime_python = runtime_dir.join("python3.11");
    fs::write(&runtime_python, b"stub")?;
    #[cfg(unix)]
    {
        let perms = fs::Permissions::from_mode(0o755);
        fs::set_permissions(&runtime_python, perms)?;
    }

    let runtime = RuntimeMetadata {
        path: runtime_python.display().to_string(),
        version: "3.11.4".into(),
        platform: "linux_x86_64".into(),
    };
    let effects = SystemEffects::new();
    let env_python =
        write_python_environment_markers(&site_dir, &runtime, &runtime_python, effects.fs())
            .expect("markers");

    let pyvenv_cfg = site_dir.join("pyvenv.cfg");
    assert!(pyvenv_cfg.exists(), "pyvenv.cfg should be written");
    let cfg_contents = fs::read_to_string(pyvenv_cfg)?;
    assert!(
        cfg_contents.contains("include-system-site-packages = false"),
        "pyvenv.cfg should disable system site packages"
    );
    assert!(
        cfg_contents.contains("version = 3.11.4"),
        "pyvenv.cfg should record runtime version"
    );
    assert!(
        cfg!(windows) || site_dir.join("bin/python").exists(),
        "python shim should be created (non-Windows)"
    );
    assert!(
        cfg!(windows) || site_dir.join("bin/python3").exists(),
        "python3 shim should be created (non-Windows)"
    );
    if cfg!(windows) {
        assert_eq!(
            env_python, runtime_python,
            "Windows uses the runtime executable directly"
        );
    } else {
        assert!(
            env_python.starts_with(site_dir.join("bin")),
            "primary python should live under the site bin dir"
        );
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn python_environment_markers_apply_manifest_env_vars() -> Result<()> {
    let temp = tempdir()?;
    let site_dir = temp.path().join("env").join("site");
    fs::create_dir_all(site_dir.join("bin"))?;

    let manifest = json!({
        "profile_oid": "demo-profile",
        "runtime_oid": "demo-runtime",
        "packages": [],
        "sys_path_order": [],
        "env_vars": { "LD_LIBRARY_PATH": "/tmp/from_manifest" }
    });
    fs::write(
        site_dir.join("manifest.json"),
        serde_json::to_string_pretty(&manifest)?,
    )?;

    let runtime_dir = temp.path().join("runtime").join("bin");
    fs::create_dir_all(&runtime_dir)?;
    let runtime_python = runtime_dir.join("python3.11");
    fs::write(
        &runtime_python,
        "#!/usr/bin/env bash\nprintf \"%s\" \"$LD_LIBRARY_PATH\"\n",
    )?;
    fs::set_permissions(&runtime_python, fs::Permissions::from_mode(0o755))?;

    let runtime = RuntimeMetadata {
        path: runtime_python.display().to_string(),
        version: "3.11.4".into(),
        platform: "linux_x86_64".into(),
    };
    let effects = SystemEffects::new();
    let env_python =
        write_python_environment_markers(&site_dir, &runtime, &runtime_python, effects.fs())?;
    let output = Command::new(&env_python).output()?;
    assert!(output.status.success(), "shim should execute");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout, "/tmp/from_manifest");
    Ok(())
}

#[test]
fn marker_applies_respects_python_version() {
    let env = match current_marker_environment() {
        Ok(env) => env,
        Err(_) => return,
    };
    assert!(
        !marker_applies("tomli>=1.1.0; python_version < '3.11'", &env),
        "non-matching marker should be skipped"
    );
}

#[test]
fn parse_exact_pin_handles_extras_and_markers() {
    let spec = r#"requests[socks]==2.32 ; python_version >= "3.10""#;
    let pin = parse_exact_pin(spec).expect("pin");
    assert_eq!(pin.name, "requests");
    assert_eq!(pin.version, "2.32");
    assert_eq!(pin.extras, vec!["socks".to_string()]);
    assert!(
        pin.specifier.contains("[socks]==2.32"),
        "specifier should include extras"
    );
    assert!(
        pin.marker
            .as_deref()
            .is_some_and(|m| m.contains("python_version")),
        "marker should be preserved"
    );
}

#[test]
fn materialize_project_site_writes_cached_paths() {
    let temp = tempdir().expect("tempdir");
    let root = temp.path();
    let cache_dir = root.join("cache");
    fs::create_dir_all(&cache_dir).expect("cache dir");
    let wheel = cache_dir.join("demo-1.0.0.whl");
    fs::write(&wheel, b"demo").expect("wheel stub");
    let dist_dir = wheel.with_extension("dist");
    fs::create_dir_all(&dist_dir).expect("dist dir");

    let snapshot = ManifestSnapshot {
        root: root.to_path_buf(),
        manifest_path: root.join("pyproject.toml"),
        lock_path: root.join("px.lock"),
        name: "demo".into(),
        python_requirement: ">=3.11".into(),
        dependencies: Vec::new(),
        dependency_groups: Vec::new(),
        declared_dependency_groups: Vec::new(),
        dependency_group_source: DependencyGroupSource::None,
        group_dependencies: Vec::new(),
        requirements: Vec::new(),
        python_override: None,
        px_options: PxOptions::default(),
        manifest_fingerprint: "demo-fingerprint".into(),
    };
    let lock = LockSnapshot {
        version: 1,
        project_name: Some("demo".into()),
        python_requirement: Some(">=3.11".into()),
        manifest_fingerprint: Some("demo-fingerprint".into()),
        lock_id: Some("lock-demo".into()),
        dependencies: Vec::new(),
        mode: Some("p0-pinned".into()),
        resolved: vec![LockedDependency {
            name: "demo".into(),
            direct: true,
            artifact: Some(LockedArtifact {
                filename: "demo.whl".into(),
                url: "https://example.invalid/demo.whl".into(),
                sha256: "abc123".into(),
                size: 4,
                cached_path: wheel.display().to_string(),
                python_tag: "py3".into(),
                abi_tag: "none".into(),
                platform_tag: "any".into(),
                is_direct_url: false,
                build_options_hash: String::new(),
            }),
            requires: Vec::new(),
            source: None,
        }],
        graph: None,
        workspace: None,
    };

    let effects = SystemEffects::new();
    let site_dir = snapshot
        .root
        .join(".px")
        .join("envs")
        .join("test-env")
        .join("site");
    let site_packages = site_packages_dir(&site_dir, "3.11.0");
    materialize_project_site(&site_dir, &site_packages, &lock, None, effects.fs())
        .expect("materialize site");

    let pxpth = site_dir.join("px.pth");
    assert!(
        pxpth.exists(),
        "env site px.pth should be created alongside install"
    );
    let contents = fs::read_to_string(pxpth).expect("read px.pth");
    assert!(
        contents.contains(dist_dir.to_str().unwrap()),
        "px.pth should reference unpacked artifact path"
    );
}

#[test]
fn materialize_project_site_skips_missing_artifacts() {
    let temp = tempdir().expect("tempdir");
    let root = temp.path();
    let snapshot = ManifestSnapshot {
        root: root.to_path_buf(),
        manifest_path: root.join("pyproject.toml"),
        lock_path: root.join("px.lock"),
        name: "demo".into(),
        python_requirement: ">=3.11".into(),
        dependencies: Vec::new(),
        dependency_groups: Vec::new(),
        declared_dependency_groups: Vec::new(),
        dependency_group_source: DependencyGroupSource::None,
        group_dependencies: Vec::new(),
        requirements: Vec::new(),
        python_override: None,
        px_options: PxOptions::default(),
        manifest_fingerprint: "demo-fingerprint".into(),
    };
    let lock = LockSnapshot {
        version: 1,
        project_name: Some("demo".into()),
        python_requirement: Some(">=3.11".into()),
        manifest_fingerprint: Some("demo-fingerprint".into()),
        lock_id: Some("lock-demo".into()),
        dependencies: Vec::new(),
        mode: Some("p0-pinned".into()),
        resolved: vec![LockedDependency {
            name: "missing".into(),
            direct: true,
            artifact: Some(LockedArtifact {
                filename: "missing.whl".into(),
                url: "https://example.invalid/missing.whl".into(),
                sha256: "deadbeef".into(),
                size: 0,
                cached_path: root.join("nope").display().to_string(),
                python_tag: "py3".into(),
                abi_tag: "none".into(),
                platform_tag: "any".into(),
                is_direct_url: false,
                build_options_hash: String::new(),
            }),
            requires: Vec::new(),
            source: None,
        }],
        graph: None,
        workspace: None,
    };

    let effects = SystemEffects::new();
    let site_dir = snapshot
        .root
        .join(".px")
        .join("envs")
        .join("test-env")
        .join("site");
    let site_packages = site_packages_dir(&site_dir, "3.11.0");
    materialize_project_site(&site_dir, &site_packages, &lock, None, effects.fs())
        .expect("materialize site with gap");
    let pxpth = site_dir.join("px.pth");
    assert!(pxpth.exists(), "px.pth should still be created");
    let contents = fs::read_to_string(pxpth).expect("read px.pth");
    assert!(
        contents.trim().is_empty(),
        "missing artifacts should not be written to px.pth"
    );
}

#[test]
fn select_wheel_prefers_linux_over_macos() {
    let files = vec![
        wheel_file("demo-1.0.0-cp312-cp312-macosx_10_13_x86_64.whl"),
        wheel_file("demo-1.0.0-cp312-cp312-manylinux_2_17_x86_64.manylinux2014_x86_64.whl"),
    ];
    let tags = linux_interpreter_tags();
    let wheel = select_wheel(&files, &tags, "demo==1.0.0").expect("linux match");
    assert!(wheel.platform_tag.contains("manylinux"));
}

#[test]
fn select_wheel_rejects_incompatible_platforms() {
    let files = vec![wheel_file("demo-1.0.0-cp312-cp312-macosx_10_13_x86_64.whl")];
    let tags = linux_interpreter_tags();
    let err = select_wheel(&files, &tags, "demo==1.0.0").expect_err("mac wheel rejected");
    assert!(err.to_string().contains("did not provide any wheels"));
}

#[test]
fn heuristic_platform_matching_handles_manylinux() {
    let files = vec![wheel_file(
        "demo-1.0.0-cp312-cp312-manylinux_2_17_x86_64.manylinux2014_x86_64.whl",
    )];
    let mut tags = linux_interpreter_tags();
    tags.supported.clear();
    let wheel = select_wheel(&files, &tags, "demo==1.0.0").expect("fallback tags");
    assert!(wheel.platform_tag.contains("manylinux"));
}

fn wheel_file(name: &str) -> PypiFile {
    PypiFile {
        filename: name.into(),
        url: format!("https://example.invalid/{name}"),
        packagetype: "bdist_wheel".into(),
        yanked: Some(false),
        digests: PypiDigests {
            sha256: "deadbeef".into(),
        },
    }
}

fn linux_interpreter_tags() -> InterpreterTags {
    InterpreterTags {
        python: vec!["cp312".into(), "py312".into(), "py3".into()],
        abi: vec!["cp312".into(), "abi3".into(), "none".into()],
        platform: vec!["linux_x86_64".into(), "any".into()],
        supported: vec![
            InterpreterSupportedTag {
                python: "cp312".into(),
                abi: "cp312".into(),
                platform: "manylinux_2_17_x86_64".into(),
            },
            InterpreterSupportedTag {
                python: "cp312".into(),
                abi: "cp312".into(),
                platform: "manylinux2014_x86_64".into(),
            },
        ],
    }
}
