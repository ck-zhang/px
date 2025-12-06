use std::{
    env,
    path::{Path, PathBuf},
};

use dirs_next::home_dir;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// High-level context for a `px status` invocation.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StatusContextKind {
    Project,
    Workspace,
    WorkspaceMember,
    None,
}

/// Describes where the command is running.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StatusContext {
    pub kind: StatusContextKind,
    pub project_name: Option<String>,
    pub workspace_name: Option<String>,
    pub project_root: Option<String>,
    pub workspace_root: Option<String>,
    pub member_path: Option<String>,
}

/// Indicates whether the runtime is px-managed or provided by the system.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeSource {
    PxManaged,
    System,
    Unknown,
}

/// Whether the runtime is tied to a project or a workspace.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeRole {
    Project,
    Workspace,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RuntimeStatus {
    pub version: Option<String>,
    pub source: RuntimeSource,
    pub role: RuntimeRole,
    pub path: Option<String>,
    pub platform: Option<String>,
}

/// Health of the lockfile relative to the manifest.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LockHealth {
    Clean,
    Mismatch,
    Missing,
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LockStatus {
    pub file: Option<String>,
    pub updated_at: Option<String>,
    pub mfingerprint: Option<String>,
    pub status: LockHealth,
}

/// Health of the environment relative to the lockfile.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EnvHealth {
    Clean,
    Stale,
    Missing,
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnvStatus {
    pub path: Option<String>,
    pub status: EnvHealth,
    pub lock_id: Option<String>,
    pub last_built_at: Option<String>,
}

/// Next action a user should take, if any.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NextActionKind {
    None,
    Init,
    Sync,
    SyncWorkspace,
    Migrate,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NextAction {
    pub kind: NextActionKind,
    pub command: Option<String>,
    pub scope: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProjectStatusPayload {
    pub manifest_exists: bool,
    pub lock_exists: bool,
    pub env_exists: bool,
    pub manifest_clean: bool,
    pub env_clean: bool,
    pub deps_empty: bool,
    pub state: String,
    pub manifest_fingerprint: Option<String>,
    pub lock_fingerprint: Option<String>,
    pub lock_id: Option<String>,
    pub lock_issue: Option<Vec<String>>,
    pub env_issue: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceMemberStatus {
    pub path: String,
    pub included: bool,
    pub manifest_status: String,
    pub manifest_fingerprint: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceStatusPayload {
    pub manifest_exists: bool,
    pub lock_exists: bool,
    pub env_exists: bool,
    pub manifest_clean: bool,
    pub env_clean: bool,
    pub deps_empty: bool,
    pub state: String,
    pub manifest_fingerprint: Option<String>,
    pub lock_fingerprint: Option<String>,
    pub lock_id: Option<String>,
    pub lock_issue: Option<Vec<String>>,
    pub env_issue: Option<Value>,
    pub members: Vec<WorkspaceMemberStatus>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StatusPayload {
    pub context: StatusContext,
    pub project: Option<ProjectStatusPayload>,
    pub workspace: Option<WorkspaceStatusPayload>,
    pub runtime: Option<RuntimeStatus>,
    pub lock: Option<LockStatus>,
    pub env: Option<EnvStatus>,
    pub next_action: NextAction,
}

impl StatusPayload {
    #[must_use]
    pub fn is_consistent(&self) -> bool {
        if let Some(workspace) = &self.workspace {
            matches!(
                workspace.state.as_str(),
                "WConsistent" | "WInitializedEmpty"
            )
        } else if let Some(project) = &self.project {
            matches!(project.state.as_str(), "Consistent" | "InitializedEmpty")
        } else {
            false
        }
    }
}

fn runtime_root() -> Option<PathBuf> {
    if let Some(path) = env::var_os("PX_RUNTIME_REGISTRY") {
        PathBuf::from(path).parent().map(|p| p.join("runtimes"))
    } else {
        home_dir().map(|home| home.join(".px").join("runtimes"))
    }
}

fn cas_runtime_root() -> Option<PathBuf> {
    if let Some(path) = env::var_os("PX_STORE_PATH") {
        return Some(PathBuf::from(path).join("runtimes"));
    }
    home_dir().map(|home| home.join(".px").join("store").join("runtimes"))
}

/// Best-effort classification of a runtime path.
#[must_use]
pub fn runtime_source_for(path: &str) -> RuntimeSource {
    let candidate = Path::new(path);
    if let Some(root) = runtime_root() {
        if candidate.starts_with(root) {
            return RuntimeSource::PxManaged;
        }
    }
    if let Some(store_root) = cas_runtime_root() {
        if candidate.starts_with(store_root) {
            return RuntimeSource::PxManaged;
        }
    }
    if candidate.is_absolute() {
        RuntimeSource::System
    } else {
        RuntimeSource::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::ffi::OsString;
    use tempfile::tempdir;

    struct EnvGuard {
        key: &'static str,
        original: Option<OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &Path) -> Self {
            let original = env::var_os(key);
            env::set_var(key, value);
            Self { key, original }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(value) = self.original.take() {
                env::set_var(self.key, value);
            } else {
                env::remove_var(self.key);
            }
        }
    }

    #[test]
    #[serial]
    fn runtime_source_marks_registry_runtimes_as_managed() {
        let temp = tempdir().unwrap();
        let registry = temp.path().join("nested").join("registry.json");
        let _guard = EnvGuard::set("PX_RUNTIME_REGISTRY", &registry);
        let runtime_path = registry
            .parent()
            .expect("registry parent")
            .join("runtimes")
            .join("cpython-3.12")
            .join("bin")
            .join("python3.12");

        assert_eq!(
            runtime_source_for(&runtime_path.display().to_string()),
            RuntimeSource::PxManaged
        );
    }

    #[test]
    #[serial]
    fn runtime_source_marks_cas_runtimes_as_managed() {
        let temp = tempdir().unwrap();
        let store = temp.path().join("store");
        let _guard = EnvGuard::set("PX_STORE_PATH", &store);
        let runtime_path = store
            .join("runtimes")
            .join("abc123")
            .join("bin")
            .join("python3.11");

        assert_eq!(
            runtime_source_for(&runtime_path.display().to_string()),
            RuntimeSource::PxManaged
        );
    }

    #[test]
    #[serial]
    fn runtime_source_defaults_to_system_for_other_paths() {
        let temp = tempdir().unwrap();
        let registry = temp.path().join("registry.json");
        let store = temp.path().join("store");
        let _guard_store = EnvGuard::set("PX_STORE_PATH", &store);
        let _guard_registry = EnvGuard::set("PX_RUNTIME_REGISTRY", &registry);
        let system_like = temp.path().join("usr").join("bin").join("python3");

        assert_eq!(
            runtime_source_for(&system_like.display().to_string()),
            RuntimeSource::System
        );
    }
}
