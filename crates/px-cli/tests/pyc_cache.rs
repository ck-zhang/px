use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::cargo::cargo_bin_cmd;
use serde_json::Value;

mod common;

use common::{find_python, parse_json, prepare_fixture, test_env_guard};

fn project_state(project_root: &Path) -> Value {
    let path = project_root.join(".px").join("state.json");
    let contents = fs::read_to_string(&path).expect("read state.json");
    serde_json::from_str(&contents).expect("parse state.json")
}

fn project_profile_oid(project_root: &Path) -> String {
    let state = project_state(project_root);
    let env = state["current_env"].as_object().expect("current_env");
    env.get("profile_oid")
        .and_then(Value::as_str)
        .or_else(|| env.get("id").and_then(Value::as_str))
        .expect("profile oid")
        .to_string()
}

#[cfg(unix)]
fn make_read_only_recursive(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = fs::metadata(path) {
        let mode = if meta.is_dir() { 0o555 } else { 0o444 };
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
        if meta.is_dir() {
            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries.flatten() {
                    make_read_only_recursive(&entry.path());
                }
            }
        }
    }
}

fn contains_suffix(root: &Path, suffix: &str) -> bool {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_dir() {
                if path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n == suffix)
                {
                    return true;
                }
                stack.push(path);
            } else if ft.is_file()
                && path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| ext.eq_ignore_ascii_case(suffix))
            {
                return true;
            }
        }
    }
    false
}

#[test]
fn pyc_cache_prefix_is_deterministic_and_store_stays_clean() {
    let _guard = test_env_guard();
    let Some(python) = find_python() else {
        eprintln!("skipping pyc cache test (python binary not found)");
        return;
    };
    let (_tmp, project) = prepare_fixture("pyc-cache");

    cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .env_remove("PYTHONPYCACHEPREFIX")
        .env_remove("PYTHONDONTWRITEBYTECODE")
        .arg("sync")
        .assert()
        .success();

    let profile_oid = project_profile_oid(&project);
    let cache_root = PathBuf::from(env::var("PX_CACHE_PATH").expect("PX_CACHE_PATH"));
    let store_root = PathBuf::from(env::var("PX_STORE_PATH").expect("PX_STORE_PATH"));
    let expected_prefix = cache_root.join("pyc").join(&profile_oid);

    let fake_pkg_site = store_root
        .join("materialized-pkg-builds")
        .join("fake-pkg-build")
        .join("site-packages");
    fs::create_dir_all(&fake_pkg_site).expect("create fake pkg site");
    let fake_module = fake_pkg_site.join("fake_dep.py");
    fs::write(
        &fake_module,
        "VALUE = 'OK'\n\ndef ping():\n    return 'PONG'\n",
    )
    .expect("write fake module");
    #[cfg(unix)]
    make_read_only_recursive(&fake_pkg_site);

    let script = format!(
        r#"
import os, sys
sys.path.insert(0, {fake_pkg_site:?})
import fake_dep
print("PYTHONPYCACHEPREFIX=" + os.environ.get("PYTHONPYCACHEPREFIX",""))
print("CACHED=" + (fake_dep.__cached__ or ""))
print("PING=" + fake_dep.ping())
"#,
        fake_pkg_site = fake_pkg_site.display().to_string()
    );

    let run_once = || -> (String, String) {
        let assert = cargo_bin_cmd!("px")
            .current_dir(&project)
            .env("PX_RUNTIME_PYTHON", &python)
            .env_remove("PYTHONPYCACHEPREFIX")
            .env_remove("PYTHONDONTWRITEBYTECODE")
            .args(["--json", "run", "--frozen", "python", "-c", &script])
            .assert()
            .success();
        let payload = parse_json(&assert);
        assert_eq!(payload["status"], "ok");
        let stdout = payload["details"]["stdout"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let mut prefix = String::new();
        let mut cached = String::new();
        for line in stdout.lines() {
            if let Some(value) = line.strip_prefix("PYTHONPYCACHEPREFIX=") {
                prefix = value.trim().to_string();
            } else if let Some(value) = line.strip_prefix("CACHED=") {
                cached = value.trim().to_string();
            }
        }
        assert!(
            stdout.contains("PING=PONG"),
            "expected module import to succeed, got stdout={stdout:?}"
        );
        (prefix, cached)
    };

    let (prefix_first, cached_first) = run_once();
    assert_eq!(
        PathBuf::from(&prefix_first),
        expected_prefix,
        "PYTHONPYCACHEPREFIX should use the px cache root"
    );
    assert!(
        !cached_first.is_empty(),
        "expected __cached__ path, got empty"
    );
    let cached_path = PathBuf::from(&cached_first);
    assert!(
        cached_path.starts_with(&expected_prefix),
        "__cached__ should live under the px cache dir, got {cached_first:?}"
    );
    assert!(
        cached_path.exists(),
        "expected pyc file to exist at {}",
        cached_path.display()
    );
    assert!(
        !fake_pkg_site.join("__pycache__").exists(),
        "expected no __pycache__ under store-backed site-packages"
    );
    assert!(
        !contains_suffix(&store_root, "pyc"),
        "expected no .pyc files under PX_STORE_PATH"
    );
    assert!(
        !contains_suffix(&store_root, "__pycache__"),
        "expected no __pycache__ dirs under PX_STORE_PATH"
    );

    let (prefix_second, cached_second) = run_once();
    assert_eq!(prefix_second, prefix_first, "cache prefix should be stable");
    assert_eq!(
        cached_second, cached_first,
        "__cached__ path should be stable for the same profile"
    );
}
