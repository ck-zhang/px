use std::{fs, path::Path};

use assert_cmd::cargo::cargo_bin_cmd;
use serde_json::{json, Value};
use tempfile::TempDir;
use toml_edit::DocumentMut;

mod common;

use common::{parse_json, prepare_fixture, require_online, test_env_guard};

fn px_cmd() -> assert_cmd::Command {
    common::ensure_test_store_env();
    cargo_bin_cmd!("px")
}

#[test]
fn project_init_creates_minimal_shape() {
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let project_dir = temp.path().join("demo_shape");
    fs::create_dir_all(&project_dir).expect("create project dir");

    px_cmd()
        .current_dir(&project_dir)
        .arg("init")
        .assert()
        .success();

    let pyproject = project_dir.join("pyproject.toml");
    assert!(pyproject.exists(), "pyproject should be created");
    assert!(
        project_dir.join("px.lock").exists(),
        "px init must create px.lock"
    );
    assert!(
        !project_dir.join("demo_shape").exists(),
        "px init must not scaffold package code"
    );
    assert!(
        !project_dir.join("dist").exists(),
        "px init must not create dist/"
    );

    let px_dir = project_dir.join(".px");
    assert!(px_dir.is_dir(), ".px directory should exist after init");
    assert!(px_dir.join("envs").is_dir(), ".px/envs should exist");
    assert!(px_dir.join("logs").is_dir(), ".px/logs should exist");
    assert!(
        px_dir.join("state.json").exists(),
        ".px/state.json should exist"
    );
    let state: Value =
        serde_json::from_str(&fs::read_to_string(px_dir.join("state.json")).expect("read state"))
            .expect("state json");
    let env = state
        .get("current_env")
        .and_then(|value| value.as_object())
        .expect("active env state");
    let site = env
        .get("site_packages")
        .and_then(Value::as_str)
        .expect("site path");
    let site_path = Path::new(site);
    assert!(site_path.join("px.pth").exists(), "env px.pth should exist");
    assert!(
        site_path.starts_with(px_dir.join("envs")),
        "site should live under .px/envs"
    );

    let contents = fs::read_to_string(&pyproject).expect("read pyproject");
    let doc: DocumentMut = contents.parse().expect("valid pyproject");
    let project = doc["project"].as_table().expect("project table");
    let deps = project
        .get("dependencies")
        .and_then(|item| item.as_array())
        .map_or(0, toml_edit::Array::len);
    assert_eq!(deps, 0, "dependencies should start empty");
    assert!(
        doc.get("tool")
            .and_then(|tool| tool.as_table())
            .and_then(|table| table.get("px"))
            .is_some(),
        "pyproject should include [tool.px]"
    );
}

#[test]
fn project_init_infers_package_name_from_directory() {
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let project_dir = temp.path().join("Fancy-App");
    fs::create_dir_all(&project_dir).expect("create project dir");

    let assert = px_cmd()
        .current_dir(&project_dir)
        .args(["init"])
        .assert()
        .success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("px init: initialized project fancy_app"),
        "expected concise init message, got {stdout:?}"
    );

    let name = read_project_name(project_dir.join("pyproject.toml"));
    assert_eq!(name, "fancy_app");
}

#[test]
fn project_init_refuses_when_pyproject_exists() {
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    scaffold_demo(&temp, "demo_pkg");
    let project_dir = temp.path();

    let assert = px_cmd()
        .current_dir(project_dir)
        .args(["init"])
        .assert()
        .failure();

    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("PX101") && stderr.contains("project already initialized"),
        "expected polite refusal, got {stderr:?}"
    );
    assert!(stderr.contains("Fix:"), "expected remediation guidance");
}

#[test]
fn project_init_reports_orphaned_lockfile() {
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let project_dir = temp.path();
    fs::write(project_dir.join("px.lock"), "").expect("write px.lock");

    let assert = px_cmd()
        .current_dir(project_dir)
        .args(["init"])
        .assert()
        .failure();

    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("px.lock found but pyproject.toml is missing"),
        "expected missing manifest warning, got {stderr:?}"
    );
    assert!(
        stderr.contains("px.lock"),
        "should mention existing px.lock: {stderr:?}"
    );
    let lower = stderr.to_ascii_lowercase();
    assert!(
        lower.contains("restore pyproject.toml") || lower.contains("restore"),
        "expected remediation about restoring/removing artifacts: {stderr:?}"
    );
}

#[test]
fn project_init_cleans_up_when_runtime_missing() {
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let project_dir = temp.path().join("no_runtime");
    fs::create_dir_all(&project_dir).expect("create project dir");
    let registry = temp.path().join("runtimes.json");

    let assert = px_cmd()
        .current_dir(&project_dir)
        .env("PX_RUNTIME_REGISTRY", &registry)
        .env_remove("PX_RUNTIME_PYTHON")
        .args(["init"])
        .assert()
        .failure();

    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("python runtime unavailable"),
        "expected runtime error message, got {stderr:?}"
    );
    assert!(
        !project_dir.join("pyproject.toml").exists(),
        "pyproject.toml should not be written when init fails"
    );
    assert!(
        !project_dir.join("px.lock").exists(),
        "px.lock should not be created when init fails"
    );
    assert!(
        !project_dir.join(".px").exists(),
        ".px directory should not be created when init fails"
    );
}

#[test]
fn project_surfaces_invalid_pyproject_without_trace() {
    let _guard = test_env_guard();
    let temp = tempfile::tempdir().expect("tempdir");
    let project_dir = temp.path();
    fs::write(
        project_dir.join("pyproject.toml"),
        "[project\nname = 'broken'\n",
    )
    .expect("write invalid pyproject");

    let assert = px_cmd()
        .current_dir(project_dir)
        .args(["--json", "status"])
        .assert()
        .failure();
    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    assert_eq!(payload["details"]["reason"], "invalid_manifest");
    let hint = payload["details"]["hint"].as_str().unwrap_or_default();
    assert!(
        hint.to_ascii_lowercase().contains("pyproject.toml"),
        "hint should direct user to fix pyproject: {hint:?}"
    );
}

#[test]
fn project_surfaces_invalid_lockfile_without_trace() {
    let _guard = test_env_guard();
    let temp = tempfile::tempdir().expect("tempdir");
    let project_dir = temp.path();
    fs::write(
        project_dir.join("pyproject.toml"),
        r#"[project]
name = "demo"
version = "0.1.0"
requires-python = ">=3.11"
dependencies = []

[tool]
[tool.px]

[build-system]
requires = ["setuptools>=70", "wheel"]
build-backend = "setuptools.build_meta"
"#,
    )
    .expect("write pyproject");
    fs::write(project_dir.join("px.lock"), "not toml").expect("write lockfile");

    let assert = px_cmd()
        .current_dir(project_dir)
        .args(["--json", "status"])
        .assert()
        .failure();
    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    assert_eq!(payload["details"]["reason"], "invalid_lock");
    let hint = payload["details"]["hint"].as_str().unwrap_or_default();
    assert!(
        hint.to_ascii_lowercase().contains("px sync"),
        "hint should suggest regenerating the lockfile: {hint:?}"
    );
}

#[test]
fn project_init_respects_python_operator() {
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let project_dir = temp.path().join("py-req");
    fs::create_dir_all(&project_dir).expect("create project dir");

    px_cmd()
        .current_dir(&project_dir)
        .args(["init", "--py", "~=3.11"])
        .assert()
        .success();

    let contents = fs::read_to_string(project_dir.join("pyproject.toml")).expect("read pyproject");
    let doc: DocumentMut = contents.parse().expect("valid pyproject");
    let requirement = doc["project"]["requires-python"]
        .as_str()
        .unwrap_or_default();
    assert_eq!(requirement, "~=3.11");
}

#[test]
fn project_init_dry_run_writes_nothing() {
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let project_dir = temp.path().join("dry-init");
    fs::create_dir_all(&project_dir).expect("create project dir");

    px_cmd()
        .current_dir(&project_dir)
        .args(["init", "--dry-run"])
        .assert()
        .success();

    assert!(
        !project_dir.join("pyproject.toml").exists(),
        "pyproject.toml should not be created during dry-run"
    );
    assert!(
        !project_dir.join("px.lock").exists(),
        "px.lock should not be created during dry-run"
    );
    assert!(
        !project_dir.join(".px").exists(),
        ".px directory should not be created during dry-run"
    );
}

#[test]
fn project_init_json_reports_details() {
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let project_dir = temp.path().join("json-demo");
    fs::create_dir_all(&project_dir).expect("create project dir");

    let assert = px_cmd()
        .current_dir(&project_dir)
        .args(["--json", "init"])
        .assert()
        .success();

    let payload: Value = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    assert_eq!(payload["details"]["package"], "json_demo");
    let created = payload["details"]["files_created"]
        .as_array()
        .expect("files array")
        .iter()
        .filter_map(|entry| entry.as_str())
        .collect::<Vec<_>>();
    assert!(
        created.contains(&"pyproject.toml"),
        "pyproject should be recorded in files_created: {created:?}"
    );
    assert!(
        created.contains(&"px.lock"),
        "px.lock should be recorded in files_created: {created:?}"
    );
    assert!(
        created.iter().any(|entry| entry.starts_with(".px")),
        "files_created should include .px paths: {created:?}"
    );
    assert!(
        created.iter().any(|entry| entry == &".px/state.json"),
        "state tracking should be recorded"
    );
}

#[test]
fn px_why_reports_direct_dependency() {
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
    let (_tmp, project) = prepare_fixture("why-direct");
    px_cmd()
        .current_dir(&project)
        .env("PX_ONLINE", "1")
        .arg("sync")
        .assert()
        .success();
    let assert = px_cmd()
        .current_dir(&project)
        .env("PX_ONLINE", "1")
        .args(["--json", "why", "rich"])
        .assert()
        .success();
    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    assert_eq!(payload["details"]["direct"], Value::Bool(true));
    let chains = payload["details"]["chains"]
        .as_array()
        .expect("chains array");
    assert!(
        chains.iter().any(|chain| chain == &json!(["rich"])),
        "expected at least one direct chain: {chains:?}"
    );
}

#[test]
fn px_why_reports_transitive_chain() {
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
    let (_tmp, project) = prepare_fixture("why-transitive");
    px_cmd()
        .current_dir(&project)
        .env("PX_ONLINE", "1")
        .arg("sync")
        .assert()
        .success();
    let assert = px_cmd()
        .current_dir(&project)
        .env("PX_ONLINE", "1")
        .args(["--json", "why", "markdown-it-py"])
        .assert()
        .success();
    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    assert_eq!(payload["details"]["direct"], Value::Bool(false));
    let chains = payload["details"]["chains"]
        .as_array()
        .expect("chains array");
    assert!(!chains.is_empty(), "expected at least one dependency chain");
    let first = chains[0]
        .as_array()
        .expect("first chain array")
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    assert_eq!(
        first.last(),
        Some(&"markdown-it-py"),
        "chain should terminate at target: {first:?}"
    );
    assert_eq!(
        first.first(),
        Some(&"rich"),
        "rich should be the root pulling markdown-it-py"
    );
}

#[test]
fn project_add_inserts_dependency() {
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    scaffold_demo(&temp, "demo_add");
    let project_dir = temp.path();

    px_cmd()
        .current_dir(project_dir)
        .args(["add", "requests==2.32.3"])
        .assert()
        .success();

    let deps = read_dependencies(project_dir.join("pyproject.toml"));
    assert!(deps.iter().any(|dep| dep == "requests==2.32.3"));
}

#[test]
fn project_add_dry_run_leaves_project_unchanged() {
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    scaffold_demo(&temp, "demo_add_dry_run");
    let project_dir = temp.path();
    let pyproject = project_dir.join("pyproject.toml");
    let lock = project_dir.join("px.lock");
    let pyproject_before = fs::read_to_string(&pyproject).expect("read pyproject");
    let lock_before = fs::read_to_string(&lock).expect("read lockfile");

    px_cmd()
        .current_dir(project_dir)
        .args(["add", "requests", "--dry-run"])
        .assert()
        .success();

    assert_eq!(
        fs::read_to_string(&pyproject).expect("read pyproject after"),
        pyproject_before,
        "pyproject.toml should remain unchanged after dry-run add"
    );
    assert_eq!(
        fs::read_to_string(&lock).expect("read lock after"),
        lock_before,
        "px.lock should remain unchanged after dry-run add"
    );
}

#[test]
fn project_update_restores_manifest_on_failure() {
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
    let (_temp, root) = common::init_empty_project("px-update-restore");
    let cache = root.join(".px-cache");
    px_cmd()
        .current_dir(&root)
        .env("PX_CACHE_PATH", &cache)
        .args(["add", "packaging==23.0"])
        .assert()
        .success();

    let lockfile = root.join("px.lock");
    let mut perms = fs::metadata(&lockfile)
        .expect("lock metadata")
        .permissions();
    perms.set_readonly(true);
    fs::set_permissions(&lockfile, perms).expect("set readonly lockfile");

    let assert = px_cmd()
        .current_dir(&root)
        .env("PX_CACHE_PATH", &cache)
        .args(["--json", "update", "packaging"])
        .assert()
        .failure();

    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "error");
    let message = payload["message"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    assert!(
        message.contains("px update"),
        "expected px update failure message, got {message:?}"
    );

    let deps = read_dependencies(root.join("pyproject.toml"));
    assert_eq!(
        deps,
        vec!["packaging==23.0"],
        "pyproject should be restored when update fails"
    );
    let lock_contents = fs::read_to_string(&lockfile).expect("read lockfile");
    assert!(
        lock_contents.contains("packaging==23.0"),
        "lockfile should remain untouched on failure"
    );
}

#[test]
fn sync_dry_run_reports_resolution_failures() {
    let _guard = test_env_guard();
    let temp = tempfile::tempdir().expect("tempdir");
    let project_dir = temp.path();
    fs::write(
        project_dir.join("pyproject.toml"),
        r#"[project]
name = "demo"
version = "0.1.0"
requires-python = ">=3.11"
dependencies = ["not a spec"]

[tool]
[tool.px]

[build-system]
requires = ["setuptools>=70", "wheel"]
build-backend = "setuptools.build_meta"
"#,
    )
    .expect("write pyproject");

    let assert = px_cmd()
        .current_dir(project_dir)
        .args(["--json", "sync", "--dry-run"])
        .assert()
        .failure();
    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    assert_eq!(
        payload["message"],
        "px sync: dependency resolution failed (dry-run)"
    );
    assert_eq!(payload["details"]["reason"], "resolve_failed");
}

#[test]
fn project_remove_deletes_dependency() {
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    scaffold_demo(&temp, "demo_remove");
    let project_dir = temp.path();

    px_cmd()
        .current_dir(project_dir)
        .args(["add", "requests==2.32.3"])
        .assert()
        .success();

    px_cmd()
        .current_dir(project_dir)
        .args(["remove", "requests"])
        .assert()
        .success();

    let deps = read_dependencies(project_dir.join("pyproject.toml"));
    assert!(deps.iter().all(|dep| !dep.starts_with("requests")));
}

#[test]
fn project_remove_requires_direct_dependency() {
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    scaffold_demo(&temp, "demo_remove_missing");
    let project_dir = temp.path();

    let assert = px_cmd()
        .current_dir(project_dir)
        .args(["remove", "missing-pkg"])
        .assert()
        .failure();

    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("PX111") && stderr.contains("package is not a direct dependency"),
        "expected a clear error when removing unknown packages, got {stderr:?}"
    );
    assert!(
        stderr.contains("px why"),
        "expected hint to mention px why, got {stderr:?}"
    );
}

#[test]
fn px_commands_require_project_root() {
    let _guard = test_env_guard();
    let temp = tempfile::tempdir().expect("tempdir");
    let assert = px_cmd()
        .current_dir(temp.path())
        .args(["add", "requests==2.32.3"])
        .assert()
        .failure();

    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("No px project found. Run `px init` in your project directory first."),
        "expected root error, got output: {stderr:?}"
    );
}

#[test]
fn missing_project_hint_recommends_migrate_when_pyproject_exists() {
    let _guard = test_env_guard();
    let temp = tempfile::tempdir().expect("tempdir");
    let pyproject = temp.path().join("pyproject.toml");
    fs::write(
        &pyproject,
        "[project]\nname = \"demo\"\nversion = \"0.0.1\"\n",
    )
    .expect("write pyproject");

    let json_assert = px_cmd()
        .current_dir(temp.path())
        .args(["--json", "status"])
        .assert()
        .failure();

    let payload = parse_json(&json_assert);
    let message = payload["message"].as_str().unwrap_or_default().to_string();
    let hint = payload["details"]["hint"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        message.contains("px migrate"),
        "expected migrate suggestion in message when pyproject exists without px metadata: {message:?}"
    );
    assert!(
        hint.contains("px migrate"),
        "expected migrate hint when pyproject exists without px metadata: {hint:?}"
    );

    let human = px_cmd()
        .current_dir(temp.path())
        .arg("status")
        .assert()
        .failure();
    let stderr = String::from_utf8(human.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("px migrate"),
        "human output should recommend px migrate when pyproject exists without px metadata: {stderr:?}"
    );
}

#[test]
fn all_project_commands_surface_missing_project_errors() {
    let _guard = test_env_guard();
    let temp = tempfile::tempdir().expect("tempdir");
    let message = "No px project found. Run `px init` in your project directory first.";

    let human_commands = vec![
        vec!["status"],
        vec!["add", "requests==2.32.3"],
        vec!["remove", "requests"],
        vec!["sync"],
        vec!["update"],
        vec!["run"],
        vec!["test"],
        vec!["fmt"],
        vec!["why", "requests"],
        vec!["build"],
        vec!["publish"],
    ];

    for args in &human_commands {
        let assert = px_cmd()
            .current_dir(temp.path())
            .args(args)
            .assert()
            .failure();
        let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
        assert!(
            stderr.contains(message),
            "expected missing-project message for {args:?}, got {stderr:?}"
        );
    }

    let json_commands = vec![
        vec!["--json", "status"],
        vec!["--json", "add", "requests==2.32.3"],
        vec!["--json", "remove", "requests"],
        vec!["--json", "sync"],
        vec!["--json", "update"],
        vec!["--json", "run"],
        vec!["--json", "test"],
        vec!["--json", "fmt"],
        vec!["--json", "why", "requests"],
        vec!["--json", "build"],
        vec!["--json", "publish"],
    ];

    for args in &json_commands {
        let assert = px_cmd()
            .current_dir(temp.path())
            .args(args)
            .assert()
            .failure();
        let payload = parse_json(&assert);
        let reason = payload["details"]["reason"].as_str().unwrap_or_default();
        let hint = payload["details"]["hint"].as_str().unwrap_or_default();
        assert_eq!(
            "missing_project", reason,
            "expected missing_project reason for {args:?}, got {payload}"
        );
        assert!(
            hint.contains("px init"),
            "expected hint to direct user to px init for {args:?}, got {hint}"
        );
    }
}

#[test]
fn px_commands_walk_up_to_project_root() {
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let project_dir = temp.path().join("root-app");
    fs::create_dir_all(project_dir.join("nested").join("deep")).expect("create dirs");

    px_cmd()
        .current_dir(&project_dir)
        .args(["init", "--package", "root_app"])
        .assert()
        .success();

    let nested = project_dir.join("nested").join("deep");
    px_cmd()
        .current_dir(&nested)
        .args(["add", "requests==2.32.3"])
        .assert()
        .success();

    let deps = read_dependencies(project_dir.join("pyproject.toml"));
    assert!(
        deps.iter().any(|dep| dep == "requests==2.32.3"),
        "dependency should be added from nested directory: {deps:?}"
    );
}

#[test]
fn sync_frozen_missing_lock_hint_is_clear() {
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let project_dir = temp.path();
    px_cmd()
        .current_dir(project_dir)
        .args(["init", "--package", "sync_hint_demo"])
        .assert()
        .success();
    fs::remove_file(project_dir.join("px.lock")).expect("remove px.lock");

    let Some(python) = find_python() else {
        eprintln!("skipping sync hint test (python binary not found)");
        return;
    };
    let assert = px_cmd()
        .current_dir(project_dir)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["--json", "sync", "--frozen"])
        .assert()
        .failure();
    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    let hint = payload["details"]["hint"].as_str().unwrap_or_default();
    assert!(
        hint.contains("generate px.lock"),
        "hint should direct users to create px.lock: {hint:?}"
    );
    assert!(
        !hint.contains("before `px sync`"),
        "hint should avoid circular wording: {hint:?}"
    );
}

#[test]
fn project_status_reports_missing_lock() {
    let _guard = test_env_guard();
    let (_tmp, project) = prepare_fixture("status-missing-lock");
    let lock = project.join("px.lock");
    fs::remove_file(&lock).expect("remove px.lock");

    let Some(python) = find_python() else {
        eprintln!("skipping status test (python binary not found)");
        return;
    };
    let assert = px_cmd()
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["--json", "status"])
        .assert()
        .failure();
    let payload = parse_json(&assert);
    assert_eq!(payload["project"]["state"], "NeedsLock");
    assert_eq!(payload["lock"]["status"], "missing");
    assert_eq!(payload["next_action"]["kind"], "sync");
}

#[test]
fn project_status_missing_lock_guides_generating_lock() {
    let _guard = test_env_guard();
    let (_tmp, project) = prepare_fixture("status-missing-lock-guidance");
    let lock = project.join("px.lock");
    fs::remove_file(&lock).expect("remove px.lock");

    let Some(python) = find_python() else {
        eprintln!("skipping status guidance test (python binary not found)");
        return;
    };
    let assert = px_cmd()
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .arg("status")
        .assert()
        .failure();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("generate px.lock"),
        "status output should recommend generating px.lock when missing: {stdout:?}"
    );
    assert!(
        !stdout.contains("update px.lock"),
        "status output should not talk about updating a missing lock: {stdout:?}"
    );
}

#[test]
fn project_update_missing_lock_guides_generating_lock() {
    let _guard = test_env_guard();
    let (_tmp, project) = prepare_fixture("update-missing-lock-guidance");
    let lock = project.join("px.lock");
    fs::remove_file(&lock).expect("remove px.lock");

    let Some(python) = find_python() else {
        eprintln!("skipping update guidance test (python binary not found)");
        return;
    };
    let assert = px_cmd()
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["--json", "update"])
        .assert()
        .failure();
    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "user-error");
    assert_eq!(payload["details"]["reason"], "missing_lock");
    let hint = payload["details"]["hint"].as_str().unwrap_or_default();
    assert!(
        hint.contains("generate px.lock"),
        "update hint should direct users to generate px.lock: {hint:?}"
    );
    assert!(
        !hint.contains("update px.lock"),
        "update hint should not talk about updating a missing lock: {hint:?}"
    );
}

#[test]
fn sync_mentions_environment_refresh_when_lock_is_unchanged() {
    let _guard = test_env_guard();
    let (_tmp, project) = prepare_fixture("sync-env-refresh-output");
    let Some(python) = find_python() else {
        eprintln!("skipping sync output test (python binary not found)");
        return;
    };

    px_cmd()
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .env_remove("PX_NO_ENSUREPIP")
        .args(["--json", "sync"])
        .assert()
        .success();

    let state_path = project.join(".px").join("state.json");
    let state: Value = serde_json::from_str(&fs::read_to_string(&state_path).expect("read state"))
        .expect("parse state");
    let site = state["current_env"]["site_packages"]
        .as_str()
        .expect("site path");
    let site_path = Path::new(site);
    if site_path.exists() {
        if let Some(env_root) = site_path.ancestors().nth(3) {
            fs::remove_dir_all(env_root).ok();
        }
    }

    let assert = px_cmd()
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .env_remove("PX_NO_ENSUREPIP")
        .args(["--json", "sync"])
        .assert()
        .success();
    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    assert_eq!(
        payload["details"]["env_refreshed"],
        Value::Bool(true),
        "expected sync to report an env refresh when env was missing"
    );
    let message = payload["message"].as_str().unwrap_or_default();
    assert!(
        message
            .to_ascii_lowercase()
            .contains("environment refreshed"),
        "expected sync output to mention environment refresh, got {message:?}"
    );
}

#[test]
fn project_status_detects_manifest_drift() {
    let _guard = test_env_guard();
    let (_tmp, project) = prepare_fixture("status-drift");
    let pyproject = project.join("pyproject.toml");
    let mut doc: DocumentMut = fs::read_to_string(&pyproject)
        .expect("read pyproject")
        .parse()
        .expect("parse pyproject");
    if let Some(array) = doc["project"]["dependencies"].as_array_mut() {
        array.push("requests==2.32.3");
    }
    fs::write(&pyproject, doc.to_string()).expect("write pyproject");

    let Some(python) = find_python() else {
        eprintln!("skipping status test (python binary not found)");
        return;
    };
    let assert = px_cmd()
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["--json", "status"])
        .assert()
        .failure();
    let payload = parse_json(&assert);
    assert_eq!(payload["project"]["state"], "NeedsLock");
    assert_eq!(payload["next_action"]["kind"], "sync");
    let env_status = payload["env"]["status"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        env_status == "stale" || env_status == "missing",
        "env status should reflect drift, got {env_status}"
    );
}

#[test]
fn project_status_ignores_dependencies_filtered_by_markers() {
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
    let (_tmp, project) = common::init_empty_project("px-status-markers");
    let cache = project.join(".px-cache");
    let store = cache.join("store");
    let envs = cache.join("envs");
    fs::create_dir_all(&envs).expect("create envs dir");
    let pyproject = project.join("pyproject.toml");
    let mut doc: DocumentMut = fs::read_to_string(&pyproject)
        .expect("read pyproject")
        .parse()
        .expect("parse pyproject");
    let deps = doc["project"]["dependencies"]
        .as_array_mut()
        .expect("dependencies array");
    deps.push("tomli>=2.0.1,<3.0.0 ; python_version < '3.11'");
    fs::write(&pyproject, doc.to_string()).expect("write pyproject");

    let Some(python) = find_python() else {
        eprintln!("skipping status marker test (python binary not found)");
        return;
    };

    // Clean once, then reuse the same test cache across both commands so the
    // environment persists between sync and status.
    let mut sync_cmd = cargo_bin_cmd!("px");
    sync_cmd
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .env("PX_CACHE_PATH", &cache)
        .env("PX_STORE_PATH", &store)
        .env("PX_ENVS_PATH", &envs)
        .env("PX_RUNTIME_HOST_ONLY", "1")
        .args(["sync"])
        .assert()
        .success();

    let assert = cargo_bin_cmd!("px")
        .current_dir(&project)
        .env("PX_RUNTIME_PYTHON", &python)
        .env("PX_CACHE_PATH", &cache)
        .env("PX_STORE_PATH", &store)
        .env("PX_ENVS_PATH", &envs)
        .env("PX_RUNTIME_HOST_ONLY", "1")
        .args(["--json", "status"])
        .assert()
        .success();
    let payload = parse_json(&assert);
    assert_eq!(payload["project"]["state"], "Consistent");
    assert_eq!(payload["env"]["status"], "clean");
}

#[test]
fn run_and_test_report_missing_manifest_command() {
    let _guard = test_env_guard();
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    fs::write(root.join("px.lock"), "").expect("touch lock");

    let run = px_cmd()
        .current_dir(root)
        .args(["--json", "run", "demo"])
        .assert()
        .failure();
    let payload = parse_json(&run);
    assert_eq!(payload["details"]["command"], json!("run"));

    let test = px_cmd()
        .current_dir(root)
        .args(["--json", "test"])
        .assert()
        .failure();
    let payload = parse_json(&test);
    assert_eq!(payload["details"]["command"], json!("test"));
}

#[test]
fn project_test_auto_installs_pytest_and_preserves_manifest() {
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
    let (_temp, root) = common::init_empty_project("px-auto-pytest");
    let cache = root.join(".px-cache");
    let store = cache.join("store");
    let envs = cache.join("envs");
    fs::create_dir_all(&envs).expect("create envs dir");

    let tools_dir = tempfile::tempdir().expect("tools dir");
    let tool_store = tempfile::tempdir().expect("tool store dir");

    let Some(python) = find_python() else {
        eprintln!("skipping pytest auto-install test (python binary not found)");
        return;
    };

    let has_pytest = std::process::Command::new(&python)
        .args([
            "-c",
            "import importlib.util,sys; sys.exit(0 if importlib.util.find_spec('pytest') else 1)",
        ])
        .status()
        .ok()
        .is_some_and(|status| status.success());
    if has_pytest {
        eprintln!("skipping pytest auto-install test (pytest already available on runtime)");
        return;
    }

    let tests = root.join("tests");
    fs::create_dir_all(&tests).expect("create tests dir");
    fs::write(
        tests.join("test_smoke.py"),
        "def test_smoke():\n    assert True\n",
    )
    .expect("write test");

    let pyproject = root.join("pyproject.toml");
    let lock = root.join("px.lock");
    let manifest_before = fs::read_to_string(&pyproject).expect("read pyproject");
    let lock_before = fs::read_to_string(&lock).expect("read lock");

    let assert = px_cmd()
        .current_dir(&root)
        .env("PX_RUNTIME_PYTHON", &python)
        .env("PX_CACHE_PATH", &cache)
        .env("PX_STORE_PATH", &store)
        .env("PX_ENVS_PATH", &envs)
        .env("PX_TOOLS_DIR", tools_dir.path())
        .env("PX_TOOL_STORE", tool_store.path())
        .env("PX_RUNTIME_HOST_ONLY", "1")
        .env("PYTHONNOUSERSITE", "1")
        .env_remove("PX_NO_ENSUREPIP")
        .args(["--json", "test"])
        .assert()
        .success();
    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");

    assert_eq!(
        manifest_before,
        fs::read_to_string(&pyproject).expect("read pyproject after test"),
        "px test should not modify pyproject.toml when auto-installing pytest"
    );
    assert_eq!(
        lock_before,
        fs::read_to_string(&lock).expect("read lock after test"),
        "px test should not modify px.lock when auto-installing pytest"
    );
}

#[test]
fn project_test_prefers_runtests_script_when_present() {
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
    let (_temp, root) = common::prepare_fixture("px-runtests-runner");

    let Some(python) = find_python() else {
        eprintln!("skipping runtests fallback test (python binary not found)");
        return;
    };

    let tests = root.join("tests");
    fs::create_dir_all(&tests).expect("create tests dir");
    fs::write(
        tests.join("runtests.py"),
        "import sys\n\n\ndef main():\n    print('runtests-runner')\n    return 0\n\nif __name__ == '__main__':\n    sys.exit(main())\n",
    )
    .expect("write runtests script");

    let assert = px_cmd()
        .current_dir(&root)
        .env("PX_RUNTIME_PYTHON", &python)
        .args(["--json", "test"])
        .assert()
        .success();
    let payload = parse_json(&assert);
    assert_eq!(payload["status"], "ok");
    let runner = payload["details"]["runner"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        runner.contains("tests/runtests.py"),
        "runner should point to tests/runtests.py: {runner:?}"
    );
    let stdout = payload["details"]["stdout"].as_str().unwrap_or_default();
    assert!(
        stdout.contains("runtests-runner"),
        "stdout should include runtests script output: {stdout:?}"
    );
}

#[test]
fn corrupt_state_file_is_reported() {
    let _guard = test_env_guard();
    if !require_online() {
        return;
    }
    let (_temp, root) = common::init_empty_project("px-corrupt-state");
    let state = root.join(".px").join("state.json");
    fs::create_dir_all(state.parent().unwrap()).expect("create .px");
    fs::write(&state, "{not-json").expect("corrupt state");

    let assert = px_cmd()
        .current_dir(root)
        .args(["--json", "status"])
        .assert()
        .failure();
    let payload = parse_json(&assert);
    assert!(payload["message"]
        .as_str()
        .unwrap_or_default()
        .contains("px state file is unreadable"));
    assert!(
        payload["details"]["hint"]
            .as_str()
            .unwrap_or_default()
            .contains(".px/state.json"),
        "expected hint to point to state file: {payload:?}"
    );
}

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

fn scaffold_demo(temp: &TempDir, package: &str) {
    px_cmd()
        .current_dir(temp.path())
        .args(["init", "--package", package])
        .assert()
        .success();
}

fn read_dependencies(path: impl AsRef<Path>) -> Vec<String> {
    let contents = fs::read_to_string(path).expect("pyproject readable");
    let doc: DocumentMut = contents.parse().expect("valid toml");
    doc["project"]["dependencies"]
        .as_array()
        .map(|array| {
            array
                .iter()
                .filter_map(|val| val.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn read_project_name(path: impl AsRef<Path>) -> String {
    let contents = fs::read_to_string(path).expect("pyproject readable");
    let doc: DocumentMut = contents.parse().expect("valid toml");
    doc["project"]["name"].as_str().unwrap().to_string()
}
