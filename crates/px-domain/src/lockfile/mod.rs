#![allow(dead_code)]
#![deny(clippy::all, warnings)]
#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate
)]

pub(crate) mod analysis;
pub(crate) mod io;
pub(crate) mod spec;
pub(crate) mod types;

#[cfg(test)]
mod tests {
    use super::{analysis, io, spec, types};
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
            px_options: crate::project::manifest::PxOptions::default(),
            manifest_fingerprint: "demo-fingerprint".into(),
        }
    }

    fn resolved() -> Vec<types::ResolvedDependency> {
        vec![types::ResolvedDependency {
            name: "demo".into(),
            specifier: "demo==1.0.0".into(),
            extras: Vec::new(),
            marker: None,
            artifact: types::LockedArtifact {
                filename: "demo-1.0.0-py3-none-any.whl".into(),
                url: "https://example.invalid/demo.whl".into(),
                sha256: "deadbeef".into(),
                size: 4,
                cached_path: String::new(),
                python_tag: "py3".into(),
                abi_tag: "none".into(),
                platform_tag: "any".into(),
                is_direct_url: false,
                build_options_hash: String::new(),
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
        let toml = io::render_lockfile(&snapshot, &resolved(), "0.1.0")?;
        let parsed: types::LockSnapshot = io::parse_lock_snapshot(&toml.parse()?);
        assert_eq!(parsed.dependencies.len(), 1);
        assert_eq!(parsed.version, types::LOCK_VERSION);
        Ok(())
    }

    #[test]
    fn diff_reports_added_dependency() {
        let dir = tempdir().unwrap();
        let mut snapshot = sample_snapshot(dir.path());
        snapshot.dependencies.push("extra==2.0.0".into());
        snapshot.requirements.push("extra==2.0.0".into());
        let lock = types::LockSnapshot {
            version: types::LOCK_VERSION,
            project_name: Some("demo".into()),
            python_requirement: Some(">=3.11".into()),
            manifest_fingerprint: Some("demo-fingerprint".into()),
            lock_id: Some("lock-demo".into()),
            dependencies: vec!["demo==1.0.0".into()],
            mode: Some(types::LOCK_MODE_PINNED.into()),
            resolved: vec![types::LockedDependency {
                name: "demo".into(),
                direct: true,
                artifact: Some(types::LockedArtifact::default()),
                source: None,
                requires: Vec::new(),
            }],
            graph: None,
            workspace: None,
        };
        let report = analysis::analyze_lock_diff(&snapshot, &lock, None);
        assert!(!report.is_clean());
        assert_eq!(report.added.len(), 1);
    }

    #[test]
    fn verify_locked_artifacts_checks_hash() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let wheel = dir.path().join("demo.whl");
        std::fs::write(&wheel, b"demo")?;
        let lock = types::LockSnapshot {
            version: types::LOCK_VERSION,
            project_name: Some("demo".into()),
            python_requirement: Some(">=3.11".into()),
            manifest_fingerprint: Some("demo-fingerprint".into()),
            lock_id: Some("lock-demo".into()),
            dependencies: vec!["demo==1.0.0".into()],
            mode: Some(types::LOCK_MODE_PINNED.into()),
            resolved: vec![types::LockedDependency {
                name: "demo".into(),
                direct: true,
                artifact: Some(types::LockedArtifact {
                    filename: "demo.whl".into(),
                    url: "https://example.invalid/demo.whl".into(),
                    sha256: spec::compute_file_sha256(&wheel)?,
                    size: 4,
                    cached_path: wheel.display().to_string(),
                    python_tag: "py3".into(),
                    abi_tag: "none".into(),
                    platform_tag: "any".into(),
                    is_direct_url: false,
                    build_options_hash: String::new(),
                }),
                requires: Vec::new(),
                source: None,
            }],
            graph: None,
            workspace: None,
        };
        assert!(analysis::verify_locked_artifacts(&lock).is_empty());
        Ok(())
    }

    #[test]
    fn collect_resolved_dependencies_merges_metadata() {
        let lock = types::LockSnapshot {
            version: types::LOCK_VERSION,
            project_name: Some("demo".into()),
            python_requirement: Some(">=3.11".into()),
            manifest_fingerprint: Some("demo-fingerprint".into()),
            lock_id: Some("lock-demo".into()),
            dependencies: vec!["demo[a]==1.0.0 ; python_version >= '3.10'".into()],
            mode: Some(types::LOCK_MODE_PINNED.into()),
            resolved: vec![types::LockedDependency {
                name: "demo".into(),
                direct: true,
                artifact: Some(types::LockedArtifact::default()),
                requires: vec!["dep==1.2".into()],
                source: Some("test".into()),
            }],
            graph: None,
            workspace: None,
        };
        let deps = analysis::collect_resolved_dependencies(&lock);
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
        let lock = types::LockSnapshot {
            version: 2,
            project_name: Some("demo".into()),
            python_requirement: Some(">=3.11".into()),
            manifest_fingerprint: Some("demo-fingerprint".into()),
            lock_id: Some("lock-demo".into()),
            dependencies: Vec::new(),
            mode: Some(types::LOCK_MODE_PINNED.into()),
            resolved: vec![types::LockedDependency {
                name: "demo".into(),
                direct: true,
                artifact: Some(types::LockedArtifact::default()),
                requires: Vec::new(),
                source: None,
            }],
            graph: Some(types::LockGraphSnapshot {
                nodes: vec![types::GraphNode {
                    name: "demo".into(),
                    version: "1.0.0".into(),
                    marker: None,
                    parents: Vec::new(),
                    extras: Vec::new(),
                }],
                targets: vec![types::GraphTarget {
                    id: "py3-none-any".into(),
                    python_tag: "py3".into(),
                    abi_tag: "none".into(),
                    platform_tag: "any".into(),
                }],
                artifacts: vec![types::GraphArtifactEntry {
                    node: "demo".into(),
                    target: "py3-none-any".into(),
                    artifact: types::LockedArtifact::default(),
                }],
            }),
            workspace: None,
        };
        let toml = io::render_lockfile_v2(&snapshot, &lock, "0.2.0")?;
        let parsed: types::LockSnapshot = io::parse_lock_snapshot(&toml.parse::<DocumentMut>()?);
        assert_eq!(parsed.version, 2);
        assert!(parsed.graph.is_some());
        Ok(())
    }
}
