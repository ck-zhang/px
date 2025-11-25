use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::{
    compute_lock_hash, dependency_name, detect_runtime_metadata, effects::FileSystem,
    materialize_project_site, prepare_project_runtime, resolve_dependencies_with_effects,
    resolve_pins, CommandContext, ExecutionOutcome, InstallUserError, ManifestSnapshot,
    PythonContext, RuntimeMetadata, StoredEnvironment, StoredRuntime, PX_VERSION,
};
use px_domain::{
    detect_lock_drift, discover_workspace_root, load_lockfile_optional, read_workspace_config,
    workspace_manifest_fingerprint, workspace_member_for_path, ManifestEditor, ProjectSnapshot,
    ResolvedDependency, WorkspaceConfig, WorkspaceLock, WorkspaceMember as WorkspaceLockMember,
    WorkspaceOwner,
};

#[derive(Clone, Debug)]
pub enum WorkspaceScope {
    Root(WorkspaceSnapshot),
    Member {
        workspace: WorkspaceSnapshot,
        member_root: PathBuf,
    },
}

#[derive(Clone, Debug)]
pub struct WorkspaceMember {
    pub rel_path: String,
    pub root: PathBuf,
    pub snapshot: ProjectSnapshot,
}

#[derive(Clone, Debug)]
pub struct WorkspaceSnapshot {
    pub config: WorkspaceConfig,
    pub members: Vec<WorkspaceMember>,
    pub manifest_fingerprint: String,
    pub lock_path: PathBuf,
    pub python_requirement: String,
    pub python_override: Option<String>,
    pub dependencies: Vec<String>,
    pub name: String,
}

impl WorkspaceSnapshot {
    fn lock_snapshot(&self) -> ManifestSnapshot {
        ProjectSnapshot {
            root: self.config.root.clone(),
            manifest_path: self.config.manifest_path.clone(),
            lock_path: self.lock_path.clone(),
            name: self.name.clone(),
            python_requirement: self.python_requirement.clone(),
            dependencies: self.dependencies.clone(),
            dependency_groups: Vec::new(),
            declared_dependency_groups: Vec::new(),
            dependency_group_source: px_domain::DependencyGroupSource::None,
            group_dependencies: Vec::new(),
            requirements: self.dependencies.clone(),
            python_override: self.python_override.clone(),
            manifest_fingerprint: self.manifest_fingerprint.clone(),
        }
    }

    fn deps_empty(&self) -> bool {
        self.dependencies.is_empty()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceStateKind {
    Uninitialized,
    InitializedEmpty,
    NeedsLock,
    NeedsEnv,
    Consistent,
}

impl WorkspaceStateKind {
    pub fn as_str(self) -> &'static str {
        match self {
            WorkspaceStateKind::Uninitialized => "uninitialized",
            WorkspaceStateKind::InitializedEmpty => "initialized-empty",
            WorkspaceStateKind::NeedsLock => "needs-lock",
            WorkspaceStateKind::NeedsEnv => "needs-env",
            WorkspaceStateKind::Consistent => "consistent",
        }
    }
}

#[derive(Clone, Debug)]
pub struct WorkspaceStateReport {
    pub manifest_exists: bool,
    pub lock_exists: bool,
    pub env_exists: bool,
    pub manifest_clean: bool,
    pub env_clean: bool,
    pub deps_empty: bool,
    pub canonical: WorkspaceStateKind,
    pub manifest_fingerprint: Option<String>,
    pub lock_fingerprint: Option<String>,
    pub lock_id: Option<String>,
    pub lock_issue: Option<Vec<String>>,
    pub env_issue: Option<Value>,
}

impl WorkspaceStateReport {
    fn is_consistent(&self) -> bool {
        matches!(
            self.canonical,
            WorkspaceStateKind::Consistent | WorkspaceStateKind::InitializedEmpty
        )
    }
}

/// Determine if CWD is inside a workspace (root or member).
pub fn discover_workspace_scope() -> Result<Option<WorkspaceScope>> {
    let Some(root) = discover_workspace_root()? else {
        return Ok(None);
    };
    let snapshot = load_workspace_snapshot(&root)?;
    let cwd = std::env::current_dir().context("unable to determine current directory")?;
    if let Some(member_root) = workspace_member_for_path(&snapshot.config, &cwd) {
        Ok(Some(WorkspaceScope::Member {
            workspace: snapshot,
            member_root,
        }))
    } else {
        Ok(Some(WorkspaceScope::Root(snapshot)))
    }
}

fn load_workspace_snapshot(root: &Path) -> Result<WorkspaceSnapshot> {
    let config = read_workspace_config(root)?;
    let mut members = Vec::new();
    for rel in &config.members {
        let member_root = config.root.join(rel);
        let abs = member_root.canonicalize().with_context(|| {
            format!("workspace member {} does not exist", member_root.display())
        })?;
        let snapshot = ProjectSnapshot::read_from(&abs)?;
        let rel_path = abs
            .strip_prefix(&config.root)
            .unwrap_or(&abs)
            .display()
            .to_string();
        members.push(WorkspaceMember {
            rel_path,
            root: abs,
            snapshot,
        });
    }
    let python_override = config.python.clone();
    let python_requirement = derive_workspace_python(&config, &members)?;
    let manifest_fingerprint = workspace_manifest_fingerprint(
        &config,
        &members
            .iter()
            .map(|m| m.snapshot.clone())
            .collect::<Vec<_>>(),
    )?;
    let mut dependencies = Vec::new();
    for member in &members {
        dependencies.extend(member.snapshot.requirements.clone());
    }
    dependencies.retain(|dep| !dep.trim().is_empty());

    let name = config
        .name
        .clone()
        .or_else(|| {
            config
                .root
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| "workspace".to_string());

    Ok(WorkspaceSnapshot {
        lock_path: config.root.join("px.workspace.lock"),
        config,
        members,
        manifest_fingerprint,
        python_requirement,
        python_override,
        dependencies,
        name,
    })
}

fn derive_workspace_python(
    config: &WorkspaceConfig,
    members: &[WorkspaceMember],
) -> Result<String> {
    if let Some(py) = &config.python {
        return Ok(py.clone());
    }
    if members.is_empty() {
        return Ok(">=3.11".to_string());
    }
    let mut requirements = members
        .iter()
        .map(|m| m.snapshot.python_requirement.clone())
        .collect::<Vec<_>>();
    requirements.sort();
    requirements.dedup();
    if requirements.len() == 1 {
        Ok(requirements[0].clone())
    } else {
        Err(anyhow!(
            "workspace members disagree on requires-python; set [tool.px.workspace].python"
        ))
    }
}

fn workspace_state_path(root: &Path) -> PathBuf {
    root.join(".px").join("workspace-state.json")
}

fn load_workspace_state(filesystem: &dyn FileSystem, root: &Path) -> Result<ProjectState> {
    let path = workspace_state_path(root);
    match filesystem.read_to_string(&path) {
        Ok(contents) => {
            let state: ProjectState = serde_json::from_str(&contents)
                .with_context(|| format!("failed to parse {}", path.display()))?;
            validate_project_state(&state)?;
            Ok(state)
        }
        Err(err) => {
            if filesystem.metadata(&path).is_ok() {
                Err(err)
            } else {
                Ok(ProjectState::default())
            }
        }
    }
}

fn persist_workspace_state(
    filesystem: &dyn FileSystem,
    root: &Path,
    env: StoredEnvironment,
    runtime: StoredRuntime,
) -> Result<()> {
    let mut state = load_workspace_state(filesystem, root)?;
    state.current_env = Some(env);
    state.runtime = Some(runtime);
    write_project_state(filesystem, &workspace_state_path(root), &state)
}

fn write_project_state(
    filesystem: &dyn FileSystem,
    path: &Path,
    state: &ProjectState,
) -> Result<()> {
    let mut contents = serde_json::to_vec_pretty(state)?;
    contents.push(b'\n');
    if let Some(dir) = path.parent() {
        filesystem.create_dir_all(dir)?;
    }
    filesystem.write(path, &contents)?;
    Ok(())
}

fn evaluate_workspace_state(
    ctx: &CommandContext,
    workspace: &WorkspaceSnapshot,
) -> Result<WorkspaceStateReport> {
    let manifest_exists = workspace.config.manifest_path.exists();
    let lock_exists = workspace.lock_path.exists();
    let mut manifest_clean = false;
    let mut env_clean = false;
    let mut lock_fingerprint = None;
    let mut lock_issue = None;
    let mut lock_id = None;
    let mut env_issue = None;
    let env_state = load_workspace_state(ctx.fs(), &workspace.config.root)?;

    if lock_exists {
        if let Some(lock) = load_lockfile_optional(&workspace.lock_path)? {
            lock_fingerprint = lock.manifest_fingerprint.clone();
            let marker_env = ctx.marker_environment().ok();
            let drift = detect_lock_drift(&workspace.lock_snapshot(), &lock, marker_env.as_ref());
            if drift.is_empty()
                && lock
                    .manifest_fingerprint
                    .as_deref()
                    .is_some_and(|fp| fp == workspace.manifest_fingerprint)
            {
                manifest_clean = true;
            } else if !drift.is_empty() {
                lock_issue = Some(drift);
            }
            lock_id = Some(
                lock.lock_id
                    .clone()
                    .unwrap_or(compute_lock_hash(&workspace.lock_path)?),
            );
            if let Some(env) = env_state.current_env.clone() {
                let site_dir = PathBuf::from(&env.site_packages);
                if let Some(expected) = lock_id.as_ref() {
                    if env.lock_id == *expected && site_dir.exists() {
                        env_clean = true;
                    } else {
                        env_issue = Some(json!({
                            "reason": if site_dir.exists() { "env_outdated" } else { "missing_env" },
                            "site": env.site_packages,
                            "expected_lock_id": lock_id.clone(),
                            "current_lock_id": env.lock_id,
                        }));
                    }
                }
            } else {
                env_issue = Some(json!({ "reason": "missing_env" }));
            }
        }
    }

    let deps_empty = workspace.deps_empty();
    let env_exists = env_state
        .current_env
        .as_ref()
        .map(|env| PathBuf::from(&env.site_packages).exists())
        .unwrap_or(false);
    let canonical = if !manifest_exists {
        WorkspaceStateKind::Uninitialized
    } else if !lock_exists || !manifest_clean {
        WorkspaceStateKind::NeedsLock
    } else if !env_clean {
        WorkspaceStateKind::NeedsEnv
    } else if deps_empty {
        WorkspaceStateKind::InitializedEmpty
    } else {
        WorkspaceStateKind::Consistent
    };

    Ok(WorkspaceStateReport {
        manifest_exists,
        lock_exists,
        env_exists,
        manifest_clean,
        env_clean,
        deps_empty,
        canonical,
        manifest_fingerprint: Some(workspace.manifest_fingerprint.clone()),
        lock_fingerprint,
        lock_id,
        lock_issue,
        env_issue,
    })
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct ProjectState {
    #[serde(default)]
    current_env: Option<StoredEnvironment>,
    #[serde(default)]
    runtime: Option<StoredRuntime>,
}

fn validate_project_state(state: &ProjectState) -> Result<()> {
    if let Some(env) = &state.current_env {
        if env.id.trim().is_empty() || env.lock_id.trim().is_empty() {
            anyhow::bail!("invalid workspace state: missing environment identity");
        }
        if env.site_packages.trim().is_empty() {
            anyhow::bail!("invalid workspace state: missing site-packages path");
        }
        if env.python.path.trim().is_empty() || env.python.version.trim().is_empty() {
            anyhow::bail!("invalid workspace state: missing python metadata");
        }
    }
    if let Some(runtime) = &state.runtime {
        if runtime.path.trim().is_empty()
            || runtime.version.trim().is_empty()
            || runtime.platform.trim().is_empty()
        {
            anyhow::bail!("invalid workspace runtime metadata");
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct WorkspaceSyncRequest {
    pub frozen: bool,
    pub dry_run: bool,
    pub force_resolve: bool,
}

pub fn workspace_sync(
    ctx: &CommandContext,
    scope: WorkspaceScope,
    request: &WorkspaceSyncRequest,
) -> Result<ExecutionOutcome> {
    let workspace_root = match &scope {
        WorkspaceScope::Root(ws) | WorkspaceScope::Member { workspace: ws, .. } => {
            ws.config.root.clone()
        }
    };
    let workspace = load_workspace_snapshot(&workspace_root)?;
    let state = evaluate_workspace_state(ctx, &workspace)?;
    if request.dry_run {
        let action = if !state.lock_exists || !state.manifest_clean || request.force_resolve {
            "resolve_lock"
        } else if state.env_clean {
            "noop"
        } else {
            "sync_env"
        };
        return Ok(ExecutionOutcome::success(
            format!("workspace {action} (dry-run)"),
            json!({
                "workspace": workspace.config.root.display().to_string(),
                "lockfile": workspace.lock_path.display().to_string(),
                "action": action,
                "state": state.canonical.as_str(),
                "dry_run": true,
            }),
        ));
    }

    if request.frozen {
        if !state.lock_exists || matches!(state.canonical, WorkspaceStateKind::NeedsLock) {
            return Ok(workspace_violation(
                "sync",
                &workspace,
                &state,
                StateViolation::MissingLock,
            ));
        }
        if !state.env_clean {
            refresh_workspace_site(ctx, &workspace)?;
        }
        return Ok(ExecutionOutcome::success(
            "workspace environment synced from existing lock",
            json!({
                "workspace": workspace.config.root.display().to_string(),
                "lockfile": workspace.lock_path.display().to_string(),
                "state": WorkspaceStateKind::Consistent.as_str(),
                "mode": "frozen",
            }),
        ));
    }

    if request.force_resolve || !state.lock_exists || !state.manifest_clean {
        resolve_workspace(ctx, &workspace)?;
    }
    refresh_workspace_site(ctx, &workspace)?;

    Ok(ExecutionOutcome::success(
        "workspace lock and environment updated",
        json!({
            "workspace": workspace.config.root.display().to_string(),
            "lockfile": workspace.lock_path.display().to_string(),
            "state": WorkspaceStateKind::Consistent.as_str(),
        }),
    ))
}

fn resolve_workspace(ctx: &CommandContext, workspace: &WorkspaceSnapshot) -> Result<()> {
    let resolved =
        resolve_dependencies_with_effects(ctx.effects(), &workspace.lock_snapshot(), true)
            .map_err(|err| match err.downcast::<InstallUserError>() {
                Ok(user) => InstallUserError::new(user.message, user.details),
                Err(other) => InstallUserError::new(
                    "dependency resolution failed",
                    json!({ "error": other.to_string() }),
                ),
            })?;
    let resolved_deps = resolve_pins(ctx, &resolved.pins, ctx.config().resolver.force_sdist)?;
    let workspace_lock = build_workspace_lock(workspace, &resolved_deps);
    let contents = px_domain::render_lockfile_with_workspace(
        &workspace.lock_snapshot(),
        &resolved_deps,
        PX_VERSION,
        Some(&workspace_lock),
    )?;
    if let Some(parent) = workspace.lock_path.parent() {
        ctx.fs().create_dir_all(parent)?;
    }
    ctx.fs()
        .write(&workspace.lock_path, contents.as_bytes())
        .context("failed to write workspace lockfile")?;
    Ok(())
}

fn refresh_workspace_site(ctx: &CommandContext, workspace: &WorkspaceSnapshot) -> Result<()> {
    let snapshot = workspace.lock_snapshot();
    let _ = prepare_project_runtime(&snapshot)?;
    let lock = load_lockfile_optional(&workspace.lock_path)?.ok_or_else(|| {
        anyhow!(
            "workspace lockfile missing at {}",
            workspace.lock_path.display()
        )
    })?;
    let runtime = detect_runtime_metadata(ctx, &snapshot)?;
    let lock_id = lock
        .lock_id
        .clone()
        .unwrap_or(compute_lock_hash(&workspace.lock_path)?);
    let env_id = compute_environment_id(&lock_id, &runtime);
    let env_root = workspace.config.root.join(".px").join("envs").join(&env_id);
    ctx.fs().create_dir_all(&env_root)?;
    let site_dir = env_root.join("site");
    ctx.fs().create_dir_all(&site_dir)?;
    materialize_project_site(&site_dir, &lock, ctx.fs())?;
    let canonical_site = ctx.fs().canonicalize(&site_dir).unwrap_or(site_dir.clone());
    let runtime_state = StoredRuntime {
        path: runtime.path.clone(),
        version: runtime.version.clone(),
        platform: runtime.platform.clone(),
    };
    let env_state = StoredEnvironment {
        id: env_id,
        lock_id,
        platform: runtime.platform.clone(),
        site_packages: canonical_site.display().to_string(),
        python: crate::StoredPython {
            path: runtime.path.clone(),
            version: runtime.version.clone(),
        },
    };
    persist_workspace_state(ctx.fs(), &workspace.config.root, env_state, runtime_state)
}

#[derive(Clone, Copy, Debug)]
enum StateViolation {
    MissingLock,
    LockDrift,
    EnvDrift,
}

fn workspace_violation(
    command: &str,
    snapshot: &WorkspaceSnapshot,
    state_report: &WorkspaceStateReport,
    violation: StateViolation,
) -> ExecutionOutcome {
    let mut base = json!({
        "workspace": snapshot.config.root.display().to_string(),
        "manifest": snapshot.config.manifest_path.display().to_string(),
        "lockfile": snapshot.lock_path.display().to_string(),
        "state": state_report.canonical.as_str(),
    });
    match violation {
        StateViolation::MissingLock => {
            let hint = if command == "sync" {
                "Run `px sync` without --frozen to generate px.workspace.lock before syncing."
                    .to_string()
            } else {
                format!("Run `px sync` before `px {command}`.")
            };
            base["hint"] = json!(hint);
            base["code"] = json!("PX120");
            base["reason"] = json!("missing_lock");
            ExecutionOutcome::user_error("workspace lock not found", base)
        }
        StateViolation::LockDrift => {
            let mut details = base;
            details["hint"] = json!("Run `px sync` to update the workspace lock and environment.");
            details["code"] = json!("PX120");
            details["reason"] = json!("lock_drift");
            if let Some(fp) = &state_report.lock_fingerprint {
                details["lock_fingerprint"] = json!(fp);
            }
            if let Some(fp) = &state_report.manifest_fingerprint {
                details["manifest_fingerprint"] = json!(fp);
            }
            if let Some(lock_id) = &state_report.lock_id {
                details["lock_id"] = json!(lock_id);
            }
            if let Some(issues) = &state_report.lock_issue {
                details["lock_issue"] = json!(issues);
            }
            ExecutionOutcome::user_error(
                "Workspace manifest has changed since px.workspace.lock was created",
                details,
            )
        }
        StateViolation::EnvDrift => {
            let mut reason = "env_outdated".to_string();
            let mut details = base;
            details["hint"] = json!(format!(
                "Run `px sync` before `px {command}` (environment is out of sync)."
            ));
            details["code"] = json!("PX201");
            if let Some(issue) = &state_report.env_issue {
                details["environment_issue"] = issue.clone();
                if let Some(r) = issue.get("reason").and_then(serde_json::Value::as_str) {
                    reason = r.to_string();
                }
            }
            if let Some(lock_id) = &state_report.lock_id {
                details["lock_id"] = json!(lock_id);
            }
            details["reason"] = json!(reason);
            let message = if reason == "missing_env" {
                "workspace environment missing"
            } else {
                "Workspace environment is out of sync with px.workspace.lock"
            };
            ExecutionOutcome::user_error(message, details)
        }
    }
}

fn build_workspace_lock(
    workspace: &WorkspaceSnapshot,
    resolved: &[ResolvedDependency],
) -> WorkspaceLock {
    let mut member_lookup: HashMap<String, Vec<String>> = HashMap::new();
    let mut members = workspace
        .members
        .iter()
        .map(|member| {
            let mut deps = member.snapshot.requirements.clone();
            deps.sort();
            for dep in &deps {
                let name = dependency_name(dep);
                if !name.is_empty() {
                    member_lookup
                        .entry(name.to_lowercase())
                        .or_default()
                        .push(member.rel_path.clone());
                }
            }
            WorkspaceLockMember {
                name: member.snapshot.name.clone(),
                path: member.rel_path.clone(),
                manifest_fingerprint: member.snapshot.manifest_fingerprint.clone(),
                dependencies: deps,
            }
        })
        .collect::<Vec<_>>();
    members.sort_by(|a, b| a.path.cmp(&b.path));

    let mut owners = Vec::new();
    for dep in resolved {
        let key = dep.name.to_lowercase();
        let mut owned_by = member_lookup.get(&key).cloned().unwrap_or_default();
        if owned_by.is_empty() {
            owned_by.push("external".to_string());
        } else {
            owned_by.sort();
            owned_by.dedup();
        }
        owners.push(WorkspaceOwner {
            name: dep.name.clone(),
            owners: owned_by,
        });
    }
    owners.sort_by(|a, b| a.name.cmp(&b.name));

    WorkspaceLock { members, owners }
}

fn compute_environment_id(lock_id: &str, runtime: &RuntimeMetadata) -> String {
    let mut hasher = Sha256::new();
    hasher.update(lock_id.as_bytes());
    hasher.update(runtime.version.as_bytes());
    hasher.update(runtime.platform.as_bytes());
    hasher.update(runtime.path.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    let short = &digest[..digest.len().min(16)];
    format!("env-{short}")
}

struct WorkspaceBackup {
    manifest_path: PathBuf,
    lock_path: PathBuf,
    manifest_contents: String,
    lock_contents: Option<String>,
    manifest_permissions: fs::Permissions,
    lock_permissions: Option<fs::Permissions>,
    lock_preexisting: bool,
}

impl WorkspaceBackup {
    fn capture(manifest_path: &Path, lock_path: &Path) -> Result<Self> {
        let manifest_contents = fs::read_to_string(manifest_path)?;
        let manifest_permissions = fs::metadata(manifest_path)?.permissions();
        let lock_preexisting = lock_path.exists();
        let lock_contents = if lock_preexisting {
            Some(fs::read_to_string(lock_path)?)
        } else {
            None
        };
        let lock_permissions = if lock_preexisting {
            Some(fs::metadata(lock_path)?.permissions())
        } else {
            None
        };
        Ok(Self {
            manifest_path: manifest_path.to_path_buf(),
            lock_path: lock_path.to_path_buf(),
            manifest_contents,
            lock_contents,
            manifest_permissions,
            lock_permissions,
            lock_preexisting,
        })
    }

    fn restore(&self) -> Result<()> {
        self.write_with_permissions(
            &self.manifest_path,
            &self.manifest_contents,
            &self.manifest_permissions,
        )?;
        match (&self.lock_contents, self.lock_preexisting) {
            (Some(contents), _) => {
                let permissions = if let Some(perms) = &self.lock_permissions {
                    perms.clone()
                } else {
                    fs::metadata(&self.lock_path)?.permissions()
                };
                self.write_with_permissions(&self.lock_path, contents, &permissions)?;
            }
            (None, false) => {
                if self.lock_path.exists() {
                    self.remove_with_permissions(&self.lock_path)?;
                }
            }
            (None, true) => {
                debug_assert!(
                    false,
                    "lock_preexisting implies lock contents should have been captured"
                );
            }
        }
        Ok(())
    }

    fn write_with_permissions(
        &self,
        path: &Path,
        contents: &str,
        permissions: &fs::Permissions,
    ) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, contents)?;
        #[cfg(unix)]
        {
            fs::set_permissions(path, permissions.clone())?;
        }
        Ok(())
    }

    fn remove_with_permissions(&self, path: &Path) -> Result<()> {
        #[cfg(unix)]
        {
            let perms = fs::metadata(path)?.permissions();
            let mut writable = perms.clone();
            writable.set_mode(0o644);
            fs::set_permissions(path, writable)?;
        }
        fs::remove_file(path)?;
        Ok(())
    }
}

pub fn workspace_status(ctx: &CommandContext, scope: WorkspaceScope) -> Result<ExecutionOutcome> {
    let workspace_root = match &scope {
        WorkspaceScope::Root(ws) | WorkspaceScope::Member { workspace: ws, .. } => {
            ws.config.root.clone()
        }
    };
    let workspace = load_workspace_snapshot(&workspace_root)?;
    let state = evaluate_workspace_state(ctx, &workspace)?;
    let mut details = json!({
        "workspace": workspace.config.root.display().to_string(),
        "lockfile": workspace.lock_path.display().to_string(),
        "state": state.canonical.as_str(),
        "flags": {
            "manifest_exists": state.manifest_exists,
            "lock_exists": state.lock_exists,
            "env_exists": state.env_exists,
            "manifest_clean": state.manifest_clean,
            "env_clean": state.env_clean,
            "consistent": state.is_consistent(),
        },
        "members": workspace.members.iter().map(|m| {
            json!({
                "path": m.rel_path,
                "manifest_fingerprint": m.snapshot.manifest_fingerprint,
                "dependency_groups": {
                    "active": m.snapshot.dependency_groups.clone(),
                    "declared": m.snapshot.declared_dependency_groups.clone(),
                    "source": m.snapshot.dependency_group_source.as_str(),
                },
            })
        }).collect::<Vec<_>>(),
    });
    if let Some(ref fp) = state.manifest_fingerprint {
        details["manifest_fingerprint"] = json!(fp);
    }
    if let Some(ref fp) = state.lock_fingerprint {
        details["lock_fingerprint"] = json!(fp);
    }
    if let Some(ref id) = state.lock_id {
        details["lock_id"] = json!(id);
    }
    if let Some(ref issue) = state.lock_issue {
        details["lock_issue"] = json!(issue);
    }
    if let Some(ref issue) = state.env_issue {
        details["environment_issue"] = issue.clone();
    }
    if state.is_consistent() {
        Ok(ExecutionOutcome::success(
            "Workspace is consistent",
            details,
        ))
    } else if matches!(state.canonical, WorkspaceStateKind::NeedsEnv) {
        details["hint"] = json!("Run `px sync` to rebuild the workspace environment.");
        Ok(ExecutionOutcome::user_error(
            "Workspace environment is missing or out of date",
            details,
        ))
    } else {
        details["hint"] = json!("Run `px sync` to update px.workspace.lock");
        Ok(ExecutionOutcome::user_error(
            "Workspace lockfile is missing or out of date",
            details,
        ))
    }
}

pub fn workspace_add(
    ctx: &CommandContext,
    request: &crate::ProjectAddRequest,
    scope: WorkspaceScope,
) -> Result<ExecutionOutcome> {
    let WorkspaceScope::Member {
        workspace,
        member_root,
    } = scope
    else {
        return Ok(ExecutionOutcome::user_error(
            "px add: not inside a workspace member",
            json!({ "hint": "run inside a configured workspace member" }),
        ));
    };
    if request.specs.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            "provide at least one dependency",
            json!({ "hint": "run `px add name==version`" }),
        ));
    }
    let cleaned_specs: Vec<String> = request
        .specs
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if cleaned_specs.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            "provide at least one dependency",
            json!({ "hint": "run `px add name==version`" }),
        ));
    }
    let manifest_path = member_root.join("pyproject.toml");
    let backup = WorkspaceBackup::capture(&manifest_path, &workspace.lock_path)?;
    let mut needs_restore = true;
    let outcome = (|| -> Result<ExecutionOutcome> {
        let mut editor = ManifestEditor::open(&manifest_path)?;
        let report = editor.add_specs(&cleaned_specs)?;
        if report.added.is_empty() && report.updated.is_empty() {
            needs_restore = request.dry_run;
            return Ok(ExecutionOutcome::success(
                "dependencies already satisfied",
                json!({
                    "pyproject": manifest_path.display().to_string(),
                    "dry_run": request.dry_run,
                }),
            ));
        }
        if request.dry_run {
            return Ok(ExecutionOutcome::success(
                "planned dependency changes (dry-run)",
                json!({
                    "pyproject": manifest_path.display().to_string(),
                    "added": report.added,
                    "updated": report.updated,
                    "dry_run": true,
                }),
            ));
        }
        workspace_sync(
            ctx,
            WorkspaceScope::Member {
                workspace: workspace.clone(),
                member_root,
            },
            &WorkspaceSyncRequest {
                frozen: false,
                dry_run: false,
                force_resolve: true,
            },
        )?;
        needs_restore = false;
        Ok(ExecutionOutcome::success(
            "updated workspace dependencies",
            json!({
                "pyproject": manifest_path.display().to_string(),
                "lockfile": workspace.lock_path.display().to_string(),
                "added": report.added,
                "updated": report.updated,
            }),
        ))
    })();
    if needs_restore {
        backup.restore()?;
    }
    outcome
}

pub fn workspace_remove(
    ctx: &CommandContext,
    request: &crate::ProjectRemoveRequest,
    scope: WorkspaceScope,
) -> Result<ExecutionOutcome> {
    let WorkspaceScope::Member {
        workspace,
        member_root,
    } = scope
    else {
        return Ok(ExecutionOutcome::user_error(
            "px remove: not inside a workspace member",
            json!({ "hint": "run inside a configured workspace member" }),
        ));
    };
    if request.specs.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            "provide at least one dependency",
            json!({ "hint": "run `px remove name`" }),
        ));
    }
    let manifest_path = member_root.join("pyproject.toml");
    let backup = WorkspaceBackup::capture(&manifest_path, &workspace.lock_path)?;
    let mut needs_restore = true;
    let outcome = (|| -> Result<ExecutionOutcome> {
        let mut editor = ManifestEditor::open(&manifest_path)?;
        let report = editor.remove_specs(&request.specs)?;
        if report.removed.is_empty() {
            needs_restore = request.dry_run;
            return Ok(ExecutionOutcome::user_error(
                "none of the requested dependencies are direct dependencies",
                json!({
                    "pyproject": manifest_path.display().to_string(),
                    "requested": request.specs,
                }),
            ));
        }
        if request.dry_run {
            return Ok(ExecutionOutcome::success(
                "planned dependency removals (dry-run)",
                json!({
                    "pyproject": manifest_path.display().to_string(),
                    "removed": report.removed,
                    "dry_run": true,
                }),
            ));
        }
        workspace_sync(
            ctx,
            WorkspaceScope::Member {
                workspace: workspace.clone(),
                member_root,
            },
            &WorkspaceSyncRequest {
                frozen: false,
                dry_run: false,
                force_resolve: true,
            },
        )?;
        needs_restore = false;
        Ok(ExecutionOutcome::success(
            "removed dependencies and updated workspace",
            json!({
                "pyproject": manifest_path.display().to_string(),
                "lockfile": workspace.lock_path.display().to_string(),
                "removed": report.removed,
            }),
        ))
    })();
    if needs_restore {
        backup.restore()?;
    }
    outcome
}

pub fn workspace_update(
    ctx: &CommandContext,
    _request: &crate::ProjectUpdateRequest,
    scope: WorkspaceScope,
) -> Result<ExecutionOutcome> {
    let workspace = match scope {
        WorkspaceScope::Root(ws) | WorkspaceScope::Member { workspace: ws, .. } => ws,
    };
    workspace_sync(
        ctx,
        WorkspaceScope::Root(workspace.clone()),
        &WorkspaceSyncRequest {
            frozen: false,
            dry_run: false,
            force_resolve: true,
        },
    )
}

pub struct WorkspaceRunContext {
    pub(crate) py_ctx: PythonContext,
    pub(crate) manifest_path: PathBuf,
    pub(crate) sync_report: Option<crate::EnvironmentSyncReport>,
}

pub fn prepare_workspace_run_context(
    ctx: &CommandContext,
    strict: bool,
) -> Result<Option<WorkspaceRunContext>, ExecutionOutcome> {
    let scope = discover_workspace_scope().map_err(|err| {
        ExecutionOutcome::failure(
            "failed to detect workspace",
            json!({ "error": err.to_string() }),
        )
    })?;
    let Some(WorkspaceScope::Member {
        workspace,
        member_root,
    }) = scope
    else {
        return Ok(None);
    };
    let state = evaluate_workspace_state(ctx, &workspace).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to evaluate workspace state",
            json!({ "error": err.to_string() }),
        )
    })?;
    if !state.lock_exists {
        return Err(workspace_violation(
            "run",
            &workspace,
            &state,
            StateViolation::MissingLock,
        ));
    }
    if matches!(state.canonical, WorkspaceStateKind::NeedsLock) {
        return Err(workspace_violation(
            "run",
            &workspace,
            &state,
            StateViolation::LockDrift,
        ));
    }

    if strict && !state.env_clean {
        return Err(workspace_violation(
            "run",
            &workspace,
            &state,
            StateViolation::EnvDrift,
        ));
    }

    let sync_report = None;
    if !strict && !state.env_clean {
        refresh_workspace_site(ctx, &workspace).map_err(|err| {
            ExecutionOutcome::failure(
                "failed to prepare workspace environment",
                json!({ "error": err.to_string() }),
            )
        })?;
    }
    let state_file = load_workspace_state(ctx.fs(), &workspace.config.root).map_err(|err| {
        ExecutionOutcome::failure(
            "workspace state unreadable",
            json!({ "error": err.to_string() }),
        )
    })?;
    let Some(env) = state_file.current_env else {
        return Err(workspace_violation(
            "run",
            &workspace,
            &state,
            StateViolation::EnvDrift,
        ));
    };
    let runtime = prepare_project_runtime(&workspace.lock_snapshot()).map_err(|err| {
        ExecutionOutcome::failure(
            "workspace runtime unavailable",
            json!({ "error": err.to_string() }),
        )
    })?;
    let site_dir = PathBuf::from(&env.site_packages);
    let (pythonpath, allowed_paths) =
        crate::build_pythonpath(ctx.fs(), &member_root, Some(site_dir.clone())).map_err(|err| {
            ExecutionOutcome::failure(
                "failed to build workspace PYTHONPATH",
                json!({ "error": err.to_string() }),
            )
        })?;
    let py_ctx = PythonContext {
        project_root: member_root.clone(),
        python: runtime.record.path.clone(),
        pythonpath,
        allowed_paths,
    };
    Ok(Some(WorkspaceRunContext {
        py_ctx,
        manifest_path: member_root.join("pyproject.toml"),
        sync_report,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CommandStatus, GlobalOptions, SystemEffects};
    use px_domain::lockfile::load_lockfile;
    use std::fs;
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
            json: false,
            config: None,
        }));
        CommandContext::new(global, Arc::new(SystemEffects::new())).unwrap()
    }

    fn write_lock(workspace: &WorkspaceSnapshot) -> String {
        let contents =
            px_domain::render_lockfile(&workspace.lock_snapshot(), &[], PX_VERSION).unwrap();
        fs::write(&workspace.lock_path, contents).unwrap();
        let lock = load_lockfile(&workspace.lock_path).unwrap();
        lock.lock_id
            .clone()
            .unwrap_or_else(|| compute_lock_hash(&workspace.lock_path).unwrap())
    }

    fn write_env_state(workspace: &WorkspaceSnapshot, lock_id: &str) {
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
                "platform": "any",
                "site_packages": site.display().to_string(),
                "python": { "path": "python", "version": "3.11" }
            },
            "runtime": {
                "path": "python",
                "version": "3.11",
                "platform": "any"
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
    fn workspace_status_reports_missing_lock() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        write_workspace(root);
        let workspace = load_workspace(root);
        let ctx = command_context();
        let outcome = workspace_status(&ctx, WorkspaceScope::Root(workspace)).unwrap();
        assert_eq!(outcome.status, CommandStatus::UserError);
        let flags = outcome.details.get("flags").expect("flags");
        assert_eq!(
            flags.get("lock_exists").and_then(Value::as_bool),
            Some(false)
        );
        let members = outcome
            .details
            .get("members")
            .and_then(Value::as_array)
            .expect("members list");
        assert!(
            members
                .iter()
                .all(|member| member.get("dependency_groups").is_some()),
            "workspace status should surface dependency group info for members"
        );
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
        assert_eq!(
            outcome
                .details
                .get("flags")
                .and_then(|f| f.get("env_exists"))
                .and_then(Value::as_bool),
            Some(false)
        );
    }

    #[test]
    fn workspace_status_reports_consistent() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        write_workspace(root);
        let workspace = load_workspace(root);
        let lock_id = write_lock(&workspace);
        write_env_state(&workspace, &lock_id);

        let ctx = command_context();
        let outcome = workspace_status(&ctx, WorkspaceScope::Root(workspace)).unwrap();
        assert_eq!(outcome.status, CommandStatus::Ok);
        assert_eq!(
            outcome
                .details
                .get("flags")
                .and_then(|f| f.get("consistent"))
                .and_then(Value::as_bool),
            Some(true)
        );
    }
}
