use super::*;
use crate::core::config::settings::EnvSnapshot;
use crate::core::python::python_sys::{
    current_marker_environment, InterpreterSupportedTag, InterpreterTags,
};
use crate::core::runtime::artifacts::{parse_exact_pin, select_wheel};
use crate::core::runtime::effects::Effects;
use crate::core::runtime::materialize_project_site;
use crate::core::runtime::run_plan::python_script_target;
use crate::core::store::pypi::{PypiDigests, PypiFile};
use crate::Config;
use crate::SystemEffects;
use anyhow::Result;
use px_domain::lockfile::{LockSnapshot, LockedArtifact, LockedDependency};
use px_domain::marker_applies;
use px_domain::{DependencyGroupSource, PxOptions};
use serde_json::json;
use std::fs;
use std::path::PathBuf;
use tempfile::tempdir;

#[test]
fn config_respects_env_flags() {
    let snapshot = EnvSnapshot::testing(&[
        ("PX_ONLINE", "1"),
        ("PX_RESOLVER", "1"),
        ("PX_FORCE_SDIST", "1"),
        ("PX_TEST_FALLBACK_STD", "1"),
        ("PX_SKIP_TESTS", "1"),
    ]);
    let effects = SystemEffects::new();
    let config = Config::from_snapshot(&snapshot, effects.cache()).expect("config");
    assert!(config.network.online);
    assert!(config.resolver.enabled);
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
    assert!(config.resolver.enabled);
}

#[test]
fn resolver_can_be_disabled_via_env() {
    let snapshot = EnvSnapshot::testing(&[("PX_RESOLVER", "0")]);
    let effects = SystemEffects::new();
    let config = Config::from_snapshot(&snapshot, effects.cache()).expect("config");
    assert!(!config.resolver.enabled);
}

#[test]
fn base_env_exports_manage_command_alias() -> Result<()> {
    let temp = tempdir()?;
    let python =
        crate::python_sys::detect_interpreter().unwrap_or_else(|_| "/usr/bin/python3".to_string());
    let site_bin = temp.path().join("site-bin");
    fs::create_dir_all(&site_bin)?;
    let ctx = PythonContext {
        project_root: temp.path().to_path_buf(),
        python,
        pythonpath: String::new(),
        allowed_paths: vec![temp.path().to_path_buf()],
        site_bin: Some(site_bin.clone()),
        px_options: PxOptions {
            manage_command: Some("self".to_string()),
            plugin_imports: Vec::new(),
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
    assert!(
        envs.iter().any(|(k, v)| k == "HTTPS_PROXY" && v.is_empty()),
        "proxy vars should be cleared"
    );
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
fn python_script_target_detects_relative_paths() {
    let root = PathBuf::from("/tmp/project");
    let (arg, path) = python_script_target("src/app.py", &root).expect("relative script detected");
    assert_eq!(arg, "src/app.py");
    assert_eq!(PathBuf::from(path), root.join("src/app.py"));
}

#[test]
fn python_script_target_detects_absolute_paths() {
    let absolute = PathBuf::from("/opt/demo/main.py");
    let entry = absolute.to_string_lossy().to_string();
    let root = PathBuf::from("/tmp/project");
    let (arg, path) = python_script_target(&entry, &root).expect("absolute script detected");
    assert_eq!(arg, entry);
    assert_eq!(PathBuf::from(path), absolute);
}

#[test]
fn python_script_target_ignores_non_python_files() {
    let root = PathBuf::from("/tmp/project");
    assert!(python_script_target("bin/tool", &root).is_none());
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
    materialize_project_site(&site_dir, &lock, None, effects.fs()).expect("materialize site");

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
    materialize_project_site(&site_dir, &lock, None, effects.fs())
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
