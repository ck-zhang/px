#![allow(dead_code)]
#![deny(clippy::all, warnings)]
#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate
)]

mod analysis;
mod io;
pub mod spec;
pub mod types;

pub use analysis::{
    analyze_lock_diff, collect_resolved_dependencies, detect_lock_drift, lock_prefetch_specs,
    verify_locked_artifacts, ChangedEntry, DiffEntry, LockDiffReport, ModeMismatch,
    ProjectMismatch, PythonMismatch, VersionMismatch,
};
pub use io::{
    load_lockfile, load_lockfile_optional, render_lockfile, render_lockfile_v2,
    render_lockfile_with_workspace,
};
pub use spec::{canonical_extras, format_specifier};
pub use types::{
    GraphArtifactEntry, GraphNode, GraphTarget, LockGraphSnapshot, LockPrefetchSpec, LockSnapshot,
    LockedArtifact, LockedDependency, ResolvedDependency, WorkspaceLock, WorkspaceMember,
    WorkspaceOwner, LOCK_MODE_PINNED, LOCK_VERSION,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::{manifest::DependencyGroupSource, snapshot::ProjectSnapshot};
    use tempfile::tempdir;
    use toml_edit::DocumentMut;

    fn sample_snapshot(root: &std::path::Path) -> ProjectSnapshot {
        ProjectSnapshot {
            root: root.to_path_buf(),
            manifest_path: root.join("pyproject.toml"),
            lock_path: root.join("px.lock"),
            name: "demo".into(),
            python_requirement: ">=3.11".into(),
            dependencies: vec!["demo==1.0.0".into()],
            dependency_groups: Vec::new(),
            declared_dependency_groups: Vec::new(),
            dependency_group_source: DependencyGroupSource::None,
            group_dependencies: Vec::new(),
            requirements: vec!["demo==1.0.0".into()],
            python_override: None,
            manifest_fingerprint: "demo-fingerprint".into(),
        }
    }

    fn resolved() -> Vec<ResolvedDependency> {
        vec![ResolvedDependency {
            name: "demo".into(),
            specifier: "demo==1.0.0".into(),
            extras: Vec::new(),
            marker: None,
            artifact: LockedArtifact {
                filename: "demo-1.0.0-py3-none-any.whl".into(),
                url: "https://example.invalid/demo.whl".into(),
                sha256: "deadbeef".into(),
                size: 4,
                cached_path: String::new(),
                python_tag: "py3".into(),
                abi_tag: "none".into(),
                platform_tag: "any".into(),
                is_direct_url: false,
            },
            direct: true,
            requires: Vec::new(),
            source: None,
        }]
    }

    #[test]
    fn renders_and_loads_lockfile() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let snapshot = sample_snapshot(dir.path());
        let toml = render_lockfile(&snapshot, &resolved(), "0.1.0")?;
        let parsed: LockSnapshot = io::parse_lock_snapshot(&toml.parse()?);
        assert_eq!(parsed.dependencies.len(), 1);
        assert_eq!(parsed.version, LOCK_VERSION);
        Ok(())
    }

    #[test]
    fn diff_reports_added_dependency() {
        let dir = tempdir().unwrap();
        let mut snapshot = sample_snapshot(dir.path());
        snapshot.dependencies.push("extra==2.0.0".into());
        snapshot.requirements.push("extra==2.0.0".into());
        let lock = LockSnapshot {
            version: LOCK_VERSION,
            project_name: Some("demo".into()),
            python_requirement: Some(">=3.11".into()),
            manifest_fingerprint: Some("demo-fingerprint".into()),
            lock_id: Some("lock-demo".into()),
            dependencies: vec!["demo==1.0.0".into()],
            mode: Some(LOCK_MODE_PINNED.into()),
            resolved: vec![LockedDependency {
                name: "demo".into(),
                direct: true,
                artifact: Some(LockedArtifact::default()),
                source: None,
                requires: Vec::new(),
            }],
            graph: None,
            workspace: None,
        };
        let report = analyze_lock_diff(&snapshot, &lock, None);
        assert!(!report.is_clean());
        assert_eq!(report.added.len(), 1);
    }

    #[test]
    fn verify_locked_artifacts_checks_hash() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let wheel = dir.path().join("demo.whl");
        std::fs::write(&wheel, b"demo")?;
        let lock = LockSnapshot {
            version: LOCK_VERSION,
            project_name: Some("demo".into()),
            python_requirement: Some(">=3.11".into()),
            manifest_fingerprint: Some("demo-fingerprint".into()),
            lock_id: Some("lock-demo".into()),
            dependencies: vec!["demo==1.0.0".into()],
            mode: Some(LOCK_MODE_PINNED.into()),
            resolved: vec![LockedDependency {
                name: "demo".into(),
                direct: true,
                artifact: Some(LockedArtifact {
                    filename: "demo.whl".into(),
                    url: "https://example.invalid/demo.whl".into(),
                    sha256: spec::compute_file_sha256(&wheel)?,
                    size: 4,
                    cached_path: wheel.display().to_string(),
                    python_tag: "py3".into(),
                    abi_tag: "none".into(),
                    platform_tag: "any".into(),
                    is_direct_url: false,
                }),
                requires: Vec::new(),
                source: None,
            }],
            graph: None,
            workspace: None,
        };
        assert!(verify_locked_artifacts(&lock).is_empty());
        Ok(())
    }

    #[test]
    fn collect_resolved_dependencies_merges_metadata() {
        let lock = LockSnapshot {
            version: LOCK_VERSION,
            project_name: Some("demo".into()),
            python_requirement: Some(">=3.11".into()),
            manifest_fingerprint: Some("demo-fingerprint".into()),
            lock_id: Some("lock-demo".into()),
            dependencies: vec!["demo[a]==1.0.0 ; python_version >= '3.10'".into()],
            mode: Some(LOCK_MODE_PINNED.into()),
            resolved: vec![LockedDependency {
                name: "demo".into(),
                direct: true,
                artifact: Some(LockedArtifact::default()),
                requires: vec!["dep==1.2".into()],
                source: Some("test".into()),
            }],
            graph: None,
            workspace: None,
        };
        let deps = collect_resolved_dependencies(&lock);
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].extras, vec!["a"]);
        assert!(deps[0]
            .marker
            .as_deref()
            .is_some_and(|m| m.contains("python_version")));
        assert_eq!(deps[0].source.as_deref(), Some("test"));
        assert_eq!(deps[0].requires, vec!["dep==1.2"]);
    }

    #[test]
    fn render_lockfile_v2_emits_graph() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let snapshot = sample_snapshot(dir.path());
        let lock = LockSnapshot {
            version: 2,
            project_name: Some("demo".into()),
            python_requirement: Some(">=3.11".into()),
            manifest_fingerprint: Some("demo-fingerprint".into()),
            lock_id: Some("lock-demo".into()),
            dependencies: Vec::new(),
            mode: Some(LOCK_MODE_PINNED.into()),
            resolved: vec![LockedDependency {
                name: "demo".into(),
                direct: true,
                artifact: Some(LockedArtifact::default()),
                requires: Vec::new(),
                source: None,
            }],
            graph: Some(LockGraphSnapshot {
                nodes: vec![GraphNode {
                    name: "demo".into(),
                    version: "1.0.0".into(),
                    marker: None,
                    parents: Vec::new(),
                    extras: Vec::new(),
                }],
                targets: vec![GraphTarget {
                    id: "py3-none-any".into(),
                    python_tag: "py3".into(),
                    abi_tag: "none".into(),
                    platform_tag: "any".into(),
                }],
                artifacts: vec![GraphArtifactEntry {
                    node: "demo".into(),
                    target: "py3-none-any".into(),
                    artifact: LockedArtifact::default(),
                }],
            }),
            workspace: None,
        };
        let toml = render_lockfile_v2(&snapshot, &lock, "0.2.0")?;
        let parsed: LockSnapshot = io::parse_lock_snapshot(&toml.parse::<DocumentMut>()?);
        assert_eq!(parsed.version, 2);
        assert!(parsed.graph.is_some());
        Ok(())
    }
}
