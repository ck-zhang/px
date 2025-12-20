use super::*;

use crate::api::{GlobalOptions, SystemEffects};
use crate::{CommandContext, CommandStatus, StatusPayload};
use anyhow::Result;
use px_domain::api::{load_lockfile, ProjectSnapshot, PxOptions, WorkspaceConfig};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::tempdir;

fn write_member(root: &Path, rel: &str, name: &str) -> ProjectSnapshot {
    let member_root = root.join(rel);
    fs::create_dir_all(&member_root).unwrap();
    let manifest = format!(
        r#"[project]
name = "{name}"
version = "0.1.0"
requires-python = ">=3.11"
dependencies = []

[tool.px]
"#
    );
    fs::write(member_root.join("pyproject.toml"), manifest).unwrap();
    ProjectSnapshot::read_from(&member_root).unwrap()
}

fn write_workspace(root: &Path) {
    let manifest = r#"[project]
name = "ws"
version = "0.0.0"
requires-python = ">=3.11"

[tool.px.workspace]
members = ["apps/a", "libs/b"]
"#;
    fs::create_dir_all(root).unwrap();
    fs::write(root.join("pyproject.toml"), manifest).unwrap();
}

fn command_context() -> CommandContext<'static> {
    let global = Box::leak(Box::new(GlobalOptions {
        quiet: false,
        verbose: 0,
        trace: false,
        debug: false,
        json: false,
    }));
    CommandContext::new(global, Arc::new(SystemEffects::new())).unwrap()
}

fn write_lock(workspace: &WorkspaceSnapshot) -> String {
    let contents =
        px_domain::api::render_lockfile(&workspace.lock_snapshot(), &[], crate::PX_VERSION)
            .unwrap();
    fs::write(&workspace.lock_path, contents).unwrap();
    let lock = load_lockfile(&workspace.lock_path).unwrap();
    lock.lock_id
        .clone()
        .unwrap_or_else(|| crate::compute_lock_hash(&workspace.lock_path).unwrap())
}

fn write_env_state_with_runtime(
    workspace: &WorkspaceSnapshot,
    lock_id: &str,
    python_version: &str,
    platform: &str,
) {
    let env_root = workspace
        .config
        .root
        .join(".px")
        .join("envs")
        .join("env-test");
    let site = env_root.join("site");
    fs::create_dir_all(&site).unwrap();
    let state = json!({
        "current_env": {
            "id": "env-test",
            "lock_id": lock_id,
            "platform": platform,
            "site_packages": site.display().to_string(),
            "python": { "path": "python", "version": python_version }
        },
        "runtime": {
            "path": "python",
            "version": python_version,
            "platform": platform
        }
    });
    let state_path = workspace
        .config
        .root
        .join(".px")
        .join("workspace-state.json");
    if let Some(dir) = state_path.parent() {
        fs::create_dir_all(dir).unwrap();
    }
    fs::write(state_path, serde_json::to_vec_pretty(&state).unwrap()).unwrap();
}

fn load_workspace(root: &Path) -> WorkspaceSnapshot {
    write_member(root, "apps/a", "a");
    write_member(root, "libs/b", "b");
    load_workspace_snapshot(root).unwrap()
}

#[test]
fn workspace_snapshot_collects_member_dependency_groups() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    let ws_manifest = root.join("pyproject.toml");
    let manifest = r#"[project]
name = "ws"
version = "0.0.0"
requires-python = ">=3.11"

[tool.px.workspace]
members = ["apps/a"]
"#;
    fs::write(&ws_manifest, manifest).unwrap();

    let member_root = root.join("apps/a");
    fs::create_dir_all(&member_root).unwrap();
    fs::write(
        member_root.join("pyproject.toml"),
        r#"[project]
name = "demo"
version = "0.1.0"
requires-python = ">=3.11"
dependencies = []

[dependency-groups]
dev = ["pytest==8.3.3"]

[tool.px]

[tool.px.dependencies]
include-groups = ["dev"]
"#,
    )
    .unwrap();

    let workspace = load_workspace_snapshot(root).unwrap();
    assert_eq!(
        workspace.dependencies,
        vec!["pytest==8.3.3".to_string()],
        "workspace dependencies should include member dependency groups"
    );
    let member = workspace.members.first().expect("workspace member");
    assert_eq!(member.snapshot.dependency_groups, vec!["dev".to_string()]);
    assert_eq!(
        member.snapshot.requirements,
        vec!["pytest==8.3.3".to_string()]
    );
}

#[test]
fn workspace_px_options_flow_into_snapshot() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    let ws_manifest = root.join("pyproject.toml");
    fs::write(
        &ws_manifest,
        r#"[tool.px.workspace]
members = ["apps/a"]

[tool.px.env]
FOO = "bar"
"#,
    )
    .unwrap();

    let member_root = root.join("apps/a");
    fs::create_dir_all(&member_root).unwrap();
    fs::write(
        member_root.join("pyproject.toml"),
        r#"[project]
name = "a"
version = "0.0.0"
requires-python = ">=3.11"
dependencies = []
"#,
    )
    .unwrap();

    let snapshot = load_workspace_snapshot(root).unwrap();
    assert_eq!(
        snapshot.px_options.env_vars.get("FOO"),
        Some(&"bar".to_string())
    );
    let lock_snapshot = snapshot.lock_snapshot();
    assert_eq!(
        lock_snapshot.px_options.env_vars.get("FOO"),
        Some(&"bar".to_string())
    );
}

#[test]
fn workspace_status_reports_missing_lock() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    write_workspace(root);
    let workspace = load_workspace(root);
    let ctx = command_context();
    let outcome = workspace_status(&ctx, WorkspaceScope::Root(workspace)).unwrap();
    assert_eq!(outcome.status, CommandStatus::UserError);
    let payload: StatusPayload =
        serde_json::from_value(outcome.details.clone()).expect("status payload");
    let workspace = payload.workspace.expect("workspace payload");
    assert!(!workspace.lock_exists);
}

#[test]
fn workspace_status_reports_missing_env() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    write_workspace(root);
    let workspace = load_workspace(root);
    write_lock(&workspace);
    let ctx = command_context();
    let outcome = workspace_status(&ctx, WorkspaceScope::Root(workspace)).unwrap();
    assert_eq!(outcome.status, CommandStatus::UserError);
    let payload: StatusPayload =
        serde_json::from_value(outcome.details.clone()).expect("status payload");
    let workspace = payload.workspace.expect("workspace payload");
    assert!(!workspace.env_exists);
    assert_eq!(workspace.state, "WNeedsEnv");
}

#[test]
fn workspace_status_reports_consistent() {
    struct EnvVarGuard {
        key: &'static str,
        prev: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &std::ffi::OsStr) -> Self {
            let prev = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, prev }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    let tmp = tempdir().unwrap();
    let root = tmp.path();
    write_workspace(root);
    let workspace = load_workspace(root);
    write_lock(&workspace);
    let python = which::which("python3")
        .or_else(|_| which::which("python"))
        .expect("locate python");
    let _python_guard = EnvVarGuard::set("PX_RUNTIME_PYTHON", python.as_os_str());
    let ctx = command_context();
    super::sync::refresh_workspace_site(&ctx, &workspace).unwrap();

    let outcome = workspace_status(&ctx, WorkspaceScope::Root(workspace)).unwrap();
    assert_eq!(outcome.status, CommandStatus::Ok);
    let payload: StatusPayload =
        serde_json::from_value(outcome.details.clone()).expect("status payload");
    assert!(payload.is_consistent());
}

#[test]
fn workspace_state_detects_runtime_mismatch() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    write_workspace(root);
    let workspace = load_workspace(root);
    let lock_id = write_lock(&workspace);
    write_env_state_with_runtime(&workspace, &lock_id, "0.0", "any");

    let ctx = command_context();
    let report = evaluate_workspace_state(&ctx, &workspace).unwrap();
    assert!(
        !report.env_clean,
        "runtime mismatches should mark workspace env dirty"
    );
    assert_eq!(report.canonical, WorkspaceStateKind::NeedsEnv);
    let reason = report.env_issue.and_then(|issue| {
        issue
            .get("reason")
            .and_then(Value::as_str)
            .map(str::to_string)
    });
    assert_eq!(reason.as_deref(), Some("runtime_mismatch"));
}

#[test]
fn workspace_warns_on_member_sandbox_config() -> Result<()> {
    let tmp = tempdir()?;
    let root = tmp.path();
    fs::create_dir_all(root.join("apps/a"))?;
    fs::write(
        root.join("pyproject.toml"),
        r#"[project]
name = "ws"
version = "0.0.0"
requires-python = ">=3.11"

[tool.px.workspace]
members = ["apps/a"]
"#,
    )?;
    fs::write(
        root.join("apps/a").join("pyproject.toml"),
        r#"[project]
name = "member-a"
version = "0.1.0"
requires-python = ">=3.11"
dependencies = []

[tool.px.sandbox]
base = "alpine-3.20"
"#,
    )?;
    let member_snapshot = ProjectSnapshot::read_from(root.join("apps/a"))?;
    let workspace = WorkspaceSnapshot {
        config: WorkspaceConfig {
            root: root.to_path_buf(),
            manifest_path: root.join("pyproject.toml"),
            members: vec![PathBuf::from("apps/a")],
            python: None,
            name: Some("ws".into()),
        },
        members: vec![WorkspaceMember {
            rel_path: "apps/a".into(),
            root: member_snapshot.root.clone(),
            snapshot: member_snapshot,
        }],
        manifest_fingerprint: "fp".into(),
        lock_path: root.join("px.workspace.lock"),
        python_requirement: ">=3.11".into(),
        python_override: None,
        dependencies: Vec::new(),
        name: "ws".into(),
        px_options: PxOptions::default(),
    };
    let warnings = super::status::collect_sandbox_warnings(&workspace);
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("apps/a"));
    Ok(())
}
