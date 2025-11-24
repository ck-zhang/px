use std::{fs, path::PathBuf, process::Command};

use assert_cmd::cargo::cargo_bin_cmd;
use serde_json::Value;
use tempfile::tempdir;

mod common;

use common::{parse_json, prepare_fixture};

const INSPECT_SCRIPT: &str = "import json, platform, sys; print(json.dumps({'version': platform.python_version(), 'executable': sys.executable}))";

#[test]
fn python_install_and_use_records_runtime() {
    let (python_path, channel) = detect_host_python();
    let registry_dir = tempdir().expect("registry tempdir");
    let registry = registry_dir.path().join("runtimes.json");
    cargo_bin_cmd!("px")
        .env("PX_RUNTIME_REGISTRY", registry.to_str().unwrap())
        .args([
            "python",
            "install",
            &channel,
            "--path",
            python_path.to_str().unwrap(),
        ])
        .assert()
        .success();

    let list = cargo_bin_cmd!("px")
        .env("PX_RUNTIME_REGISTRY", registry.to_str().unwrap())
        .args(["--json", "python", "list"])
        .assert()
        .success();
    let payload = parse_json(&list);
    let runtimes = payload["details"]["runtimes"]
        .as_array()
        .expect("runtimes array");
    assert!(
        runtimes.iter().any(|rt| rt["version"] == channel),
        "expected runtime {channel} in list: {payload:?}"
    );

    let (_tmp, project) = prepare_fixture("python-use");
    cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_REGISTRY", registry.to_str().unwrap())
        .args(["python", "use", &channel])
        .assert()
        .success();
    let contents = fs::read_to_string(project.join("pyproject.toml")).expect("pyproject");
    assert!(
        contents.contains(&format!("python = \"{channel}\"")),
        "pyproject should record runtime: {contents}"
    );
}

#[test]
fn python_list_reports_corrupt_registry() {
    let registry_dir = tempdir().expect("registry tempdir");
    let registry = registry_dir.path().join("broken.json");
    fs::write(&registry, "not-json").expect("write registry");
    let assert = cargo_bin_cmd!("px")
        .env("PX_RUNTIME_REGISTRY", registry.to_str().unwrap())
        .args(["--json", "python", "list"])
        .assert()
        .failure();
    let payload = parse_json(&assert);
    assert!(payload["message"]
        .as_str()
        .unwrap_or_default()
        .contains("unable to read px runtime registry"));
    assert_eq!(
        payload["details"]["registry"].as_str().unwrap_or_default(),
        registry.display().to_string()
    );
}

#[test]
fn python_info_surfaces_manifest_errors() {
    let temp = tempdir().expect("tempdir");
    let root = temp.path();
    fs::write(root.join("pyproject.toml"), "[project\nname='broken'").expect("write pyproject");
    let assert = cargo_bin_cmd!("px")
        .current_dir(root)
        .args(["--json", "python", "info"])
        .assert()
        .failure();
    let payload = parse_json(&assert);
    let message = payload["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("pyproject.toml"),
        "expected manifest parse failure, got {message:?}"
    );
}

fn detect_host_python() -> (PathBuf, String) {
    for candidate in ["python3", "python"] {
        if let Ok(output) = Command::new(candidate)
            .arg("-c")
            .arg(INSPECT_SCRIPT)
            .output()
        {
            if output.status.success() {
                let payload: Value =
                    serde_json::from_slice(&output.stdout).expect("inspection payload");
                let path = payload["executable"]
                    .as_str()
                    .expect("executable")
                    .to_string();
                let version = payload["version"].as_str().expect("version").to_string();
                let parts: Vec<_> = version.split('.').collect();
                let major = parts.first().unwrap_or(&"0");
                let minor = parts.get(1).unwrap_or(&"0");
                let channel = format!("{major}.{minor}");
                return (PathBuf::from(path), channel);
            }
        }
    }
    panic!("python3 or python must be available on PATH for runtime tests");
}
