use serde::Serialize;

pub const LOCK_VERSION: i64 = 1;
pub const LOCK_MODE_PINNED: &str = "p0-pinned";

#[derive(Clone, Debug, Default, Serialize)]
pub struct LockedArtifact {
    pub filename: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
    pub cached_path: String,
    pub python_tag: String,
    pub abi_tag: String,
    pub platform_tag: String,
    #[serde(default)]
    pub is_direct_url: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub build_options_hash: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct LockedDependency {
    pub name: String,
    pub direct: bool,
    pub artifact: Option<LockedArtifact>,
    pub requires: Vec<String>,
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct LockSnapshot {
    pub version: i64,
    pub project_name: Option<String>,
    pub python_requirement: Option<String>,
    pub manifest_fingerprint: Option<String>,
    pub lock_id: Option<String>,
    pub dependencies: Vec<String>,
    pub mode: Option<String>,
    pub resolved: Vec<LockedDependency>,
    pub graph: Option<LockGraphSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceLock>,
}

#[derive(Clone, Debug)]
pub struct ResolvedDependency {
    pub name: String,
    pub specifier: String,
    pub extras: Vec<String>,
    pub marker: Option<String>,
    pub artifact: LockedArtifact,
    pub direct: bool,
    pub requires: Vec<String>,
    pub source: Option<String>,
}

#[derive(Clone, Debug)]
pub struct LockPrefetchSpec {
    pub name: String,
    pub version: String,
    pub filename: String,
    pub url: String,
    pub sha256: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct LockGraphSnapshot {
    pub nodes: Vec<GraphNode>,
    pub targets: Vec<GraphTarget>,
    pub artifacts: Vec<GraphArtifactEntry>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct GraphNode {
    pub name: String,
    pub version: String,
    pub marker: Option<String>,
    pub parents: Vec<String>,
    pub extras: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct GraphTarget {
    pub id: String,
    pub python_tag: String,
    pub abi_tag: String,
    pub platform_tag: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct GraphArtifactEntry {
    pub node: String,
    pub target: String,
    pub artifact: LockedArtifact,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct WorkspaceLock {
    pub members: Vec<WorkspaceMember>,
    pub owners: Vec<WorkspaceOwner>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct WorkspaceMember {
    pub name: String,
    pub path: String,
    pub manifest_fingerprint: String,
    pub dependencies: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct WorkspaceOwner {
    pub name: String,
    pub owners: Vec<String>,
}
