use super::system_deps::system_deps_container_args;
use super::*;
use crate::core::system_deps::{resolve_system_deps, write_sys_deps_metadata, SystemDeps};
use px_domain::api::{LockSnapshot, SandboxConfig};
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use tempfile::tempdir;

static PROXY_ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn lock_with_deps(dependencies: Vec<String>) -> LockSnapshot {
    LockSnapshot {
        version: 1,
        project_name: Some("demo".into()),
        python_requirement: Some(">=3.11".into()),
        manifest_fingerprint: Some("fp".into()),
        lock_id: Some("lid".into()),
        dependencies,
        mode: None,
        resolved: vec![],
        graph: None,
        workspace: None,
    }
}

#[test]
fn system_deps_container_avoids_host_apt_cache_mounts() {
    let workdir = PathBuf::from("/tmp/work");
    let proxies = vec!["HTTP_PROXY=".to_string()];
    let args = system_deps_container_args(&workdir, true, &proxies);
    let volume = format!("{}:/work:rw,Z", workdir.display());
    assert!(
        args.windows(2)
            .any(|pair| pair[0] == "--volume" && pair[1] == volume),
        "work directory should be mounted into container"
    );
    assert!(
        args.iter().all(|arg| {
            !arg.contains("apt-cache")
                && !arg.contains("/var/cache/apt")
                && !arg.contains("/var/lib/apt/lists")
        }),
        "system deps container should not mount host apt cache directories"
    );
    assert!(
        args.iter().any(|arg| arg == "--network"),
        "host networking should be enabled when proxies are kept"
    );
}

#[test]
fn internal_containers_forward_proxy_env_when_set() {
    let _guard = PROXY_ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap();
    let keys = [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "NO_PROXY",
        "no_proxy",
        "FTP_PROXY",
    ];
    let originals: Vec<(String, Option<String>)> = keys
        .iter()
        .map(|key| (key.to_string(), env::var(key).ok()))
        .collect();

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
        env::remove_var(key);
    }
    env::set_var("HTTP_PROXY", "http://proxy.example:3128");
    if cfg!(windows) {
        env::set_var("http_proxy", "http://proxy.example:3128");
    } else {
        env::set_var("http_proxy", "http://proxy-lower.example:3128");
    }
    env::set_var("ALL_PROXY", "http://proxy.example:3128");
    env::set_var("NO_PROXY", "localhost,127.0.0.1");
    env::set_var("FTP_PROXY", "http://should-not-pass");

    let backend = ContainerBackend {
        program: PathBuf::from("docker"),
        kind: runner::BackendKind::Docker,
    };
    let proxy_envs = internal_proxy_env_overrides(&backend);
    assert!(
        proxy_envs.contains(&"HTTP_PROXY=http://proxy.example:3128".to_string()),
        "HTTP_PROXY should be forwarded"
    );
    assert!(
        proxy_envs.contains(&format!(
            "http_proxy={}",
            if cfg!(windows) {
                "http://proxy.example:3128"
            } else {
                "http://proxy-lower.example:3128"
            }
        )),
        "http_proxy should be forwarded"
    );
    assert!(
        proxy_envs.contains(&"ALL_PROXY=http://proxy.example:3128".to_string()),
        "ALL_PROXY should be forwarded"
    );
    assert!(
        proxy_envs.contains(&"NO_PROXY=localhost,127.0.0.1".to_string()),
        "NO_PROXY should be forwarded"
    );
    assert!(
        proxy_envs.iter().all(|env| !env.starts_with("FTP_PROXY=")),
        "only allowlisted proxy env vars should be forwarded"
    );

    let workdir = PathBuf::from("/tmp/work");
    let args = system_deps_container_args(&workdir, internal_keep_proxies(), &proxy_envs);
    assert!(
        args.windows(2).any(|pair| {
            pair[0] == "--env" && pair[1] == "HTTP_PROXY=http://proxy.example:3128"
        }),
        "system deps container args should include forwarded proxy"
    );

    for (key, original) in originals {
        match original {
            Some(value) => env::set_var(&key, value),
            None => env::remove_var(&key),
        }
    }
}

#[test]
fn internal_apt_mirror_env_overrides_empty_when_unset() {
    let _guard = PROXY_ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap();
    let keys = ["PX_APT_MIRROR", "PX_APT_SECURITY_MIRROR"];
    let originals: Vec<(String, Option<String>)> = keys
        .iter()
        .map(|key| (key.to_string(), env::var(key).ok()))
        .collect();
    for key in keys {
        env::remove_var(key);
    }

    assert!(
        internal_apt_mirror_env_overrides().is_empty(),
        "apt mirror env list should be empty when unset"
    );

    for (key, original) in originals {
        match original {
            Some(value) => env::set_var(&key, value),
            None => env::remove_var(&key),
        }
    }
}

#[test]
fn internal_apt_mirror_env_overrides_supports_tsinghua_preset() {
    let _guard = PROXY_ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap();
    let keys = ["PX_APT_MIRROR", "PX_APT_SECURITY_MIRROR"];
    let originals: Vec<(String, Option<String>)> = keys
        .iter()
        .map(|key| (key.to_string(), env::var(key).ok()))
        .collect();
    for key in keys {
        env::remove_var(key);
    }
    env::set_var("PX_APT_MIRROR", "tsinghua");

    let envs = internal_apt_mirror_env_overrides();
    assert!(
        envs.contains(&"PX_APT_MIRROR=http://mirrors.tuna.tsinghua.edu.cn/debian".to_string()),
        "tsinghua preset should populate debian mirror"
    );
    assert!(
        envs.contains(
            &"PX_APT_SECURITY_MIRROR=http://mirrors.tuna.tsinghua.edu.cn/debian-security"
                .to_string()
        ),
        "tsinghua preset should populate security mirror"
    );

    for (key, original) in originals {
        match original {
            Some(value) => env::set_var(&key, value),
            None => env::remove_var(&key),
        }
    }
}

#[test]
fn internal_apt_mirror_env_overrides_derives_security_from_debian_suffix() {
    let _guard = PROXY_ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap();
    let keys = ["PX_APT_MIRROR", "PX_APT_SECURITY_MIRROR"];
    let originals: Vec<(String, Option<String>)> = keys
        .iter()
        .map(|key| (key.to_string(), env::var(key).ok()))
        .collect();
    for key in keys {
        env::remove_var(key);
    }
    env::set_var("PX_APT_MIRROR", "https://example.invalid/debian");

    let envs = internal_apt_mirror_env_overrides();
    assert!(
        envs.contains(&"PX_APT_MIRROR=https://example.invalid/debian".to_string()),
        "explicit mirror url should be forwarded"
    );
    assert!(
        envs.contains(
            &"PX_APT_SECURITY_MIRROR=https://example.invalid/debian-security".to_string()
        ),
        "security mirror should be derived when mirror ends with /debian"
    );

    for (key, original) in originals {
        match original {
            Some(value) => env::set_var(&key, value),
            None => env::remove_var(&key),
        }
    }
}

#[test]
fn apt_mirror_setup_snippet_writes_sources_list() {
    let snippet = internal_apt_mirror_setup_snippet();
    assert!(
        snippet.contains("/etc/apt/sources.list"),
        "apt mirror snippet should write sources.list"
    );
    assert!(
        snippet.contains("PX_APT_MIRROR"),
        "apt mirror snippet should be gated on PX_APT_MIRROR"
    );
}

#[test]
fn should_not_disable_apt_proxy_when_all_proxy_is_socks_but_http_proxy_present() {
    let _guard = PROXY_ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap();
    let keys = [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
    ];
    let originals: Vec<(String, Option<String>)> = keys
        .iter()
        .map(|key| (key.to_string(), env::var(key).ok()))
        .collect();
    for key in keys {
        env::remove_var(key);
    }

    env::set_var("HTTP_PROXY", "http://127.0.0.1:3128");
    env::set_var("HTTPS_PROXY", "http://127.0.0.1:3128");
    env::set_var("ALL_PROXY", "socks5h://127.0.0.1:12334");
    assert!(
        !should_disable_apt_proxy(),
        "ALL_PROXY socks should not disable apt when HTTP(S)_PROXY is set"
    );
    assert!(
        internal_keep_proxies(),
        "internal containers should keep proxies when HTTP(S)_PROXY is set"
    );

    for (key, original) in originals {
        match original {
            Some(value) => env::set_var(&key, value),
            None => env::remove_var(&key),
        }
    }
}

#[test]
fn internal_containers_do_not_forward_proxy_env_when_unset() {
    let _guard = PROXY_ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap();
    let keys = [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "NO_PROXY",
        "no_proxy",
    ];
    let originals: Vec<(String, Option<String>)> = keys
        .iter()
        .map(|key| (key.to_string(), env::var(key).ok()))
        .collect();
    for key in keys {
        env::remove_var(key);
    }

    let backend = ContainerBackend {
        program: PathBuf::from("docker"),
        kind: runner::BackendKind::Docker,
    };
    let proxy_envs = internal_proxy_env_overrides(&backend);
    for key in keys {
        assert!(
            proxy_envs.contains(&format!("{key}=")),
            "{key} should be cleared when unset"
        );
    }
    assert!(
        !internal_keep_proxies(),
        "internal_keep_proxies should be false without proxies"
    );

    for (key, original) in originals {
        match original {
            Some(value) => env::set_var(&key, value),
            None => env::remove_var(&key),
        }
    }
}

#[test]
fn sandbox_id_is_stable_when_capabilities_unsorted() {
    let mut caps_a = BTreeSet::new();
    caps_a.insert("postgres".to_string());
    caps_a.insert("imagecodecs".to_string());
    let sys_deps_a = resolve_system_deps(&caps_a, None);
    let def_a = SandboxDefinition {
        base_os_oid: "base".to_string(),
        capabilities: caps_a,
        system_deps: sys_deps_a,
        profile_oid: "profile".to_string(),
        sbx_version: SBX_VERSION,
    };
    let mut caps_b = BTreeSet::new();
    caps_b.insert("imagecodecs".to_string());
    caps_b.insert("postgres".to_string());
    let sys_deps_b = resolve_system_deps(&caps_b, None);
    let def_b = SandboxDefinition {
        base_os_oid: "base".to_string(),
        capabilities: caps_b,
        system_deps: sys_deps_b,
        profile_oid: "profile".to_string(),
        sbx_version: SBX_VERSION,
    };
    assert_eq!(def_a.sbx_id(), def_b.sbx_id());
}

#[test]
fn sandbox_id_reflects_system_dep_versions() {
    let mut caps = BTreeSet::new();
    caps.insert("postgres".to_string());
    let mut deps_a = resolve_system_deps(&caps, None);
    deps_a.apt_packages.insert("libpq-dev".into());
    deps_a.apt_versions.insert("libpq-dev".into(), "1".into());
    let def_a = SandboxDefinition {
        base_os_oid: "base".to_string(),
        capabilities: caps.clone(),
        system_deps: deps_a,
        profile_oid: "profile".to_string(),
        sbx_version: SBX_VERSION,
    };
    let mut deps_b = resolve_system_deps(&caps, None);
    deps_b.apt_packages.insert("libpq-dev".into());
    deps_b.apt_versions.insert("libpq-dev".into(), "2".into());
    let def_b = SandboxDefinition {
        base_os_oid: "base".to_string(),
        capabilities: caps,
        system_deps: deps_b,
        profile_oid: "profile".to_string(),
        sbx_version: SBX_VERSION,
    };
    assert_ne!(def_a.sbx_id(), def_b.sbx_id());
}

#[test]
fn ensure_image_writes_manifest_and_reuses_store() {
    let temp = tempdir().expect("tempdir");
    let previous_mode = env::var("PX_SYSTEM_DEPS_MODE").ok();
    env::set_var("PX_SYSTEM_DEPS_MODE", "offline");
    let mut config = SandboxConfig {
        auto: false,
        ..Default::default()
    };
    config.capabilities.insert("postgres".to_string(), true);
    let lock = lock_with_deps(vec![]);
    let store = SandboxStore::new(temp.path().to_path_buf());
    let env_root = temp.path().join("env");
    fs::create_dir_all(&env_root).expect("env root");
    let artifacts = ensure_sandbox_image(
        &store,
        &config,
        Some(&lock),
        None,
        "profile-1",
        &env_root,
        None,
    )
    .expect("sandbox artifacts");
    assert_eq!(artifacts.definition.sbx_id(), artifacts.manifest.sbx_id);
    let again = ensure_sandbox_image(
        &store,
        &config,
        Some(&lock),
        None,
        "profile-1",
        &env_root,
        None,
    )
    .expect("reuse sandbox image");
    assert_eq!(artifacts.manifest.sbx_id, again.manifest.sbx_id);
    match previous_mode {
        Some(value) => env::set_var("PX_SYSTEM_DEPS_MODE", value),
        None => env::remove_var("PX_SYSTEM_DEPS_MODE"),
    }
}

#[test]
fn site_inference_detects_postgres_library() {
    let temp = tempdir().expect("tempdir");
    let site = temp.path().join("site");
    fs::create_dir_all(&site).expect("create site dir");
    fs::write(site.join("libpq.so.5"), b"").expect("write sentinel");
    let config = SandboxConfig::default();
    let lock = lock_with_deps(vec![]);
    let resolved =
        resolve_sandbox_definition(&config, Some(&lock), None, "profile", Some(site.as_path()))
            .expect("resolution");
    assert!(
        resolved.definition.capabilities.contains("postgres"),
        "libpq should infer postgres capability"
    );
}

#[test]
fn lock_inference_detects_gdal_stack() {
    let config = SandboxConfig::default();
    let lock = lock_with_deps(vec!["gdal==3.8.0".into()]);
    let resolved = resolve_sandbox_definition(&config, Some(&lock), None, "profile", None)
        .expect("resolution");
    assert!(
        resolved.definition.capabilities.contains("gdal"),
        "gdal package should infer gdal capability"
    );
}

#[test]
fn site_inference_detects_gdal_library() {
    let temp = tempdir().expect("tempdir");
    let site = temp.path().join("site");
    fs::create_dir_all(&site).expect("create site dir");
    fs::write(site.join("libgdal.so.34"), b"").expect("write sentinel");
    let config = SandboxConfig::default();
    let lock = lock_with_deps(vec![]);
    let resolved =
        resolve_sandbox_definition(&config, Some(&lock), None, "profile", Some(site.as_path()))
            .expect("resolution");
    assert!(
        resolved.definition.capabilities.contains("gdal"),
        "libgdal should infer gdal capability"
    );
}

#[test]
fn site_inference_reads_builder_metadata() {
    let temp = tempdir().expect("tempdir");
    let site = temp.path().join("site");
    fs::create_dir_all(&site).expect("create site dir");
    let deps = SystemDeps {
        capabilities: ["postgres".into()].into_iter().collect(),
        apt_packages: ["libpq-dev".into()].into_iter().collect(),
        apt_versions: [("libpq-dev".into(), "1.0".into())].into_iter().collect(),
    };
    write_sys_deps_metadata(&site, "demo", &deps).expect("write metadata");
    let config = SandboxConfig::default();
    let lock = lock_with_deps(vec![]);
    let resolved =
        resolve_sandbox_definition(&config, Some(&lock), None, "profile", Some(site.as_path()))
            .expect("resolution");
    assert!(
        resolved.definition.capabilities.contains("postgres"),
        "metadata should propagate capabilities"
    );
    assert!(
        resolved
            .definition
            .system_deps
            .apt_packages
            .contains("libpq-dev"),
        "apt packages from metadata should propagate"
    );
    assert_eq!(
        resolved
            .definition
            .system_deps
            .apt_versions
            .get("libpq-dev"),
        Some(&"1.0".to_string())
    );
}

#[test]
fn explicit_false_overrides_inference() {
    let mut config = SandboxConfig::default();
    config.capabilities.insert("postgres".into(), false);
    let lock = lock_with_deps(vec!["psycopg2".into()]);
    let resolved = resolve_sandbox_definition(&config, Some(&lock), None, "profile", None)
        .expect("resolution");
    assert!(
        !resolved.definition.capabilities.contains("postgres"),
        "explicit false should disable inferred capability"
    );
}
