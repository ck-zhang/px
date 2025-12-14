use std::path::PathBuf;

use serde_json::Value;

use crate::{ManifestSnapshot, PythonContext};
use px_domain::api::{ProjectSnapshot, PxOptions, WorkspaceConfig};

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
    pub px_options: PxOptions,
}

impl WorkspaceSnapshot {
    pub(crate) fn lock_snapshot(&self) -> ManifestSnapshot {
        ProjectSnapshot {
            root: self.config.root.clone(),
            manifest_path: self.config.manifest_path.clone(),
            lock_path: self.lock_path.clone(),
            name: self.name.clone(),
            python_requirement: self.python_requirement.clone(),
            dependencies: self.dependencies.clone(),
            dependency_groups: Vec::new(),
            declared_dependency_groups: Vec::new(),
            dependency_group_source: px_domain::api::DependencyGroupSource::None,
            group_dependencies: Vec::new(),
            requirements: self.dependencies.clone(),
            python_override: self.python_override.clone(),
            px_options: self.px_options.clone(),
            manifest_fingerprint: self.manifest_fingerprint.clone(),
        }
    }

    pub(super) fn deps_empty(&self) -> bool {
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

#[derive(Clone, Debug)]
pub struct WorkspaceSyncRequest {
    pub frozen: bool,
    pub dry_run: bool,
    pub force_resolve: bool,
}

pub struct WorkspaceRunContext {
    pub(crate) py_ctx: PythonContext,
    pub(crate) manifest_path: PathBuf,
    pub(crate) sync_report: Option<crate::EnvironmentSyncReport>,
    pub(crate) workspace_deps: Vec<String>,
    pub(crate) lock_path: PathBuf,
    pub(crate) profile_oid: Option<String>,
    pub(crate) workspace_root: PathBuf,
    pub(crate) workspace_manifest: PathBuf,
    pub(crate) site_packages: PathBuf,
    pub(crate) state: WorkspaceStateReport,
}
