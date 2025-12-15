use assert_cmd::cargo::cargo_bin_cmd;
use std::fs;
use std::path::Path;

mod common;

use common::{parse_json, require_online};

fn contains_named_dir(root: &Path, name: &str) -> bool {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                if path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n == name)
                {
                    return true;
                }
                stack.push(path);
            }
        }
    }
    false
}

fn assert_no_ephemeral_writes(root: &Path) {
    assert!(
        !root.join(".px").exists(),
        "ephemeral runs must not create .px/ in {}",
        root.display()
    );
    assert!(
        !root.join("px.lock").exists(),
        "ephemeral runs must not create px.lock in {}",
        root.display()
    );
    assert!(
        !contains_named_dir(root, ".pytest_cache"),
        "ephemeral runs must not create .pytest_cache under {}",
        root.display()
    );
    assert!(
        !contains_named_dir(root, "__pycache__"),
        "ephemeral runs must not create __pycache__ under {}",
        root.display()
    );
}

#[test]
fn ephemeral_run_pyproject_offline_reuses_cache_across_dirs() {
    if !require_online() {
        return;
    }
    let _guard = common::test_env_guard();
    common::reset_test_store_env();
    common::ensure_test_store_env();

    let Some(python) = common::find_python() else {
        eprintln!("skipping ephemeral run test (python binary not found)");
        return;
    };

    let temp = tempfile::tempdir().expect("tempdir");
    let proj1 = temp.path().join("proj1");
    let proj2 = temp.path().join("proj2");
    fs::create_dir_all(&proj1).expect("create proj1");
    fs::create_dir_all(&proj2).expect("create proj2");

    let pyproject = r#"[project]
name = "ephemeral-demo"
version = "0.0.0"
requires-python = ">=3.8"
dependencies = ["colorama==0.4.6"]
"#;
    fs::write(proj1.join("pyproject.toml"), pyproject).expect("write pyproject");
    fs::write(proj2.join("pyproject.toml"), pyproject).expect("write pyproject");

    let script = r#"import colorama
print("COLORAMA=" + colorama.__version__)
"#;
    fs::write(proj1.join("hello.py"), script).expect("write script");
    fs::write(proj2.join("hello.py"), script).expect("write script");

    let pyproject_before = fs::read_to_string(proj1.join("pyproject.toml")).expect("read");
    cargo_bin_cmd!("px")
        .current_dir(&proj1)
        .env("PX_RUNTIME_PYTHON", &python)
        .env("PX_ONLINE", "1")
        .env_remove("CI")
        .args(["--json", "run", "--ephemeral", "hello.py"])
        .assert()
        .success();
    let pyproject_after = fs::read_to_string(proj1.join("pyproject.toml")).expect("read");
    assert_eq!(
        pyproject_after, pyproject_before,
        "ephemeral run must not mutate pyproject.toml"
    );
    assert_no_ephemeral_writes(&proj1);

    let assert = cargo_bin_cmd!("px")
        .current_dir(&proj2)
        .env("PX_RUNTIME_PYTHON", &python)
        .env_remove("CI")
        .args(["--offline", "--json", "run", "--ephemeral", "hello.py"])
        .assert()
        .success();
    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    let stdout = payload["details"]["stdout"].as_str().unwrap_or_default();
    assert!(
        stdout.contains("COLORAMA="),
        "expected script output, got stdout={stdout:?}"
    );
    assert_no_ephemeral_writes(&proj2);
}

#[test]
fn ephemeral_run_requirements_works_and_creates_no_files() {
    if !require_online() {
        return;
    }
    let _guard = common::test_env_guard();
    common::reset_test_store_env();
    common::ensure_test_store_env();

    let Some(python) = common::find_python() else {
        eprintln!("skipping ephemeral run test (python binary not found)");
        return;
    };

    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("proj");
    fs::create_dir_all(&root).expect("create project dir");

    fs::write(root.join("requirements.txt"), "colorama==0.4.6\n").expect("write requirements");
    let script = r#"import colorama
print("COLORAMA_REQ=" + colorama.__version__)
"#;
    fs::write(root.join("hello.py"), script).expect("write script");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&root)
        .env("PX_RUNTIME_PYTHON", &python)
        .env("PX_ONLINE", "1")
        .env_remove("CI")
        .args(["--json", "run", "--ephemeral", "hello.py"])
        .assert()
        .success();
    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    let stdout = payload["details"]["stdout"].as_str().unwrap_or_default();
    assert!(
        stdout.contains("COLORAMA_REQ="),
        "expected script output, got stdout={stdout:?}"
    );
    assert_no_ephemeral_writes(&root);
}

#[test]
fn ephemeral_run_unpinned_pyproject_offline_reuses_cached_lock() {
    if !require_online() {
        return;
    }
    let _guard = common::test_env_guard();
    common::reset_test_store_env();
    common::ensure_test_store_env();

    let Some(python) = common::find_python() else {
        eprintln!("skipping ephemeral run test (python binary not found)");
        return;
    };

    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("proj");
    fs::create_dir_all(&root).expect("create project dir");

    let pyproject = r#"[project]
name = "ephemeral-demo"
version = "0.0.0"
requires-python = ">=3.8"
dependencies = ["colorama>=0.4.0"]
"#;
    fs::write(root.join("pyproject.toml"), pyproject).expect("write pyproject");
    fs::write(
        root.join("hello.py"),
        "import colorama\nprint(colorama.__version__)\n",
    )
    .expect("write script");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&root)
        .env("PX_RUNTIME_PYTHON", &python)
        .env("PX_ONLINE", "1")
        .env_remove("CI")
        .args(["--json", "run", "--ephemeral", "hello.py"])
        .assert()
        .success();
    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    assert_no_ephemeral_writes(&root);

    let assert = cargo_bin_cmd!("px")
        .current_dir(&root)
        .env("PX_RUNTIME_PYTHON", &python)
        .env_remove("CI")
        .args(["--offline", "--json", "run", "--ephemeral", "hello.py"])
        .assert()
        .success();
    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    assert_no_ephemeral_writes(&root);
}

#[test]
fn ephemeral_run_requirements_supports_includes() {
    if !require_online() {
        return;
    }
    let _guard = common::test_env_guard();
    common::reset_test_store_env();
    common::ensure_test_store_env();

    let Some(python) = common::find_python() else {
        eprintln!("skipping ephemeral run test (python binary not found)");
        return;
    };

    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("proj");
    fs::create_dir_all(&root).expect("create project dir");

    fs::write(root.join("requirements-base.txt"), "colorama==0.4.6\n").expect("write base");
    fs::write(root.join("requirements.txt"), "-r requirements-base.txt\n").expect("write reqs");
    fs::write(
        root.join("hello.py"),
        "import colorama\nprint(colorama.__version__)\n",
    )
    .expect("write script");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&root)
        .env("PX_RUNTIME_PYTHON", &python)
        .env("PX_ONLINE", "1")
        .env_remove("CI")
        .args(["--json", "run", "--ephemeral", "hello.py"])
        .assert()
        .success();
    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    assert_no_ephemeral_writes(&root);
}

#[test]
fn ephemeral_run_requirements_supports_pip_compile_hashes() {
    if !require_online() {
        return;
    }
    let _guard = common::test_env_guard();
    common::reset_test_store_env();
    common::ensure_test_store_env();

    let Some(python) = common::find_python() else {
        eprintln!("skipping ephemeral run test (python binary not found)");
        return;
    };

    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("proj");
    fs::create_dir_all(&root).expect("create project dir");

    let requirements = r#"colorama==0.4.6 \
    --hash=sha256:deadbeef \
    --hash=sha256:cafebabe
"#;
    fs::write(root.join("requirements.txt"), requirements).expect("write requirements");
    fs::write(
        root.join("hello.py"),
        "import colorama\nprint('COLORAMA_HASHED=' + colorama.__version__)\n",
    )
    .expect("write script");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&root)
        .env("PX_RUNTIME_PYTHON", &python)
        .env("PX_ONLINE", "1")
        .env_remove("CI")
        .args(["--json", "run", "--ephemeral", "hello.py"])
        .assert()
        .success();
    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    let stdout = payload["details"]["stdout"].as_str().unwrap_or_default();
    assert!(
        stdout.contains("COLORAMA_HASHED="),
        "expected script output, got stdout={stdout:?}"
    );
    assert_no_ephemeral_writes(&root);

    let assert = cargo_bin_cmd!("px")
        .current_dir(&root)
        .env("PX_RUNTIME_PYTHON", &python)
        .env_remove("CI")
        .args(["--offline", "--json", "run", "--ephemeral", "hello.py"])
        .assert()
        .success();
    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    let stdout = payload["details"]["stdout"].as_str().unwrap_or_default();
    assert!(
        stdout.contains("COLORAMA_HASHED="),
        "expected script output, got stdout={stdout:?}"
    );
    assert_no_ephemeral_writes(&root);
}

#[test]
fn ephemeral_run_refuses_unpinned_inputs_in_ci() {
    let _guard = common::test_env_guard();
    common::reset_test_store_env();
    common::ensure_test_store_env();

    let Some(python) = common::find_python() else {
        eprintln!("skipping ephemeral run pinned test (python binary not found)");
        return;
    };

    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("proj");
    fs::create_dir_all(&root).expect("create project dir");

    let pyproject = r#"[project]
name = "ephemeral-demo"
version = "0.0.0"
requires-python = ">=3.8"
dependencies = ["colorama>=0.4.6"]
"#;
    fs::write(root.join("pyproject.toml"), pyproject).expect("write pyproject");
    fs::write(root.join("hello.py"), "print('nope')\n").expect("write script");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&root)
        .env("PX_RUNTIME_PYTHON", &python)
        .env("CI", "1")
        .args(["--json", "run", "--ephemeral", "hello.py"])
        .assert()
        .failure();
    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    let message = payload["message"].as_str().unwrap_or_default();
    assert!(
        message.to_ascii_lowercase().contains("fully pinned"),
        "expected pinned-inputs refusal message, got {message:?}"
    );
    assert_eq!(payload["details"]["reason"], "ephemeral_unpinned_inputs");
    assert_no_ephemeral_writes(&root);
}

#[test]
fn ephemeral_run_refuses_local_path_requirements() {
    let _guard = common::test_env_guard();
    common::reset_test_store_env();
    common::ensure_test_store_env();

    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("proj");
    fs::create_dir_all(&root).expect("create project dir");

    fs::write(root.join("requirements.txt"), "./localpkg\n").expect("write requirements");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&root)
        .env_remove("CI")
        .args([
            "--json",
            "run",
            "--ephemeral",
            "python",
            "-c",
            "print('hi')",
        ])
        .assert()
        .failure();
    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    assert_eq!(
        payload["details"]["reason"],
        "ephemeral_requirements_local_path_unsupported"
    );
    assert_no_ephemeral_writes(&root);
}

#[test]
fn ephemeral_test_refuses_unpinned_inputs_when_frozen() {
    let _guard = common::test_env_guard();
    common::reset_test_store_env();
    common::ensure_test_store_env();

    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("proj");
    fs::create_dir_all(&root).expect("create project dir");

    fs::write(root.join("requirements.txt"), "colorama\n").expect("write requirements");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&root)
        .env_remove("CI")
        .args(["--json", "test", "--ephemeral", "--frozen"])
        .assert()
        .failure();
    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    assert_eq!(payload["details"]["reason"], "ephemeral_unpinned_inputs");
    assert_no_ephemeral_writes(&root);
}

#[test]
fn ephemeral_test_pyproject_runs_without_writing_state_files() {
    if !require_online() {
        return;
    }
    let _guard = common::test_env_guard();
    common::reset_test_store_env();
    common::ensure_test_store_env();

    let Some(python) = common::find_python() else {
        eprintln!("skipping ephemeral test (python binary not found)");
        return;
    };

    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("proj");
    fs::create_dir_all(&root).expect("create project dir");
    fs::create_dir_all(root.join("tests")).expect("create tests dir");

    let pyproject = r#"[project]
name = "ephemeral-test-demo"
version = "0.0.0"
requires-python = ">=3.8"
dependencies = ["pytest==8.3.3"]
"#;
    fs::write(root.join("pyproject.toml"), pyproject).expect("write pyproject");
    fs::write(
        root.join("tests").join("test_ok.py"),
        "def test_ok():\n    assert True\n",
    )
    .expect("write test");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&root)
        .env("PX_RUNTIME_PYTHON", &python)
        .env("PX_ONLINE", "1")
        .env_remove("CI")
        .args(["--json", "test", "--ephemeral"])
        .assert()
        .success();
    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    assert_no_ephemeral_writes(&root);
}
