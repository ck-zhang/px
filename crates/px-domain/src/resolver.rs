#![deny(clippy::all, warnings)]

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    str::FromStr,
};

use anyhow::{anyhow, Context, Result};
use pep508_rs::{MarkerEnvironment as PxMarkerEnvironment, StringVersion};
use petgraph::graph::NodeIndex;
use tokio::runtime::Builder;
use uv_cache::Cache;
use uv_client::{BaseClientBuilder, RegistryClientBuilder};
use uv_configuration::{BuildOptions, Concurrency, Constraints, IndexStrategy, SourceStrategy};
use uv_dispatch::{BuildDispatch, SharedState};
use uv_distribution::DistributionDatabase;
use uv_distribution_types::{
    ConfigSettings, DependencyMetadata, DistributionMetadata, ExtraBuildRequires,
    ExtraBuildVariables, Index, IndexLocations, Name, Node, PackageConfigSettings,
    Requirement as UvRequirement, Resolution as UvResolution, ResolvedDist, VersionOrUrlRef,
};
use uv_install_wheel::LinkMode;
use uv_pep508::Requirement as UvPepRequirement;
use uv_preview::Preview;
use uv_pypi_types::Conflicts;
use uv_python::Interpreter;
use uv_resolver::MetadataResponse;
use uv_resolver::{
    ExcludeNewer, FlatIndex, InMemoryIndex, Manifest, OptionsBuilder, PythonRequirement, Resolver,
    ResolverEnvironment,
};
use uv_types::{BuildIsolation, EmptyInstalledPackages, HashStrategy};
use uv_workspace::WorkspaceCache;

use crate::manifest::dependency_name;

pub fn resolve(request: &ResolveRequest) -> Result<Vec<ResolvedSpecifier>> {
    if request.requirements.is_empty() {
        return Ok(Vec::new());
    }

    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to create resolver runtime")?;

    runtime.block_on(resolve_with_uv(request))
}

async fn resolve_with_uv(request: &ResolveRequest) -> Result<Vec<ResolvedSpecifier>> {
    let cache_root = request.cache_dir.join("resolver-cache");
    let cache = Cache::from_path(&cache_root)
        .init()
        .context("failed to initialize uv cache")?;

    let interpreter = Interpreter::query(&request.python, &cache)
        .context("failed to inspect Python interpreter for resolver")?;
    let marker_env = interpreter.resolver_marker_environment();
    let tags = interpreter
        .tags()
        .context("failed to compute interpreter tags for resolver")?;
    let python_requirement = PythonRequirement::from_interpreter(&interpreter);

    let manifest = Manifest::simple(parse_requirements(&request.requirements)?);
    let options = OptionsBuilder::new().build();
    let flat_index = FlatIndex::default();
    let in_memory_index = InMemoryIndex::default();
    let index_locations = build_index_locations(&request.indexes)?;
    let constraints = Constraints::default();
    let dependency_metadata = DependencyMetadata::default();
    let shared_state = SharedState::default();
    let config_settings = ConfigSettings::default();
    let config_settings_package = PackageConfigSettings::default();
    let build_isolation = BuildIsolation::default();
    let extra_build_requires = ExtraBuildRequires::default();
    let extra_build_variables = ExtraBuildVariables::default();
    let link_mode = LinkMode::default();
    let build_options = BuildOptions::default();
    let hashes = HashStrategy::default();
    let exclude_newer = ExcludeNewer::default();
    let sources = SourceStrategy::default();
    let workspace_cache = WorkspaceCache::default();
    let concurrency = Concurrency::default();

    let client = RegistryClientBuilder::new(BaseClientBuilder::default(), cache.clone()).build();
    let build_context = BuildDispatch::new(
        &client,
        &cache,
        &constraints,
        &interpreter,
        &index_locations,
        &flat_index,
        &dependency_metadata,
        shared_state,
        IndexStrategy::default(),
        &config_settings,
        &config_settings_package,
        build_isolation,
        &extra_build_requires,
        &extra_build_variables,
        link_mode,
        &build_options,
        &hashes,
        exclude_newer,
        sources,
        workspace_cache,
        concurrency,
        Preview::default(),
    );

    let resolver = Resolver::new(
        manifest,
        options,
        &python_requirement,
        ResolverEnvironment::specific(marker_env.clone()),
        interpreter.markers(),
        Conflicts::empty(),
        Some(tags),
        &flat_index,
        &in_memory_index,
        &hashes,
        &build_context,
        EmptyInstalledPackages,
        DistributionDatabase::new(&client, &build_context, concurrency.downloads),
    )
    .context("failed to construct uv resolver")?;

    let output = resolver.resolve().await?;
    let resolution: UvResolution = output.into();
    let direct_requirements = parse_direct_requirements(&request.requirements)?;

    map_resolution_to_specifiers(&resolution, &in_memory_index, &direct_requirements)
}

fn map_resolution_to_specifiers(
    resolution: &UvResolution,
    index: &InMemoryIndex,
    direct_requirements: &HashMap<String, UvPepRequirement>,
) -> Result<Vec<ResolvedSpecifier>> {
    let graph = resolution.graph();
    let mut builders: HashMap<NodeIndex, SpecBuilder> = HashMap::new();
    let mut name_to_index = HashMap::new();

    for idx in graph.node_indices() {
        let node = &graph[idx];
        if let Node::Dist { dist, install, .. } = node {
            if !install {
                continue;
            }
            let name = dist.name().to_string();
            let normalized = normalize_dist_name(&name);
            let selected_version = selected_version(dist)?;
            let specifier = format!("{name}=={selected_version}");
            builders.insert(
                idx,
                SpecBuilder {
                    name,
                    normalized: normalized.clone(),
                    selected_version,
                    specifier,
                    extras: Vec::new(),
                    marker: None,
                    direct: false,
                    requires: HashSet::new(),
                    source: None,
                },
            );
            name_to_index.insert(normalized, idx);
        }
    }

    for (normalized, requirement) in direct_requirements {
        if let Some(idx) = name_to_index.get(normalized) {
            if let Some(builder) = builders.get_mut(idx) {
                builder.direct = true;
                builder.extras = requirement
                    .extras
                    .iter()
                    .map(|extra| extra.to_string())
                    .collect();
                builder.extras.sort();
                builder.marker = requirement_marker(requirement);
                builder.specifier = requirement.to_string();
            }
        }
    }

    for edge_idx in graph.edge_indices() {
        let Some((parent_idx, child_idx)) = graph.edge_endpoints(edge_idx) else {
            continue;
        };
        let Some(child_norm) = builders
            .get(&child_idx)
            .map(|child| child.normalized.clone())
        else {
            continue;
        };
        let Some(parent_info) = builders
            .get(&parent_idx)
            .map(|parent| (parent.name.clone(), parent.selected_version.clone()))
        else {
            continue;
        };

        if let Some(parent_builder) = builders.get_mut(&parent_idx) {
            parent_builder.requires.insert(child_norm.clone());
        }

        if let Some(child_builder) = builders.get_mut(&child_idx) {
            if child_builder.source.is_none() {
                child_builder.source = Some(format!("{}=={}", parent_info.0, parent_info.1));
            }

            if !child_builder.direct {
                if let Some(requirement) =
                    requirement_for_edge(&graph[parent_idx], &child_builder.normalized, index)
                {
                    if child_builder.marker.is_none() {
                        child_builder.marker = requirement_marker(&requirement);
                    }
                    if child_builder.extras.is_empty() {
                        let mut extras = requirement
                            .extras
                            .iter()
                            .map(|extra| extra.to_string())
                            .collect::<Vec<_>>();
                        extras.sort();
                        child_builder.extras = extras;
                    }
                }
            }
        }
    }

    let mut specs: Vec<_> = builders
        .into_values()
        .map(|builder| ResolvedSpecifier {
            name: builder.name,
            specifier: builder.specifier,
            normalized: builder.normalized,
            selected_version: builder.selected_version,
            extras: builder.extras,
            marker: builder.marker,
            requires: {
                let mut requires: Vec<_> = builder.requires.into_iter().collect();
                requires.sort();
                requires
            },
            direct: builder.direct,
            source: builder.source,
            artifact: None,
        })
        .collect();
    specs.sort_by(|a, b| a.normalized.cmp(&b.normalized));
    Ok(specs)
}

fn requirement_for_edge(
    parent: &Node,
    child_normalized: &str,
    index: &InMemoryIndex,
) -> Option<UvPepRequirement> {
    let Node::Dist { dist, .. } = parent else {
        return None;
    };
    let version_id = dist.version_id();
    let response = index.distributions().get(&version_id)?;
    let metadata = match response.as_ref() {
        MetadataResponse::Found(archive) => Some(&archive.metadata),
        _ => None,
    }?;
    let mut requirements: Vec<UvPepRequirement> = metadata
        .requires_dist
        .iter()
        .cloned()
        .map(UvPepRequirement::from)
        .collect();
    requirements.extend(
        metadata
            .dependency_groups
            .values()
            .flat_map(|entries| entries.iter().cloned().map(UvPepRequirement::from)),
    );
    requirements
        .into_iter()
        .find(|req| normalize_dist_name(req.name.as_ref()) == child_normalized)
}

fn selected_version(dist: &ResolvedDist) -> Result<String> {
    if let Some(version) = dist.version() {
        return Ok(version.to_string());
    }
    let VersionOrUrlRef::Url(_) = dist.version_or_url() else {
        return Err(anyhow!("missing version for {}", dist.name()));
    };
    Err(anyhow!(
        "missing version for {} from direct URL",
        dist.name()
    ))
}

fn parse_requirements(requirements: &[String]) -> Result<Vec<UvRequirement>> {
    let mut parsed = Vec::with_capacity(requirements.len());
    for raw in requirements {
        let req = UvPepRequirement::from_str(raw)
            .map_err(|err| anyhow!("invalid requirement `{raw}`: {err}"))?;
        parsed.push(UvRequirement::from(req));
    }
    Ok(parsed)
}

fn parse_direct_requirements(requirements: &[String]) -> Result<HashMap<String, UvPepRequirement>> {
    let mut map = HashMap::new();
    for raw in requirements {
        let requirement = UvPepRequirement::from_str(raw)
            .map_err(|err| anyhow!("invalid requirement `{raw}`: {err}"))?;
        let normalized = normalize_dist_name(requirement.name.as_ref());
        map.entry(normalized).or_insert(requirement);
    }
    Ok(map)
}

fn build_index_locations(indexes: &[String]) -> Result<IndexLocations> {
    if indexes.is_empty() {
        return Ok(IndexLocations::default());
    }
    let mut parsed = Vec::new();
    for (idx, raw) in indexes.iter().enumerate() {
        let url = if raw.ends_with("/simple") {
            raw.clone()
        } else if let Some(stripped) = raw.strip_suffix("/pypi") {
            format!("{stripped}/simple")
        } else {
            format!("{raw}/simple")
        };
        let index_url = uv_distribution_types::IndexUrl::parse(&url, None)
            .with_context(|| format!("invalid index url `{raw}`"))?;
        let entry = if idx == 0 {
            Index::from_index_url(index_url)
        } else {
            Index::from_extra_index_url(index_url)
        };
        parsed.push(entry);
    }
    Ok(IndexLocations::new(parsed, Vec::new(), false))
}

fn requirement_marker(requirement: &UvPepRequirement) -> Option<String> {
    if requirement.marker.is_true() {
        return None;
    }
    requirement
        .to_string()
        .split_once(';')
        .map(|(_, marker)| marker.trim().to_string())
}

#[derive(Debug)]
struct SpecBuilder {
    name: String,
    normalized: String,
    selected_version: String,
    specifier: String,
    extras: Vec<String>,
    marker: Option<String>,
    direct: bool,
    requires: HashSet<String>,
    source: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ResolveRequest {
    pub project: String,
    pub requirements: Vec<String>,
    pub tags: ResolverTags,
    pub env: ResolverEnv,
    pub indexes: Vec<String>,
    pub cache_dir: PathBuf,
    pub python: String,
}

#[derive(Debug, Clone, Default)]
pub struct ResolverTags {
    pub python: Vec<String>,
    pub abi: Vec<String>,
    pub platform: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ResolverEnv {
    pub implementation_name: String,
    pub implementation_version: String,
    pub os_name: String,
    pub platform_machine: String,
    pub platform_python_implementation: String,
    pub platform_release: String,
    pub platform_system: String,
    pub platform_version: String,
    pub python_full_version: String,
    pub python_version: String,
    pub sys_platform: String,
}

#[derive(Debug, Clone)]
pub struct ResolvedSpecifier {
    pub name: String,
    pub specifier: String,
    pub normalized: String,
    pub selected_version: String,
    pub extras: Vec<String>,
    pub marker: Option<String>,
    pub requires: Vec<String>,
    pub direct: bool,
    pub source: Option<String>,
    pub artifact: Option<ResolvedArtifact>,
}

#[derive(Debug, Clone)]
pub struct ResolvedArtifact {
    pub url: String,
    pub filename: String,
    pub sha256: Option<String>,
    pub python_tag: String,
    pub abi_tag: String,
    pub platform_tag: String,
    pub is_direct: bool,
}

pub fn normalize_dist_name(name: &str) -> String {
    dependency_name(name)
}

impl ResolverEnv {
    pub fn to_marker_environment(&self) -> Result<PxMarkerEnvironment> {
        Ok(PxMarkerEnvironment {
            implementation_name: self.implementation_name.clone(),
            implementation_version: string_version(
                &self.implementation_version,
                "implementation_version",
            )?,
            os_name: self.os_name.clone(),
            platform_machine: self.platform_machine.clone(),
            platform_python_implementation: self.platform_python_implementation.clone(),
            platform_release: self.platform_release.clone(),
            platform_system: self.platform_system.clone(),
            platform_version: self.platform_version.clone(),
            python_full_version: string_version(&self.python_full_version, "python_full_version")?,
            python_version: string_version(&self.python_version, "python_version")?,
            sys_platform: self.sys_platform.clone(),
        })
    }
}

fn string_version(value: &str, field: &str) -> Result<StringVersion> {
    StringVersion::from_str(value)
        .map_err(|err| anyhow!("`{value}` is not a valid PEP 440 version for `{field}`: {err}"))
}
