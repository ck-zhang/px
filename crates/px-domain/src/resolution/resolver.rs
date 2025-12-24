#![deny(clippy::all, warnings)]

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    env,
    path::Path,
    path::PathBuf,
    str::FromStr,
};

use anyhow::{anyhow, Context, Result};
use pep508_rs::{MarkerEnvironment as PxMarkerEnvironment, StringVersion};
use petgraph::graph::NodeIndex;
use tokio::runtime::Builder;
use uv_cache::Cache;
use uv_client::{BaseClientBuilder, RegistryClientBuilder};
use uv_configuration::{
    BuildOptions, Concurrency, Constraints, Excludes, IndexStrategy, Overrides, SourceStrategy,
};
use uv_dispatch::{BuildDispatch, SharedState};
use uv_distribution::DistributionDatabase;
use uv_distribution_types::{
    ConfigSettings, DependencyMetadata, DistributionMetadata, ExtraBuildRequires,
    ExtraBuildVariables, Index, IndexLocations, Name, Node, PackageConfigSettings,
    Requirement as UvRequirement, RequirementSource, Resolution as UvResolution, ResolvedDist,
    VersionOrUrlRef,
};
use uv_install_wheel::LinkMode;
use uv_pep508::{Requirement as UvPepRequirement, VerbatimUrl};
use uv_preview::Preview;
use uv_pypi_types::Conflicts;
use uv_python::Interpreter;
use uv_resolver::MetadataResponse;
use uv_resolver::{
    ExcludeNewer, FlatIndex, InMemoryIndex, Manifest, OptionsBuilder, PythonRequirement, Resolver,
    ResolverEnvironment,
};
use uv_types::{BuildIsolation, EmptyInstalledPackages, HashStrategy};
use uv_workspace::{DiscoveryOptions, MemberDiscovery, ProjectWorkspace, WorkspaceCache};

use crate::project::manifest::dependency_name;

fn px_is_online() -> bool {
    match env::var("PX_ONLINE") {
        Ok(value) => {
            let lowered = value.to_ascii_lowercase();
            !matches!(lowered.as_str(), "0" | "false" | "no" | "off" | "")
        }
        Err(_) => true,
    }
}

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

    let workspace_cache = WorkspaceCache::default();
    let mut requirements = parse_requirements(&request.requirements, &request.root)?;
    let workspace_sources = apply_uv_workspace_sources(
        &mut requirements,
        &request.root,
        &marker_env,
        &workspace_cache,
    )
    .await?;
    let constraints = Constraints::from_requirements(workspace_sources.constraints.into_iter());
    let overrides = Overrides::from_requirements(workspace_sources.overrides);
    let manifest = Manifest::new(
        requirements,
        constraints,
        overrides,
        Excludes::default(),
        uv_resolver::Preferences::default(),
        None,
        BTreeSet::new(),
        uv_resolver::Exclusions::default(),
        Vec::new(),
    );
    let options = OptionsBuilder::new().build();
    let flat_index = FlatIndex::default();
    let in_memory_index = InMemoryIndex::default();
    let index_locations = build_index_locations(&request.indexes)?;
    let build_constraints = Constraints::default();
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
    let concurrency = Concurrency::default();

    let base_builder = BaseClientBuilder::default().connectivity(if px_is_online() {
        uv_client::Connectivity::Online
    } else {
        uv_client::Connectivity::Offline
    });
    let client = RegistryClientBuilder::new(base_builder, cache.clone()).build();
    let build_context = BuildDispatch::new(
        &client,
        &cache,
        &build_constraints,
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
    let direct_requirements = parse_direct_requirements(&request.requirements, &request.root)?;

    map_resolution_to_specifiers(&resolution, &in_memory_index, &direct_requirements)
}

async fn apply_uv_workspace_sources(
    requirements: &mut [UvRequirement],
    project_root: &Path,
    marker_env: &uv_pep508::MarkerEnvironment,
    cache: &WorkspaceCache,
) -> Result<UvWorkspaceSources> {
    fn normalize_lookahead_requirement(requirement: &mut UvRequirement) {
        if let RequirementSource::Directory { editable, .. } = &mut requirement.source {
            *editable = None;
        }
    }

    fn push_non_registry_requirement(requirement: UvRequirement, out: &mut Vec<UvRequirement>) {
        if matches!(requirement.source, RequirementSource::Registry { .. }) {
            return;
        }

        out.push(requirement);
    }

    let discovery = DiscoveryOptions {
        members: MemberDiscovery::All,
        ..DiscoveryOptions::default()
    };
    let project_workspace = match ProjectWorkspace::discover(project_root, &discovery, cache).await
    {
        Ok(workspace) => workspace,
        Err(err) => {
            tracing::debug!(?err, "uv_workspace_discovery_failed");
            return Ok(UvWorkspaceSources {
                constraints: Vec::new(),
                overrides: Vec::new(),
            });
        }
    };

    let mut lookahead_requirements = {
        let mut lookaheads = Vec::new();
        for mut requirement in project_workspace.workspace().members_requirements() {
            normalize_lookahead_requirement(&mut requirement);
            push_non_registry_requirement(requirement, &mut lookaheads);
        }
        lookaheads
    };

    for member in project_workspace.workspace().packages().values() {
        let Some(project) = member.pyproject_toml().project.as_ref() else {
            continue;
        };

        if let Some(deps) = project.dependencies.as_ref() {
            for dep in deps {
                let parsed = UvPepRequirement::parse(dep, member.root()).with_context(|| {
                    format!("invalid requirement `{dep}` in {}", member.root().display())
                })?;
                let mut requirement = UvRequirement::from(parsed);
                normalize_lookahead_requirement(&mut requirement);
                push_non_registry_requirement(requirement, &mut lookahead_requirements);
            }
        }

        if let Some(optional) = project.optional_dependencies.as_ref() {
            for deps in optional.values() {
                for dep in deps {
                    let Ok(parsed) = UvPepRequirement::parse(dep, member.root()) else {
                        tracing::debug!(
                            dependency = dep,
                            root = %member.root().display(),
                            "uv_optional_dependency_parse_failed"
                        );
                        continue;
                    };
                    let mut requirement = UvRequirement::from(parsed);
                    normalize_lookahead_requirement(&mut requirement);
                    push_non_registry_requirement(requirement, &mut lookahead_requirements);
                }
            }
        }

        let Ok(dependency_groups) =
            uv_workspace::dependency_groups::FlatDependencyGroups::from_pyproject_toml(
                member.root(),
                member.pyproject_toml(),
            )
        else {
            tracing::debug!(
                root = %member.root().display(),
                "uv_dependency_groups_parse_failed"
            );
            continue;
        };
        for group in dependency_groups.into_inner().values() {
            for requirement in &group.requirements {
                let mut requirement = UvRequirement::from(requirement.clone());
                normalize_lookahead_requirement(&mut requirement);
                push_non_registry_requirement(requirement, &mut lookahead_requirements);
            }
        }
    }

    let Some(tool_sources) = project_workspace
        .current_project()
        .pyproject_toml()
        .tool
        .as_ref()
        .and_then(|tool| tool.uv.as_ref())
        .and_then(|uv| uv.sources.as_ref())
        .map(uv_workspace::pyproject::ToolUvSources::inner)
    else {
        return Ok(UvWorkspaceSources {
            constraints: lookahead_requirements,
            overrides: Vec::new(),
        });
    };

    let mut overrides = Vec::new();
    for (name, package_sources) in tool_sources {
        let Some(source) = package_sources
            .iter()
            .filter(|source| source.extra().is_none() && source.group().is_none())
            .find(|source| source.marker().evaluate(marker_env, &[]))
        else {
            continue;
        };

        let uv_workspace::pyproject::Source::Workspace {
            workspace: true,
            editable,
            ..
        } = source
        else {
            continue;
        };

        let Some(member) = project_workspace.workspace().packages().get(name) else {
            tracing::debug!(
                source_name = %name,
                project_root = %project_root.display(),
                "uv_workspace_source_member_missing"
            );
            continue;
        };

        let editable = editable.unwrap_or(true);
        let url = VerbatimUrl::from_absolute_path(member.root())
            .map_err(|err| anyhow!("invalid workspace member url for {name}: {err}"))?
            .with_given(member.root().to_string_lossy());
        overrides.push(UvRequirement {
            name: name.clone(),
            extras: Box::new([]),
            groups: Box::new([]),
            marker: uv_pep508::MarkerTree::TRUE,
            source: RequirementSource::Directory {
                install_path: member.root().clone().into_boxed_path(),
                editable: Some(editable),
                r#virtual: Some(false),
                url,
            },
            origin: None,
        });
    }

    for requirement in requirements {
        let Some(package_sources) = tool_sources.get(&requirement.name) else {
            continue;
        };
        let Some(source) = package_sources
            .iter()
            .filter(|source| source.extra().is_none() && source.group().is_none())
            .find(|source| source.marker().evaluate(marker_env, &[]))
        else {
            continue;
        };

        let uv_workspace::pyproject::Source::Workspace {
            workspace: true,
            editable,
            ..
        } = source
        else {
            continue;
        };

        let Some(member) = project_workspace
            .workspace()
            .packages()
            .get(&requirement.name)
        else {
            return Err(anyhow!(
                "`tool.uv.sources` marks `{}` as `workspace = true`, but no workspace member named `{}` was found",
                requirement.name,
                requirement.name,
            ));
        };

        let editable = editable.unwrap_or(true);
        let url = VerbatimUrl::from_absolute_path(member.root())
            .map_err(|err| {
                anyhow!(
                    "invalid workspace member url for {}: {err}",
                    requirement.name
                )
            })?
            .with_given(member.root().to_string_lossy());
        requirement.source = RequirementSource::Directory {
            install_path: member.root().clone().into_boxed_path(),
            editable: Some(editable),
            r#virtual: Some(false),
            url,
        };
    }

    Ok(UvWorkspaceSources {
        constraints: lookahead_requirements,
        overrides,
    })
}

struct UvWorkspaceSources {
    constraints: Vec<UvRequirement>,
    overrides: Vec<UvRequirement>,
}

#[derive(Debug, Clone)]
pub enum ResolvedDistSource {
    Directory { path: PathBuf },
    Path { path: PathBuf },
    Url { url: String },
    Git { url: String },
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
            let dist_source = resolved_dist_source(dist);
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
                    dist_source,
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
                        let marker = requirement_marker(&requirement);
                        if marker.as_deref().is_some_and(marker_mentions_extra) {
                            child_builder.marker = None;
                        } else {
                            child_builder.marker = marker;
                        }
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
            dist_source: builder.dist_source,
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

fn parse_requirements(requirements: &[String], working_dir: &Path) -> Result<Vec<UvRequirement>> {
    let mut parsed = Vec::with_capacity(requirements.len());
    for raw in requirements {
        let req = UvPepRequirement::parse(raw, working_dir)
            .map_err(|err| anyhow!("invalid requirement `{raw}`: {err}"))?;
        parsed.push(UvRequirement::from(req));
    }
    Ok(parsed)
}

fn parse_direct_requirements(
    requirements: &[String],
    working_dir: &Path,
) -> Result<HashMap<String, UvPepRequirement>> {
    let mut map = HashMap::new();
    for raw in requirements {
        let requirement = UvPepRequirement::parse(raw, working_dir)
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

fn marker_mentions_extra(marker: &str) -> bool {
    marker
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
        .any(|token| token == "extra")
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
    dist_source: Option<ResolvedDistSource>,
}

#[derive(Debug, Clone)]
pub struct ResolveRequest {
    pub project: String,
    pub root: PathBuf,
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
    pub dist_source: Option<ResolvedDistSource>,
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

fn resolved_dist_source(dist: &ResolvedDist) -> Option<ResolvedDistSource> {
    let ResolvedDist::Installable { dist, .. } = dist else {
        return None;
    };
    match dist.as_ref() {
        uv_distribution_types::Dist::Built(built) => match built {
            uv_distribution_types::BuiltDist::Registry(_) => None,
            uv_distribution_types::BuiltDist::DirectUrl(built) => Some(ResolvedDistSource::Url {
                url: built.url.to_string(),
            }),
            uv_distribution_types::BuiltDist::Path(built) => Some(ResolvedDistSource::Path {
                path: built.install_path.as_ref().to_path_buf(),
            }),
        },
        uv_distribution_types::Dist::Source(source) => match source {
            uv_distribution_types::SourceDist::Registry(_) => None,
            uv_distribution_types::SourceDist::DirectUrl(source) => Some(ResolvedDistSource::Url {
                url: source.url.to_string(),
            }),
            uv_distribution_types::SourceDist::Git(source) => Some(ResolvedDistSource::Git {
                url: source.url.to_string(),
            }),
            uv_distribution_types::SourceDist::Path(source) => Some(ResolvedDistSource::Path {
                path: source.install_path.as_ref().to_path_buf(),
            }),
            uv_distribution_types::SourceDist::Directory(source) => {
                Some(ResolvedDistSource::Directory {
                    path: source.install_path.as_ref().to_path_buf(),
                })
            }
        },
    }
}

#[cfg(test)]
mod extras_tests {
    use std::env;

    use super::*;

    fn require_online() -> bool {
        if env::var("PX_ONLINE").ok().as_deref() == Some("1") {
            true
        } else {
            eprintln!("skipping resolver extras tests (PX_ONLINE!=1)");
            false
        }
    }

    fn dummy_env() -> ResolverEnv {
        ResolverEnv {
            implementation_name: "cpython".to_string(),
            implementation_version: "3.13.0".to_string(),
            os_name: "posix".to_string(),
            platform_machine: "x86_64".to_string(),
            platform_python_implementation: "CPython".to_string(),
            platform_release: "0".to_string(),
            platform_system: "Linux".to_string(),
            platform_version: "0".to_string(),
            python_full_version: "3.13.0".to_string(),
            python_version: "3.13".to_string(),
            sys_platform: "linux".to_string(),
        }
    }

    #[test]
    fn resolver_includes_extra_dependencies_for_requests_socks() -> Result<()> {
        if !require_online() {
            return Ok(());
        }
        let temp = tempfile::tempdir().expect("tempdir");
        let cache_dir = temp.path().join("cache");
        std::fs::create_dir_all(&cache_dir).expect("cache dir");
        let request = ResolveRequest {
            project: "demo".to_string(),
            root: temp.path().to_path_buf(),
            requirements: vec!["requests[socks]==2.32.3".to_string()],
            tags: ResolverTags::default(),
            env: dummy_env(),
            indexes: vec!["https://pypi.org/simple".to_string()],
            cache_dir,
            python: "/usr/bin/python3".to_string(),
        };
        let resolved = resolve(&request)?;
        let pysocks = resolved.iter().find(|spec| spec.normalized == "pysocks");
        assert!(
            pysocks.is_some(),
            "expected PySocks to be part of the resolved closure, got {:?}",
            resolved
                .iter()
                .map(|spec| spec.normalized.as_str())
                .collect::<Vec<_>>()
        );
        let pysocks = pysocks.expect("pysocks resolved");
        assert!(
            !pysocks.marker.as_deref().is_some_and(marker_mentions_extra),
            "expected pysocks marker to exclude `extra`, got {:?}",
            pysocks.marker
        );
        Ok(())
    }

    #[test]
    fn resolver_includes_extra_dependencies_for_cachecontrol_filecache() -> Result<()> {
        if !require_online() {
            return Ok(());
        }
        let temp = tempfile::tempdir().expect("tempdir");
        let cache_dir = temp.path().join("cache");
        std::fs::create_dir_all(&cache_dir).expect("cache dir");
        let request = ResolveRequest {
            project: "demo".to_string(),
            root: temp.path().to_path_buf(),
            requirements: vec!["cachecontrol[filecache]==0.14.3".to_string()],
            tags: ResolverTags::default(),
            env: dummy_env(),
            indexes: vec!["https://pypi.org/simple".to_string()],
            cache_dir,
            python: "/usr/bin/python3".to_string(),
        };
        let resolved = resolve(&request)?;
        let filelock = resolved.iter().find(|spec| spec.normalized == "filelock");
        assert!(
            filelock.is_some(),
            "expected filelock to be part of the resolved closure, got {:?}",
            resolved
                .iter()
                .map(|spec| spec.normalized.as_str())
                .collect::<Vec<_>>()
        );
        let filelock = filelock.expect("filelock resolved");
        assert!(
            !filelock.marker.as_deref().is_some_and(marker_mentions_extra),
            "expected filelock marker to exclude `extra`, got {:?}",
            filelock.marker
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn applies_uv_workspace_sources_for_workspace_deps() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let root = dir.path();
        let member_root = root.join("member");
        fs::create_dir_all(&member_root)?;

        fs::write(
            root.join("pyproject.toml"),
            r#"[project]
name = "root"
version = "0.1.0"
dependencies = ["member-pkg==0.1.0"]

[tool.uv.sources]
member-pkg = { workspace = true }

[tool.uv.workspace]
members = [".", "member"]
"#,
        )?;
        fs::write(
            member_root.join("pyproject.toml"),
            r#"[project]
name = "member-pkg"
version = "0.1.0"
"#,
        )?;

        let mut requirements = parse_requirements(&["member-pkg==0.1.0".to_string()], root)?;
        let marker_env =
            uv_pep508::MarkerEnvironment::try_from(uv_pep508::MarkerEnvironmentBuilder {
                implementation_name: "cpython",
                implementation_version: "3.12.0",
                os_name: "posix",
                platform_machine: "x86_64",
                platform_python_implementation: "CPython",
                platform_release: "0",
                platform_system: "Linux",
                platform_version: "0",
                python_full_version: "3.12.0",
                python_version: "3.12",
                sys_platform: "linux",
            })?;
        let cache = WorkspaceCache::default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let _lookaheads = runtime.block_on(apply_uv_workspace_sources(
            &mut requirements,
            root,
            &marker_env,
            &cache,
        ))?;

        let requirement = requirements
            .into_iter()
            .next()
            .expect("expected one requirement");
        match requirement.source {
            RequirementSource::Directory {
                install_path,
                editable,
                r#virtual,
                ..
            } => {
                assert_eq!(install_path.as_ref(), member_root.as_path());
                assert_eq!(editable, Some(true));
                assert_eq!(r#virtual, Some(false));
            }
            other => anyhow::bail!("expected Directory source, got {other:?}"),
        }

        Ok(())
    }

    #[test]
    fn parses_url_requirement_without_spaces() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let root = dir.path();
        let parsed = UvPepRequirement::parse(
            "sphinx-airflow-theme@https://example.invalid/theme-0.1.0-py3-none-any.whl",
            root,
        )?;
        let requirement = UvRequirement::from(parsed);
        assert!(
            matches!(requirement.source, RequirementSource::Url { .. }),
            "expected URL requirement source, got {:?}",
            requirement.source
        );
        Ok(())
    }

    #[test]
    #[ignore]
    fn debug_airflow_workspace_constraints_include_sphinx_theme() -> anyhow::Result<()> {
        let root = std::path::Path::new("/home/toxictoast/test/pythonstress/airflow");
        if !root.exists() {
            return Ok(());
        }
        let marker_env =
            uv_pep508::MarkerEnvironment::try_from(uv_pep508::MarkerEnvironmentBuilder {
                implementation_name: "cpython",
                implementation_version: "3.12.0",
                os_name: "posix",
                platform_machine: "x86_64",
                platform_python_implementation: "CPython",
                platform_release: "0",
                platform_system: "Linux",
                platform_version: "0",
                python_full_version: "3.12.0",
                python_version: "3.12",
                sys_platform: "linux",
            })?;
        let cache = WorkspaceCache::default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let mut requirements: Vec<UvRequirement> = Vec::new();
        let sources = runtime.block_on(apply_uv_workspace_sources(
            &mut requirements,
            root,
            &marker_env,
            &cache,
        ))?;
        assert!(
            sources.constraints.iter().any(|req| {
                req.name.as_ref() == "sphinx-airflow-theme"
                    && matches!(req.source, RequirementSource::Url { .. })
            }),
            "expected sphinx-airflow-theme URL constraint, got {} entries",
            sources.constraints.len()
        );
        Ok(())
    }
}
