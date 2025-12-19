use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use assert_cmd::cargo::cargo_bin_cmd;
use px_domain::api::ProjectSnapshot;

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

fn commit_all(repo: &Path, message: &str) -> String {
    git(repo, &["add", "."]);
    git(repo, &["commit", "-m", message]);
    git(repo, &["rev-parse", "HEAD"])
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

fn write_run_script(repo: &Path, requires: &str) -> PathBuf {
    let script = repo.join("scripts").join("run.py");
    fs::create_dir_all(script.parent().expect("script parent")).expect("create scripts dir");
    let contents = format!(
        "#!/usr/bin/env python3\n# /// script\n# requires-python = \"{requires}\"\n# dependencies = []\n# ///\nfrom demo_pkg.cli import main\nmain()\n"
    );
    fs::write(&script, contents).expect("write script");
    script
}

fn write_src_pkg(repo: &Path) {
    let pkg_dir = repo.join("src").join("demo_pkg");
    fs::create_dir_all(&pkg_dir).expect("create pkg dir");
    fs::write(pkg_dir.join("__init__.py"), "").expect("write init");
    fs::write(
        pkg_dir.join("cli.py"),
        "def main():\n    print('hello from demo_pkg')\n",
    )
    .expect("write cli");
}

fn write_pyproject_with_script(root: &Path, name: &str, python_req: &str) {
    let pyproject = format!(
        r#"[project]
name = "{name}"
version = "0.1.0"
requires-python = "{python_req}"
dependencies = []

[project.scripts]
demo = "demo_pkg.cli:main"

[build-system]
requires = ["setuptools"]
build-backend = "setuptools.build_meta"
"#
    );
    fs::write(root.join("pyproject.toml"), pyproject).expect("write pyproject.toml");
}

fn write_pyproject_with_scripts(root: &Path, name: &str, python_req: &str, scripts: &[(&str, &str)]) {
    let mut scripts_body = String::new();
    for (script, target) in scripts {
        scripts_body.push_str(&format!("{script} = \"{target}\"\n"));
    }
    let pyproject = format!(
        r#"[project]
name = "{name}"
version = "0.1.0"
requires-python = "{python_req}"
dependencies = []

[project.scripts]
{scripts_body}
[build-system]
requires = ["setuptools"]
build-backend = "setuptools.build_meta"
"#
    );
    fs::write(root.join("pyproject.toml"), pyproject).expect("write pyproject.toml");
}

fn write_minimal_lock(root: &Path, project_name: &str, python_req: &str) {
    let snapshot = ProjectSnapshot::read_from(root).expect("read project snapshot");
    let manifest_fingerprint = snapshot.manifest_fingerprint;
    let contents = format!(
        "version = 1\n\n[metadata]\nmode = \"p0-pinned\"\n\nmanifest_fingerprint = \"{manifest_fingerprint}\"\n\n[project]\nname = \"{project_name}\"\n\n[python]\nrequirement = \"{python_req}\"\n"
    );
    fs::write(root.join("px.lock"), contents).expect("write px.lock");
}

#[test]
fn run_url_github_blob_pinned_works_and_does_not_touch_cwd() {
    let _guard = common::test_env_guard();
    common::reset_test_store_env();
    common::ensure_test_store_env();

    if !git_available() {
        eprintln!("skipping URL run test (git not found)");
        return;
    }
    let Some(python) = common::find_python() else {
        eprintln!("skipping URL run test (python binary not found)");
        return;
    };
    let Some(requires) = python_requirement(&python) else {
        eprintln!("skipping URL run test (unable to determine python version)");
        return;
    };

    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("create repo dir");
    init_git_repo(&repo);
    write_src_pkg(&repo);
    write_run_script(&repo, &requires);
    let commit = commit_all(&repo, "add script");

    let url = format!("https://github.com/Foo/Bar/blob/{commit}/scripts/run.py");

    let caller = temp.path().join("caller");
    fs::create_dir_all(&caller).expect("create caller dir");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&caller)
        .env("PX_RUNTIME_PYTHON", &python)
        .env("PX_TEST_GITHUB_FILE_REPO", repo.display().to_string())
        .env_remove("CI")
        .args(["run", &url])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("hello from demo_pkg"),
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
}

#[test]
fn run_url_requires_pin_by_default_and_prints_fix() {
    let _guard = common::test_env_guard();
    common::reset_test_store_env();
    common::ensure_test_store_env();

    if !git_available() {
        eprintln!("skipping URL run pinning test (git not found)");
        return;
    }
    let Some(python) = common::find_python() else {
        eprintln!("skipping URL run pinning test (python binary not found)");
        return;
    };
    let Some(requires) = python_requirement(&python) else {
        eprintln!("skipping URL run pinning test (unable to determine python version)");
        return;
    };

    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("create repo dir");
    init_git_repo(&repo);
    write_src_pkg(&repo);
    write_run_script(&repo, &requires);
    let _commit = commit_all(&repo, "add script");

    let url = "https://github.com/Foo/Bar/blob/HEAD/scripts/run.py";

    let caller = temp.path().join("caller");
    fs::create_dir_all(&caller).expect("create caller dir");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&caller)
        .env("PX_RUNTIME_PYTHON", &python)
        .env("PX_TEST_GITHUB_FILE_REPO", repo.display().to_string())
        .env_remove("CI")
        .args(["run", url])
        .assert()
        .failure();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("unpinned URL refused"),
        "expected pinned-by-default URL error, got {stdout:?}"
    );
    assert!(
        stdout.contains("Fix:") && stdout.contains("--allow-floating"),
        "expected Fix guidance for allow-floating, got {stdout:?}"
    );
}

#[test]
fn run_url_floating_is_refused_in_ci_and_frozen_even_with_allow_floating() {
    let _guard = common::test_env_guard();
    common::reset_test_store_env();
    common::ensure_test_store_env();

    if !git_available() {
        eprintln!("skipping URL run floating ref test (git not found)");
        return;
    }
    let Some(python) = common::find_python() else {
        eprintln!("skipping URL run floating ref test (python binary not found)");
        return;
    };
    let Some(requires) = python_requirement(&python) else {
        eprintln!("skipping URL run floating ref test (unable to determine python version)");
        return;
    };

    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("create repo dir");
    init_git_repo(&repo);
    write_src_pkg(&repo);
    write_run_script(&repo, &requires);
    let _commit = commit_all(&repo, "add script");

    let url = "https://github.com/Foo/Bar/blob/HEAD/scripts/run.py";
    let caller = temp.path().join("caller");
    fs::create_dir_all(&caller).expect("create caller dir");

    cargo_bin_cmd!("px")
        .current_dir(&caller)
        .env("PX_RUNTIME_PYTHON", &python)
        .env("PX_TEST_GITHUB_FILE_REPO", repo.display().to_string())
        .env("CI", "1")
        .args(["run", "--allow-floating", url])
        .assert()
        .failure();

    cargo_bin_cmd!("px")
        .current_dir(&caller)
        .env("PX_RUNTIME_PYTHON", &python)
        .env("PX_TEST_GITHUB_FILE_REPO", repo.display().to_string())
        .env_remove("CI")
        .args(["run", "--frozen", "--allow-floating", url])
        .assert()
        .failure();
}

#[test]
fn run_url_offline_requires_cached_snapshot() {
    let _guard = common::test_env_guard();
    common::reset_test_store_env();
    common::ensure_test_store_env();

    if !git_available() {
        eprintln!("skipping URL offline test (git not found)");
        return;
    }
    let Some(python) = common::find_python() else {
        eprintln!("skipping URL offline test (python binary not found)");
        return;
    };
    let Some(requires) = python_requirement(&python) else {
        eprintln!("skipping URL offline test (unable to determine python version)");
        return;
    };

    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("create repo dir");
    init_git_repo(&repo);
    write_src_pkg(&repo);
    write_run_script(&repo, &requires);
    let commit = commit_all(&repo, "add script");

    let url = format!("https://github.com/Foo/Bar/blob/{commit}/scripts/run.py");
    let caller = temp.path().join("caller");
    fs::create_dir_all(&caller).expect("create caller dir");

    // Cache miss should fail cleanly.
    let assert = cargo_bin_cmd!("px")
        .current_dir(&caller)
        .env("PX_RUNTIME_PYTHON", &python)
        .env("PX_TEST_GITHUB_FILE_REPO", repo.display().to_string())
        .env_remove("CI")
        .args(["--offline", "run", &url])
        .assert()
        .failure();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("repo snapshot is not cached"),
        "expected offline cache miss error, got {stdout:?}"
    );

    // Populate cache online.
    cargo_bin_cmd!("px")
        .current_dir(&caller)
        .env("PX_RUNTIME_PYTHON", &python)
        .env("PX_TEST_GITHUB_FILE_REPO", repo.display().to_string())
        .env_remove("CI")
        .args(["run", &url])
        .assert()
        .success();

    // Move the repo away to prove offline does not touch it.
    let moved = temp.path().join("repo-moved");
    fs::rename(&repo, &moved).expect("rename repo dir");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&caller)
        .env("PX_RUNTIME_PYTHON", &python)
        .env("PX_TEST_GITHUB_FILE_REPO", repo.display().to_string())
        .env_remove("CI")
        .args(["--offline", "run", &url])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("hello from demo_pkg"),
        "expected cached script output, got {stdout:?}"
    );
}

#[test]
fn run_url_sys_path_parity_supports_src_layout_like_local_console_script() {
    let _guard = common::test_env_guard();
    common::reset_test_store_env();
    common::ensure_test_store_env();

    if !git_available() {
        eprintln!("skipping URL sys.path parity test (git not found)");
        return;
    }
    let Some(python) = common::find_python() else {
        eprintln!("skipping URL sys.path parity test (python binary not found)");
        return;
    };
    let Some(python_req) = python_requirement(&python) else {
        eprintln!("skipping URL sys.path parity test (unable to determine python version)");
        return;
    };

    // Local project: `px run demo` should work and import from src/ layout.
    let temp = tempfile::tempdir().expect("tempdir");
    let local_project = temp.path().join("local");
    fs::create_dir_all(&local_project).expect("create local project dir");
    write_src_pkg(&local_project);
    write_pyproject_with_script(&local_project, "local_demo", &python_req);
    write_minimal_lock(&local_project, "local_demo", &python_req);

    let assert = cargo_bin_cmd!("px")
        .current_dir(&local_project)
        .env("PX_RUNTIME_PYTHON", &python)
        .env_remove("CI")
        .args(["run", "demo"])
        .assert()
        .success();
    let local_stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        local_stdout.contains("hello from demo_pkg"),
        "expected local console_script output, got {local_stdout:?}"
    );

    // Remote URL: run a script out of a pinned repo snapshot that imports from src/ layout.
    let remote_repo = temp.path().join("remote-repo");
    fs::create_dir_all(&remote_repo).expect("create remote repo dir");
    init_git_repo(&remote_repo);
    write_src_pkg(&remote_repo);
    write_run_script(&remote_repo, &python_req);
    let commit = commit_all(&remote_repo, "add src-layout script");

    let url = format!("https://github.com/Foo/Bar/blob/{commit}/scripts/run.py");
    let caller = temp.path().join("caller");
    fs::create_dir_all(&caller).expect("create caller dir");
    let assert = cargo_bin_cmd!("px")
        .current_dir(&caller)
        .env("PX_RUNTIME_PYTHON", &python)
        .env("PX_TEST_GITHUB_FILE_REPO", remote_repo.display().to_string())
        .env_remove("CI")
        .args(["run", &url])
        .assert()
        .success();
    let remote_stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        remote_stdout.contains("hello from demo_pkg"),
        "expected remote src-layout script output, got {remote_stdout:?}"
    );
}

#[test]
fn run_url_repo_tree_pinned_infers_console_script_entrypoint() {
    let _guard = common::test_env_guard();
    common::reset_test_store_env();
    common::ensure_test_store_env();

    if !git_available() {
        eprintln!("skipping URL repo test (git not found)");
        return;
    }
    let Some(python) = common::find_python() else {
        eprintln!("skipping URL repo test (python binary not found)");
        return;
    };
    let Some(python_req) = python_requirement(&python) else {
        eprintln!("skipping URL repo test (unable to determine python version)");
        return;
    };

    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("create repo dir");
    init_git_repo(&repo);
    write_src_pkg(&repo);
    write_pyproject_with_script(&repo, "remote_demo", &python_req);
    let commit = commit_all(&repo, "add pyproject + src");

    let url = format!("https://github.com/Foo/Bar/tree/{commit}/");
    let caller = temp.path().join("caller");
    fs::create_dir_all(&caller).expect("create caller dir");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&caller)
        .env("PX_RUNTIME_PYTHON", &python)
        .env("PX_TEST_GITHUB_FILE_REPO", repo.display().to_string())
        .env_remove("CI")
        .args(["run", &url])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("hello from demo_pkg"),
        "expected inferred console_script output, got {stdout:?}"
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
}

#[test]
fn run_url_repo_tree_requires_explicit_entrypoint_when_ambiguous() {
    let _guard = common::test_env_guard();
    common::reset_test_store_env();
    common::ensure_test_store_env();

    if !git_available() {
        eprintln!("skipping URL repo ambiguity test (git not found)");
        return;
    }
    let Some(python) = common::find_python() else {
        eprintln!("skipping URL repo ambiguity test (python binary not found)");
        return;
    };
    let Some(python_req) = python_requirement(&python) else {
        eprintln!("skipping URL repo ambiguity test (unable to determine python version)");
        return;
    };

    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("create repo dir");
    init_git_repo(&repo);
    write_src_pkg(&repo);
    fs::write(
        repo.join("src")
            .join("demo_pkg")
            .join("cli2.py"),
        "def main():\n    print('hello from demo_pkg two')\n",
    )
    .expect("write cli2");
    write_pyproject_with_scripts(
        &repo,
        "remote_demo",
        &python_req,
        &[
            ("demo", "demo_pkg.cli:main"),
            ("demo2", "demo_pkg.cli2:main"),
        ],
    );
    let commit = commit_all(&repo, "add multiple scripts");

    let url = format!("https://github.com/Foo/Bar/tree/{commit}/");
    let caller = temp.path().join("caller");
    fs::create_dir_all(&caller).expect("create caller dir");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&caller)
        .env("PX_RUNTIME_PYTHON", &python)
        .env("PX_TEST_GITHUB_FILE_REPO", repo.display().to_string())
        .env_remove("CI")
        .args(["run", &url])
        .assert()
        .failure();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("multiple console scripts"),
        "expected ambiguity error, got {stdout:?}"
    );
    assert!(
        stdout.contains("Specify one explicitly"),
        "expected hint to specify an entrypoint, got {stdout:?}"
    );
}

#[test]
fn run_url_repo_tree_allows_explicit_entrypoint_arg() {
    let _guard = common::test_env_guard();
    common::reset_test_store_env();
    common::ensure_test_store_env();

    if !git_available() {
        eprintln!("skipping URL repo entrypoint arg test (git not found)");
        return;
    }
    let Some(python) = common::find_python() else {
        eprintln!("skipping URL repo entrypoint arg test (python binary not found)");
        return;
    };
    let Some(python_req) = python_requirement(&python) else {
        eprintln!("skipping URL repo entrypoint arg test (unable to determine python version)");
        return;
    };

    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("create repo dir");
    init_git_repo(&repo);
    write_src_pkg(&repo);
    fs::write(
        repo.join("src")
            .join("demo_pkg")
            .join("cli2.py"),
        "def main():\n    print('hello from demo_pkg two')\n",
    )
    .expect("write cli2");
    write_pyproject_with_scripts(
        &repo,
        "remote_demo",
        &python_req,
        &[
            ("demo", "demo_pkg.cli:main"),
            ("demo2", "demo_pkg.cli2:main"),
        ],
    );
    let commit = commit_all(&repo, "add multiple scripts");

    let url = format!("https://github.com/Foo/Bar/tree/{commit}/");
    let caller = temp.path().join("caller");
    fs::create_dir_all(&caller).expect("create caller dir");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&caller)
        .env("PX_RUNTIME_PYTHON", &python)
        .env("PX_TEST_GITHUB_FILE_REPO", repo.display().to_string())
        .env_remove("CI")
        .args(["run", &url, "demo2"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("hello from demo_pkg two"),
        "expected explicit entrypoint output, got {stdout:?}"
    );
}

#[test]
fn run_url_repo_tree_offline_requires_cached_snapshot() {
    let _guard = common::test_env_guard();
    common::reset_test_store_env();
    common::ensure_test_store_env();

    if !git_available() {
        eprintln!("skipping URL repo offline test (git not found)");
        return;
    }
    let Some(python) = common::find_python() else {
        eprintln!("skipping URL repo offline test (python binary not found)");
        return;
    };
    let Some(python_req) = python_requirement(&python) else {
        eprintln!("skipping URL repo offline test (unable to determine python version)");
        return;
    };

    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("create repo dir");
    init_git_repo(&repo);
    write_src_pkg(&repo);
    write_pyproject_with_script(&repo, "remote_demo", &python_req);
    let commit = commit_all(&repo, "add pyproject + src");

    let url = format!("https://github.com/Foo/Bar/tree/{commit}/");
    let caller = temp.path().join("caller");
    fs::create_dir_all(&caller).expect("create caller dir");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&caller)
        .env("PX_RUNTIME_PYTHON", &python)
        .env("PX_TEST_GITHUB_FILE_REPO", repo.display().to_string())
        .env_remove("CI")
        .args(["--offline", "run", &url, "demo"])
        .assert()
        .failure();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("repo snapshot is not cached"),
        "expected offline cache miss error, got {stdout:?}"
    );

    cargo_bin_cmd!("px")
        .current_dir(&caller)
        .env("PX_RUNTIME_PYTHON", &python)
        .env("PX_TEST_GITHUB_FILE_REPO", repo.display().to_string())
        .env_remove("CI")
        .args(["run", &url, "demo"])
        .assert()
        .success();

    let moved = temp.path().join("repo-moved");
    fs::rename(&repo, &moved).expect("rename repo dir");

    let assert = cargo_bin_cmd!("px")
        .current_dir(&caller)
        .env("PX_RUNTIME_PYTHON", &python)
        .env("PX_TEST_GITHUB_FILE_REPO", repo.display().to_string())
        .env_remove("CI")
        .args(["--offline", "run", &url, "demo"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("hello from demo_pkg"),
        "expected cached repo run output, got {stdout:?}"
    );
}
