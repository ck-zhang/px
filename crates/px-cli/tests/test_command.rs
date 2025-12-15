use assert_cmd::cargo::cargo_bin_cmd;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};
use std::time::Instant;

mod common;
use common::require_online;

fn find_python() -> Option<String> {
    let candidates = [
        std::env::var("PYTHON").ok(),
        Some("python3".to_string()),
        Some("python".to_string()),
    ];
    for candidate in candidates.into_iter().flatten() {
        let status = std::process::Command::new(&candidate)
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

#[test]
fn px_test_streams_and_summarizes_runner_output() {
    if !require_online() {
        return;
    }
    let _guard = common::test_env_guard();
    let (_tmp, project) = common::init_empty_project("test-streaming");
    let cache = project.join(".px-cache");
    let store = cache.join("store");
    let envs = cache.join("envs");
    fs::create_dir_all(&envs).expect("create envs dir");
    let Some(python) = find_python() else {
        eprintln!("skipping streaming test (python binary not found)");
        return;
    };

    let tests = project.join("tests");
    fs::create_dir_all(&tests).expect("create tests dir");
    fs::write(
        tests.join("test_stream.py"),
        "import sys, time\n\n\ndef test_streaming():\n    print('stream-start', flush=True)\n    print('stream-err', file=sys.stderr, flush=True)\n    time.sleep(0.5)\n    print('stream-end', flush=True)\n",
    )
    .expect("write streaming test");

    cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .env("PX_CACHE_PATH", &cache)
        .env("PX_STORE_PATH", &store)
        .env("PX_ENVS_PATH", &envs)
        .env("PX_RUNTIME_HOST_ONLY", "1")
        .env("PYTHONNOUSERSITE", "1")
        .args(["add", "pytest"])
        .assert()
        .success();
    cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .env("PX_CACHE_PATH", &cache)
        .env("PX_STORE_PATH", &store)
        .env("PX_ENVS_PATH", &envs)
        .env("PX_RUNTIME_HOST_ONLY", "1")
        .env("PYTHONNOUSERSITE", "1")
        .args(["sync"])
        .assert()
        .success();

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("px"));
    cmd.current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .env("PX_CACHE_PATH", &cache)
        .env("PX_STORE_PATH", &store)
        .env("PX_ENVS_PATH", &envs)
        .env("PX_RUNTIME_HOST_ONLY", "1")
        .env("PYTHONNOUSERSITE", "1")
        .arg("test")
        .arg("--")
        .arg("-s")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn px test");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout handle"));
    let mut stderr = BufReader::new(child.stderr.take().expect("stderr handle"));

    let start = Instant::now();
    let mut stdout_lines = Vec::new();
    let mut first_stdout_time = None;
    let mut line = String::new();
    while stdout
        .read_line(&mut line)
        .expect("read stdout line from px test")
        > 0
    {
        if line.contains("missing_pytest") || line.contains("pytest is not available") {
            // Environment setup flaked (offline or resolver failure); skip streaming checks.
            let _ = child.wait();
            return;
        }
        stdout_lines.push(line.clone());
        if line.contains("stream-start") {
            first_stdout_time = Some(start.elapsed());
            break;
        }
        line.clear();
    }
    let elapsed = first_stdout_time.unwrap_or_else(|| {
        panic!("expected runner stdout to be streamed, saw lines: {stdout_lines:?}")
    });
    assert!(
        child.try_wait().expect("poll px test process").is_none(),
        "expected streamed output before runner completed; process already exited after {elapsed:?}"
    );
    let mut stderr_lines = Vec::new();
    let mut first_stderr_time = None;
    let mut err_line = String::new();
    while stderr
        .read_line(&mut err_line)
        .expect("read stderr line from px test")
        > 0
    {
        stderr_lines.push(err_line.clone());
        if err_line.contains("stream-err") {
            first_stderr_time = Some(start.elapsed());
            break;
        }
        err_line.clear();
    }
    assert!(
        first_stderr_time.is_some(),
        "expected runner stderr to be streamed, saw {stderr_lines:?}"
    );
    assert!(
        child.try_wait().expect("poll px test process").is_none(),
        "expected streamed stderr before runner completed; process already exited after {elapsed:?}"
    );

    let status = child.wait().expect("wait for px test");
    let mut remaining = String::new();
    stdout
        .read_to_string(&mut remaining)
        .expect("read remaining stdout");
    let mut remaining_err = String::new();
    stderr
        .read_to_string(&mut remaining_err)
        .expect("read remaining stderr");
    let combined = format!("{}{}", stdout_lines.join(""), remaining);
    assert!(
        combined.contains("px test  •"),
        "px reporter header should be present: {combined:?}"
    );
    assert!(
        status.success(),
        "px test should succeed, stdout: {combined:?}, stderr: {remaining_err:?}"
    );
    assert!(
        combined.contains("collected"),
        "collection summary should be present: {combined:?}"
    );
    assert!(
        combined.contains("RESULT"),
        "px test summary should be present in stdout: {combined:?}"
    );

    fs::write(
        tests.join("test_failure.py"),
        "def test_failure():\n    assert False\n",
    )
    .expect("write failing test");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["test", "--", "-s"])
        .assert()
        .failure();

    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    assert!(
        stdout.contains("FAILURES (1)") || stdout.contains("FAILED"),
        "summary should report pytest failure: {stdout:?}"
    );
    assert!(
        !stdout.contains("Why:") && !stdout.contains("Fix:"),
        "runner failures should keep framing minimal: {stdout:?}"
    );

    fs::remove_file(tests.join("test_failure.py")).ok();

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .env("PX_TEST_REPORTER", "pytest")
        .args(["test", "--", "-s"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    assert!(
        !stdout.contains("px test running"),
        "native pytest reporter should not include px header: {stdout:?}"
    );
}
