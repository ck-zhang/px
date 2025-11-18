#![allow(dead_code)]

use std::{
    collections::HashMap,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    str::FromStr,
};

use anyhow::{bail, Result};
use pep508_rs::{MarkerEnvironment, Requirement as PepRequirement};
use px_project::ProjectSnapshot;
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, Value as TomlValue};

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
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct LockedDependency {
    pub name: String,
    pub artifact: Option<LockedArtifact>,
}

#[derive(Clone, Debug, Serialize)]
pub struct LockSnapshot {
    pub version: i64,
    pub project_name: Option<String>,
    pub python_requirement: Option<String>,
    pub dependencies: Vec<String>,
    pub mode: Option<String>,
    pub resolved: Vec<LockedDependency>,
    pub graph: Option<LockGraphSnapshot>,
}

#[derive(Clone, Debug)]
pub struct ResolvedDependency {
    pub name: String,
    pub specifier: String,
    pub extras: Vec<String>,
    pub marker: Option<String>,
    pub artifact: LockedArtifact,
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

pub fn load_lockfile(path: &Path) -> Result<LockSnapshot> {
    let contents = fs::read_to_string(path)?;
    let doc: DocumentMut = contents.parse()?;
    Ok(parse_lock_snapshot(&doc))
}

pub fn load_lockfile_optional(path: &Path) -> Result<Option<LockSnapshot>> {
    if path.exists() {
        Ok(Some(load_lockfile(path)?))
    } else {
        Ok(None)
    }
}

pub fn render_lockfile(
    snapshot: &ProjectSnapshot,
    resolved: &[ResolvedDependency],
    px_version: &str,
) -> Result<String> {
    let mut doc = DocumentMut::new();
    doc.insert("version", Item::Value(TomlValue::from(LOCK_VERSION)));

    let mut metadata = Table::new();
    metadata.insert("px_version", Item::Value(TomlValue::from(px_version)));
    metadata.insert(
        "created_at",
        Item::Value(TomlValue::from(current_timestamp()?)),
    );
    metadata.insert("mode", Item::Value(TomlValue::from(LOCK_MODE_PINNED)));
    doc.insert("metadata", Item::Table(metadata));

    let mut project = Table::new();
    project.insert("name", Item::Value(TomlValue::from(snapshot.name.clone())));
    doc.insert("project", Item::Table(project));

    let mut python = Table::new();
    python.insert(
        "requirement",
        Item::Value(TomlValue::from(snapshot.python_requirement.clone())),
    );
    doc.insert("python", Item::Table(python));

    let mut ordered = resolved.to_vec();
    ordered.sort_by(|a, b| a.name.cmp(&b.name).then(a.specifier.cmp(&b.specifier)));
    let mut deps = ArrayOfTables::new();
    for dep in ordered {
        let mut table = Table::new();
        table.insert("name", Item::Value(TomlValue::from(dep.name.clone())));
        table.insert(
            "specifier",
            Item::Value(TomlValue::from(dep.specifier.clone())),
        );
        if !dep.extras.is_empty() {
            let mut extras = Array::new();
            for extra in &dep.extras {
                extras.push(TomlValue::from(extra.as_str()));
            }
            table.insert("extras", Item::Value(TomlValue::Array(extras)));
        }
        if let Some(marker) = &dep.marker {
            table.insert("marker", Item::Value(TomlValue::from(marker.clone())));
        }
        table.insert("artifact", Item::Table(render_artifact(&dep.artifact)));
        deps.push(table);
    }
    doc.insert("dependencies", Item::ArrayOfTables(deps));

    Ok(doc.to_string())
}

pub fn render_lockfile_v2(
    snapshot: &ProjectSnapshot,
    lock: &LockSnapshot,
    px_version: &str,
) -> Result<String> {
    let mut doc = DocumentMut::new();
    doc.insert("version", Item::Value(TomlValue::from(2)));

    let mut metadata = Table::new();
    metadata.insert("px_version", Item::Value(TomlValue::from(px_version)));
    metadata.insert(
        "created_at",
        Item::Value(TomlValue::from(current_timestamp()?)),
    );
    metadata.insert("mode", Item::Value(TomlValue::from(LOCK_MODE_PINNED)));
    doc.insert("metadata", Item::Table(metadata));

    let mut project = Table::new();
    project.insert("name", Item::Value(TomlValue::from(snapshot.name.clone())));
    doc.insert("project", Item::Table(project));

    let mut python = Table::new();
    python.insert(
        "requirement",
        Item::Value(TomlValue::from(snapshot.python_requirement.clone())),
    );
    doc.insert("python", Item::Table(python));

    let resolved = collect_resolved_dependencies(lock);
    let mut deps = ArrayOfTables::new();
    for dep in &resolved {
        let mut table = Table::new();
        table.insert("name", Item::Value(TomlValue::from(dep.name.clone())));
        table.insert(
            "specifier",
            Item::Value(TomlValue::from(dep.specifier.clone())),
        );
        table.insert("artifact", Item::Table(render_artifact(&dep.artifact)));
        deps.push(table);
    }
    doc.insert("dependencies", Item::ArrayOfTables(deps));

    if let Some(graph) = &lock.graph {
        doc.insert("graph", Item::Table(render_graph(graph)));
    }

    Ok(doc.to_string())
}

fn render_artifact(artifact: &LockedArtifact) -> Table {
    let mut table = Table::new();
    table.insert(
        "filename",
        Item::Value(TomlValue::from(artifact.filename.clone())),
    );
    table.insert("url", Item::Value(TomlValue::from(artifact.url.clone())));
    table.insert(
        "sha256",
        Item::Value(TomlValue::from(artifact.sha256.clone())),
    );
    table.insert("size", Item::Value(TomlValue::from(artifact.size as i64)));
    table.insert(
        "cached_path",
        Item::Value(TomlValue::from(artifact.cached_path.clone())),
    );
    table.insert(
        "python_tag",
        Item::Value(TomlValue::from(artifact.python_tag.clone())),
    );
    table.insert(
        "abi_tag",
        Item::Value(TomlValue::from(artifact.abi_tag.clone())),
    );
    table.insert(
        "platform_tag",
        Item::Value(TomlValue::from(artifact.platform_tag.clone())),
    );
    table
}

fn render_graph(graph: &LockGraphSnapshot) -> Table {
    let mut table = Table::new();
    if !graph.nodes.is_empty() {
        let mut node_tables = ArrayOfTables::new();
        for node in &graph.nodes {
            let mut entry = Table::new();
            entry.insert("name", Item::Value(TomlValue::from(node.name.clone())));
            entry.insert(
                "version",
                Item::Value(TomlValue::from(node.version.clone())),
            );
            if let Some(marker) = &node.marker {
                entry.insert("marker", Item::Value(TomlValue::from(marker.clone())));
            }
            if !node.extras.is_empty() {
                let mut extras = Array::new();
                for extra in &node.extras {
                    extras.push(TomlValue::from(extra.as_str()));
                }
                entry.insert("extras", Item::Value(TomlValue::Array(extras)));
            }
            if !node.parents.is_empty() {
                let mut parents = Array::new();
                for parent in &node.parents {
                    parents.push(TomlValue::from(parent.as_str()));
                }
                entry.insert("parents", Item::Value(TomlValue::Array(parents)));
            }
            node_tables.push(entry);
        }
        table.insert("nodes", Item::ArrayOfTables(node_tables));
    }

    if !graph.targets.is_empty() {
        let mut target_tables = ArrayOfTables::new();
        for target in &graph.targets {
            let mut entry = Table::new();
            entry.insert("id", Item::Value(TomlValue::from(target.id.clone())));
            entry.insert(
                "python_tag",
                Item::Value(TomlValue::from(target.python_tag.clone())),
            );
            entry.insert(
                "abi_tag",
                Item::Value(TomlValue::from(target.abi_tag.clone())),
            );
            entry.insert(
                "platform_tag",
                Item::Value(TomlValue::from(target.platform_tag.clone())),
            );
            target_tables.push(entry);
        }
        table.insert("targets", Item::ArrayOfTables(target_tables));
    }

    if !graph.artifacts.is_empty() {
        let mut artifact_tables = ArrayOfTables::new();
        for entry in &graph.artifacts {
            let mut artifact = render_artifact(&entry.artifact);
            artifact.insert("node", Item::Value(TomlValue::from(entry.node.clone())));
            artifact.insert("target", Item::Value(TomlValue::from(entry.target.clone())));
            artifact_tables.push(artifact);
        }
        table.insert("artifacts", Item::ArrayOfTables(artifact_tables));
    }

    table
}

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
        let (extras, marker) = parse_spec_metadata(&specifier);
        deps.push(ResolvedDependency {
            name: entry.name.clone(),
            specifier,
            extras,
            marker,
            artifact,
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

pub fn analyze_lock_diff(
    snapshot: &ProjectSnapshot,
    lock: &LockSnapshot,
    marker_env: Option<&MarkerEnvironment>,
) -> LockDiffReport {
    let mut report = LockDiffReport::default();
    let manifest_map = spec_map(&snapshot.dependencies, marker_env);
    let lock_map = spec_map(&lock.dependencies, None);

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
            })
        }
        None => {
            report.project_mismatch = Some(ProjectMismatch {
                manifest: snapshot.name.clone(),
                lock: None,
            })
        }
    }

    match lock.python_requirement.as_ref() {
        Some(req) if req == &snapshot.python_requirement => {}
        Some(req) => {
            report.python_mismatch = Some(PythonMismatch {
                manifest: snapshot.python_requirement.clone(),
                lock: Some(req.clone()),
            })
        }
        None => {
            report.python_mismatch = Some(PythonMismatch {
                manifest: snapshot.python_requirement.clone(),
                lock: None,
            })
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
        if let Ok(meta) = fs::metadata(&path) {
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
        json!({
            "status": if self.is_clean() { "clean" } else { "drift" },
            "pyproject": snapshot.manifest_path.display().to_string(),
            "lockfile": snapshot.lock_path.display().to_string(),
            "added": self
                .added
                .iter()
                .map(|entry| json!({
                    "name": entry.name,
                    "specifier": entry.specifier,
                    "source": entry.source,
                }))
                .collect::<Vec<_>>(),
            "removed": self
                .removed
                .iter()
                .map(|entry| json!({
                    "name": entry.name,
                    "specifier": entry.specifier,
                    "source": entry.source,
                }))
                .collect::<Vec<_>>(),
            "changed": self
                .changed
                .iter()
                .map(|entry| json!({
                    "name": entry.name,
                    "from": entry.from,
                    "to": entry.to,
                }))
                .collect::<Vec<_>>(),
            "python_mismatch": self.python_mismatch.as_ref().map(|m| json!({
                "manifest": m.manifest,
                "lock": m.lock,
            })),
            "version_mismatch": self.version_mismatch.as_ref().map(|m| json!({
                "expected": m.expected,
                "found": m.found,
            })),
            "mode_mismatch": self.mode_mismatch.as_ref().map(|m| json!({
                "expected": m.expected,
                "found": m.found,
            })),
            "project_mismatch": self.project_mismatch.as_ref().map(|m| json!({
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

fn parse_lock_snapshot(doc: &DocumentMut) -> LockSnapshot {
    let version = doc.get("version").and_then(Item::as_integer).unwrap_or(0);
    let project_name = doc
        .get("project")
        .and_then(Item::as_table)
        .and_then(|table| table.get("name"))
        .and_then(Item::as_str)
        .map(|s| s.to_string());
    let python_requirement = doc
        .get("python")
        .and_then(Item::as_table)
        .and_then(|table| table.get("requirement"))
        .and_then(Item::as_str)
        .map(|s| s.to_string());
    let mode = doc
        .get("metadata")
        .and_then(Item::as_table)
        .and_then(|table| table.get("mode"))
        .and_then(Item::as_str)
        .map(|s| s.to_string());

    if version >= 2 {
        if let Some(graph) = parse_graph_snapshot(doc) {
            let (dependencies, resolved) = normalized_from_graph(&graph);
            return LockSnapshot {
                version,
                project_name,
                python_requirement,
                dependencies,
                mode,
                resolved,
                graph: Some(graph),
            };
        }
    }

    let mut dependencies = Vec::new();
    let mut resolved = Vec::new();
    if let Some(tables) = doc.get("dependencies").and_then(Item::as_array_of_tables) {
        for table in tables.iter() {
            let specifier = table
                .get("specifier")
                .and_then(Item::as_str)
                .map(|s| s.to_string())
                .unwrap_or_default();
            if !specifier.is_empty() {
                dependencies.push(specifier.clone());
            }
            let name = table
                .get("name")
                .and_then(Item::as_str)
                .map(|s| s.to_string())
                .unwrap_or_else(|| dependency_name(&specifier));
            let artifact = table
                .get("artifact")
                .and_then(Item::as_table)
                .and_then(parse_artifact_table);
            resolved.push(LockedDependency { name, artifact });
        }
    } else if let Some(array) = doc.get("dependencies").and_then(Item::as_array) {
        dependencies = array
            .iter()
            .filter_map(|val| val.as_str().map(|s| s.to_string()))
            .collect();
    }

    LockSnapshot {
        version,
        project_name,
        python_requirement,
        dependencies,
        mode,
        resolved,
        graph: None,
    }
}

fn parse_artifact_table(table: &Table) -> Option<LockedArtifact> {
    let filename = table.get("filename").and_then(Item::as_str)?.to_string();
    let url = table.get("url").and_then(Item::as_str)?.to_string();
    let sha256 = table.get("sha256").and_then(Item::as_str)?.to_string();
    let size = table
        .get("size")
        .and_then(Item::as_integer)
        .map(|v| v as u64)
        .unwrap_or(0);
    let cached_path = table
        .get("cached_path")
        .and_then(Item::as_str)
        .unwrap_or_default()
        .to_string();
    let python_tag = table
        .get("python_tag")
        .and_then(Item::as_str)
        .unwrap_or_default()
        .to_string();
    let abi_tag = table
        .get("abi_tag")
        .and_then(Item::as_str)
        .unwrap_or_default()
        .to_string();
    let platform_tag = table
        .get("platform_tag")
        .and_then(Item::as_str)
        .unwrap_or_default()
        .to_string();

    Some(LockedArtifact {
        filename,
        url,
        sha256,
        size,
        cached_path,
        python_tag,
        abi_tag,
        platform_tag,
    })
}

fn parse_graph_snapshot(doc: &DocumentMut) -> Option<LockGraphSnapshot> {
    let graph = doc.get("graph")?.as_table()?;
    let node_tables = graph.get("nodes")?.as_array_of_tables()?;
    let mut nodes = Vec::new();
    for table in node_tables.iter() {
        let name = table.get("name").and_then(Item::as_str)?.to_string();
        let version = table
            .get("version")
            .and_then(Item::as_str)
            .unwrap_or_default()
            .to_string();
        let marker = table
            .get("marker")
            .and_then(Item::as_str)
            .map(|s| s.to_string());
        let extras = table
            .get("extras")
            .and_then(Item::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|val| val.as_str().map(|s| s.to_string()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(Vec::new);
        let parents = table
            .get("parents")
            .and_then(Item::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|val| val.as_str().map(|s| s.to_string()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(Vec::new);
        nodes.push(GraphNode {
            name,
            version,
            marker,
            parents,
            extras,
        });
    }
    if nodes.is_empty() {
        return None;
    }

    let mut targets = Vec::new();
    if let Some(target_tables) = graph.get("targets").and_then(Item::as_array_of_tables) {
        for table in target_tables.iter() {
            let target = GraphTarget {
                id: table
                    .get("id")
                    .and_then(Item::as_str)
                    .unwrap_or_default()
                    .to_string(),
                python_tag: table
                    .get("python_tag")
                    .and_then(Item::as_str)
                    .unwrap_or_default()
                    .to_string(),
                abi_tag: table
                    .get("abi_tag")
                    .and_then(Item::as_str)
                    .unwrap_or_default()
                    .to_string(),
                platform_tag: table
                    .get("platform_tag")
                    .and_then(Item::as_str)
                    .unwrap_or_default()
                    .to_string(),
            };
            targets.push(target);
        }
    }

    let mut artifacts = Vec::new();
    if let Some(artifact_tables) = graph.get("artifacts").and_then(Item::as_array_of_tables) {
        for table in artifact_tables.iter() {
            let node = table
                .get("node")
                .and_then(Item::as_str)
                .unwrap_or_default()
                .to_string();
            let target = table
                .get("target")
                .and_then(Item::as_str)
                .unwrap_or_default()
                .to_string();
            if let Some(artifact) = parse_artifact_table(table) {
                artifacts.push(GraphArtifactEntry {
                    node,
                    target,
                    artifact,
                });
            }
        }
    }

    Some(LockGraphSnapshot {
        nodes,
        targets,
        artifacts,
    })
}

fn normalized_from_graph(graph: &LockGraphSnapshot) -> (Vec<String>, Vec<LockedDependency>) {
    let mut nodes = graph.nodes.clone();
    nodes.sort_by(|a, b| a.name.cmp(&b.name).then(a.version.cmp(&b.version)));
    let mut dependencies = Vec::new();
    let mut resolved = Vec::new();
    for node in nodes {
        let marker = node.marker.as_deref().filter(|m| !m.is_empty());
        let spec = format_specifier(&node.name, &node.extras, &node.version, marker);
        dependencies.push(spec.clone());
        let artifact = graph
            .artifacts
            .iter()
            .find(|entry| entry.node == node.name)
            .map(|entry| entry.artifact.clone());
        resolved.push(LockedDependency {
            name: node.name,
            artifact,
        });
    }
    (dependencies, resolved)
}

fn format_specifier(
    normalized: &str,
    extras: &[String],
    version: &str,
    marker: Option<&str>,
) -> String {
    let mut spec = normalized.to_string();
    let extras = canonical_extras(extras);
    if !extras.is_empty() {
        spec.push('[');
        spec.push_str(&extras.join(","));
        spec.push(']');
    }
    spec.push_str("==");
    spec.push_str(version);
    if let Some(marker) = marker.and_then(|m| {
        let trimmed = m.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    }) {
        spec.push_str(" ; ");
        spec.push_str(marker);
    }
    spec
}

fn canonical_extras(extras: &[String]) -> Vec<String> {
    let mut values = extras
        .iter()
        .map(|extra| extra.to_ascii_lowercase())
        .collect::<Vec<_>>();
    values.sort();
    values.dedup();
    values
}

fn parse_spec_metadata(spec: &str) -> (Vec<String>, Option<String>) {
    match PepRequirement::from_str(spec.trim()) {
        Ok(req) => {
            let extras = req
                .extras
                .iter()
                .map(|extra| extra.to_string())
                .collect::<Vec<_>>();
            let marker = req.marker.map(|expr| expr.to_string());
            (extras, marker)
        }
        Err(_) => (Vec::new(), None),
    }
}

fn current_timestamp() -> Result<String> {
    Ok(OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339)?)
}

fn dependency_name(spec: &str) -> String {
    let trimmed = strip_wrapping_quotes(spec.trim());
    let mut end = trimmed.len();
    for (idx, ch) in trimmed.char_indices() {
        if ch.is_ascii_whitespace() || matches!(ch, '<' | '>' | '=' | '!' | '~' | ';') {
            end = idx;
            break;
        }
    }
    let head = &trimmed[..end];
    head.split('[').next().unwrap_or(head).to_lowercase()
}

fn strip_wrapping_quotes(input: &str) -> &str {
    if input.len() >= 2 {
        let bytes = input.as_bytes();
        if (bytes[0] == b'"' && bytes[input.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[input.len() - 1] == b'\'')
        {
            return &input[1..input.len() - 1];
        }
    }
    input
}

fn version_from_specifier(spec: &str) -> Option<&str> {
    spec.trim()
        .split_once("==")
        .map(|(_, version)| version.trim())
}

fn marker_applies(spec: &str, marker_env: &MarkerEnvironment) -> bool {
    let cleaned = strip_wrapping_quotes(spec.trim());
    match PepRequirement::from_str(cleaned) {
        Ok(req) => req.evaluate_markers(marker_env, &[]),
        Err(_) => true,
    }
}

fn spec_map<'a>(
    specs: &'a [String],
    marker_env: Option<&MarkerEnvironment>,
) -> HashMap<String, &'a String> {
    let mut map = HashMap::new();
    for spec in specs {
        if let Some(env) = marker_env {
            if !marker_applies(spec, env) {
                continue;
            }
        }
        map.insert(dependency_name(spec), spec);
    }
    map
}

fn compute_file_sha256(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use toml_edit::DocumentMut;

    fn sample_snapshot(root: &Path) -> ProjectSnapshot {
        ProjectSnapshot {
            root: root.to_path_buf(),
            manifest_path: root.join("pyproject.toml"),
            lock_path: root.join("px.lock"),
            name: "demo".into(),
            python_requirement: ">=3.12".into(),
            dependencies: vec!["demo==1.0.0".into()],
            python_override: None,
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
            },
        }]
    }

    #[test]
    fn renders_and_loads_lockfile() -> Result<()> {
        let dir = tempdir()?;
        let snapshot = sample_snapshot(dir.path());
        let toml = render_lockfile(&snapshot, &resolved(), "0.1.0")?;
        let parsed: LockSnapshot = parse_lock_snapshot(&toml.parse()?);
        assert_eq!(parsed.dependencies.len(), 1);
        assert_eq!(parsed.version, LOCK_VERSION);
        Ok(())
    }

    #[test]
    fn diff_reports_added_dependency() {
        let dir = tempdir().unwrap();
        let mut snapshot = sample_snapshot(dir.path());
        snapshot.dependencies.push("extra==2.0.0".into());
        let lock = LockSnapshot {
            version: LOCK_VERSION,
            project_name: Some("demo".into()),
            python_requirement: Some(">=3.12".into()),
            dependencies: vec!["demo==1.0.0".into()],
            mode: Some(LOCK_MODE_PINNED.into()),
            resolved: vec![LockedDependency {
                name: "demo".into(),
                artifact: Some(LockedArtifact::default()),
            }],
            graph: None,
        };
        let report = analyze_lock_diff(&snapshot, &lock, None);
        assert!(!report.is_clean());
        assert_eq!(report.added.len(), 1);
    }

    #[test]
    fn verify_locked_artifacts_checks_hash() -> Result<()> {
        let dir = tempdir()?;
        let wheel = dir.path().join("demo.whl");
        fs::write(&wheel, b"demo")?;
        let lock = LockSnapshot {
            version: LOCK_VERSION,
            project_name: Some("demo".into()),
            python_requirement: Some(">=3.12".into()),
            dependencies: vec!["demo==1.0.0".into()],
            mode: Some(LOCK_MODE_PINNED.into()),
            resolved: vec![LockedDependency {
                name: "demo".into(),
                artifact: Some(LockedArtifact {
                    filename: "demo.whl".into(),
                    url: "https://example.invalid/demo.whl".into(),
                    sha256: compute_file_sha256(&wheel)?,
                    size: 4,
                    cached_path: wheel.display().to_string(),
                    python_tag: "py3".into(),
                    abi_tag: "none".into(),
                    platform_tag: "any".into(),
                }),
            }],
            graph: None,
        };
        assert!(verify_locked_artifacts(&lock).is_empty());
        Ok(())
    }

    #[test]
    fn collect_resolved_dependencies_merges_metadata() {
        let lock = LockSnapshot {
            version: LOCK_VERSION,
            project_name: Some("demo".into()),
            python_requirement: Some(">=3.12".into()),
            dependencies: vec!["Demo[Extra]==1.0.0 ; python_version >= '3.12'".into()],
            mode: Some(LOCK_MODE_PINNED.into()),
            resolved: vec![LockedDependency {
                name: "demo".into(),
                artifact: Some(LockedArtifact {
                    filename: "demo.whl".into(),
                    url: "https://example.invalid/demo.whl".into(),
                    sha256: "abc".into(),
                    size: 4,
                    cached_path: "/tmp/demo.whl".into(),
                    python_tag: "py3".into(),
                    abi_tag: "none".into(),
                    platform_tag: "any".into(),
                }),
            }],
            graph: None,
        };

        let deps = collect_resolved_dependencies(&lock);
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "demo");
        assert_eq!(deps[0].extras, vec!["extra".to_string()]);
        assert_eq!(deps[0].marker.as_deref(), Some("python_version >= '3.12'"));
    }

    #[test]
    fn render_lockfile_v2_emits_graph() -> Result<()> {
        let dir = tempdir()?;
        let snapshot = sample_snapshot(dir.path());
        let lock = LockSnapshot {
            version: LOCK_VERSION,
            project_name: Some(snapshot.name.clone()),
            python_requirement: Some(snapshot.python_requirement.clone()),
            dependencies: snapshot.dependencies.clone(),
            mode: Some(LOCK_MODE_PINNED.into()),
            resolved: vec![LockedDependency {
                name: "demo".into(),
                artifact: Some(LockedArtifact {
                    filename: "demo.whl".into(),
                    url: "https://example.invalid/demo.whl".into(),
                    sha256: "deadbeef".into(),
                    size: 4,
                    cached_path: dir.path().join("cache/demo.whl").display().to_string(),
                    python_tag: "py3".into(),
                    abi_tag: "abi3".into(),
                    platform_tag: "any".into(),
                }),
            }],
            graph: Some(LockGraphSnapshot {
                nodes: vec![GraphNode {
                    name: "demo".into(),
                    version: "1.0.0".into(),
                    marker: Some("python_version >= '3.12'".into()),
                    parents: vec!["root".into()],
                    extras: vec!["extra".into()],
                }],
                targets: vec![GraphTarget {
                    id: "py3-abi3-any".into(),
                    python_tag: "py3".into(),
                    abi_tag: "abi3".into(),
                    platform_tag: "any".into(),
                }],
                artifacts: vec![GraphArtifactEntry {
                    node: "demo".into(),
                    target: "py3-abi3-any".into(),
                    artifact: LockedArtifact {
                        filename: "demo.whl".into(),
                        url: "https://example.invalid/demo.whl".into(),
                        sha256: "deadbeef".into(),
                        size: 4,
                        cached_path: dir.path().join("cache/demo.whl").display().to_string(),
                        python_tag: "py3".into(),
                        abi_tag: "abi3".into(),
                        platform_tag: "any".into(),
                    },
                }],
            }),
        };

        let toml = render_lockfile_v2(&snapshot, &lock, "0.2.0")?;
        let doc: DocumentMut = toml.parse()?;
        assert_eq!(doc["version"].as_integer(), Some(2));
        assert!(doc["graph"].is_table());
        Ok(())
    }

    #[test]
    fn lock_prefetch_specs_extracts_artifacts() -> Result<()> {
        let lock = LockSnapshot {
            version: LOCK_VERSION,
            project_name: Some("demo".into()),
            python_requirement: Some(">=3.12".into()),
            dependencies: vec!["demo==1.0.0".into()],
            mode: Some(LOCK_MODE_PINNED.into()),
            resolved: vec![LockedDependency {
                name: "demo".into(),
                artifact: Some(LockedArtifact {
                    filename: "demo-1.0.0.whl".into(),
                    url: "https://example.invalid/demo.whl".into(),
                    sha256: "deadbeef".into(),
                    size: 4,
                    cached_path: "/tmp/demo.whl".into(),
                    python_tag: "py3".into(),
                    abi_tag: "none".into(),
                    platform_tag: "any".into(),
                }),
            }],
            graph: None,
        };

        let specs = lock_prefetch_specs(&lock)?;
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "demo");
        assert_eq!(specs[0].version, "1.0.0");
        assert_eq!(specs[0].filename, "demo-1.0.0.whl");
        Ok(())
    }
}
