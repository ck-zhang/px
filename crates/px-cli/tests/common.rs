#![allow(dead_code)]

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    fs, io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Mutex,
};

use assert_cmd::{assert::Assert, cargo::cargo_bin_cmd};
use serde_json::Value;
use std::cell::RefCell;
use std::env;
use std::sync::OnceLock;
use tempfile::TempDir;
use toml_edit::DocumentMut;

thread_local! {
    static TEST_CACHE: RefCell<Option<TempDir>> = const { RefCell::new(None) };
}

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn serial_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

pub fn reset_test_store_env() {
    let _guard = env_lock().lock().unwrap();
    TEST_CACHE.with(|cell| {
        if let Some(dir) = cell.borrow_mut().take() {
            let _ = dir.close();
        }
    });
    for key in [
        "PX_CACHE_PATH",
        "PX_STORE_PATH",
        "PX_ENVS_PATH",
        "PX_TOOLS_DIR",
        "PX_ONLINE",
        "PX_FORCE_SDIST",
        "PX_RUNTIME_PYTHON",
        "PX_KEEP_PROXIES",
        "PX_INDEX_URL",
        "PIP_INDEX_URL",
        "PIP_EXTRA_INDEX_URL",
        "PX_RUNTIME_HOST_ONLY",
        "PX_NO_ENSUREPIP",
        "PX_DEBUG_PIP",
        "PX_SYSTEM_DEPS_MODE",
        "PX_SYSTEM_DEPS_OFFLINE",
    ] {
        env::remove_var(key);
    }
}

pub fn test_env_guard() -> std::sync::MutexGuard<'static, ()> {
    serial_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

pub fn ensure_test_store_env() {
    let _guard = env_lock().lock().unwrap();
    TEST_CACHE.with(|cell| {
        if cell.borrow().is_none() {
            let dir = tempfile::Builder::new()
                .prefix("px-test-cache")
                .tempdir()
                .expect("tempdir");
            *cell.borrow_mut() = Some(dir);
        }
        let cache = cell
            .borrow()
            .as_ref()
            .expect("cache dir")
            .path()
            .to_path_buf();
        let store = cache.join("store");
        let envs = cache.join("envs");
        let tools = cache.join("tools");
        let _ = fs::create_dir_all(&store);
        let _ = fs::create_dir_all(&envs);
        let _ = fs::create_dir_all(&tools);
        env::set_var("PX_CACHE_PATH", &cache);
        env::set_var("PX_STORE_PATH", store);
        env::set_var("PX_ENVS_PATH", envs);
        env::set_var("PX_TOOLS_DIR", tools);
        env::set_var("PX_NO_ENSUREPIP", "1");
        env::set_var("PX_RUNTIME_HOST_ONLY", "1");
        if env::var_os("PX_RUNTIME_PYTHON").is_none() {
            if let Some(python) = find_python() {
                env::set_var("PX_RUNTIME_PYTHON", python);
            }
        }
        env::set_var("PX_SYSTEM_DEPS_MODE", "offline");
    });
}
#[must_use]
/// Copies the sample fixture into a temporary directory.
///
/// # Panics
/// Panics if the temporary directory cannot be created or the fixture copy fails.
pub fn prepare_fixture(prefix: &str) -> (TempDir, PathBuf) {
    prepare_named_fixture("sample_px_app", prefix)
}

#[must_use]
/// Copies a named fixture directory into a temporary location.
///
/// # Panics
/// Panics if the temporary directory cannot be created or the fixture copy fails.
pub fn prepare_named_fixture(fixture: &str, prefix: &str) -> (TempDir, PathBuf) {
    reset_test_store_env();
    ensure_test_store_env();
    let temp = tempfile::Builder::new()
        .prefix(prefix)
        .tempdir()
        .expect("tempdir");
    let dst = temp.path().join(fixture);
    copy_dir_all(&fixture_root(fixture), &dst).expect("copy fixture");
    (temp, dst)
}

#[must_use]
/// Returns the root of the workspace.
///
/// # Panics
/// Panics if the workspace root cannot be determined from `CARGO_MANIFEST_DIR`.
pub fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .to_path_buf()
}

#[must_use]
pub fn fixture_source() -> PathBuf {
    workspace_root().join("fixtures").join("sample_px_app")
}

#[must_use]
pub fn fixture_root(name: &str) -> PathBuf {
    workspace_root().join("fixtures").join(name)
}

#[must_use]
pub fn traceback_fixture_source() -> PathBuf {
    workspace_root().join("fixtures").join("traceback_lab")
}

#[must_use]
/// Copies the traceback fixture into a temporary directory.
///
/// # Panics
/// Panics if the temporary directory cannot be created or copying fails.
pub fn prepare_traceback_fixture(prefix: &str) -> (TempDir, PathBuf) {
    reset_test_store_env();
    ensure_test_store_env();
    let temp = tempfile::Builder::new()
        .prefix(prefix)
        .tempdir()
        .expect("tempdir");
    let dst = temp.path().join("traceback_lab");
    copy_dir_all(&traceback_fixture_source(), &dst).expect("copy traceback fixture");
    (temp, dst)
}

#[must_use]
pub fn find_python() -> Option<String> {
    let candidates = [
        std::env::var("PYTHON").ok(),
        Some("python3".to_string()),
        Some("python".to_string()),
    ];
    for candidate in candidates.into_iter().flatten() {
        let status = Command::new(&candidate)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if matches!(status, Ok(code) if code.success()) {
            return Some(candidate);
        }
    }
    None
}

pub fn detect_host_python(python: &str) -> Option<(String, String)> {
    const INSPECT_SCRIPT: &str =
        "import json, platform, sys; print(json.dumps({'version': platform.python_version(), 'executable': sys.executable}))";
    let output = Command::new(python)
        .arg("-c")
        .arg(INSPECT_SCRIPT)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let payload: Value = serde_json::from_slice(&output.stdout).ok()?;
    let executable = payload.get("executable")?.as_str()?.to_string();
    let version = payload.get("version")?.as_str()?.to_string();
    let mut parts = version.split('.');
    let major = parts.next().unwrap_or("0");
    let minor = parts.next().unwrap_or("0");
    let channel = format!("{major}.{minor}");
    Some((executable, channel))
}

pub fn fake_sandbox_backend(dir: &Path) -> io::Result<(PathBuf, PathBuf)> {
    let script = dir.join("fake-sandbox-backend.sh");
    let log = dir.join("sandbox.log");
    let contents = r#"#!/bin/sh
log="${PX_FAKE_SANDBOX_LOG:-}"
cmd="$1"
shift
case "$cmd" in
    image)
        if [ "$1" = "inspect" ]; then
            exit ${PX_FAKE_SANDBOX_INSPECT_EXIT:-1}
        fi
        ;;
    load)
        while [ "$#" -gt 0 ]; do
            if [ "$1" = "--input" ] && [ -n "$log" ]; then
                shift
                echo "load:$1" >> "$log"
            fi
            shift
        done
        echo "loaded"
        exit 0
        ;;
    run)
        workdir=""
        project_root="${PX_FAKE_SANDBOX_PROJECT_ROOT:-}"
        while [ "$#" -gt 0 ]; do
            case "$1" in
                --workdir|-w)
                    shift
                    workdir="$1"
                    if [ "$workdir" = "/app" ] && [ -n "$project_root" ]; then
                        workdir="$project_root"
                    fi
                    shift
                    continue
                    ;;
                --volume|-v|--env|-e)
                    shift
                    shift
                    continue
                    ;;
                -i|--rm)
                    shift
                    continue
                    ;;
                *)
                    tag="$1"
                    shift
                    break
                    ;;
            esac
        done
        if [ -n "$log" ]; then
            echo "run:${tag}:$*" >> "$log"
        fi
        if [ -n "$workdir" ]; then
            cd "$workdir" 2>/dev/null || true
        fi
        if [ "$#" -gt 0 ]; then
        if [ -n "$project_root" ]; then
            mapped=""
            while [ "$#" -gt 0 ]; do
                case "$1" in
                    /app/*)
                        mapped="$mapped $project_root/${1#/app/}"
                        ;;
                    *)
                        mapped="$mapped $1"
                        ;;
                esac
                shift
            done
            # shellcheck disable=SC2086
            set -- $mapped
        fi
        if [ -n "$PX_FAKE_SANDBOX_ENV_ROOT" ]; then
            case "$1" in
                /px/env/*)
                        candidate="$PX_FAKE_SANDBOX_ENV_ROOT/${1#/px/env/}"
                        if [ -x "$candidate" ]; then
                            shift
                            set -- "$candidate" "$@"
                        fi
                        ;;
                esac
            fi
            case "$1" in
                /px/env/bin/python|/px/env/bin/python3|/px/env/bin/python3.*)
                    if [ -n "$PX_RUNTIME_PYTHON" ]; then
                        shift
                        set -- "$PX_RUNTIME_PYTHON" "$@"
                    fi
                    ;;
            esac
        fi
        exec "$@"
        ;;
esac
exit 0
"#;
    fs::write(&script, contents)?;
    #[cfg(unix)]
    {
        let mut perms = fs::metadata(&script)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script, perms)?;
    }
    Ok((script, log))
}

fn copy_dir_all(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_all(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

#[must_use]
/// Parses the JSON payload from a completed command assertion.
///
/// # Panics
/// Panics if the output cannot be parsed as JSON.
pub fn parse_json(assert: &Assert) -> Value {
    serde_json::from_slice(&assert.get_output().stdout).expect("valid json")
}

#[must_use]
/// Initializes an empty px project with the provided prefix.
///
/// # Panics
/// Panics if the temporary project directory cannot be created or commands fail.
pub fn init_empty_project(prefix: &str) -> (TempDir, PathBuf) {
    reset_test_store_env();
    ensure_test_store_env();
    let temp = tempfile::Builder::new()
        .prefix(prefix)
        .tempdir()
        .expect("tempdir");
    cargo_bin_cmd!("px")
        .current_dir(temp.path())
        .arg("init")
        .assert()
        .success();
    // Materialize the environment up front so build/publish paths in tests start
    // from a consistent, ready state.
    let _ = cargo_bin_cmd!("px")
        .current_dir(temp.path())
        .arg("sync")
        .assert();
    let root = temp.path().to_path_buf();
    (temp, root)
}

#[must_use]
/// Reads the project identity from the pyproject manifest.
///
/// # Panics
/// Panics if the manifest cannot be read or parsed.
pub fn project_identity(root: &Path) -> (String, String, String) {
    let pyproject = root.join("pyproject.toml");
    let doc: DocumentMut = fs::read_to_string(&pyproject)
        .expect("read pyproject")
        .parse()
        .expect("parse pyproject");
    let name = doc["project"]["name"]
        .as_str()
        .expect("project name")
        .to_string();
    let version = doc["project"]["version"]
        .as_str()
        .expect("project version")
        .to_string();
    let normalized = name.replace('-', "_");
    (name, normalized, version)
}

#[must_use]
pub fn require_online() -> bool {
    if let Some("1") = env::var("PX_ONLINE").ok().as_deref() {
        true
    } else {
        eprintln!("skipping test that needs PX_ONLINE=1");
        false
    }
}

#[must_use]
/// Retrieves the cached artifact path for the specified dependency.
///
/// # Panics
/// Panics if the lockfile cannot be read or the dependency entry is missing.
pub fn artifact_from_lock(project_root: &Path, name: &str) -> PathBuf {
    let lock = project_root.join("px.lock");
    let contents = fs::read_to_string(&lock).expect("read lock");
    let doc: DocumentMut = contents.parse().expect("valid lock");
    let deps = doc["dependencies"]
        .as_array_of_tables()
        .expect("deps table");
    let entry = deps
        .iter()
        .find(|table| table.get("name").and_then(toml_edit::Item::as_str) == Some(name))
        .expect("dependency entry");
    let artifact = entry["artifact"].as_table().expect("artifact table");
    let path = artifact
        .get("cached_path")
        .and_then(toml_edit::Item::as_str)
        .expect("cached path");
    PathBuf::from(path)
}
