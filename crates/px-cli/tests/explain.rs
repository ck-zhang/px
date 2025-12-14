use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use assert_cmd::cargo::cargo_bin_cmd;
use px_domain::ProjectSnapshot;
use serde_json::Value;

mod common;

use common::{ensure_test_store_env, find_python, parse_json, reset_test_store_env, test_env_guard};

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

fn px_cmd() -> assert_cmd::Command {
    ensure_test_store_env();
    cargo_bin_cmd!("px")
}

fn python_requirement(python: &str) -> Option<String> {
    let output = Command::new(python)
        .arg("-c")
        .arg("import sys; print(f\"{sys.version_info[0]}.{sys.version_info[1]}\")")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if version.is_empty() {
        None
    } else {
        Some(format!(">={version}"))
    }
}

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
        "files": [],
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

fn snapshot_paths(root: &Path) -> Vec<String> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        let mut entries = entries.flatten().collect::<Vec<_>>();
        entries.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
        for entry in entries {
            let path = entry.path();
            let rel = path
                .strip_prefix(base)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();
            out.push(rel.clone());
            if path.is_dir() {
                walk(base, &path, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
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

#[test]
fn explain_run_json_includes_schema_and_core_fields() {
    let _guard = test_env_guard();
    reset_test_store_env();
    ensure_test_store_env();
    let Some(python) = find_python() else {
        eprintln!("skipping explain run test (python binary not found)");
        return;
    };
    let Some(python_req) = python_requirement(&python) else {
        eprintln!("skipping explain run test (unable to determine python version)");
        return;
    };
    let temp = tempfile::Builder::new()
        .prefix("px-explain-run-json")
        .tempdir()
        .expect("tempdir");
    let project = temp.path().join("proj");
    fs::create_dir_all(&project).expect("create project dir");
    write_pyproject(&project, "explain_run_json", &python_req, &[]);
    write_lock(&project, "explain_run_json", &python_req, &[]);

    let before = snapshot_paths(&project);
    let assert = px_cmd()
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["explain", "run", "--json", "python", "-c", "print(1)"])
        .assert()
        .success();
    let after = snapshot_paths(&project);
    assert_eq!(before, after, "px explain run must not write to CWD");

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    let details = &payload["details"];
    assert_eq!(details["schema_version"], 1);
    assert!(details["runtime"].is_object());
    assert!(details["lock_profile"].is_object());
    assert!(details["engine"].is_object());
    assert!(details["engine"]["mode"].is_string());
}

#[test]
fn explain_run_needs_env_reports_would_repair_and_does_not_create_envs() {
    let _guard = test_env_guard();
    reset_test_store_env();
    ensure_test_store_env();
    let Some(python) = find_python() else {
        eprintln!("skipping explain NeedsEnv test (python binary not found)");
        return;
    };
    let Some(python_req) = python_requirement(&python) else {
        eprintln!("skipping explain NeedsEnv test (unable to determine python version)");
        return;
    };
    let temp = tempfile::Builder::new()
        .prefix("px-explain-needs-env")
        .tempdir()
        .expect("tempdir");
    let project = temp.path().join("proj");
    fs::create_dir_all(&project).expect("create project dir");
    write_pyproject(&project, "explain_needs_env", &python_req, &[]);
    write_lock(&project, "explain_needs_env", &python_req, &[]);

    assert_envs_empty("before explain");
    let before = snapshot_paths(&project);
    let assert = px_cmd()
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["explain", "run", "--json", "python", "-c", "print(1)"])
        .assert()
        .success();
    let after = snapshot_paths(&project);
    assert_eq!(before, after, "px explain run must not write to CWD");
    assert_envs_empty("after explain");

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    assert_eq!(payload["details"]["would_repair_env"], Value::Bool(true));
}

#[test]
fn explain_entrypoint_reports_provider_and_target() {
    let _guard = test_env_guard();
    reset_test_store_env();
    ensure_test_store_env();
    let Some(python) = find_python() else {
        eprintln!("skipping explain entrypoint test (python binary not found)");
        return;
    };
    let Some(python_req) = python_requirement(&python) else {
        eprintln!("skipping explain entrypoint test (unable to determine python version)");
        return;
    };
    let temp = tempfile::Builder::new()
        .prefix("px-explain-entrypoint")
        .tempdir()
        .expect("tempdir");
    let project = temp.path().join("proj");
    fs::create_dir_all(&project).expect("create project dir");
    let artifacts = project.join("artifacts");
    fs::create_dir_all(&artifacts).expect("artifacts dir");

    let wheel = artifacts.join("hello_console-0.1.0-py3-none-any.whl");
    let (sha256, size) = build_wheel(
        &python,
        &wheel,
        "hello_console",
        "0.1.0",
        vec![("hello-console", "hello_console:main")],
    );
    write_pyproject(
        &project,
        "explain_entrypoint",
        &python_req,
        &["hello_console==0.1.0".to_string()],
    );
    write_lock(
        &project,
        "explain_entrypoint",
        &python_req,
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

    px_cmd()
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .arg("sync")
        .assert()
        .success();

    let before = snapshot_paths(&project);
    let assert = px_cmd()
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["--json", "explain", "entrypoint", "hello-console"])
        .assert()
        .success();
    let after = snapshot_paths(&project);
    assert_eq!(
        before, after,
        "px explain entrypoint must not write to CWD"
    );

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    assert_eq!(payload["details"]["schema_version"], 1);
    assert_eq!(
        payload["details"]["provider"]["distribution"],
        Value::String("hello_console".to_string())
    );
    assert_eq!(
        payload["details"]["target"]["entry_point"],
        Value::String("hello_console:main".to_string())
    );
    assert!(
        payload["details"]["provider"]["pkg_build_oid"]
            .as_str()
            .is_some_and(|value| !value.is_empty()),
        "expected pkg_build_oid in explain entrypoint output"
    );
}

#[test]
fn explain_entrypoint_reports_conflicts_deterministically() {
    let _guard = test_env_guard();
    reset_test_store_env();
    ensure_test_store_env();
    let Some(python) = find_python() else {
        eprintln!("skipping explain entrypoint conflict test (python binary not found)");
        return;
    };
    let Some(python_req) = python_requirement(&python) else {
        eprintln!("skipping explain entrypoint conflict test (unable to determine python version)");
        return;
    };
    let temp = tempfile::Builder::new()
        .prefix("px-explain-entrypoint-conflict")
        .tempdir()
        .expect("tempdir");
    let project = temp.path().join("proj");
    fs::create_dir_all(&project).expect("create project dir");
    let artifacts = project.join("artifacts");
    fs::create_dir_all(&artifacts).expect("artifacts dir");

    let wheel_a = artifacts.join("aaa_console-0.1.0-py3-none-any.whl");
    let (sha256_a, size_a) = build_wheel(
        &python,
        &wheel_a,
        "aaa_console",
        "0.1.0",
        vec![("dupe", "aaa_console:main")],
    );
    let wheel_b = artifacts.join("bbb_console-0.1.0-py3-none-any.whl");
    let (sha256_b, size_b) = build_wheel(
        &python,
        &wheel_b,
        "bbb_console",
        "0.1.0",
        vec![("dupe", "bbb_console:main")],
    );

    write_pyproject(
        &project,
        "explain_entrypoint_conflict",
        &python_req,
        &[
            "aaa_console==0.1.0".to_string(),
            "bbb_console==0.1.0".to_string(),
        ],
    );
    write_lock(
        &project,
        "explain_entrypoint_conflict",
        &python_req,
        &[
            LockEntry {
                name: "aaa_console".to_string(),
                version: "0.1.0".to_string(),
                filename: wheel_a
                    .file_name()
                    .expect("wheel filename")
                    .to_string_lossy()
                    .to_string(),
                cached_path: "artifacts/aaa_console-0.1.0-py3-none-any.whl".to_string(),
                sha256: sha256_a,
                size: size_a,
            },
            LockEntry {
                name: "bbb_console".to_string(),
                version: "0.1.0".to_string(),
                filename: wheel_b
                    .file_name()
                    .expect("wheel filename")
                    .to_string_lossy()
                    .to_string(),
                cached_path: "artifacts/bbb_console-0.1.0-py3-none-any.whl".to_string(),
                sha256: sha256_b,
                size: size_b,
            },
        ],
    );

    px_cmd()
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .arg("sync")
        .assert()
        .success();

    let before = snapshot_paths(&project);
    let assert = px_cmd()
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["--json", "explain", "entrypoint", "dupe"])
        .assert()
        .failure();
    let after = snapshot_paths(&project);
    assert_eq!(
        before, after,
        "px explain entrypoint must not write to CWD"
    );

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    assert_eq!(payload["details"]["reason"], "ambiguous_console_script");
    let candidates = payload["details"]["candidates"]
        .as_array()
        .expect("candidates array");
    assert_eq!(candidates.len(), 2);
    assert_eq!(
        candidates[0]["distribution"],
        Value::String("aaa_console".to_string())
    );
    assert_eq!(
        candidates[1]["distribution"],
        Value::String("bbb_console".to_string())
    );
}

