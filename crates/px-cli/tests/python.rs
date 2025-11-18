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
                let channel = format!("{}.{}", parts.get(0).unwrap(), parts.get(1).unwrap());
                return (PathBuf::from(path), channel);
            }
        }
    }
    panic!("python3 or python must be available on PATH for runtime tests");
}
