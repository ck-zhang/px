use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use assert_cmd::cargo::cargo_bin_cmd;
use px_domain::api::ProjectSnapshot;
use serde_json::Value;

mod common;

use common::{
    ensure_test_store_env, fake_sandbox_backend, find_python, parse_json, reset_test_store_env,
    test_env_guard,
};

const WHEEL_BUILDER: &str = r#"
import hashlib
import json
import zipfile
from pathlib import Path

payload = json.load(sys.stdin)
wheel_path = Path(payload["wheel_path"])
dist_name = payload["dist_name"]
version = payload["version"]
tag = payload.get("tag") or "py3-none-any"
files = payload.get("files") or []
console_scripts = payload.get("console_scripts") or []

dist_info = f"{dist_name.replace('-', '_')}-{version}.dist-info"
metadata = "\n".join(
    [
        "Metadata-Version: 2.1",
        f"Name: {dist_name}",
        f"Version: {version}",
        "",
    ]
)
wheel = "\n".join(
    [
        "Wheel-Version: 1.0",
        "Generator: px-test",
        "Root-Is-Purelib: true",
        f"Tag: {tag}",
        "",
    ]
)
entry_points = ""
if console_scripts:
    lines = ["[console_scripts]"]
    for entry in console_scripts:
        lines.append(f"{entry['name']} = {entry['value']}")
    lines.append("")
    entry_points = "\n".join(lines)

wheel_path.parent.mkdir(parents=True, exist_ok=True)
with zipfile.ZipFile(wheel_path, "w", compression=zipfile.ZIP_DEFLATED) as zf:
    zf.writestr(f"{dist_info}/METADATA", metadata)
    zf.writestr(f"{dist_info}/WHEEL", wheel)
    if entry_points:
        zf.writestr(f"{dist_info}/entry_points.txt", entry_points)
    for entry in files:
        kind = entry.get("kind")
        rel = entry["path"]
        if kind == "text":
            zf.writestr(rel, entry["contents"])
        elif kind == "file":
            zf.write(entry["src"], rel)
        else:
            raise SystemExit(f"unknown file kind: {kind!r}")

data = wheel_path.read_bytes()
print(json.dumps({"sha256": hashlib.sha256(data).hexdigest(), "size": len(data)}))
"#;

fn write_pyproject(root: &Path, name: &str, python_req: &str, deps: &[String]) {
    let mut buf = String::new();
    buf.push_str("[project]\n");
    buf.push_str(&format!("name = \"{name}\"\n"));
    buf.push_str("version = \"0.0.0\"\n");
    buf.push_str(&format!("requires-python = \"{python_req}\"\n"));
    buf.push_str("dependencies = [\n");
    for dep in deps {
        buf.push_str(&format!("    {dep:?},\n"));
    }
    buf.push_str("]\n\n[tool.px]\n\n[build-system]\n");
    buf.push_str("requires = [\"setuptools>=70\", \"wheel\"]\n");
    buf.push_str("build-backend = \"setuptools.build_meta\"\n");
    fs::write(root.join("pyproject.toml"), buf).expect("write pyproject.toml");
}

struct LockEntry {
    name: String,
    version: String,
    filename: String,
    cached_path: String,
    sha256: String,
    size: u64,
}

fn write_lock(root: &Path, project_name: &str, python_req: &str, deps: &[LockEntry]) {
    let snapshot = ProjectSnapshot::read_from(root).expect("read project snapshot");
    let manifest_fingerprint = snapshot.manifest_fingerprint;
    let mut buf = String::new();
    buf.push_str("version = 1\n\n[metadata]\nmode = \"p0-pinned\"\n\n");
    buf.push_str(&format!(
        "manifest_fingerprint = \"{manifest_fingerprint}\"\n\n"
    ));
    buf.push_str("[project]\n");
    buf.push_str(&format!("name = \"{project_name}\"\n\n"));
    buf.push_str("[python]\n");
    buf.push_str(&format!("requirement = \"{python_req}\"\n\n"));

    for dep in deps {
        buf.push_str("[[dependencies]]\n");
        buf.push_str(&format!("name = \"{}\"\n", dep.name));
        buf.push_str(&format!("specifier = \"{}=={}\"\n", dep.name, dep.version));
        buf.push_str("direct = true\n");
        buf.push_str("[dependencies.artifact]\n");
        buf.push_str(&format!("filename = \"{}\"\n", dep.filename));
        buf.push_str(&format!(
            "url = \"https://example.invalid/{}\"\n",
            dep.filename
        ));
        buf.push_str(&format!("sha256 = \"{}\"\n", dep.sha256));
        buf.push_str(&format!("size = {}\n", dep.size));
        buf.push_str(&format!("cached_path = \"{}\"\n", dep.cached_path));
        buf.push_str("python_tag = \"py3\"\nabi_tag = \"none\"\nplatform_tag = \"any\"\n\n");
    }

    fs::write(root.join("px.lock"), buf).expect("write px.lock");
}

fn build_wheel(
    python: &str,
    wheel_path: &Path,
    dist_name: &str,
    version: &str,
    files: Vec<Value>,
    console_scripts: Vec<(&str, &str)>,
) -> (String, u64) {
    let scripts = console_scripts
        .into_iter()
        .map(|(name, value)| serde_json::json!({ "name": name, "value": value }))
        .collect::<Vec<_>>();
    let payload = serde_json::json!({
        "wheel_path": wheel_path.display().to_string(),
        "dist_name": dist_name,
        "version": version,
        "tag": "py3-none-any",
        "files": files,
        "console_scripts": scripts,
    });

    let mut child = Command::new(python)
        .arg("-c")
        .arg(format!("import sys\n{WHEEL_BUILDER}"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn python wheel builder");
    {
        let mut stdin = child.stdin.take().expect("stdin");
        stdin
            .write_all(payload.to_string().as_bytes())
            .expect("write payload");
    }
    let output = child.wait_with_output().expect("wheel builder output");
    assert!(
        output.status.success(),
        "wheel builder failed stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let meta: Value = serde_json::from_slice(&output.stdout).expect("wheel builder json");
    let sha256 = meta["sha256"].as_str().expect("sha256").to_string();
    let size = meta["size"].as_u64().expect("size");
    (sha256, size)
}

fn envs_dir() -> PathBuf {
    PathBuf::from(env::var("PX_ENVS_PATH").expect("PX_ENVS_PATH"))
}

fn assert_envs_empty(context: &str) {
    let envs = envs_dir();
    let entries = fs::read_dir(&envs)
        .unwrap_or_else(|_| panic!("read envs dir {} ({context})", envs.display()))
        .flatten()
        .collect::<Vec<_>>();
    assert!(
        entries.is_empty(),
        "expected no env materialization under {}, found {:?} ({context})",
        envs.display(),
        entries.iter().map(|e| e.path()).collect::<Vec<_>>()
    );
}

fn assert_envs_non_empty(context: &str) {
    let envs = envs_dir();
    let entries = fs::read_dir(&envs)
        .unwrap_or_else(|_| panic!("read envs dir {} ({context})", envs.display()))
        .flatten()
        .collect::<Vec<_>>();
    assert!(
        !entries.is_empty(),
        "expected env materialization under {} ({context})",
        envs.display()
    );
}

#[test]
fn cas_native_runs_console_script_without_env_materialization() {
    let _guard = test_env_guard();
    reset_test_store_env();
    ensure_test_store_env();
    let Some(python) = find_python() else {
        eprintln!("skipping cas-native console script test (python binary not found)");
        return;
    };
    let temp = tempfile::Builder::new()
        .prefix("px-cas-native-console")
        .tempdir()
        .expect("tempdir");
    let project = temp.path();
    let artifacts = project.join("artifacts");
    fs::create_dir_all(&artifacts).expect("artifacts dir");

    let wheel = artifacts.join("hello_console-0.1.0-py3-none-any.whl");
    let (sha256, size) = build_wheel(
        &python,
        &wheel,
        "hello_console",
        "0.1.0",
        vec![serde_json::json!({
            "kind": "text",
            "path": "hello_console.py",
            "contents": "def main():\n    print('hello from console')\n",
        })],
        vec![("hello-console", "hello_console:main")],
    );

    write_pyproject(
        project,
        "cas_native_console",
        ">=3.11",
        &["hello_console==0.1.0".to_string()],
    );
    write_lock(
        project,
        "cas_native_console",
        ">=3.11",
        &[LockEntry {
            name: "hello_console".to_string(),
            version: "0.1.0".to_string(),
            filename: wheel
                .file_name()
                .expect("wheel filename")
                .to_string_lossy()
                .to_string(),
            cached_path: "artifacts/hello_console-0.1.0-py3-none-any.whl".to_string(),
            sha256,
            size,
        }],
    );

    assert_envs_empty("before run");
    let assert = cargo_bin_cmd!("px")
        .current_dir(project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["--json", "run", "hello-console"])
        .assert()
        .success();
    assert_envs_empty("after run");

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    let stdout = payload["details"]["stdout"].as_str().unwrap_or_default();
    assert!(
        stdout.contains("hello from console"),
        "expected console script output, got {stdout:?}"
    );
}

#[test]
fn cas_native_python_minus_s_can_import_profile_deps() {
    let _guard = test_env_guard();
    reset_test_store_env();
    ensure_test_store_env();
    let Some(python) = find_python() else {
        eprintln!("skipping cas-native python -S test (python binary not found)");
        return;
    };
    let temp = tempfile::Builder::new()
        .prefix("px-cas-native-minus-s")
        .tempdir()
        .expect("tempdir");
    let project = temp.path();
    let artifacts = project.join("artifacts");
    fs::create_dir_all(&artifacts).expect("artifacts dir");

    let wheel = artifacts.join("hello_console-0.1.0-py3-none-any.whl");
    let (sha256, size) = build_wheel(
        &python,
        &wheel,
        "hello_console",
        "0.1.0",
        vec![serde_json::json!({
            "kind": "text",
            "path": "hello_console.py",
            "contents": "def main():\n    print('hello from console')\n",
        })],
        vec![],
    );

    write_pyproject(
        project,
        "cas_native_minus_s",
        ">=3.11",
        &["hello_console==0.1.0".to_string()],
    );
    write_lock(
        project,
        "cas_native_minus_s",
        ">=3.11",
        &[LockEntry {
            name: "hello_console".to_string(),
            version: "0.1.0".to_string(),
            filename: wheel
                .file_name()
                .expect("wheel filename")
                .to_string_lossy()
                .to_string(),
            cached_path: "artifacts/hello_console-0.1.0-py3-none-any.whl".to_string(),
            sha256,
            size,
        }],
    );

    assert_envs_empty("before run");
    let assert = cargo_bin_cmd!("px")
        .current_dir(project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args([
            "--json",
            "run",
            "python",
            "-S",
            "-c",
            "import hello_console; hello_console.main()",
        ])
        .assert()
        .success();
    assert_envs_empty("after run");

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    let stdout = payload["details"]["stdout"].as_str().unwrap_or_default();
    assert!(
        stdout.contains("hello from console"),
        "expected import output, got {stdout:?}"
    );
}

#[cfg(unix)]
#[test]
fn cas_native_loads_extension_module() {
    let _guard = test_env_guard();
    reset_test_store_env();
    ensure_test_store_env();
    let Some(python) = find_python() else {
        eprintln!("skipping cas-native extension test (python binary not found)");
        return;
    };
    let temp = tempfile::Builder::new()
        .prefix("px-cas-native-ext")
        .tempdir()
        .expect("tempdir");
    let project = temp.path();
    let artifacts = project.join("artifacts");
    fs::create_dir_all(&artifacts).expect("artifacts dir");

    let config = Command::new(&python)
        .args([
            "-c",
            "import json, sysconfig; print(json.dumps({'include': sysconfig.get_path('include'), 'ext': sysconfig.get_config_var('EXT_SUFFIX') or '.so'}))",
        ])
        .output()
        .expect("python sysconfig");
    assert!(config.status.success(), "sysconfig probe failed");
    let cfg: Value = serde_json::from_slice(&config.stdout).expect("sysconfig json");
    let include = cfg["include"].as_str().expect("include");
    let ext = cfg["ext"].as_str().expect("ext suffix");

    let c_path = artifacts.join("native_ext.c");
    fs::write(
        &c_path,
        r#"#include <Python.h>

static PyObject* answer(PyObject* self, PyObject* args) {
    return PyLong_FromLong(42);
}

static PyMethodDef Methods[] = {
    {"answer", answer, METH_NOARGS, "Return 42."},
    {NULL, NULL, 0, NULL}
};

static struct PyModuleDef module = {
    PyModuleDef_HEAD_INIT,
    "native_ext",
    NULL,
    -1,
    Methods
};

PyMODINIT_FUNC PyInit_native_ext(void) {
    return PyModule_Create(&module);
}
"#,
    )
    .expect("write c source");

    let so_name = format!("native_ext{ext}");
    let so_path = artifacts.join(&so_name);
    let status = Command::new("cc")
        .args([
            "-shared",
            "-fPIC",
            &format!("-I{include}"),
            "-o",
            so_path.to_str().expect("so path"),
            c_path.to_str().expect("c path"),
        ])
        .status()
        .expect("compile extension");
    assert!(status.success(), "failed to compile extension module");

    let wheel = artifacts.join("native_ext-0.1.0-py3-none-any.whl");
    let (sha256, size) = build_wheel(
        &python,
        &wheel,
        "native_ext",
        "0.1.0",
        vec![serde_json::json!({
            "kind": "file",
            "path": so_name,
            "src": so_path.display().to_string(),
        })],
        vec![],
    );

    write_pyproject(
        project,
        "cas_native_ext",
        ">=3.11",
        &["native_ext==0.1.0".to_string()],
    );
    write_lock(
        project,
        "cas_native_ext",
        ">=3.11",
        &[LockEntry {
            name: "native_ext".to_string(),
            version: "0.1.0".to_string(),
            filename: wheel
                .file_name()
                .expect("wheel filename")
                .to_string_lossy()
                .to_string(),
            cached_path: "artifacts/native_ext-0.1.0-py3-none-any.whl".to_string(),
            sha256,
            size,
        }],
    );

    assert_envs_empty("before run");
    let assert = cargo_bin_cmd!("px")
        .current_dir(project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args([
            "--json",
            "run",
            "python",
            "-c",
            "import native_ext; print(native_ext.answer())",
        ])
        .assert()
        .success();
    assert_envs_empty("after run");

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    let stdout = payload["details"]["stdout"].as_str().unwrap_or_default();
    assert!(
        stdout.contains("42"),
        "expected extension module output, got {stdout:?}"
    );
}

#[test]
fn cas_native_falls_back_on_duplicate_console_scripts() {
    let _guard = test_env_guard();
    reset_test_store_env();
    ensure_test_store_env();
    let Some(python) = find_python() else {
        eprintln!("skipping cas-native duplicate console script test (python binary not found)");
        return;
    };
    let temp = tempfile::Builder::new()
        .prefix("px-cas-native-dupe")
        .tempdir()
        .expect("tempdir");
    let project = temp.path();
    let artifacts = project.join("artifacts");
    fs::create_dir_all(&artifacts).expect("artifacts dir");

    let wheel_a = artifacts.join("dupe_a-0.1.0-py3-none-any.whl");
    let (sha_a, size_a) = build_wheel(
        &python,
        &wheel_a,
        "dupe_a",
        "0.1.0",
        vec![serde_json::json!({
            "kind": "text",
            "path": "dupe_a.py",
            "contents": "def main():\n    print('a')\n",
        })],
        vec![("dupe", "dupe_a:main")],
    );
    let wheel_b = artifacts.join("dupe_b-0.1.0-py3-none-any.whl");
    let (sha_b, size_b) = build_wheel(
        &python,
        &wheel_b,
        "dupe_b",
        "0.1.0",
        vec![serde_json::json!({
            "kind": "text",
            "path": "dupe_b.py",
            "contents": "def main():\n    print('b')\n",
        })],
        vec![("dupe", "dupe_b:main")],
    );

    write_pyproject(
        project,
        "cas_native_dupe",
        ">=3.11",
        &["dupe_a==0.1.0".to_string(), "dupe_b==0.1.0".to_string()],
    );
    write_lock(
        project,
        "cas_native_dupe",
        ">=3.11",
        &[
            LockEntry {
                name: "dupe_a".to_string(),
                version: "0.1.0".to_string(),
                filename: wheel_a
                    .file_name()
                    .expect("wheel filename")
                    .to_string_lossy()
                    .to_string(),
                cached_path: "artifacts/dupe_a-0.1.0-py3-none-any.whl".to_string(),
                sha256: sha_a,
                size: size_a,
            },
            LockEntry {
                name: "dupe_b".to_string(),
                version: "0.1.0".to_string(),
                filename: wheel_b
                    .file_name()
                    .expect("wheel filename")
                    .to_string_lossy()
                    .to_string(),
                cached_path: "artifacts/dupe_b-0.1.0-py3-none-any.whl".to_string(),
                sha256: sha_b,
                size: size_b,
            },
        ],
    );

    assert_envs_empty("before run");
    let assert = cargo_bin_cmd!("px")
        .current_dir(project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["-v", "--json", "run", "dupe"])
        .assert()
        .success();
    assert_envs_non_empty("after run");

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    assert_eq!(
        payload["details"]["cas_native_fallback"]["code"],
        "ambiguous_console_script"
    );
    let stdout = payload["details"]["stdout"].as_str().unwrap_or_default();
    assert!(
        stdout.contains("b"),
        "expected fallback to env materialization (dupe_b wins), got {stdout:?}"
    );
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.contains("CAS_NATIVE_FALLBACK=ambiguous_console_script"),
        "expected fallback log line, got stderr={stderr:?}"
    );
}

#[test]
fn cas_native_fallback_is_silent_without_verbose() {
    let _guard = test_env_guard();
    reset_test_store_env();
    ensure_test_store_env();
    let Some(python) = find_python() else {
        eprintln!("skipping cas-native fallback silence test (python binary not found)");
        return;
    };
    let temp = tempfile::Builder::new()
        .prefix("px-cas-native-fallback-silent")
        .tempdir()
        .expect("tempdir");
    let project = temp.path();
    let artifacts = project.join("artifacts");
    fs::create_dir_all(&artifacts).expect("artifacts dir");

    let wheel_a = artifacts.join("dupe_a-0.1.0-py3-none-any.whl");
    let (sha_a, size_a) = build_wheel(
        &python,
        &wheel_a,
        "dupe_a",
        "0.1.0",
        vec![serde_json::json!({
            "kind": "text",
            "path": "dupe_a.py",
            "contents": "def main():\n    print('a')\n",
        })],
        vec![("dupe", "dupe_a:main")],
    );
    let wheel_b = artifacts.join("dupe_b-0.1.0-py3-none-any.whl");
    let (sha_b, size_b) = build_wheel(
        &python,
        &wheel_b,
        "dupe_b",
        "0.1.0",
        vec![serde_json::json!({
            "kind": "text",
            "path": "dupe_b.py",
            "contents": "def main():\n    print('b')\n",
        })],
        vec![("dupe", "dupe_b:main")],
    );

    write_pyproject(
        project,
        "cas_native_dupe_silent",
        ">=3.11",
        &["dupe_a==0.1.0".to_string(), "dupe_b==0.1.0".to_string()],
    );
    write_lock(
        project,
        "cas_native_dupe_silent",
        ">=3.11",
        &[
            LockEntry {
                name: "dupe_a".to_string(),
                version: "0.1.0".to_string(),
                filename: wheel_a
                    .file_name()
                    .expect("wheel filename")
                    .to_string_lossy()
                    .to_string(),
                cached_path: "artifacts/dupe_a-0.1.0-py3-none-any.whl".to_string(),
                sha256: sha_a,
                size: size_a,
            },
            LockEntry {
                name: "dupe_b".to_string(),
                version: "0.1.0".to_string(),
                filename: wheel_b
                    .file_name()
                    .expect("wheel filename")
                    .to_string_lossy()
                    .to_string(),
                cached_path: "artifacts/dupe_b-0.1.0-py3-none-any.whl".to_string(),
                sha256: sha_b,
                size: size_b,
            },
        ],
    );

    assert_envs_empty("before run");
    let assert = cargo_bin_cmd!("px")
        .current_dir(project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["--json", "run", "dupe"])
        .assert()
        .success();
    assert_envs_non_empty("after run");

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    assert_eq!(
        payload["details"]["cas_native_fallback"]["code"],
        "ambiguous_console_script"
    );
    let stdout = payload["details"]["stdout"].as_str().unwrap_or_default();
    assert!(
        stdout.contains("b"),
        "expected fallback to env materialization (dupe_b wins), got {stdout:?}"
    );
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        !stderr.contains("CAS_NATIVE_FALLBACK="),
        "expected fallback to be silent by default, got stderr={stderr:?}"
    );
}

#[test]
fn sync_repairs_stale_cached_paths_and_run_succeeds() {
    let _guard = test_env_guard();
    reset_test_store_env();
    ensure_test_store_env();
    let Some(python) = find_python() else {
        eprintln!("skipping cached path repair test (python binary not found)");
        return;
    };
    let temp = tempfile::Builder::new()
        .prefix("px-cached-path-repair")
        .tempdir()
        .expect("tempdir");
    let project = temp.path();

    let cache_root = PathBuf::from(env::var("PX_CACHE_PATH").expect("PX_CACHE_PATH"));
    let cached_wheel = cache_root
        .join("wheels")
        .join("hello_console")
        .join("0.1.0")
        .join("hello_console-0.1.0-py3-none-any.whl");
    let (sha256, size) = build_wheel(
        &python,
        &cached_wheel,
        "hello_console",
        "0.1.0",
        vec![serde_json::json!({
            "kind": "text",
            "path": "hello_console.py",
            "contents": "def main():\n    print('ok')\n",
        })],
        vec![],
    );

    write_pyproject(
        project,
        "cached_path_repair",
        ">=3.11",
        &["hello_console==0.1.0".to_string()],
    );
    write_lock(
        project,
        "cached_path_repair",
        ">=3.11",
        &[LockEntry {
            name: "hello_console".to_string(),
            version: "0.1.0".to_string(),
            filename: cached_wheel
                .file_name()
                .expect("wheel filename")
                .to_string_lossy()
                .to_string(),
            cached_path: "artifacts/stale-hello_console-0.1.0.whl".to_string(),
            sha256,
            size,
        }],
    );

    assert_envs_empty("before sync");
    cargo_bin_cmd!("px")
        .current_dir(project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["sync"])
        .assert()
        .success();
    assert_envs_non_empty("after sync");

    let lock_text = fs::read_to_string(project.join("px.lock")).expect("read px.lock");
    assert!(
        lock_text.contains(&cached_wheel.display().to_string()),
        "px sync should repair cached_path to point at {}",
        cached_wheel.display()
    );

    cargo_bin_cmd!("px")
        .current_dir(project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args([
            "--json",
            "run",
            "python",
            "-c",
            "import hello_console; hello_console.main()",
        ])
        .assert()
        .success();
}

#[test]
fn sandbox_bypasses_cas_native_default() {
    let _guard = test_env_guard();
    reset_test_store_env();
    ensure_test_store_env();
    let Some(python) = find_python() else {
        eprintln!("skipping sandbox bypass test (python binary not found)");
        return;
    };
    let temp = tempfile::Builder::new()
        .prefix("px-sandbox-bypass")
        .tempdir()
        .expect("tempdir");
    let project = temp.path();

    write_pyproject(project, "sandbox_bypass", ">=3.11", &[]);
    write_lock(project, "sandbox_bypass", ">=3.11", &[]);

    let (backend, log) = fake_sandbox_backend(project).expect("backend script");
    let store = project.join("sandbox-store");

    assert_envs_empty("before sandbox run");
    let assert = cargo_bin_cmd!("px")
        .current_dir(project)
        .env("PX_SANDBOX_STORE", &store)
        .env("PX_SANDBOX_BACKEND", &backend)
        .env("PX_FAKE_SANDBOX_LOG", &log)
        .env("PX_FAKE_SANDBOX_PROJECT_ROOT", project)
        .env("PX_FAKE_SANDBOX_INSPECT_EXIT", "1")
        .env("PX_RUNTIME_PYTHON", &python)
        .args([
            "--json",
            "run",
            "--sandbox",
            "python",
            "-c",
            "print('sandbox-ok')",
        ])
        .assert()
        .success();
    assert_envs_non_empty("after sandbox run");

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    assert!(payload["details"]["sandbox"].is_object());

    let log_contents = fs::read_to_string(&log).unwrap_or_default();
    assert!(
        log_contents.contains("run:"),
        "expected sandbox backend invocation, log={log_contents:?}"
    );
}
