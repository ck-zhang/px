use std::{cell::RefCell, env, fs, path::PathBuf, process::Command};

use assert_cmd::cargo::cargo_bin_cmd;
use serde_json::Value;
use tempfile::TempDir;
use toml_edit::DocumentMut;

mod common;

thread_local! {
    static SHARED_CACHE: RefCell<Option<TempDir>> = const { RefCell::new(None) };
}

fn shared_cache_base() -> PathBuf {
    SHARED_CACHE.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(
                tempfile::Builder::new()
                    .prefix("px-migrate-cache")
                    .tempdir()
                    .expect("tempdir"),
            );
        }
        slot.as_ref().expect("cache dir").path().to_path_buf()
    })
}

fn require_online() -> bool {
    if let Some("1") = env::var("PX_ONLINE").ok().as_deref() {
        true
    } else {
        eprintln!("skipping migrate autopin test (PX_ONLINE!=1)");
        false
    }
}

fn px_command(temp: &TempDir) -> assert_cmd::Command {
    let base = shared_cache_base();
    let cache_path = base.join("cache");
    let store_path = base.join("store");
    let python = common::find_python().unwrap_or_else(|| "python3".to_string());
    let mut cmd = cargo_bin_cmd!("px");
    cmd.current_dir(temp.path())
        .env("PX_ONLINE", "1")
        .env("PX_CACHE_PATH", &cache_path)
        .env("PX_STORE_PATH", &store_path)
        .env("PX_RUNTIME_HOST_ONLY", "1")
        .env("PX_RUNTIME_PYTHON", python);
    cmd
}

fn px_command_offline(temp: &TempDir) -> assert_cmd::Command {
    let base = shared_cache_base();
    let cache_path = base.join("cache");
    let store_path = base.join("store");
    let python = common::find_python().unwrap_or_else(|| "python3".to_string());
    let mut cmd = cargo_bin_cmd!("px");
    cmd.current_dir(temp.path())
        .env("PX_ONLINE", "0")
        .env("PX_CACHE_PATH", &cache_path)
        .env("PX_STORE_PATH", &store_path)
        .env("PX_RUNTIME_HOST_ONLY", "1")
        .env("PX_RUNTIME_PYTHON", python);
    cmd
}

fn run_migrate_json(temp: &TempDir, extra: &[&str]) -> Value {
    let mut args = vec!["--json", "migrate"];
    args.extend_from_slice(extra);
    let assert = px_command(temp).args(&args).assert().success();
    serde_json::from_slice(&assert.get_output().stdout).expect("json output")
}

fn write_file(temp: &TempDir, name: &str, contents: &str) {
    fs::write(temp.path().join(name), contents).expect("write file");
}

fn autopinned(details: &Value) -> Vec<String> {
    details
        .get("autopinned")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|entry| entry.get("name").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn command_output(assert: &assert_cmd::assert::Assert) -> String {
    let out = assert.get_output();
    let mut buffer = String::new();
    if !out.stdout.is_empty() {
        buffer.push_str(&String::from_utf8_lossy(&out.stdout));
    }
    if !out.stderr.is_empty() {
        buffer.push_str(&String::from_utf8_lossy(&out.stderr));
    }
    buffer
}

#[test]
fn migrate_reports_pyproject_dependencies() {
    if !require_online() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    scaffold_demo(&temp, "demo_onboard");

    px_command(&temp)
        .args(["add", "requests==2.32.3"])
        .assert()
        .success();

    let assert = px_command(&temp)
        .args(["migrate"])
        .assert()
        .success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(stdout.contains("px migrate: plan ready"));
    assert!(stdout.contains("requests==2.32.3"));
}

#[test]
fn migrate_reads_requirements_with_json() {
    let temp = tempfile::tempdir().expect("tempdir");
    let requirements = temp.path().join("requirements.txt");
    fs::write(&requirements, "rich==13.7.1\n").expect("write requirements");

    let assert = cargo_bin_cmd!("px")
        .current_dir(temp.path())
        .args(["--json", "migrate"])
        .assert()
        .success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let payload: Value = serde_json::from_str(&stdout).expect("json output");
    assert_eq!(payload["status"], "ok");
    let packages = payload["details"]["packages"].as_array().unwrap();
    assert_eq!(packages.len(), 1);
    assert_eq!(packages[0]["name"], "rich");
    assert_eq!(packages[0]["source"], "requirements.txt");
    let actions = payload["details"]["actions"].as_object().unwrap();
    assert_eq!(actions["pyproject_updated"], Value::Bool(false));
    assert_eq!(actions["lock_written"], Value::Bool(false));
    assert!(actions["backups"].as_array().unwrap().is_empty());
}

#[test]
fn migrate_apply_works_without_tool_section() {
    if !require_online() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let pyproject = temp.path().join("pyproject.toml");
    fs::write(
        &pyproject,
        r#"[project]
name = "tool-less"
version = "0.1.0"
requires-python = ">=3.11"
dependencies = ["rich==13.7.1"]

[build-system]
requires = ["setuptools>=70", "wheel"]
build-backend = "setuptools.build_meta"
"#,
    )
    .expect("write pyproject");

    let assert = px_command(&temp)
        .args(["--json", "migrate", "--apply", "--allow-dirty"])
        .assert()
        .success();
    let payload: Value = serde_json::from_slice(&assert.get_output().stdout).expect("json payload");
    assert_eq!(payload["status"], "ok");
    assert!(
        temp.path().join("px.lock").exists(),
        "px.lock should be created"
    );
}

#[test]
fn migrate_apply_builds_environment() {
    if !require_online() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    write_file(&temp, "requirements.txt", "rich==13.7.1\n");

    let assert = px_command(&temp)
        .args([
            "--json",
            "migrate",
            "--apply",
            "--allow-dirty",
            "--source",
            "requirements.txt",
        ])
        .assert()
        .success();
    let payload: Value = serde_json::from_slice(&assert.get_output().stdout).expect("json payload");
    assert_eq!(payload["status"], "ok");

    let status = px_command(&temp)
        .args(["--json", "status"])
        .assert()
        .success();
    let status_json: Value =
        serde_json::from_slice(&status.get_output().stdout).expect("json status");
    assert_eq!(status_json["env"]["status"], "clean");
    let project_state = status_json["project"]["state"].as_str().unwrap_or_default();
    assert_eq!(
        project_state, "Consistent",
        "expected Consistent state, got {project_state}"
    );
}

#[test]
fn migrate_prefers_poetry_lock_pins() {
    if !require_online() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    write_file(
        &temp,
        "pyproject.toml",
        r#"[project]
name = "poetry-lock-demo"
version = "0.1.0"
requires-python = ">=3.11"
dependencies = [
    "cachecontrol[filecache]>=0.14,<0.15",
    "requests>=2.30,<3.0",
]

[build-system]
requires = ["setuptools>=70", "wheel"]
build-backend = "setuptools.build_meta"

[tool.poetry]
package-mode = false
"#,
    );
    write_file(
        &temp,
        "poetry.lock",
        r#"[[package]]
name = "cachecontrol"
version = "0.14.3"
category = "main"
optional = false
python-versions = ">=3.8"
files = []

[[package]]
name = "requests"
version = "2.31.0"
category = "main"
optional = false
python-versions = ">=3.8"
files = []

[metadata]
lock-version = "2.0"
python-versions = ">=3.11"
content-hash = "dev"

[metadata.files]
cachecontrol = []
requests = []
"#,
    );

    px_command(&temp)
        .args(["migrate", "--apply", "--allow-dirty"])
        .assert()
        .success();

    let lock_contents = fs::read_to_string(temp.path().join("px.lock")).expect("px.lock contents");
    assert!(
        lock_contents.contains("cachecontrol[filecache]==0.14.3"),
        "px.lock should prefer versions from poetry.lock for cachecontrol"
    );
    assert!(
        lock_contents.contains("requests==2.31.0"),
        "px.lock should prefer versions from poetry.lock for requests"
    );
}

#[test]
fn migrate_respects_uv_lock_without_pinning_manifest() {
    if !require_online() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    write_file(
        &temp,
        "pyproject.toml",
        r#"[project]
name = "uv-lock-demo"
version = "0.1.0"
requires-python = ">=3.11"
dependencies = ["click>=8.1"]

[build-system]
requires = ["setuptools>=70", "wheel"]
build-backend = "setuptools.build_meta"
"#,
    );
    write_file(
        &temp,
        "uv.lock",
        r#"version = 1
requires-python = ">=3.11"

[[package]]
name = "click"
version = "8.1.7"
source = { registry = "https://pypi.org/simple" }
"#,
    );

    px_command(&temp)
        .args(["migrate", "--apply", "--allow-dirty"])
        .assert()
        .success();

    let pyproject = fs::read_to_string(temp.path().join("pyproject.toml")).expect("pyproject");
    assert!(
        pyproject.contains("click>=8.1"),
        "uv.lock migration should not pin manifest dependencies"
    );
    let lock_contents = fs::read_to_string(temp.path().join("px.lock")).expect("px.lock");
    assert!(
        lock_contents.contains("click==8.1.7"),
        "px.lock should reuse versions from uv.lock"
    );
}

#[test]
fn migrate_errors_without_project_files() {
    let temp = tempfile::tempdir().expect("tempdir");
    cargo_bin_cmd!("px")
        .current_dir(temp.path())
        .args(["migrate"])
        .assert()
        .failure();
}

#[test]
fn migrate_write_creates_lock_from_requirements() {
    if !require_online() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let requirements = temp.path().join("requirements.txt");
    fs::write(&requirements, "rich==13.7.1\n").expect("write requirements");

    let assert = px_command(&temp)
        .args(["--json", "migrate", "--apply", "--source", "requirements.txt"])
        .assert()
        .success();

    let lock_path = temp.path().join("px.lock");
    assert!(lock_path.exists(), "px.lock should be written");
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let payload: Value = serde_json::from_str(&stdout).expect("json output");
    let actions = payload["details"]["actions"].as_object().unwrap();
    assert_eq!(actions["lock_written"], Value::Bool(true));
    assert_eq!(actions["pyproject_updated"], Value::Bool(true));
}

#[test]
fn migrate_write_backs_up_pyproject() {
    if !require_online() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    scaffold_demo(&temp, "demo_backup");
    let requirements = temp.path().join("requirements.txt");
    fs::write(&requirements, "click==8.1.7\n").expect("write requirements");

    let assert = px_command(&temp)
        .args(["--json", "migrate", "--apply", "--source", "requirements.txt"])
        .assert()
        .success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let payload: Value = serde_json::from_str(&stdout).expect("json output");
    let actions = payload["details"]["actions"].as_object().unwrap();
    assert_eq!(actions["pyproject_updated"], Value::Bool(true));
    let backups = actions["backups"].as_array().unwrap();
    assert!(!backups.is_empty(), "expected a pyproject backup entry");
    assert!(backups
        .iter()
        .any(|entry| entry.as_str().unwrap().ends_with("pyproject.toml")));
    let backup_dir = temp.path().join(".px").join("onboard-backups");
    assert!(backup_dir.exists(), "backup directory should exist");
}

#[test]
fn migrate_autopins_loose_requirements() {
    if !require_online() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    write_file(&temp, "requirements.txt", "packaging>=23.0\n");
    let output = run_migrate_json(&temp, &["--apply", "--source", "requirements.txt"]);
    assert!(
        !autopinned(&output["details"]).is_empty(),
        "expected autopinned entries"
    );
    let pyproject = fs::read_to_string(temp.path().join("pyproject.toml")).expect("pyproject");
    assert!(
        pyproject.contains("packaging=="),
        "pyproject missing pinned version"
    );
    assert!(temp.path().join("px.lock").exists(), "px.lock should exist");
}

#[test]
fn migrate_autopins_only_loose_specs() {
    if !require_online() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    write_file(&temp, "requirements.txt", "attrs==23.2.0\nrequests>=2.30\n");
    let output = run_migrate_json(&temp, &["--apply", "--source", "requirements.txt"]);
    let names = autopinned(&output["details"]);
    assert_eq!(
        names,
        vec!["requests"],
        "only loose spec should be autopinned"
    );
    let pyproject = fs::read_to_string(temp.path().join("pyproject.toml")).expect("pyproject");
    assert!(
        pyproject.contains("attrs==23.2.0"),
        "pinned spec should not change"
    );
    assert!(
        pyproject.contains("requests=="),
        "loose spec should be pinned"
    );
}

#[test]
fn migrate_sets_dependency_group_config() {
    if !require_online() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    write_file(&temp, "requirements.txt", "packaging==23.2\n");
    write_file(&temp, "requirements-dev.txt", "pytest==8.3.3\n");

    run_migrate_json(&temp, &["--apply"]);

    let pyproject = fs::read_to_string(temp.path().join("pyproject.toml")).expect("pyproject");
    let doc: DocumentMut = pyproject.parse().expect("pyproject toml");
    let include = doc["tool"]["px"]["dependencies"]["include-groups"]
        .as_array()
        .expect("include-groups array");
    let groups: Vec<_> = include.iter().filter_map(|item| item.as_str()).collect();
    assert!(
        groups.contains(&"px-dev"),
        "px migrate should activate dev dependency groups by default"
    );
}

#[test]
fn migrate_keeps_dev_requirements_out_of_prod() {
    if !require_online() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    write_file(&temp, "requirements-dev.txt", "pytest>=8.3\n");

    px_command(&temp)
        .args(["--json", "migrate", "--apply", "--allow-dirty"])
        .assert()
        .success();

    let pyproject = fs::read_to_string(temp.path().join("pyproject.toml")).expect("pyproject");
    let doc: DocumentMut = pyproject.parse().expect("pyproject toml");
    let deps = doc["project"]["dependencies"]
        .as_array()
        .expect("dependencies array");
    assert!(
        deps.is_empty(),
        "prod dependencies should remain empty when only dev requirements are present"
    );

    let dev_array = doc["project"]["optional-dependencies"]["px-dev"]
        .as_array()
        .expect("px-dev optional deps");
    assert!(
        dev_array
            .iter()
            .filter_map(|item| item.as_str())
            .any(|spec| spec.starts_with("pytest==")),
        "dev requirements should be pinned into px-dev"
    );

    let include = doc["tool"]["px"]["dependencies"]["include-groups"]
        .as_array()
        .expect("include-groups array");
    let groups: Vec<_> = include.iter().filter_map(|item| item.as_str()).collect();
    assert!(
        groups.contains(&"px-dev"),
        "px-dev group should be auto-included when dev requirements exist"
    );
}

#[test]
fn migrate_reads_setup_cfg_requires_dist() {
    if !require_online() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    write_file(
        &temp,
        "setup.cfg",
        r#"[metadata]
requires-dist =
  idna>=3.6
  urllib3>=2.0,<3
[options]
install_requires =
  certifi>=2024.0.0
"#,
    );

    px_command(&temp)
        .args(["--json", "migrate", "--apply", "--allow-dirty"])
        .assert()
        .success();

    let pyproject = fs::read_to_string(temp.path().join("pyproject.toml")).expect("pyproject");
    let doc: DocumentMut = pyproject.parse().expect("pyproject toml");
    let deps = doc["project"]["dependencies"]
        .as_array()
        .expect("dependencies array");
    let specs: Vec<_> = deps.iter().filter_map(|item| item.as_str()).collect();
    assert!(
        specs.iter().any(|spec| spec.starts_with("idna==")),
        "setup.cfg dependencies should be pinned into prod requirements"
    );
    assert!(
        specs.iter().any(|spec| spec.starts_with("urllib3==")),
        "urllib3 requirement should be captured from setup.cfg"
    );
    assert!(
        specs.iter().any(|spec| spec.starts_with("certifi==")),
        "install_requires entries should be imported from setup.cfg"
    );
}

#[test]
fn migrate_no_autopin_flag_errors() {
    if !require_online() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    write_file(&temp, "requirements.txt", "rich>=13.0\n");
    let assert = px_command(&temp)
        .args([
            "migrate",
            "--apply",
            "--source",
            "requirements.txt",
            "--no-autopin",
        ])
        .assert()
        .failure();
    let output = command_output(&assert);
    assert!(
        output.contains("automatic pinning disabled"),
        "missing autopin error: {output}"
    );
}

#[test]
fn migrate_autopin_reports_resolver_failure() {
    if !require_online() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    write_file(&temp, "requirements.txt", "definitely-not-a-real-pkg>=1\n");
    let assert = px_command(&temp)
        .args(["migrate", "--apply", "--source", "requirements.txt"])
        .assert()
        .failure();
    let output = command_output(&assert);
    assert!(
        output.contains("definitely-not-a-real-pkg"),
        "missing resolver error"
    );
}

#[test]
fn migrate_autopins_dev_requirements() {
    if !require_online() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    write_file(&temp, "requirements.txt", "packaging==23.2\n");
    write_file(&temp, "requirements-dev.txt", "pytest>=7.0\n");
    let output = run_migrate_json(&temp, &["--apply"]);
    let names = autopinned(&output["details"]);
    assert!(
        names.contains(&"pytest".to_string()),
        "dev dependency should be pinned"
    );
    let pycontents = fs::read_to_string(temp.path().join("pyproject.toml")).expect("pyproject");
    let doc: DocumentMut = pycontents.parse().expect("pyproject toml");
    let dev_array = doc["project"]["optional-dependencies"]["px-dev"]
        .as_array()
        .expect("px-dev optional dependency group should exist");
    let pinned_dev = dev_array
        .iter()
        .filter_map(|item| item.as_str())
        .any(|spec| spec.contains("pytest=="));
    assert!(pinned_dev, "dev dependency should be pinned");
}

#[test]
fn migrate_skips_non_matching_marker_dependencies() {
    if !require_online() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    write_file(
        &temp,
        "pyproject.toml",
        r#"
[project]
name = "marker-demo"
version = "0.1.0"
requires-python = ">=3.9"
dependencies = [
  "click>=8.0.0",
  "tomli>=1.1.0; python_version < '3.11'"
]

[project.optional-dependencies]
px-dev = ["pytest>=7.0"]
"#,
    );
    let output = run_migrate_json(&temp, &["--apply"]);
    let names = autopinned(&output["details"]);
    assert!(names.iter().any(|name| name == "click" || name == "pytest"));
    assert!(
        !names.iter().any(|name| name == "tomli"),
        "tomli should be skipped when python_version >= 3.11"
    );
}

#[test]
fn migrate_apply_rolls_back_on_failure() {
    if !require_online() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let pyproject_path = temp.path().join("pyproject.toml");
    write_file(
        &temp,
        "pyproject.toml",
        r#"[project]
name = "rollback-demo"
version = "0.1.0"
requires-python = ">=3.11"
dependencies = ["click==8.1.7"]

[build-system]
requires = ["setuptools>=70", "wheel"]
build-backend = "setuptools.build_meta"
"#,
    );
    let original = fs::read_to_string(&pyproject_path).expect("pyproject");
    write_file(&temp, "requirements.txt", "definitely-not-a-real-pkg>=1\n");

    let assert = px_command(&temp)
        .args([
            "migrate",
            "--apply",
            "--source",
            "requirements.txt",
            "--allow-dirty",
        ])
        .assert()
        .failure();
    let output = command_output(&assert);
    assert!(
        output.contains("definitely-not-a-real-pkg"),
        "expected resolver failure to bubble up ({output})"
    );

    let current = fs::read_to_string(&pyproject_path).expect("pyproject restored");
    assert_eq!(current, original, "pyproject should be restored on failure");
    assert!(
        !temp.path().join("px.lock").exists(),
        "px.lock should not be written when migration fails"
    );
}

#[test]
fn migrate_respects_source_overrides() {
    let temp = tempfile::tempdir().expect("tempdir");
    write_file(&temp, "requirements.txt", "rich==13.7.1\n");
    write_file(&temp, "alt-reqs.txt", "httpx==0.27.0\n");
    write_file(&temp, "requirements-dev.txt", "pytest==8.1.0\n");
    write_file(&temp, "dev-alt.txt", "coverage==7.4.4\n");

    let output = run_migrate_json(
        &temp,
        &["--source", "alt-reqs.txt", "--dev-source", "dev-alt.txt"],
    );
    let packages = output["details"]["packages"]
        .as_array()
        .expect("packages array");

    let has_httpx = packages.iter().any(|pkg| {
        pkg["name"] == "httpx" && pkg["source"] == "alt-reqs.txt" && pkg["scope"] == "prod"
    });
    let has_coverage = packages.iter().any(|pkg| {
        pkg["name"] == "coverage" && pkg["source"] == "dev-alt.txt" && pkg["scope"] == "dev"
    });
    let has_rich = packages.iter().any(|pkg| pkg["name"] == "rich");
    let has_pytest = packages.iter().any(|pkg| pkg["name"] == "pytest");

    assert!(has_httpx, "override prod source should be used");
    assert!(has_coverage, "override dev source should be used");
    assert!(
        !has_rich,
        "default prod requirements should be ignored when override is set"
    );
    assert!(
        !has_pytest,
        "default dev requirements should be ignored when dev override is set"
    );
}

#[test]
fn migrate_blocks_dirty_worktree_without_flag() {
    let _guard = common::test_env_guard();
    if !require_online() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    write_file(
        &temp,
        "pyproject.toml",
        r#"[project]
name = "dirty-demo"
version = "0.1.0"
requires-python = ">=3.11"
dependencies = []

[build-system]
requires = ["setuptools>=70", "wheel"]
build-backend = "setuptools.build_meta"
"#,
    );
    Command::new("git")
        .arg("init")
        .current_dir(temp.path())
        .output()
        .expect("git init");

    let assert = px_command(&temp)
        .args(["migrate", "--apply"])
        .assert()
        .failure();
    let output = command_output(&assert);
    assert!(
        output.contains("worktree dirty") || output.contains("allow-dirty"),
        "expected dirty worktree failure: {output}"
    );

    let success = px_command(&temp)
        .args(["--json", "migrate", "--apply", "--allow-dirty"])
        .assert()
        .success();
    let payload: Value = serde_json::from_slice(&success.get_output().stdout).expect("json");
    assert_eq!(payload["status"], "ok");
}

#[test]
fn migrate_lock_only_requires_pyproject() {
    let temp = tempfile::tempdir().expect("tempdir");
    let assert = px_command(&temp)
        .args(["migrate", "--apply", "--lock-only"])
        .assert()
        .failure();
    let output = command_output(&assert);
    assert!(
        output.contains("pyproject.toml required"),
        "expected lock-only to require pyproject: {output}"
    );
}

#[test]
fn migrate_autopins_dev_specs_without_clobbering_existing() {
    if !require_online() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    write_file(
        &temp,
        "pyproject.toml",
        r#"[project]
name = "dev-autopin"
version = "0.1.0"
requires-python = ">=3.11"
dependencies = ["click==8.1.7"]

[project.optional-dependencies]
px-dev = ["pytest==7.4.4"]
"#,
    );
    write_file(&temp, "requirements-dev.txt", "coverage>=7.4\n");

    let output = run_migrate_json(&temp, &["--apply"]);
    let names = autopinned(&output["details"]);
    assert!(
        names.contains(&"coverage".to_string()),
        "coverage should be autopinned from requirements-dev"
    );

    let pycontents = fs::read_to_string(temp.path().join("pyproject.toml")).expect("pyproject");
    let doc: DocumentMut = pycontents.parse().expect("pyproject toml");
    let dev_array = doc["project"]["optional-dependencies"]["px-dev"]
        .as_array()
        .expect("px-dev optional dependency group should exist");
    let dev_specs: Vec<_> = dev_array.iter().filter_map(|item| item.as_str()).collect();
    assert!(
        dev_specs.iter().any(|spec| spec.starts_with("coverage==")),
        "coverage should be pinned in px-dev"
    );
    assert!(
        dev_specs.iter().any(|spec| spec == &"pytest==7.4.4"),
        "existing px-dev entries should remain intact"
    );
    assert!(
        temp.path().join("px.lock").exists(),
        "px.lock should be created after autopin apply"
    );
}

#[test]
fn migrate_preview_respects_offline_mode() {
    let temp = tempfile::tempdir().expect("tempdir");
    write_file(&temp, "requirements.txt", "rich==13.7.1\n");

    let assert = px_command_offline(&temp)
        .args(["--json", "migrate"])
        .assert()
        .success();
    let payload: Value = serde_json::from_slice(&assert.get_output().stdout).expect("json");
    assert_eq!(payload["status"], "ok");
    let actions = payload["details"]["actions"].as_object().expect("actions");
    assert_eq!(actions["pyproject_updated"], Value::Bool(false));
    assert_eq!(actions["lock_written"], Value::Bool(false));
}

#[test]
fn migrate_apply_fails_fast_offline() {
    let temp = tempfile::tempdir().expect("tempdir");
    write_file(&temp, "requirements.txt", "rich==13.7.1\n");

    let assert = px_command_offline(&temp)
        .args(["migrate", "--apply", "--allow-dirty"])
        .assert()
        .failure();
    let output = command_output(&assert);
    assert!(
        output.contains("PX_ONLINE=1 required"),
        "offline apply should fail fast with hint: {output}"
    );
}

#[test]
fn migrate_conflict_reports_precedence() {
    let temp = tempfile::tempdir().expect("tempdir");
    write_file(
        &temp,
        "pyproject.toml",
        r#"[project]
name = "conflict-demo"
version = "0.1.0"
requires-python = ">=3.11"
dependencies = ["click==8.1.7"]

[build-system]
requires = ["setuptools>=70", "wheel"]
build-backend = "setuptools.build_meta"
"#,
    );
    write_file(&temp, "requirements.txt", "click==7.1.0\n");

    let assert = px_command(&temp)
        .args(["--json", "migrate", "--apply", "--allow-dirty"])
        .assert()
        .failure();
    let output = command_output(&assert);
    let payload: Value = serde_json::from_str(output.trim()).expect("json error payload");
    let message = payload["message"].as_str().unwrap_or("");
    let hint = payload["details"]["hint"].as_str().unwrap_or("");
    assert!(
        message.contains("conflicting dependency sources"),
        "expected conflict message: {message}"
    );
    assert!(
        hint.contains("pyproject") || hint.contains("pyproject.toml"),
        "conflict hint should mention precedence: {hint}"
    );
    assert!(
        hint.contains("--source") || hint.contains("--dev-source"),
        "conflict hint should mention explicit source flags: {hint}"
    );
}

#[test]
fn migrate_allows_marker_split_specs() {
    let temp = tempfile::tempdir().expect("tempdir");
    write_file(
        &temp,
        "requirements.txt",
        "setuptools==65.5.1 ; python_version < '3.12'\nsetuptools ; python_version >= '3.12'\n",
    );

    let assert = px_command(&temp)
        .args([
            "--json",
            "migrate",
            "--source",
            "requirements.txt",
            "--no-input",
        ])
        .assert()
        .success();
    let payload: Value = serde_json::from_slice(&assert.get_output().stdout).expect("json");
    assert_eq!(payload["status"], "ok");
    let packages = payload["details"]["packages"]
        .as_array()
        .expect("packages array");
    assert_eq!(
        packages.len(),
        2,
        "marker-split dependencies should be preserved during migration"
    );
}

#[test]
fn migrate_crash_restores_backup() {
    let temp = tempfile::tempdir().expect("tempdir");
    write_file(&temp, "requirements.txt", "rich==13.7.1\n");

    let assert = px_command(&temp)
        .env("PX_TEST_MIGRATE_CRASH", "1")
        .args([
            "migrate",
            "--apply",
            "--source",
            "requirements.txt",
            "--allow-dirty",
        ])
        .assert()
        .failure();
    let output = command_output(&assert);
    assert!(output.contains("test crash hook"));

    let pyproject = temp.path().join("pyproject.toml");
    assert!(
        !pyproject.exists(),
        "pyproject should be removed when migration creates it and then crashes"
    );
    assert!(
        !temp.path().join("px.lock").exists(),
        "px.lock should not exist after crash"
    );
}

#[test]
fn migrate_preserves_foreign_tool_sections() {
    if !require_online() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    write_file(
        &temp,
        "pyproject.toml",
        r#"[project]
name = "foreign-tool"
version = "0.1.0"
requires-python = ">=3.11"
dependencies = []

[tool.poetry]
package-mode = false

[build-system]
requires = ["setuptools>=70", "wheel"]
build-backend = "setuptools.build_meta"
"#,
    );
    write_file(&temp, "requirements.txt", "rich==13.7.1\n");

    px_command(&temp)
        .args([
            "migrate",
            "--apply",
            "--allow-dirty",
            "--source",
            "requirements.txt",
        ])
        .assert()
        .success();

    let contents = fs::read_to_string(temp.path().join("pyproject.toml")).expect("pyproject");
    assert!(
        contents.contains("[tool.poetry]"),
        "foreign tool section should be preserved"
    );
    assert!(
        contents.contains("rich==13.7.1"),
        "requirements should be merged"
    );
}

#[test]
fn migrate_preview_reports_foreign_tools() {
    let temp = tempfile::tempdir().expect("tempdir");
    write_file(
        &temp,
        "pyproject.toml",
        r#"[project]
name = "foreign-preview"
version = "0.1.0"
requires-python = ">=3.11"
dependencies = []

[tool.poetry]
package-mode = false

[build-system]
requires = ["setuptools>=70", "wheel"]
build-backend = "setuptools.build_meta"
"#,
    );

    write_file(&temp, "requirements.txt", "rich==13.7.1\n");

    let assert = px_command(&temp)
        .args(["--json", "migrate"])
        .assert()
        .success();
    let payload: Value = serde_json::from_slice(&assert.get_output().stdout).expect("json");
    let tools = payload["details"]["foreign_tools"]
        .as_array()
        .expect("foreign_tools");
    assert!(
        tools.iter().any(|t| t == "poetry"),
        "foreign tool list should include poetry"
    );
    let hint = payload["details"]["hint"].as_str().unwrap_or("");
    assert!(
        hint.contains("foreign tool"),
        "hint should mention foreign tools: {hint}"
    );
}

#[test]
fn migrate_rejects_poetry_owned_dependencies() {
    let temp = tempfile::tempdir().expect("tempdir");
    write_file(
        &temp,
        "pyproject.toml",
        r#"[project]
name = "poetry-owned"
version = "0.1.0"
requires-python = ">=3.11"
dependencies = []

[tool.poetry]
package-mode = false

[tool.poetry.dependencies]
requests = "^2.32"

[build-system]
requires = ["setuptools>=70", "wheel"]
build-backend = "setuptools.build_meta"
"#,
    );

    let assert = px_command_offline(&temp)
        .args(["--json", "migrate", "--apply", "--allow-dirty"])
        .assert()
        .failure();

    let payload: Value = serde_json::from_slice(&assert.get_output().stdout).expect("json");
    assert_eq!(payload["status"], "error");
    let message = payload["message"].as_str().unwrap_or("");
    assert!(
        message.contains("pyproject managed"),
        "expected foreign owner refusal: {message}"
    );
    let hint = payload["details"]["hint"].as_str().unwrap_or("");
    assert!(
        hint.contains("poetry"),
        "hint should reference poetry: {hint}"
    );
}

fn scaffold_demo(temp: &TempDir, package: &str) {
    px_command(temp)
        .args(["init", "--package", package])
        .assert()
        .success();
}
