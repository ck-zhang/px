use std::collections::{HashMap, HashSet};
use std::str::FromStr;

use anyhow::{bail, Result};
use pep440_rs::{Version, VersionSpecifiers};
use pep508_rs::{ExtraName, MarkerEnvironment, Requirement as PepRequirement, VersionOrUrl};
use serde_json::Value;

use crate::project::snapshot::ProjectSnapshot;

use super::spec::{dependency_name, spec_map, strip_wrapping_quotes, version_from_specifier};
use super::types::{
    LockPrefetchSpec, LockSnapshot, LockedArtifact, ResolvedDependency, LOCK_MODE_PINNED,
    LOCK_VERSION,
};

pub fn collect_resolved_dependencies(lock: &LockSnapshot) -> Vec<ResolvedDependency> {
    let mut deps = Vec::new();
    let mut spec_lookup = HashMap::new();
    for spec in &lock.dependencies {
        spec_lookup.insert(dependency_name(spec), spec.clone());
    }
    for entry in &lock.resolved {
        let specifier = spec_lookup
            .get(&entry.name)
            .cloned()
            .unwrap_or_else(|| entry.name.clone());
        let artifact = entry
            .artifact
            .clone()
            .unwrap_or_else(LockedArtifact::default);
        let (extras, marker) = super::spec::parse_spec_metadata(&specifier);
        deps.push(ResolvedDependency {
            name: entry.name.clone(),
            specifier,
            extras,
            marker,
            artifact,
            direct: entry.direct,
            requires: entry.requires.clone(),
            source: entry.source.clone(),
        });
    }
    deps.sort_by(|a, b| a.name.cmp(&b.name).then(a.specifier.cmp(&b.specifier)));
    deps
}

pub fn lock_prefetch_specs(lock: &LockSnapshot) -> Result<Vec<LockPrefetchSpec>> {
    let mut spec_lookup = HashMap::new();
    for spec in &lock.dependencies {
        spec_lookup.insert(dependency_name(spec), spec.clone());
    }

    let mut specs = Vec::new();
    for dep in &lock.resolved {
        let Some(artifact) = &dep.artifact else {
            continue;
        };
        let Some(specifier) = spec_lookup.get(&dep.name) else {
            bail!("lock entry `{}` missing from dependencies list", dep.name);
        };
        let Some(version) = version_from_specifier(specifier) else {
            bail!("lock entry `{}` is missing a pinned version", dep.name);
        };
        if artifact.filename.is_empty() || artifact.url.is_empty() || artifact.sha256.is_empty() {
            bail!("lock entry `{}` is missing artifact metadata", dep.name);
        }
        specs.push(LockPrefetchSpec {
            name: dep.name.clone(),
            version: version.to_string(),
            filename: artifact.filename.clone(),
            url: artifact.url.clone(),
            sha256: artifact.sha256.clone(),
        });
    }

    Ok(specs)
}

#[must_use]
pub fn analyze_lock_diff(
    snapshot: &ProjectSnapshot,
    lock: &LockSnapshot,
    marker_env: Option<&MarkerEnvironment>,
) -> LockDiffReport {
    let mut report = LockDiffReport::default();
    let manifest_map = spec_map(&snapshot.requirements, marker_env);
    let direct_specs: Vec<String> = lock
        .resolved
        .iter()
        .zip(lock.dependencies.iter())
        .filter(|(entry, _)| entry.direct)
        .map(|(_, spec)| spec.clone())
        .collect();
    let lock_map = spec_map(&direct_specs, marker_env);

    for (name, spec) in &manifest_map {
        match lock_map.get(name) {
            Some(lock_spec) => {
                if *lock_spec != *spec && !spec_satisfied(spec, lock_spec, marker_env) {
                    report.changed.push(ChangedEntry {
                        name: name.clone(),
                        from: (*lock_spec).clone(),
                        to: (*spec).clone(),
                    });
                }
            }
            None => {
                report.added.push(DiffEntry {
                    name: name.clone(),
                    specifier: (*spec).clone(),
                    source: "pyproject",
                });
            }
        }
    }

    for (name, spec) in &lock_map {
        if !manifest_map.contains_key(name) {
            report.removed.push(DiffEntry {
                name: name.clone(),
                specifier: (*spec).clone(),
                source: "px.lock",
            });
        }
    }

    match lock.project_name.as_deref() {
        Some(name) if name == snapshot.name => {}
        Some(name) => {
            report.project_mismatch = Some(ProjectMismatch {
                manifest: snapshot.name.clone(),
                lock: Some(name.to_string()),
            });
        }
        None => {
            report.project_mismatch = Some(ProjectMismatch {
                manifest: snapshot.name.clone(),
                lock: None,
            });
        }
    }

    match lock.python_requirement.as_ref() {
        Some(req) if req == &snapshot.python_requirement => {}
        Some(req) => {
            report.python_mismatch = Some(PythonMismatch {
                manifest: snapshot.python_requirement.clone(),
                lock: Some(req.clone()),
            });
        }
        None => {
            report.python_mismatch = Some(PythonMismatch {
                manifest: snapshot.python_requirement.clone(),
                lock: None,
            });
        }
    }

    if lock.version != LOCK_VERSION && lock.version != 2 {
        report.version_mismatch = Some(VersionMismatch {
            expected: LOCK_VERSION,
            found: lock.version,
        });
    }

    if lock.mode.as_deref() != Some(LOCK_MODE_PINNED) {
        report.mode_mismatch = Some(ModeMismatch {
            expected: LOCK_MODE_PINNED,
            found: lock.mode.clone(),
        });
    }

    report
}

fn spec_satisfied(
    manifest_spec: &str,
    lock_spec: &str,
    marker_env: Option<&MarkerEnvironment>,
) -> bool {
    let manifest_req = pep508_rs::Requirement::from_str(strip_wrapping_quotes(manifest_spec));
    let lock_req = pep508_rs::Requirement::from_str(strip_wrapping_quotes(lock_spec));
    let (Ok(manifest), Ok(lock)) = (manifest_req, lock_req) else {
        return false;
    };
    if !manifest
        .name
        .as_ref()
        .eq_ignore_ascii_case(lock.name.as_ref())
    {
        return false;
    }
    if let Some(env) = marker_env {
        if !manifest.evaluate_markers(env, &[]) {
            return true;
        }
    }
    let Some(VersionOrUrl::VersionSpecifier(specifiers)) = manifest.version_or_url.as_ref() else {
        return true;
    };
    let Ok(specs) = VersionSpecifiers::from_str(&specifiers.to_string()) else {
        return false;
    };
    let Some(lock_version) = super::spec::version_from_specifier(lock_spec)
        .and_then(|value| value.split(';').next().map(str::trim))
    else {
        return false;
    };
    let Ok(version) = Version::from_str(lock_version) else {
        return false;
    };
    specs.contains(&version)
}

pub fn detect_lock_drift(
    snapshot: &ProjectSnapshot,
    lock: &LockSnapshot,
    marker_env: Option<&MarkerEnvironment>,
) -> Vec<String> {
    analyze_lock_diff(snapshot, lock, marker_env).to_messages()
}

pub fn verify_locked_artifacts(lock: &LockSnapshot) -> Vec<String> {
    let mut issues = Vec::new();
    for dep in &lock.resolved {
        let Some(artifact) = &dep.artifact else {
            continue;
        };
        if artifact.filename.is_empty() {
            issues.push(format!(
                "dependency `{}` missing artifact filename in lock",
                dep.name
            ));
        }
        if artifact.url.is_empty() {
            issues.push(format!(
                "dependency `{}` missing artifact url in lock",
                dep.name
            ));
        }
        if artifact.sha256.is_empty() {
            issues.push(format!(
                "dependency `{}` missing artifact sha256 in lock",
                dep.name
            ));
        }
    }
    issues
}

/// Validate that the lockfile contains a complete transitive closure for the active marker env.
///
/// A lock is considered incomplete when any active locked dependency declares an active `requires`
/// entry whose name is missing from the active lock set.
#[must_use]
pub fn validate_lock_closure(
    lock: &LockSnapshot,
    marker_env: Option<&MarkerEnvironment>,
) -> Vec<String> {
    if lock.dependencies.is_empty() || lock.resolved.is_empty() {
        return Vec::new();
    }

    let mut spec_lookup = HashMap::new();
    for spec in &lock.dependencies {
        spec_lookup.insert(dependency_name(spec), spec.clone());
    }

    let mut active_names: HashSet<String> = HashSet::with_capacity(lock.resolved.len());
    let mut extras_lookup: HashMap<String, Vec<ExtraName>> =
        HashMap::with_capacity(lock.resolved.len());
    for dep in &lock.resolved {
        let canonical = dependency_name(&dep.name);
        if canonical.is_empty() {
            continue;
        }
        let specifier = spec_lookup
            .get(&canonical)
            .cloned()
            .unwrap_or_else(|| canonical.clone());
        if marker_env.is_some_and(|env| !super::spec::marker_applies(&specifier, env)) {
            continue;
        }
        active_names.insert(canonical.clone());
        let (extras, _) = super::spec::parse_spec_metadata(&specifier);
        extras_lookup.insert(
            canonical,
            extras
                .into_iter()
                .filter_map(|extra| ExtraName::from_str(&extra).ok())
                .collect(),
        );
    }

    let mut issues = Vec::new();
    for dep in &lock.resolved {
        let canonical = dependency_name(&dep.name);
        if canonical.is_empty() || !active_names.contains(&canonical) {
            continue;
        }
        let extras = extras_lookup.get(&canonical).cloned().unwrap_or_default();
        for req in &dep.requires {
            if let Some(env) = marker_env {
                if let Ok(requirement) = PepRequirement::from_str(strip_wrapping_quotes(req.trim()))
                {
                    if !requirement.evaluate_markers(env, &extras) {
                        continue;
                    }
                }
            }
            let required_name = dependency_name(req);
            if required_name.is_empty() || required_name == canonical {
                continue;
            }
            if !active_names.contains(&required_name) {
                issues.push(format!(
                    "px.lock missing transitive dependency `{required_name}` (required by `{}`)",
                    canonical
                ));
            }
        }
    }

    issues.sort();
    issues.dedup();
    issues
}

#[derive(Default)]
pub struct LockDiffReport {
    pub added: Vec<DiffEntry>,
    pub removed: Vec<DiffEntry>,
    pub changed: Vec<ChangedEntry>,
    pub python_mismatch: Option<PythonMismatch>,
    pub version_mismatch: Option<VersionMismatch>,
    pub mode_mismatch: Option<ModeMismatch>,
    pub project_mismatch: Option<ProjectMismatch>,
}

#[derive(Clone)]
pub struct DiffEntry {
    pub name: String,
    pub specifier: String,
    pub source: &'static str,
}

#[derive(Clone)]
pub struct ChangedEntry {
    pub name: String,
    pub from: String,
    pub to: String,
}

pub struct PythonMismatch {
    pub manifest: String,
    pub lock: Option<String>,
}

pub struct VersionMismatch {
    pub expected: i64,
    pub found: i64,
}

pub struct ModeMismatch {
    pub expected: &'static str,
    pub found: Option<String>,
}

pub struct ProjectMismatch {
    pub manifest: String,
    pub lock: Option<String>,
}

impl LockDiffReport {
    pub fn is_clean(&self) -> bool {
        self.added.is_empty()
            && self.removed.is_empty()
            && self.changed.is_empty()
            && self.python_mismatch.is_none()
            && self.version_mismatch.is_none()
            && self.mode_mismatch.is_none()
    }

    pub fn summary(&self) -> String {
        if self.is_clean() {
            return "clean".to_string();
        }
        let mut chunks = Vec::new();
        if !self.added.is_empty() {
            chunks.push(format!("{} added", self.added.len()));
        }
        if !self.removed.is_empty() {
            chunks.push(format!("{} removed", self.removed.len()));
        }
        if !self.changed.is_empty() {
            chunks.push(format!("{} changed", self.changed.len()));
        }
        if self.python_mismatch.is_some() {
            chunks.push("python mismatch".to_string());
        }
        if self.version_mismatch.is_some() {
            chunks.push("lock version mismatch".to_string());
        }
        if self.mode_mismatch.is_some() {
            chunks.push("mode mismatch".to_string());
        }
        chunks.join(", ")
    }

    pub fn to_json(&self, snapshot: &ProjectSnapshot) -> Value {
        serde_json::json!({
            "status": if self.is_clean() { "clean" } else { "drift" },
            "pyproject": snapshot.manifest_path.display().to_string(),
            "lockfile": snapshot.lock_path.display().to_string(),
            "added": self
                .added
                .iter()
                .map(|entry| serde_json::json!({
                    "name": entry.name,
                    "specifier": entry.specifier,
                    "source": entry.source,
                }))
                .collect::<Vec<_>>(),
            "removed": self
                .removed
                .iter()
                .map(|entry| serde_json::json!({
                    "name": entry.name,
                    "specifier": entry.specifier,
                    "source": entry.source,
                }))
                .collect::<Vec<_>>(),
            "changed": self
                .changed
                .iter()
                .map(|entry| serde_json::json!({
                    "name": entry.name,
                    "from": entry.from,
                    "to": entry.to,
                }))
                .collect::<Vec<_>>(),
            "python_mismatch": self.python_mismatch.as_ref().map(|m| serde_json::json!({
                "manifest": m.manifest,
                "lock": m.lock,
            })),
            "version_mismatch": self.version_mismatch.as_ref().map(|m| serde_json::json!({
                "expected": m.expected,
                "found": m.found,
            })),
            "mode_mismatch": self.mode_mismatch.as_ref().map(|m| serde_json::json!({
                "expected": m.expected,
                "found": m.found,
            })),
            "project_mismatch": self.project_mismatch.as_ref().map(|m| serde_json::json!({
                "manifest": m.manifest,
                "lock": m.lock,
            })),
        })
    }

    pub fn to_messages(&self) -> Vec<String> {
        let mut messages = Vec::new();
        for entry in &self.added {
            messages.push(format!("added {} ({})", entry.name, entry.specifier));
        }
        for entry in &self.removed {
            messages.push(format!("removed {} ({})", entry.name, entry.specifier));
        }
        for entry in &self.changed {
            messages.push(format!(
                "updated {} ({} → {})",
                entry.name, entry.from, entry.to
            ));
        }
        if let Some(python) = &self.python_mismatch {
            messages.push(format!(
                "python requirement mismatch (pyproject {}, lock {:?})",
                python.manifest, python.lock
            ));
        }
        if let Some(version) = &self.version_mismatch {
            messages.push(format!(
                "lockfile version mismatch (expected {}, found {})",
                version.expected, version.found
            ));
        }
        if let Some(mode) = &self.mode_mismatch {
            messages.push(format!(
                "lockfile mode mismatch (expected {}, found {:?})",
                mode.expected, mode.found
            ));
        }
        if let Some(project) = &self.project_mismatch {
            messages.push(format!(
                "project name mismatch (pyproject {}, lock {:?})",
                project.manifest, project.lock
            ));
        }
        messages
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use crate::api::{DependencyGroupSource, ProjectSnapshot};
    use crate::lockfile::types::{LockSnapshot, LockedDependency, LOCK_MODE_PINNED, LOCK_VERSION};

    #[test]
    fn drift_detects_missing_dependency_group_entries() {
        let snapshot = ProjectSnapshot {
            root: PathBuf::from("/proj"),
            manifest_path: PathBuf::from("/proj/pyproject.toml"),
            lock_path: PathBuf::from("/proj/px.lock"),
            name: "demo".into(),
            python_requirement: ">=3.11".into(),
            dependencies: Vec::new(),
            dependency_groups: vec!["dev".to_string()],
            declared_dependency_groups: vec!["dev".to_string()],
            dependency_group_source: DependencyGroupSource::DeclaredDefault,
            group_dependencies: vec!["pytest==8.3.3".to_string()],
            requirements: vec!["pytest==8.3.3".to_string()],
            python_override: None,
            px_options: crate::project::manifest::PxOptions::default(),
            manifest_fingerprint: "mf".into(),
        };
        let lock = LockSnapshot {
            version: LOCK_VERSION,
            project_name: Some("demo".into()),
            python_requirement: Some(">=3.11".into()),
            manifest_fingerprint: Some("mf".into()),
            lock_id: Some("lock-demo".into()),
            dependencies: Vec::new(),
            mode: Some(LOCK_MODE_PINNED.into()),
            resolved: Vec::new(),
            graph: None,
            workspace: None,
        };

        let drift = detect_lock_drift(&snapshot, &lock, None);
        assert!(
            drift
                .iter()
                .any(|entry| entry.contains("pytest") || entry.contains("added")),
            "dependency group entries should participate in lock drift detection"
        );
    }

    #[test]
    fn closure_reports_missing_transitive_dependency() {
        let lock = LockSnapshot {
            version: LOCK_VERSION,
            project_name: Some("demo".into()),
            python_requirement: Some(">=3.11".into()),
            manifest_fingerprint: Some("mf".into()),
            lock_id: Some("lock-demo".into()),
            dependencies: vec!["requests==1.0.0".to_string()],
            mode: Some(LOCK_MODE_PINNED.into()),
            resolved: vec![LockedDependency {
                name: "requests".into(),
                direct: true,
                artifact: None,
                requires: vec!["urllib3>=2".to_string()],
                source: None,
            }],
            graph: None,
            workspace: None,
        };

        let issues = validate_lock_closure(&lock, None);
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("urllib3"));
        assert!(issues[0].contains("requests"));
    }

    #[test]
    fn closure_accepts_complete_lock() {
        let lock = LockSnapshot {
            version: LOCK_VERSION,
            project_name: Some("demo".into()),
            python_requirement: Some(">=3.11".into()),
            manifest_fingerprint: Some("mf".into()),
            lock_id: Some("lock-demo".into()),
            dependencies: vec!["requests==1.0.0".to_string(), "urllib3==2.0.0".to_string()],
            mode: Some(LOCK_MODE_PINNED.into()),
            resolved: vec![
                LockedDependency {
                    name: "requests".into(),
                    direct: true,
                    artifact: None,
                    requires: vec!["urllib3>=2".to_string()],
                    source: None,
                },
                LockedDependency {
                    name: "urllib3".into(),
                    direct: false,
                    artifact: None,
                    requires: Vec::new(),
                    source: None,
                },
            ],
            graph: None,
            workspace: None,
        };

        let issues = validate_lock_closure(&lock, None);
        assert!(issues.is_empty(), "unexpected closure issues: {issues:?}");
    }

    #[test]
    fn closure_normalizes_names_for_matching() {
        let lock = LockSnapshot {
            version: LOCK_VERSION,
            project_name: Some("demo".into()),
            python_requirement: Some(">=3.11".into()),
            manifest_fingerprint: Some("mf".into()),
            lock_id: Some("lock-demo".into()),
            dependencies: vec!["requests==1.0.0".to_string(), "PySocks==1.7.1".to_string()],
            mode: Some(LOCK_MODE_PINNED.into()),
            resolved: vec![
                LockedDependency {
                    name: "requests".into(),
                    direct: true,
                    artifact: None,
                    requires: vec!["pysocks".to_string()],
                    source: None,
                },
                LockedDependency {
                    name: "PySocks".into(),
                    direct: false,
                    artifact: None,
                    requires: Vec::new(),
                    source: None,
                },
            ],
            graph: None,
            workspace: None,
        };

        let issues = validate_lock_closure(&lock, None);
        assert!(issues.is_empty(), "unexpected closure issues: {issues:?}");
    }
}
