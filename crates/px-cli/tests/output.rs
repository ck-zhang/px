use assert_cmd::cargo::cargo_bin_cmd;
use flate2::read::GzDecoder;
use serde_json::Value;
use std::fs::File;
use std::path::Path;
use std::process::Command;
use tar::Archive;
use zip::ZipArchive;

mod common;

use common::{init_empty_project, parse_json, prepare_fixture, project_identity, require_online};

#[test]
fn output_build_produces_wheel_and_sdist() {
    let _guard = common::test_env_guard();
    if !require_online() {
        return;
    }
    let (_tmp, project) = prepare_fixture("output-build");
    let (name, normalized, version) = project_identity(&project);
    let dist_dir = project.join("dist-artifacts");
    let dist_arg = dist_dir.to_string_lossy().to_string();

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["--json", "build", "both", "--out", &dist_arg])
        .assert()
        .success();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    let artifacts = payload["details"]["artifacts"]
        .as_array()
        .expect("artifacts array");
    assert_eq!(artifacts.len(), 2, "expected wheel + sdist entries");
    let paths: Vec<String> = artifacts
        .iter()
        .filter_map(|entry| {
            entry
                .as_object()
                .and_then(|map| map.get("path"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    let expected_files = [
        format!("dist-artifacts/{name}-{version}.tar.gz"),
        format!("dist-artifacts/{normalized}-{version}-py3-none-any.whl"),
    ];
    for rel in &expected_files {
        assert!(
            paths.iter().any(|entry| entry == rel),
            "artifacts should include {rel}, got {paths:?}"
        );
        assert!(
            project.join(rel).exists(),
            "built file {rel} should exist on disk"
        );
    }
}

#[test]
fn build_outputs_do_not_synthesize_src_packages() {
    let _guard = common::test_env_guard();
    if !require_online() {
        return;
    }
    let (_tmp, project) = prepare_fixture("output-build-layout");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["--json", "build", "both"])
        .assert()
        .success();
    let payload = parse_json(&assert);
    let artifacts = payload["details"]["artifacts"]
        .as_array()
        .expect("artifacts array");
    let (name, normalized, version) = project_identity(&project);

    let mut wheel = None::<String>;
    let mut sdist = None::<String>;
    for entry in artifacts {
        let Some(path) = entry
            .as_object()
            .and_then(|map| map.get("path"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        if path.ends_with(".whl") {
            wheel = Some(path.to_string());
        } else if path.ends_with(".tar.gz") {
            sdist = Some(path.to_string());
        }
    }
    let wheel = wheel.expect("wheel artifact path");
    let sdist = sdist.expect("sdist artifact path");

    let sdist_paths = tar_gz_paths(&project.join(&sdist));
    let expected_module = format!("{normalized}/__init__.py");
    let unexpected_module = format!("src/{normalized}/__init__.py");
    assert!(
        sdist_paths.iter().any(|path| path.ends_with(&expected_module)),
        "sdist should contain {expected_module}, got {sdist_paths:?}"
    );
    assert!(
        !sdist_paths
            .iter()
            .any(|path| path.ends_with(&unexpected_module)),
        "sdist should not invent src layout ({unexpected_module})"
    );

    let wheel_paths = wheel_paths(&project.join(&wheel));
    assert!(
        wheel_paths.iter().any(|path| path == &expected_module),
        "wheel should contain {expected_module}, got {wheel_paths:?}"
    );
    assert!(
        !wheel_paths.iter().any(|path| path == &unexpected_module),
        "wheel should not contain src layout entries"
    );

    assert!(
        project
            .join(format!("dist/{name}-{version}.tar.gz"))
            .exists(),
        "expected sdist to exist on disk"
    );
}

#[test]
fn publish_dry_run_reports_registry_and_artifacts() {
    let _guard = common::test_env_guard();
    if !require_online() {
        return;
    }
    let (_tmp, project) = prepare_fixture("output-publish-dry-run");
    cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["sync"])
        .assert()
        .success();
    cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["build"])
        .assert()
        .success();

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["--json", "publish", "--registry", "testpypi"])
        .assert()
        .success();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    assert_eq!(payload["details"]["registry"], "testpypi");
    assert_eq!(payload["details"]["dry_run"], Value::Bool(true));
    let artifacts = payload["details"]["artifacts"]
        .as_array()
        .expect("artifacts array");
    assert!(
        !artifacts.is_empty(),
        "dry-run publish should report existing artifacts"
    );
}

#[test]
fn publish_default_dry_run_does_not_require_token() {
    let _guard = common::test_env_guard();
    if !require_online() {
        return;
    }
    let (_tmp, project) = prepare_fixture("output-publish-default-dry-run");
    cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["sync"])
        .assert()
        .success();
    cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["build"])
        .assert()
        .success();

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_ONLINE", "1")
        .args(["--json", "publish"])
        .assert()
        .success();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    assert_eq!(payload["details"]["dry_run"], Value::Bool(true));
}

#[test]
fn publish_requires_token_when_uploading_online() {
    let _guard = common::test_env_guard();
    if !require_online() {
        return;
    }
    let (_tmp, project) = prepare_fixture("output-publish-token");
    cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["sync"])
        .assert()
        .success();
    cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["build"])
        .assert()
        .success();

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_ONLINE", "1")
        .args(["--json", "publish", "--upload"])
        .assert()
        .failure();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    let message = payload["message"].as_str().expect("message string");
    assert!(
        message.contains("PX_PUBLISH_TOKEN must be set"),
        "expected missing token error, got {message:?}"
    );
    let hint = payload["details"]["hint"].as_str().expect("hint field");
    assert!(
        hint.contains("PX_PUBLISH_TOKEN"),
        "hint should mention token variable: {hint:?}"
    );
}

#[test]
fn publish_errors_when_dist_missing() {
    let _guard = common::test_env_guard();
    if !require_online() {
        return;
    }
    let (_tmp, project) = init_empty_project("output-publish-missing-dist");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["--json", "publish"])
        .assert()
        .failure();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    let message = payload["message"].as_str().expect("message string");
    assert!(
        message.contains("no artifacts"),
        "expected publish to fail when dist/ is empty: {message:?}"
    );
    assert_eq!(payload["details"]["dist_dir"], "dist");
}

#[test]
fn build_dry_run_reports_empty_artifacts() {
    let _guard = common::test_env_guard();
    if !require_online() {
        return;
    }
    let (_tmp, project) = init_empty_project("output-build-dry-run");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["--json", "build", "--dry-run"])
        .assert()
        .success();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    assert_eq!(payload["details"]["dry_run"], Value::Bool(true));
    let artifacts = payload["details"]["artifacts"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        artifacts.is_empty(),
        "dry-run build should not report artifacts: {artifacts:?}"
    );
    assert!(
        !project.join("dist").exists(),
        "dry-run build should not create dist directory"
    );
}

#[test]
fn build_errors_when_project_has_no_module() {
    let _guard = common::test_env_guard();
    if !require_online() {
        return;
    }
    let (_tmp, project) = init_empty_project("output-build-missing-module");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["--json", "build"])
        .assert()
        .failure();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    let message = payload["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("no Python module"),
        "expected missing module build error, got {message:?}"
    );
    let hint = payload["details"]["hint"].as_str().unwrap_or_default();
    assert!(
        hint.contains("__init__.py") || hint.contains("tool.uv.build-backend"),
        "expected hint to mention package layout/config, got {hint:?}"
    );
}

#[test]
fn publish_requires_online_flag_when_artifacts_exist() {
    let _guard = common::test_env_guard();
    if !require_online() {
        return;
    }
    let (_tmp, project) = prepare_fixture("output-publish-offline");
    cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["sync"])
        .assert()
        .success();
    cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["build"])
        .assert()
        .success();

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_ONLINE", "0")
        .env("PX_PUBLISH_TOKEN", "dummy-token")
        .args(["--json", "publish", "--upload"])
        .assert()
        .failure();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    let message = payload["message"].as_str().expect("message string");
    assert!(
        message.contains("PX_ONLINE=1 required for uploads"),
        "expected offline guard message, got {message:?}"
    );
    let hint = payload["details"]["hint"].as_str().expect("hint string");
    assert!(
        hint.contains("PX_ONLINE=1"),
        "hint should instruct enabling PX_ONLINE: {hint:?}"
    );
}

#[test]
fn publish_rejects_empty_token_value() {
    let _guard = common::test_env_guard();
    if !require_online() {
        return;
    }
    let (_tmp, project) = prepare_fixture("output-publish-empty-token");
    cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["sync"])
        .assert()
        .success();
    cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["build"])
        .assert()
        .success();

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_ONLINE", "1")
        .env("PX_PUBLISH_TOKEN", "")
        .args(["--json", "publish", "--upload"])
        .assert()
        .failure();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    let message = payload["message"].as_str().expect("message string");
    assert!(
        message.contains("PX_PUBLISH_TOKEN is empty"),
        "expected empty token error, got {message:?}"
    );
    assert_eq!(
        payload["details"]["token_env"], "PX_PUBLISH_TOKEN",
        "details should reflect the token env used"
    );
}

#[test]
fn publish_dry_run_accepts_custom_registry_url() {
    let _guard = common::test_env_guard();
    if !require_online() {
        return;
    }
    let (_tmp, project) = prepare_fixture("output-publish-custom-registry");
    cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["sync"])
        .assert()
        .success();
    cargo_bin_cmd!("px")
        .current_dir(&project)
        .args(["build"])
        .assert()
        .success();

    let registry = "https://registry.example.invalid/upload/";
    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .args([
            "--json",
            "publish",
            "--dry-run",
            "--registry",
            registry,
            "--token-env",
            "PX_FAKE_TOKEN",
        ])
        .assert()
        .success();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    assert_eq!(payload["details"]["registry"], registry);
    assert_eq!(payload["details"]["dry_run"], Value::Bool(true));
}

#[test]
fn built_wheel_is_installable_with_pip() {
    let _guard = common::test_env_guard();
    if !require_online() {
        return;
    }
    let (_tmp, project) = prepare_fixture("sample_px_app");
    let Some(python) = find_python() else {
        eprintln!("skipping wheel install test (python binary not found)");
        return;
    };

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["--json", "build", "wheel"])
        .assert()
        .success();

    let payload = parse_json(&assert);
    let artifacts = payload["details"]["artifacts"]
        .as_array()
        .expect("artifacts array");
    let wheel_rel = artifacts
        .iter()
        .filter_map(|entry| {
            entry
                .as_object()
                .and_then(|map| map.get("path"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .find(|path| path.ends_with(".whl"))
        .expect("wheel artifact path");
    let wheel_path = project.join(&wheel_rel);
    assert!(
        wheel_path.exists(),
        "wheel should exist at {}",
        wheel_path.display()
    );

    let venv = tempfile::tempdir().expect("tempdir");
    let status = Command::new(&python)
        .args(["-m", "venv", venv.path().to_string_lossy().as_ref()])
        .status()
        .expect("spawn venv");
    assert!(status.success(), "python -m venv failed with {status:?}");

    let (python_bin, pip_bin) = venv_binaries(venv.path());
    let status = Command::new(&pip_bin)
        .args([
            "install",
            "--no-deps",
            wheel_path.to_string_lossy().as_ref(),
        ])
        .status()
        .expect("spawn pip install");
    assert!(
        status.success(),
        "pip install should succeed for built wheel"
    );

    let status = Command::new(&python_bin)
        .args([
            "-c",
            "import sample_px_app.cli as c; assert c.greet('PxWheel') == 'Hello, PxWheel!'",
        ])
        .status()
        .expect("spawn python import test");
    assert!(status.success(), "installed wheel should import correctly");
}

fn find_python() -> Option<String> {
    let candidates = [
        std::env::var("PYTHON").ok(),
        Some("python3".to_string()),
        Some("python".to_string()),
    ];
    for candidate in candidates.into_iter().flatten() {
        let status = Command::new(&candidate)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if matches!(status, Ok(code) if code.success()) {
            return Some(candidate);
        }
    }
    None
}

fn venv_binaries(root: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    if cfg!(windows) {
        let python = root.join("Scripts").join("python.exe");
        let pip = root.join("Scripts").join("pip.exe");
        (python, pip)
    } else {
        let python = root.join("bin").join("python");
        let pip = root.join("bin").join("pip");
        (python, pip)
    }
}

fn tar_gz_paths(path: &Path) -> Vec<String> {
    let file = File::open(path).expect("open tar.gz");
    let decoder = GzDecoder::new(file);
    let mut archive = Archive::new(decoder);
    let mut paths = Vec::new();
    for entry in archive.entries().expect("tar entries") {
        let entry = entry.expect("tar entry");
        let path = entry
            .path()
            .expect("tar path")
            .into_owned()
            .to_string_lossy()
            .replace('\\', "/");
        paths.push(path);
    }
    paths
}

fn wheel_paths(path: &Path) -> Vec<String> {
    let file = File::open(path).expect("open wheel");
    let mut archive = ZipArchive::new(file).expect("zip archive");
    let mut paths = Vec::new();
    for idx in 0..archive.len() {
        let entry = archive.by_index(idx).expect("zip entry");
        paths.push(entry.name().to_string());
    }
    paths
}
