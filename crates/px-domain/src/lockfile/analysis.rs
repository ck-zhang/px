use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{bail, Result};
use pep508_rs::MarkerEnvironment;
use serde_json::Value;

use crate::project::snapshot::ProjectSnapshot;

use super::spec::{compute_file_sha256, dependency_name, spec_map, version_from_specifier};
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
    let manifest_map = spec_map(&snapshot.dependencies, marker_env);
    let direct_specs: Vec<String> = lock
        .resolved
        .iter()
        .zip(lock.dependencies.iter())
        .filter(|(entry, _)| entry.direct)
        .map(|(_, spec)| spec.clone())
        .collect();
    let lock_map = spec_map(&direct_specs, None);

    for (name, spec) in &manifest_map {
        match lock_map.get(name) {
            Some(lock_spec) => {
                if *lock_spec != *spec {
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
        if artifact.cached_path.is_empty() {
            issues.push(format!(
                "dependency `{}` missing cached_path in lock",
                dep.name
            ));
            continue;
        }
        let path = PathBuf::from(&artifact.cached_path);
        if !path.exists() {
            issues.push(format!(
                "artifact for `{}` missing at {}",
                dep.name,
                path.display()
            ));
            continue;
        }
        match compute_file_sha256(&path) {
            Ok(actual) if actual == artifact.sha256 => {}
            Ok(actual) => {
                issues.push(format!(
                    "artifact for `{}` has sha256 {} but lock expects {}",
                    dep.name, actual, artifact.sha256
                ));
                continue;
            }
            Err(err) => {
                issues.push(format!(
                    "unable to hash `{}` at {}: {}",
                    dep.name,
                    path.display(),
                    err
                ));
                continue;
            }
        }
        if let Ok(meta) = std::fs::metadata(&path) {
            if meta.len() != artifact.size {
                issues.push(format!(
                    "artifact for `{}` size mismatch (have {}, lock {})",
                    dep.name,
                    meta.len(),
                    artifact.size
                ));
            }
        }
    }
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
