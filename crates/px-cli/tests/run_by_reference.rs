use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use assert_cmd::cargo::cargo_bin_cmd;

mod common;

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("git command");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn init_git_repo(repo: &Path) {
    git(repo, &["init"]);
    git(repo, &["config", "user.email", "px-test@example.invalid"]);
    git(repo, &["config", "user.name", "px test"]);
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

fn write_script(repo: &Path, requires: &str) -> PathBuf {
    let script = repo.join("scripts").join("hello.py");
    fs::create_dir_all(script.parent().expect("script parent")).expect("create scripts dir");
    let contents = format!(
        "#!/usr/bin/env python3\n# /// script\n# requires-python = \"{requires}\"\n# dependencies = []\n# ///\nprint('Hello from run-by-reference')\n"
    );
    fs::write(&script, contents).expect("write script");
    script
}

fn write_script_with_body(repo: &Path, requires: &str, body: &str) -> PathBuf {
    let script = repo.join("scripts").join("hello.py");
    fs::create_dir_all(script.parent().expect("script parent")).expect("create scripts dir");
    let contents = format!(
        "#!/usr/bin/env python3\n# /// script\n# requires-python = \"{requires}\"\n# dependencies = []\n# ///\n{body}\n"
    );
    fs::write(&script, contents).expect("write script");
    script
}

fn commit_all(repo: &Path, message: &str) -> String {
    git(repo, &["add", "."]);
    git(repo, &["commit", "-m", message]);
    git(repo, &["rev-parse", "HEAD"])
}

#[test]
fn run_by_reference_pinned_git_file_works_and_offline_hits_cache() {
    let _guard = common::test_env_guard();
    common::reset_test_store_env();
    common::ensure_test_store_env();

    if !git_available() {
        eprintln!("skipping run-by-reference test (git not found)");
        return;
    }
    let Some(python) = common::find_python() else {
        eprintln!("skipping run-by-reference test (python binary not found)");
        return;
    };
    let Some(requires) = python_requirement(&python) else {
        eprintln!("skipping run-by-reference test (unable to determine python version)");
        return;
    };

    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("create repo dir");
    init_git_repo(&repo);
    let script = write_script(&repo, &requires);
    let commit = commit_all(&repo, "add script");

    let locator = common::git_file_locator(&repo);
    let target = format!("{locator}@{commit}:scripts/hello.py");

    let caller = temp.path().join("caller");
    fs::create_dir_all(&caller).expect("create caller dir");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&caller)
        .env("PX_RUNTIME_PYTHON", &python)
        .env_remove("CI")
        .args(["run", &target])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("Hello from run-by-reference"),
        "expected script output, got {stdout:?}"
    );

    assert!(
        !caller.join(".px").exists(),
        "caller directory must not contain a .px/ directory"
    );
    assert!(
        !caller.join("pyproject.toml").exists(),
        "caller directory must not contain pyproject.toml"
    );
    assert!(
        !caller.join("px.lock").exists(),
        "caller directory must not contain px.lock"
    );

    // Prove the offline path does not touch the source repo.
    let moved = temp.path().join("repo-moved");
    fs::rename(&repo, &moved).expect("rename repo dir");
    assert!(
        !locator.contains(moved.to_string_lossy().as_ref()),
        "locator should refer to the original path"
    );
    assert!(!script.exists(), "script should be moved away");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&caller)
        .env("PX_RUNTIME_PYTHON", &python)
        .env_remove("CI")
        .args(["--offline", "run", &target])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("Hello from run-by-reference"),
        "expected cached script output, got {stdout:?}"
    );
}

#[test]
#[cfg(not(windows))]
fn run_by_reference_sandbox_executes_via_container_backend() {
    let _guard = common::test_env_guard();
    common::reset_test_store_env();
    common::ensure_test_store_env();

    if !git_available() {
        eprintln!("skipping run-by-reference sandbox test (git not found)");
        return;
    }
    let Some(python) = common::find_python() else {
        eprintln!("skipping run-by-reference sandbox test (python binary not found)");
        return;
    };
    let Some(requires) = python_requirement(&python) else {
        eprintln!("skipping run-by-reference sandbox test (unable to determine python version)");
        return;
    };

    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("create repo dir");
    init_git_repo(&repo);
    write_script(&repo, &requires);
    let commit = commit_all(&repo, "add script");

    let locator = common::git_file_locator(&repo);
    let target = format!("{locator}@{commit}:scripts/hello.py");

    let caller = temp.path().join("caller");
    fs::create_dir_all(&caller).expect("create caller dir");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&caller)
        .env("PX_RUNTIME_PYTHON", &python)
        .env_remove("CI")
        .args(["--json", "run", &target])
        .assert()
        .success();
    let payload = common::parse_json(&assert);
    let snapshot_root = payload["details"]["repo_snapshot_materialized_root"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        !snapshot_root.is_empty(),
        "expected repo_snapshot_materialized_root in response: {payload:?}"
    );

    let (backend, log) = common::fake_sandbox_backend(temp.path()).expect("backend script");
    let store_dir = common::sandbox_store_dir("sandbox-store");
    let store = store_dir.path().to_path_buf();

    let assert = cargo_bin_cmd!("px")
        .current_dir(&caller)
        .env("PX_SANDBOX_STORE", &store)
        .env("PX_SANDBOX_BACKEND", &backend)
        .env("PX_FAKE_SANDBOX_LOG", &log)
        .env("PX_FAKE_SANDBOX_PROJECT_ROOT", &snapshot_root)
        .env("PX_FAKE_SANDBOX_INSPECT_EXIT", "1")
        .env("PX_RUNTIME_PYTHON", &python)
        .env_remove("CI")
        .args(["--json", "run", "--sandbox", &target])
        .assert()
        .success();

    let payload = common::parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    assert!(payload["details"]["sandbox"].is_object());

    let log_contents = fs::read_to_string(&log).unwrap_or_default();
    assert!(
        log_contents.contains("run:"),
        "sandbox backend should have handled run: log={log_contents:?}"
    );
}

#[test]
fn run_by_reference_missing_runtime_reports_install_version() {
    let _guard = common::test_env_guard();
    common::reset_test_store_env();
    common::ensure_test_store_env();

    if !git_available() {
        eprintln!("skipping run-by-reference runtime prompt test (git not found)");
        return;
    }

    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("create repo dir");
    init_git_repo(&repo);
    write_script(&repo, ">=3.12");
    let commit = commit_all(&repo, "add script");

    let locator = common::git_file_locator(&repo);
    let target = format!("{locator}@{commit}:scripts/hello.py");

    let caller = temp.path().join("caller");
    fs::create_dir_all(&caller).expect("create caller dir");
    let registry = temp.path().join("runtimes.json");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&caller)
        .env("PX_RUNTIME_REGISTRY", &registry)
        .env_remove("PX_RUNTIME_PYTHON")
        .env_remove("CI")
        .args(["--json", "run", &target])
        .assert()
        .failure();

    let payload = common::parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    assert_eq!(payload["details"]["reason"], "missing_runtime");
    assert_eq!(payload["details"]["install_version"], "3.12");
}

#[test]
fn run_by_reference_relative_imports_work_from_snapshot_root() {
    let _guard = common::test_env_guard();
    common::reset_test_store_env();
    common::ensure_test_store_env();

    if !git_available() {
        eprintln!("skipping run-by-reference test (git not found)");
        return;
    }
    let Some(python) = common::find_python() else {
        eprintln!("skipping run-by-reference test (python binary not found)");
        return;
    };
    let Some(requires) = python_requirement(&python) else {
        eprintln!("skipping run-by-reference test (unable to determine python version)");
        return;
    };

    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("create repo dir");
    init_git_repo(&repo);
    fs::create_dir_all(repo.join("lib")).expect("create lib dir");
    fs::write(repo.join("lib").join("__init__.py"), "").expect("write init");
    fs::write(
        repo.join("lib").join("util.py"),
        "def greet():\n    return 'Hello from import'\n",
    )
    .expect("write util");
    write_script_with_body(
        &repo,
        &requires,
        "from lib.util import greet\nprint(greet())",
    );
    let commit = commit_all(&repo, "add importable module");

    let locator = common::git_file_locator(&repo);
    let target = format!("{locator}@{commit}:scripts/hello.py");

    let caller = temp.path().join("caller");
    fs::create_dir_all(&caller).expect("create caller dir");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&caller)
        .env("PX_RUNTIME_PYTHON", &python)
        .env_remove("CI")
        .args(["run", &target])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("Hello from import"),
        "expected import to work, got {stdout:?}"
    );
}

#[test]
fn run_by_reference_missing_script_path_error_is_clean() {
    let _guard = common::test_env_guard();
    common::reset_test_store_env();
    common::ensure_test_store_env();

    if !git_available() {
        eprintln!("skipping run-by-reference test (git not found)");
        return;
    }
    let Some(python) = common::find_python() else {
        eprintln!("skipping run-by-reference test (python binary not found)");
        return;
    };

    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("create repo dir");
    init_git_repo(&repo);
    fs::write(repo.join("README.md"), "hello").expect("write readme");
    let commit = commit_all(&repo, "initial");

    let locator = common::git_file_locator(&repo);
    let target = format!("{locator}@{commit}:does/not/exist.py");

    let caller = temp.path().join("caller");
    fs::create_dir_all(&caller).expect("create caller dir");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&caller)
        .env("PX_RUNTIME_PYTHON", &python)
        .env_remove("CI")
        .args(["run", &target])
        .assert()
        .failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("script path does not exist in snapshot"),
        "expected clean missing script path error, got {stderr:?}"
    );
    assert!(
        stderr.contains("check the path after ':' exists"),
        "expected actionable hint, got {stderr:?}"
    );
}

#[test]
fn run_by_reference_offline_missing_snapshot_fails() {
    let _guard = common::test_env_guard();
    common::reset_test_store_env();
    common::ensure_test_store_env();

    if !git_available() {
        eprintln!("skipping run-by-reference test (git not found)");
        return;
    }
    let Some(python) = common::find_python() else {
        eprintln!("skipping run-by-reference test (python binary not found)");
        return;
    };
    let Some(requires) = python_requirement(&python) else {
        eprintln!("skipping run-by-reference test (unable to determine python version)");
        return;
    };

    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("create repo dir");
    init_git_repo(&repo);
    write_script(&repo, &requires);
    let commit = commit_all(&repo, "add script");

    let locator = common::git_file_locator(&repo);
    let target = format!("{locator}@{commit}:scripts/hello.py");

    let caller = temp.path().join("caller");
    fs::create_dir_all(&caller).expect("create caller dir");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&caller)
        .env("PX_RUNTIME_PYTHON", &python)
        .env_remove("CI")
        .args(["--offline", "run", &target])
        .assert()
        .failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("repo snapshot is not cached"),
        "expected offline cache miss error, got {stderr:?}"
    );
}

#[test]
fn run_by_reference_floating_ref_rules() {
    let _guard = common::test_env_guard();
    common::reset_test_store_env();
    common::ensure_test_store_env();

    if !git_available() {
        eprintln!("skipping run-by-reference test (git not found)");
        return;
    }
    let Some(python) = common::find_python() else {
        eprintln!("skipping run-by-reference test (python binary not found)");
        return;
    };
    let Some(requires) = python_requirement(&python) else {
        eprintln!("skipping run-by-reference test (unable to determine python version)");
        return;
    };

    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("create repo dir");
    init_git_repo(&repo);
    write_script(&repo, &requires);
    let _commit = commit_all(&repo, "add script");

    let locator = common::git_file_locator(&repo);
    let floating = format!("{locator}:scripts/hello.py");

    let caller = temp.path().join("caller");
    fs::create_dir_all(&caller).expect("create caller dir");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&caller)
        .env("PX_RUNTIME_PYTHON", &python)
        .env_remove("CI")
        .args(["run", &floating])
        .assert()
        .failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("pinned commit") || stderr.contains("--allow-floating"),
        "expected pinned-by-default error, got {stderr:?}"
    );

    cargo_bin_cmd!("px")
        .current_dir(&caller)
        .env("PX_RUNTIME_PYTHON", &python)
        .env_remove("CI")
        .args(["run", "--allow-floating", &floating])
        .assert()
        .success();

    cargo_bin_cmd!("px")
        .current_dir(&caller)
        .env("PX_RUNTIME_PYTHON", &python)
        .env("CI", "1")
        .args(["run", "--allow-floating", &floating])
        .assert()
        .failure();

    cargo_bin_cmd!("px")
        .current_dir(&caller)
        .env("PX_RUNTIME_PYTHON", &python)
        .env_remove("CI")
        .args(["run", "--frozen", "--allow-floating", &floating])
        .assert()
        .failure();
}

#[test]
fn run_by_reference_short_sha_is_rejected_by_default_but_resolves_with_allow_floating() {
    let _guard = common::test_env_guard();
    common::reset_test_store_env();
    common::ensure_test_store_env();

    if !git_available() {
        eprintln!("skipping run-by-reference test (git not found)");
        return;
    }
    let Some(python) = common::find_python() else {
        eprintln!("skipping run-by-reference test (python binary not found)");
        return;
    };
    let Some(requires) = python_requirement(&python) else {
        eprintln!("skipping run-by-reference test (unable to determine python version)");
        return;
    };

    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("create repo dir");
    init_git_repo(&repo);
    write_script(&repo, &requires);
    let commit = commit_all(&repo, "add script");
    let short = &commit[..8];

    let locator = common::git_file_locator(&repo);
    let target = format!("{locator}@{short}:scripts/hello.py");

    let caller = temp.path().join("caller");
    fs::create_dir_all(&caller).expect("create caller dir");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&caller)
        .env("PX_RUNTIME_PYTHON", &python)
        .env_remove("CI")
        .args(["run", &target])
        .assert()
        .failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("full commit SHA"),
        "expected full SHA guidance, got {stderr:?}"
    );

    let assert = cargo_bin_cmd!("px")
        .current_dir(&caller)
        .env("PX_RUNTIME_PYTHON", &python)
        .env_remove("CI")
        .args(["run", "--allow-floating", &target])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("Hello from run-by-reference"),
        "expected script output, got {stdout:?}"
    );
}

#[test]
fn run_by_reference_rejects_credentials_in_locator_without_leaking() {
    let _guard = common::test_env_guard();
    common::reset_test_store_env();
    common::ensure_test_store_env();

    let Some(python) = common::find_python() else {
        eprintln!("skipping run-by-reference test (python binary not found)");
        return;
    };

    let caller = tempfile::tempdir().expect("tempdir");
    let target = "git+https://user:supersecret@example.invalid/repo.git@0123456789abcdef0123456789abcdef01234567:scripts/hello.py";

    let assert = cargo_bin_cmd!("px")
        .current_dir(caller.path())
        .env("PX_RUNTIME_PYTHON", &python)
        .env_remove("CI")
        .args(["run", target])
        .assert()
        .failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("credentials"),
        "expected credential error, got {stderr:?}"
    );
    assert!(
        !stderr.contains("supersecret"),
        "must not leak credentials, got {stderr:?}"
    );
}
