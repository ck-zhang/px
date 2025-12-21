use assert_cmd::cargo::cargo_bin_cmd;
use std::fs;

mod common;

use common::{
    ensure_test_store_env, find_python, require_online, reset_test_store_env, test_env_guard,
};

#[test]
fn px_run_exits_with_target_exit_code() {
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
    reset_test_store_env();
    ensure_test_store_env();
    let Some(python) = find_python() else {
        eprintln!("skipping exit code test (python not found)");
        return;
    };

    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path();

    cargo_bin_cmd!("px")
        .current_dir(project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["init"])
        .assert()
        .success();
    cargo_bin_cmd!("px")
        .current_dir(project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["sync"])
        .assert()
        .success();

    cargo_bin_cmd!("px")
        .current_dir(project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["run", "python", "-c", "import sys; sys.exit(7)"])
        .assert()
        .code(7);
}

#[test]
fn px_test_exits_with_runner_exit_code() {
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
    reset_test_store_env();
    ensure_test_store_env();
    let Some(python) = find_python() else {
        eprintln!("skipping exit code test (python not found)");
        return;
    };

    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path();

    cargo_bin_cmd!("px")
        .current_dir(project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["init"])
        .assert()
        .success();
    cargo_bin_cmd!("px")
        .current_dir(project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["add", "pytest"])
        .assert()
        .success();
    cargo_bin_cmd!("px")
        .current_dir(project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["sync"])
        .assert()
        .success();

    let tests_dir = project.join("tests");
    fs::create_dir_all(&tests_dir).expect("create tests dir");
    fs::write(
        tests_dir.join("test_failure.py"),
        "def test_failure():\n    assert False\n",
    )
    .expect("write failing test");

    cargo_bin_cmd!("px")
        .current_dir(project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["test"])
        .assert()
        .code(1);
}

#[cfg(unix)]
#[test]
fn px_run_exits_with_128_plus_signal_on_unix() {
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
    reset_test_store_env();
    ensure_test_store_env();
    let Some(python) = find_python() else {
        eprintln!("skipping signal exit code test (python not found)");
        return;
    };

    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path();

    cargo_bin_cmd!("px")
        .current_dir(project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["init"])
        .assert()
        .success();
    cargo_bin_cmd!("px")
        .current_dir(project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["sync"])
        .assert()
        .success();

    // SIGKILL is 9; shell-style exit codes are 128 + signal.
    cargo_bin_cmd!("px")
        .current_dir(project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args([
            "run",
            "python",
            "-c",
            "import os,signal; os.kill(os.getpid(), signal.SIGKILL)",
        ])
        .assert()
        .code(137);
}
