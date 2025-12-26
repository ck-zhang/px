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
    fn lockfile_is_deterministic_across_project_paths() -> anyhow::Result<()> {
        let dir_a = tempdir()?;
        let dir_b = tempdir()?;
        let pyproject = r#"[project]
name = "demo"
version = "0.1.0"
requires-python = ">=3.11"
dependencies = ["demo==1.0.0"]

[tool.px]

[build-system]
requires = ["setuptools>=70", "wheel"]
build-backend = "setuptools.build_meta"
"#;
        std::fs::write(dir_a.path().join("pyproject.toml"), pyproject)?;
        std::fs::write(dir_b.path().join("pyproject.toml"), pyproject)?;
        let snapshot_a = ProjectSnapshot::read_from(dir_a.path())?;
        let snapshot_b = ProjectSnapshot::read_from(dir_b.path())?;
        assert_eq!(
            snapshot_a.manifest_fingerprint,
            snapshot_b.manifest_fingerprint
        );

        let lock_a = io::render_lockfile(&snapshot_a, &resolved(), "0.1.0")?;
        let lock_b = io::render_lockfile(&snapshot_b, &resolved(), "0.1.0")?;

        assert_eq!(lock_a, lock_b);
        assert!(
            !lock_a.contains(&dir_a.path().display().to_string()),
            "lock should not include project root paths"
        );
        assert!(
            !lock_a.contains(&dir_b.path().display().to_string()),
            "lock should not include project root paths"
        );
        assert!(
            !lock_a.contains("created_at"),
            "lock should not include timestamps"
        );
        assert!(
            !lock_a.contains("cached_path"),
            "lock should not include local cache paths"
        );
        Ok(())
    }

    #[test]
    fn lockfile_rerender_ignores_cached_path_field() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let snapshot = sample_snapshot(dir.path());

        let lock_contents = |cached_path: &str| {
            format!(
                r#"version = 1

[metadata]
px_version = "0.1.0"
created_at = "2025-01-01T00:00:00Z"
mode = "p0-pinned"
manifest_fingerprint = "demo-fingerprint"
lock_id = "lock-demo"

[project]
name = "demo"

[python]
requirement = ">=3.11"

[[dependencies]]
name = "numpy"
specifier = "numpy==2.3.5"
direct = true

[dependencies.artifact]
filename = "numpy-2.3.5-py3-none-any.whl"
url = "https://example.invalid/numpy.whl"
sha256 = "deadbeef"
size = 1
cached_path = "{cached_path}"
"#
            )
        };

        let lock_a_path = "/tmp/numpy.whl";
        let lock_b_path = "/Users/alice/Library/Caches/numpy.whl";
        let parsed_a = io::parse_lockfile(&lock_contents(lock_a_path))?;
        let parsed_b = io::parse_lockfile(&lock_contents(lock_b_path))?;

        let rendered_a = io::render_lockfile(
            &snapshot,
            &analysis::collect_resolved_dependencies(&parsed_a),
            "0.1.0",
        )?;
        let rendered_b = io::render_lockfile(
            &snapshot,
            &analysis::collect_resolved_dependencies(&parsed_b),
            "0.1.0",
        )?;

        assert_eq!(rendered_a, rendered_b);
        assert!(
            !rendered_a.contains("cached_path"),
            "rerendered lock should not include cached_path"
        );
        assert!(
            !rendered_a.contains(lock_a_path),
            "rerendered lock should drop cached_path value"
        );
        assert!(
            !rendered_a.contains(lock_b_path),
            "rerendered lock should drop cached_path value"
        );
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
    fn verify_locked_artifacts_requires_metadata() -> anyhow::Result<()> {
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

    #[test]
    fn parse_lock_snapshot_backfills_wheel_tags_from_filename() {
        let doc: DocumentMut = r#"version = 1

[metadata]
px_version = "0.1.0"
created_at = "2025-01-01T00:00:00Z"
mode = "p0-pinned"
manifest_fingerprint = "demo-fingerprint"
lock_id = "lock-demo"

[project]
name = "demo"

[python]
requirement = ">=3.11"

[[dependencies]]
name = "numpy"
specifier = "numpy==2.3.5"
direct = true

[dependencies.artifact]
filename = "numpy-2.3.5-cp311-cp311-manylinux_2_27_x86_64.manylinux_2_28_x86_64.whl"
url = "https://example.invalid/numpy.whl"
sha256 = "deadbeef"
size = 1
cached_path = "/tmp/numpy.whl"
"#
        .parse()
        .expect("doc parses");

        let lock = io::parse_lock_snapshot(&doc);
        let dep = lock
            .resolved
            .iter()
            .find(|entry| entry.name == "numpy")
            .expect("numpy entry exists");
        let artifact = dep.artifact.as_ref().expect("artifact exists");
        assert_eq!(artifact.python_tag, "cp311");
        assert_eq!(artifact.abi_tag, "cp311");
        assert_eq!(
            artifact.platform_tag,
            "manylinux_2_27_x86_64.manylinux_2_28_x86_64"
        );
    }
}
