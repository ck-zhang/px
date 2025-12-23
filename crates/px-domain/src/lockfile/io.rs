use std::convert::TryFrom;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, Value as TomlValue};

use crate::project::snapshot::ProjectSnapshot;

use super::analysis::collect_resolved_dependencies;
use super::spec::dependency_name;
use super::types::{
    GraphArtifactEntry, GraphNode, GraphTarget, LockGraphSnapshot, LockSnapshot, LockedArtifact,
    LockedDependency, ResolvedDependency, WorkspaceLock, WorkspaceMember, WorkspaceOwner,
    LOCK_MODE_PINNED, LOCK_VERSION,
};

pub fn load_lockfile(path: &Path) -> Result<LockSnapshot> {
    let contents = fs::read_to_string(path)?;
    let doc: DocumentMut = contents
        .parse()
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(parse_lock_snapshot(&doc))
}

pub fn parse_lockfile(contents: &str) -> Result<LockSnapshot> {
    let doc: DocumentMut = contents.parse().context("failed to parse lockfile")?;
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
    render_lockfile_with_workspace(snapshot, resolved, px_version, None)
}

pub fn render_lockfile_with_workspace(
    snapshot: &ProjectSnapshot,
    resolved: &[ResolvedDependency],
    px_version: &str,
    workspace: Option<&WorkspaceLock>,
) -> Result<String> {
    let mut doc = DocumentMut::new();
    doc.insert("version", Item::Value(TomlValue::from(LOCK_VERSION)));

    let mut metadata = Table::new();
    metadata.insert("px_version", Item::Value(TomlValue::from(px_version)));
    metadata.insert("mode", Item::Value(TomlValue::from(LOCK_MODE_PINNED)));
    metadata.insert(
        "manifest_fingerprint",
        Item::Value(TomlValue::from(snapshot.manifest_fingerprint.clone())),
    );
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
    let lock_id = compute_lock_identity(&snapshot.manifest_fingerprint, &ordered);
    if let Some(metadata) = doc.get_mut("metadata").and_then(Item::as_table_mut) {
        metadata.insert("lock_id", Item::Value(TomlValue::from(lock_id.clone())));
    }
    let mut deps = ArrayOfTables::new();
    for dep in &ordered {
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
        table.insert("direct", Item::Value(TomlValue::from(dep.direct)));
        if let Some(source) = &dep.source {
            table.insert("source", Item::Value(TomlValue::from(source.as_str())));
        }
        if !dep.requires.is_empty() {
            let mut requires = Array::new();
            for req in &dep.requires {
                requires.push(TomlValue::from(req.as_str()));
            }
            table.insert("requires", Item::Value(TomlValue::Array(requires)));
        }
        deps.push(table);
    }
    doc.insert("dependencies", Item::ArrayOfTables(deps));

    if let Some(workspace) = workspace {
        doc.insert("workspace", Item::Table(render_workspace(workspace)));
    }

    Ok(doc.to_string())
}

fn compute_lock_identity(fingerprint: &str, deps: &[ResolvedDependency]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(fingerprint.as_bytes());
    for dep in deps {
        hasher.update(dep.name.as_bytes());
        hasher.update(dep.specifier.as_bytes());
        if let Some(marker) = &dep.marker {
            hasher.update(marker.as_bytes());
        }
        let mut extras = dep.extras.clone();
        extras.sort();
        for extra in extras {
            hasher.update(extra.as_bytes());
            hasher.update([0]);
        }
        let mut requires = dep.requires.clone();
        requires.sort();
        for req in requires {
            hasher.update(req.as_bytes());
            hasher.update([0]);
        }
        hash_artifact(&mut hasher, &dep.artifact);
    }
    format!("{:x}", hasher.finalize())
}

fn hash_artifact(hasher: &mut Sha256, artifact: &LockedArtifact) {
    hasher.update(artifact.filename.as_bytes());
    hasher.update(artifact.url.as_bytes());
    hasher.update(artifact.sha256.as_bytes());
    hasher.update(artifact.size.to_le_bytes());
    hasher.update(artifact.python_tag.as_bytes());
    hasher.update(artifact.abi_tag.as_bytes());
    hasher.update(artifact.platform_tag.as_bytes());
    hasher.update(artifact.build_options_hash.as_bytes());
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
    metadata.insert("mode", Item::Value(TomlValue::from(LOCK_MODE_PINNED)));
    let manifest_fingerprint = lock
        .manifest_fingerprint
        .clone()
        .unwrap_or_else(|| snapshot.manifest_fingerprint.clone());
    metadata.insert(
        "manifest_fingerprint",
        Item::Value(TomlValue::from(manifest_fingerprint.clone())),
    );
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
    let lock_id = lock
        .lock_id
        .clone()
        .unwrap_or_else(|| compute_lock_identity(&manifest_fingerprint, &resolved));
    if let Some(metadata) = doc.get_mut("metadata").and_then(Item::as_table_mut) {
        metadata.insert("lock_id", Item::Value(TomlValue::from(lock_id)));
    }
    let mut deps = ArrayOfTables::new();
    for dep in &resolved {
        let mut table = Table::new();
        table.insert("name", Item::Value(TomlValue::from(dep.name.clone())));
        table.insert(
            "specifier",
            Item::Value(TomlValue::from(dep.specifier.clone())),
        );
        table.insert("artifact", Item::Table(render_artifact(&dep.artifact)));
        table.insert("direct", Item::Value(TomlValue::from(dep.direct)));
        if !dep.requires.is_empty() {
            let mut requires = Array::new();
            for req in &dep.requires {
                requires.push(TomlValue::from(req.as_str()));
            }
            table.insert("requires", Item::Value(TomlValue::Array(requires)));
        }
        deps.push(table);
    }
    doc.insert("dependencies", Item::ArrayOfTables(deps));

    if let Some(graph) = &lock.graph {
        doc.insert("graph", Item::Table(render_graph(graph)));
    }

    Ok(doc.to_string())
}

pub(crate) fn parse_lock_snapshot(doc: &DocumentMut) -> LockSnapshot {
    let version = doc.get("version").and_then(Item::as_integer).unwrap_or(0);
    let project_name = doc
        .get("project")
        .and_then(Item::as_table)
        .and_then(|table| table.get("name"))
        .and_then(Item::as_str)
        .map(std::string::ToString::to_string);
    let python_requirement = doc
        .get("python")
        .and_then(Item::as_table)
        .and_then(|table| table.get("requirement"))
        .and_then(Item::as_str)
        .map(std::string::ToString::to_string);
    let metadata = doc.get("metadata").and_then(Item::as_table);
    let mode = metadata
        .and_then(|table| table.get("mode"))
        .and_then(Item::as_str)
        .map(std::string::ToString::to_string);
    let manifest_fingerprint = metadata
        .and_then(|table| table.get("manifest_fingerprint"))
        .and_then(Item::as_str)
        .map(std::string::ToString::to_string);
    let lock_id = metadata
        .and_then(|table| table.get("lock_id"))
        .and_then(Item::as_str)
        .map(std::string::ToString::to_string);

    let workspace = doc
        .get("workspace")
        .and_then(Item::as_table)
        .and_then(parse_workspace);

    if version >= 2 {
        if let Some(graph) = parse_graph_snapshot(doc) {
            let (dependencies, resolved) = normalized_from_graph(&graph);
            return LockSnapshot {
                version,
                project_name,
                python_requirement,
                manifest_fingerprint,
                lock_id,
                dependencies,
                mode,
                resolved,
                graph: Some(graph),
                workspace,
            };
        }
    }

    let mut dependencies = Vec::new();
    let mut resolved = Vec::new();
    if let Some(tables) = doc.get("dependencies").and_then(Item::as_array_of_tables) {
        for table in tables {
            let specifier = table
                .get("specifier")
                .and_then(Item::as_str)
                .map(std::string::ToString::to_string)
                .unwrap_or_default();
            if !specifier.is_empty() {
                dependencies.push(specifier.clone());
            }
            let name = table.get("name").and_then(Item::as_str).map_or_else(
                || dependency_name(&specifier),
                std::string::ToString::to_string,
            );
            let artifact = table
                .get("artifact")
                .and_then(Item::as_table)
                .and_then(parse_artifact_table);
            let direct = table.get("direct").and_then(Item::as_bool).unwrap_or(true);
            let requires = table
                .get("requires")
                .and_then(Item::as_array)
                .map(|array| {
                    array
                        .iter()
                        .filter_map(|val| val.as_str().map(std::string::ToString::to_string))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let source = table
                .get("source")
                .and_then(Item::as_str)
                .map(std::string::ToString::to_string);
            resolved.push(LockedDependency {
                name,
                direct,
                artifact,
                requires,
                source,
            });
        }
    } else if let Some(array) = doc.get("dependencies").and_then(Item::as_array) {
        dependencies = array
            .iter()
            .filter_map(|val| val.as_str().map(std::string::ToString::to_string))
            .collect();
    }

    LockSnapshot {
        version,
        project_name,
        python_requirement,
        manifest_fingerprint,
        lock_id,
        dependencies,
        mode,
        resolved,
        graph: None,
        workspace,
    }
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
    let size_value = i64::try_from(artifact.size).unwrap_or(i64::MAX);
    table.insert("size", Item::Value(TomlValue::from(size_value)));
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
    if artifact.is_direct_url {
        table.insert(
            "is_direct_url",
            Item::Value(TomlValue::from(artifact.is_direct_url)),
        );
    }
    if !artifact.build_options_hash.is_empty() {
        table.insert(
            "build_options_hash",
            Item::Value(TomlValue::from(artifact.build_options_hash.clone())),
        );
    }
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

fn render_workspace(workspace: &WorkspaceLock) -> Table {
    let mut table = Table::new();
    if !workspace.members.is_empty() {
        let mut members = ArrayOfTables::new();
        for member in &workspace.members {
            let mut entry = Table::new();
            entry.insert("name", Item::Value(TomlValue::from(member.name.clone())));
            entry.insert("path", Item::Value(TomlValue::from(member.path.clone())));
            entry.insert(
                "manifest_fingerprint",
                Item::Value(TomlValue::from(member.manifest_fingerprint.clone())),
            );
            if !member.dependencies.is_empty() {
                let mut deps = Array::new();
                for dep in &member.dependencies {
                    deps.push(TomlValue::from(dep.as_str()));
                }
                entry.insert("dependencies", Item::Value(TomlValue::Array(deps)));
            }
            members.push(entry);
        }
        table.insert("members", Item::ArrayOfTables(members));
    }

    if !workspace.owners.is_empty() {
        let mut owners = ArrayOfTables::new();
        for owned in &workspace.owners {
            let mut entry = Table::new();
            entry.insert("name", Item::Value(TomlValue::from(owned.name.clone())));
            let mut names = Array::new();
            for owner in &owned.owners {
                names.push(TomlValue::from(owner.as_str()));
            }
            entry.insert("owners", Item::Value(TomlValue::Array(names)));
            owners.push(entry);
        }
        table.insert("owners", Item::ArrayOfTables(owners));
    }

    table
}

fn parse_workspace(table: &Table) -> Option<WorkspaceLock> {
    let mut members_out = Vec::new();
    if let Some(members) = table.get("members").and_then(Item::as_array_of_tables) {
        for member in members {
            let name = member
                .get("name")
                .and_then(Item::as_str)
                .unwrap_or_default()
                .to_string();
            let path = member
                .get("path")
                .and_then(Item::as_str)
                .unwrap_or_default()
                .to_string();
            let manifest_fingerprint = member
                .get("manifest_fingerprint")
                .and_then(Item::as_str)
                .unwrap_or_default()
                .to_string();
            let dependencies = member
                .get("dependencies")
                .and_then(Item::as_array)
                .map(|array| {
                    array
                        .iter()
                        .filter_map(|val| val.as_str().map(std::string::ToString::to_string))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            members_out.push(WorkspaceMember {
                name,
                path,
                manifest_fingerprint,
                dependencies,
            });
        }
    }

    let mut owners_out = Vec::new();
    if let Some(owners) = table.get("owners").and_then(Item::as_array_of_tables) {
        for owner in owners {
            let name = owner
                .get("name")
                .and_then(Item::as_str)
                .unwrap_or_default()
                .to_string();
            let owners = owner
                .get("owners")
                .and_then(Item::as_array)
                .map(|array| {
                    array
                        .iter()
                        .filter_map(|val| val.as_str().map(std::string::ToString::to_string))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            owners_out.push(WorkspaceOwner { name, owners });
        }
    }

    if members_out.is_empty() && owners_out.is_empty() {
        None
    } else {
        Some(WorkspaceLock {
            members: members_out,
            owners: owners_out,
        })
    }
}

fn parse_artifact_table(table: &Table) -> Option<LockedArtifact> {
    fn wheel_tags_from_filename(filename: &str) -> Option<(String, String, String)> {
        let filename = filename.trim();
        if filename.is_empty() {
            return None;
        }
        if !filename.to_ascii_lowercase().ends_with(".whl") {
            return None;
        }
        let stem = filename.get(..filename.len().saturating_sub(4))?;
        let parts: Vec<&str> = stem.split('-').collect();
        if parts.len() < 5 {
            return None;
        }
        let py = parts[parts.len() - 3].to_ascii_lowercase();
        let abi = parts[parts.len() - 2].to_ascii_lowercase();
        let platform = parts[parts.len() - 1].to_ascii_lowercase();
        Some((py, abi, platform))
    }

    let filename = table.get("filename").and_then(Item::as_str)?.to_string();
    let url = table.get("url").and_then(Item::as_str)?.to_string();
    let sha256 = table.get("sha256").and_then(Item::as_str)?.to_string();
    let size = table
        .get("size")
        .and_then(Item::as_integer)
        .and_then(|value| u64::try_from(value).ok())
        .unwrap_or(0);
    let mut python_tag = table
        .get("python_tag")
        .and_then(Item::as_str)
        .unwrap_or_default()
        .to_string();
    let mut abi_tag = table
        .get("abi_tag")
        .and_then(Item::as_str)
        .unwrap_or_default()
        .to_string();
    let mut platform_tag = table
        .get("platform_tag")
        .and_then(Item::as_str)
        .unwrap_or_default()
        .to_string();
    if (python_tag.is_empty() || abi_tag.is_empty() || platform_tag.is_empty())
        && filename.to_ascii_lowercase().ends_with(".whl")
    {
        if let Some((py, abi, platform)) = wheel_tags_from_filename(&filename) {
            if python_tag.is_empty() {
                python_tag = py;
            }
            if abi_tag.is_empty() {
                abi_tag = abi;
            }
            if platform_tag.is_empty() {
                platform_tag = platform;
            }
        }
    }
    let is_direct_url = table
        .get("is_direct_url")
        .and_then(Item::as_bool)
        .unwrap_or(false);
    let build_options_hash = table
        .get("build_options_hash")
        .and_then(Item::as_str)
        .unwrap_or_default()
        .to_string();
    Some(LockedArtifact {
        filename,
        url,
        sha256,
        size,
        python_tag,
        abi_tag,
        platform_tag,
        is_direct_url,
        build_options_hash,
    })
}

fn parse_graph_snapshot(doc: &DocumentMut) -> Option<LockGraphSnapshot> {
    let graph = doc.get("graph")?.as_table()?;
    let node_tables = graph.get("nodes")?.as_array_of_tables()?;
    let mut nodes = Vec::new();
    for table in node_tables {
        let name = table.get("name").and_then(Item::as_str)?.to_string();
        let version = table
            .get("version")
            .and_then(Item::as_str)
            .unwrap_or_default()
            .to_string();
        let marker = table
            .get("marker")
            .and_then(Item::as_str)
            .map(std::string::ToString::to_string);
        let extras = table
            .get("extras")
            .and_then(Item::as_array)
            .map_or_else(Vec::new, |arr| {
                arr.iter()
                    .filter_map(|val| val.as_str().map(std::string::ToString::to_string))
                    .collect::<Vec<_>>()
            });
        let parents = table
            .get("parents")
            .and_then(Item::as_array)
            .map_or_else(Vec::new, |arr| {
                arr.iter()
                    .filter_map(|val| val.as_str().map(std::string::ToString::to_string))
                    .collect::<Vec<_>>()
            });
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
        for table in target_tables {
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
        for table in artifact_tables {
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
        let spec = super::spec::format_specifier(&node.name, &node.extras, &node.version, marker);
        dependencies.push(spec.clone());
        let artifact = graph
            .artifacts
            .iter()
            .find(|entry| entry.node == node.name)
            .map(|entry| entry.artifact.clone());
        resolved.push(LockedDependency {
            name: node.name,
            direct: true,
            artifact,
            requires: Vec::new(),
            source: None,
        });
    }
    (dependencies, resolved)
}

// Intentionally omit timestamps to keep lockfiles deterministic.
