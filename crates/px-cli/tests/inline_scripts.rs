use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use assert_cmd::cargo::cargo_bin_cmd;

mod common;

#[test]
fn run_inline_script_metadata_creates_cached_env() {
    let _guard = common::test_env_guard();
    common::reset_test_store_env();
    common::ensure_test_store_env();

    let Some(python) = find_python() else {
        eprintln!("skipping inline script test (python binary not found)");
        return;
    };
    let Some(requires) = python_requirement(&python) else {
        eprintln!("skipping inline script test (unable to determine python version)");
        return;
    };

    let temp = tempfile::tempdir().expect("tempdir");
    let script = temp.path().join("hello.py");
    let contents = format!(
        "#!/usr/bin/env python3\n# /// px\n# requires-python = \"{requires}\"\n# dependencies = []\n# ///\nprint('Inline hello from px')\n"
    );
    fs::write(&script, contents).expect("write script");

    let assert = cargo_bin_cmd!("px")
        .current_dir(temp.path())
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["run", script.to_str().expect("script path")])
        .assert()
        .success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("Inline hello"),
        "inline script should print greeting, got {stdout:?}"
    );
    assert!(
        !temp.path().join("pyproject.toml").exists(),
        "px should not write pyproject.toml beside inline scripts"
    );
    assert!(
        !temp.path().join("px.lock").exists(),
        "px should not write px.lock beside inline scripts"
    );

    let cache_root = PathBuf::from(std::env::var("PX_CACHE_PATH").expect("PX_CACHE_PATH"));
    let scripts_dir = cache_root.join("scripts");
    assert!(
        has_cached_manifest(&scripts_dir),
        "inline manifests should be cached under {}",
        scripts_dir.display()
    );
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

fn has_cached_manifest(root: &Path) -> bool {
    if !root.exists() {
        return false;
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path
                    .file_name()
                    .is_some_and(|name| name == "pyproject.toml")
                {
                    return true;
                }
            }
        }
    }
    false
}
