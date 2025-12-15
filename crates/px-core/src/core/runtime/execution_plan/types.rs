use std::fmt;

use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EngineMode {
    CasNative,
    MaterializedEnv,
}

impl EngineMode {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::CasNative => "cas_native",
            Self::MaterializedEnv => "materialized_env",
        }
    }
}

impl fmt::Display for EngineMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct EnginePlan {
    pub(crate) mode: EngineMode,
    pub(crate) fallback_reason_code: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum SourceProvenance {
    WorkingTree,
    GitRef {
        git_ref: String,
        repo_root: String,
        manifest_repo_path: String,
        lock_repo_path: String,
    },
    RepoSnapshot {
        locator: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        git_ref: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        commit: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        repo_snapshot_oid: Option<String>,
        script_repo_path: String,
    },
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum PlanContext {
    Project {
        project_root: String,
        manifest_path: String,
        lock_path: String,
        project_name: String,
    },
    Workspace {
        workspace_root: String,
        workspace_manifest: String,
        workspace_lock_path: String,
        member_root: String,
        member_manifest: String,
    },
    #[allow(dead_code)]
    Tool { tool_name: String },
    UrlRun {
        locator: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        git_ref: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        commit: Option<String>,
        script_repo_path: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TargetKind {
    File,
    Executable,
    Python,
    Module,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct TargetResolution {
    pub(crate) kind: TargetKind,
    pub(crate) resolved: String,
    pub(crate) argv: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct RuntimePlan {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) python_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) python_abi: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) runtime_oid: Option<String>,
    pub(crate) executable: String,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct LockProfilePlan {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) l_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) wl_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tool_lock_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) profile_oid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) env_id: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct SysPathSummary {
    pub(crate) first: Vec<String>,
    pub(crate) count: usize,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct SysPathPlan {
    pub(crate) entries: Vec<String>,
    pub(crate) summary: SysPathSummary,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct SandboxPlan {
    pub(crate) enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) sbx_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) base: Option<String>,
    pub(crate) capabilities: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ProvenancePlan {
    pub(crate) sandbox: SandboxPlan,
    pub(crate) source: SourceProvenance,
}

/// Shared internal planning payload used by `px run`/`px test` and `px explain`.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct ExecutionPlan {
    pub(crate) schema_version: u32,
    pub(crate) context: PlanContext,
    pub(crate) runtime: RuntimePlan,
    pub(crate) lock_profile: LockProfilePlan,
    pub(crate) engine: EnginePlan,
    pub(crate) target_resolution: TargetResolution,
    pub(crate) working_dir: String,
    pub(crate) sys_path: SysPathPlan,
    pub(crate) provenance: ProvenancePlan,
    pub(crate) would_repair_env: bool,
}
