#![deny(clippy::all, warnings)]

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    iter::FromIterator,
    path::Path,
    str::FromStr,
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Context, Result};
use pep440_rs::{Version, VersionSpecifiers};
use pep508_rs::{
    ExtraName, MarkerEnvironment, Requirement as PepRequirement, StringVersion, VersionOrUrl,
};
use reqwest::blocking::Client;
use serde::Deserialize;
use url::Url;

use crate::manifest::dependency_name;

const PYPI_BASE: &str = "https://pypi.org/pypi";
const MAX_CANDIDATES: usize = 20;

#[derive(Debug, Clone)]
pub struct ResolveRequest {
    pub project: String,
    pub requirements: Vec<String>,
    pub tags: ResolverTags,
    pub env: ResolverEnv,
    pub indexes: Vec<String>,
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
}

#[derive(Debug, Clone)]
struct RequirementFrame {
    raw: String,
    parent_extras: Vec<ExtraName>,
    direct: bool,
}

#[derive(Debug, Clone)]
struct Candidate {
    version: Version,
    version_string: String,
    files: Vec<ReleaseFile>,
    requires: Vec<(String, Vec<ExtraName>)>,
}

#[derive(Debug, Clone, Default)]
struct ConstraintSet {
    specs: Vec<VersionSpecifiers>,
}

impl ConstraintSet {
    fn add(&mut self, spec: VersionSpecifiers) {
        self.specs.push(spec);
    }

    fn allows(&self, version: &Version) -> bool {
        self.specs.iter().all(|spec| spec.contains(version))
    }

    fn display(&self) -> String {
        self.specs
            .iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>()
            .join(" && ")
    }
}

pub fn resolve(request: &ResolveRequest) -> Result<Vec<ResolvedSpecifier>> {
    if request.requirements.is_empty() {
        return Ok(Vec::new());
    }

    let indexes = if request.indexes.is_empty() {
        vec![PYPI_BASE.to_string()]
    } else {
        request.indexes.clone()
    };
    let marker_env = request.env.to_marker_environment()?;
    let mut ctx = ResolverContext::new(request, marker_env, indexes)?;

    let initial: Vec<RequirementFrame> = request
        .requirements
        .iter()
        .map(|req| RequirementFrame {
            raw: req.clone(),
            parent_extras: Vec::new(),
            direct: true,
        })
        .collect();

    let result = ctx.solve(initial)?;
    Ok(result)
}

struct ResolverContext<'a> {
    request: &'a ResolveRequest,
    marker_env: MarkerEnvironment,
    client: Client,
    indexes: Vec<String>,
    constraints: HashMap<String, ConstraintSet>,
    resolved: HashMap<String, ResolvedSpecifier>,
    project_cache: HashMap<(String, String), ProjectResponse>,
    version_cache: HashMap<(String, String, String), VersionResponse>,
    steps: usize,
    started: Instant,
}

impl<'a> ResolverContext<'a> {
    fn new(
        request: &'a ResolveRequest,
        marker_env: MarkerEnvironment,
        indexes: Vec<String>,
    ) -> Result<Self> {
        Ok(Self {
            request,
            marker_env,
            client: build_http_client()?,
            indexes,
            constraints: HashMap::new(),
            resolved: HashMap::new(),
            project_cache: HashMap::new(),
            version_cache: HashMap::new(),
            steps: 0,
            started: Instant::now(),
        })
    }

    fn solve(&mut self, mut pending: Vec<RequirementFrame>) -> Result<Vec<ResolvedSpecifier>> {
        if pending.is_empty() {
            return Ok(Vec::new());
        }
        self.prefetch_projects(&pending);
        if self.backtrack(&mut pending)? {
            let mut out: Vec<_> = self.resolved.values().cloned().collect();
            out.sort_by(|a, b| a.normalized.cmp(&b.normalized));
            Ok(out)
        } else {
            bail!("dependency resolution failed: incompatible requirements")
        }
    }

    fn backtrack(&mut self, pending: &mut Vec<RequirementFrame>) -> Result<bool> {
        while let Some(frame) = pending.pop() {
            self.steps += 1;
            if self.steps > 5000 {
                bail!("dependency resolution exceeded search limit");
            }
            if self.started.elapsed() > Duration::from_secs(20) {
                bail!("dependency resolution timed out");
            }
            let Some(req) = self.parse_requirement(&frame)? else {
                continue;
            };
            let name = req.normalized.clone();

            self.constraints
                .entry(name.clone())
                .or_default()
                .add(req.specifiers.clone());

            if let Some(existing) = self.resolved.get(&name) {
                if let Ok(version) = Version::from_str(&existing.selected_version) {
                    if !self.constraints[&name].allows(&version) {
                        return Ok(false);
                    }
                    Self::enqueue_requires(pending, &existing.requires, false);
                    continue;
                } else {
                    return Ok(false);
                }
            }

            let mut candidates = self.candidates_for_requirement(&req)?;
            candidates.sort_by(|a, b| b.version.cmp(&a.version));
            if candidates.len() > MAX_CANDIDATES {
                candidates.truncate(MAX_CANDIDATES);
            }

            let snapshot_constraints = self.constraints.clone();
            let snapshot_resolved = self.resolved.clone();

            for candidate in candidates {
                if !self.constraints[&name].allows(&candidate.version) {
                    continue;
                }
                let mut spec = ResolvedSpecifier {
                    name: req.original_name.clone(),
                    specifier: req.original_spec.clone(),
                    normalized: name.clone(),
                    selected_version: candidate.version_string.clone(),
                    extras: req.extras.clone(),
                    marker: req.marker.clone(),
                    requires: Vec::new(),
                    direct: frame.direct,
                };
                let child_requires: Vec<String> = candidate
                    .requires
                    .iter()
                    .filter_map(|(raw, _)| {
                        let dep = dependency_name(raw);
                        if dep.is_empty() {
                            None
                        } else {
                            Some(dep)
                        }
                    })
                    .collect();
                spec.requires = child_requires;
                self.resolved.insert(name.clone(), spec);

                let mut new_pending = pending.clone();
                for (child, extras) in candidate.requires {
                    new_pending.push(RequirementFrame {
                        raw: child,
                        parent_extras: extras,
                        direct: false,
                    });
                }

                if self.backtrack(&mut new_pending)? {
                    *pending = new_pending;
                    return Ok(true);
                }

                self.constraints = snapshot_constraints.clone();
                self.resolved = snapshot_resolved.clone();
            }

            return Ok(false);
        }
        Ok(true)
    }

    fn conflict_report(&self) -> String {
        let mut lines = Vec::new();
        lines.push("dependency resolution failed due to conflicts:".to_string());
        let mut entries: Vec<_> = self.constraints.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        for (name, set) in entries {
            let picked = self
                .resolved
                .get(name)
                .map(|spec| spec.selected_version.clone())
                .unwrap_or_else(|| "<none>".to_string());
            lines.push(format!(
                "  - {name}: picked {picked}, constraints [{}]",
                set.display()
            ));
        }
        lines.join("\n")
    }

    fn parse_requirement(&self, frame: &RequirementFrame) -> Result<Option<ParsedRequirement>> {
        let requirement = PepRequirement::from_str(&frame.raw)
            .map_err(|err| anyhow!("failed to parse requirement `{}`: {err}", frame.raw))?;
        if !requirement.evaluate_markers(&self.marker_env, &frame.parent_extras) {
            return Ok(None);
        }
        let normalized = normalize_dist_name(requirement.name.as_ref());
        let extras = normalized_extras(&requirement);
        let marker = requirement
            .marker
            .as_ref()
            .map(std::string::ToString::to_string);

        let specifiers = match requirement.version_or_url.as_ref() {
            Some(VersionOrUrl::VersionSpecifier(spec)) => {
                let spec_str = spec.to_string();
                VersionSpecifiers::from_str(&spec_str)
                    .map_err(|err| anyhow!("failed to parse specifiers `{spec_str}`: {err}"))?
            }
            Some(VersionOrUrl::Url(_)) => VersionSpecifiers::from_iter([]),
            None => VersionSpecifiers::from_iter([]),
        };

        Ok(Some(ParsedRequirement {
            original_name: requirement.name.to_string(),
            original_spec: requirement.to_string(),
            normalized,
            extras,
            marker,
            specifiers,
            raw: frame.raw.clone(),
            direct_url: matches!(requirement.version_or_url, Some(VersionOrUrl::Url(_))),
            pep: requirement,
        }))
    }

    fn candidates_for_requirement(&mut self, req: &ParsedRequirement) -> Result<Vec<Candidate>> {
        if req.direct_url {
            return self.direct_url_candidate(req);
        }
        let mut all_versions = Vec::new();
        for index in self.indexes.clone() {
            if let Some(project) = self.fetch_project(&index, &req.normalized)? {
                for (version_str, files) in &project.releases {
                    let Ok(version) = Version::from_str(version_str) else {
                        continue;
                    };
                    all_versions.push((version, version_str.clone(), files.clone(), index.clone()));
                }
            }
        }
        all_versions.sort_by(|a, b| b.0.cmp(&a.0));
        all_versions.dedup_by(|a, b| a.0 == b.0);

        let constraints = self.constraints.get(&req.normalized).cloned();
        let mut candidates = Vec::new();
        for (version, version_str, files, index) in all_versions {
            if !req.specifiers.contains(&version)
                || constraints
                    .as_ref()
                    .is_some_and(|set| !set.allows(&version))
                || !Self::files_match_tags(&files, &self.request.tags)
            {
                continue;
            }
            let requires = self
                .fetch_version_requires(&index, &req.normalized, &version_str)
                .unwrap_or_default();
            candidates.push(Candidate {
                version,
                version_string: version_str,
                files,
                requires,
            });
        }

        Ok(candidates)
    }

    fn direct_url_candidate(&self, req: &ParsedRequirement) -> Result<Vec<Candidate>> {
        let Some(VersionOrUrl::Url(url)) = req.pep.version_or_url.as_ref() else {
            return Ok(Vec::new());
        };
        let url = Url::parse(url.as_str())?;
        let filename = url
            .path_segments()
            .and_then(|mut segments| segments.next_back())
            .ok_or_else(|| anyhow!("direct URL missing filename"))?;
        let (version_str, requires) = if filename.ends_with(".whl") {
            let (name_part, version_part, tags) = parse_wheel_filename(filename)
                .ok_or_else(|| anyhow!("invalid wheel filename in direct URL: {filename}"))?;
            if normalize_dist_name(&name_part) != req.normalized {
                bail!(
                    "direct URL name `{name_part}` does not match requirement `{}`",
                    req.normalized
                );
            }
            if !Self::wheel_tags_match(&tags, &self.request.tags) {
                bail!("wheel tags for {filename} are incompatible with current platform");
            }
            (version_part, Vec::new())
        } else {
            let version_part = parse_sdist_version(filename).ok_or_else(|| {
                anyhow!("unable to parse version from direct URL filename: {filename}")
            })?;
            (version_part, Vec::new())
        };
        let version =
            Version::from_str(&version_str).map_err(|err| anyhow!("invalid version: {err}"))?;
        Ok(vec![Candidate {
            version,
            version_string: version_str,
            files: Vec::new(),
            requires,
        }])
    }

    fn fetch_project(&mut self, index: &str, normalized: &str) -> Result<Option<ProjectResponse>> {
        if let Some(cached) = self
            .project_cache
            .get(&(index.to_string(), normalized.to_string()))
        {
            return Ok(Some(cached.clone()));
        }
        let url = format!("{index}/{normalized}/json");
        match self
            .client
            .get(&url)
            .send()
            .and_then(|res| res.error_for_status())
        {
            Ok(response) => {
                let parsed: ProjectResponse = response
                    .json()
                    .context("invalid project metadata response")?;
                self.project_cache
                    .insert((index.to_string(), normalized.to_string()), parsed.clone());
                Ok(Some(parsed))
            }
            Err(err) => {
                if index == PYPI_BASE {
                    Err(anyhow!("failed to query PyPI for {normalized}: {err}"))
                } else {
                    Ok(None)
                }
            }
        }
    }

    fn fetch_version_requires(
        &mut self,
        index: &str,
        normalized: &str,
        version: &str,
    ) -> Result<Vec<(String, Vec<ExtraName>)>> {
        if let Some(cached) = self.version_cache.get(&(
            index.to_string(),
            normalized.to_string(),
            version.to_string(),
        )) {
            return Ok(parse_requires(&cached.info.requires_dist));
        }
        let url = format!("{index}/{normalized}/{version}/json");
        let response = self
            .client
            .get(&url)
            .send()
            .with_context(|| format!("failed to query {index} for {normalized} {version}"))?
            .error_for_status()
            .context("index responded with error")?;
        let parsed: VersionResponse = response
            .json()
            .context("invalid JSON in version metadata")?;
        self.version_cache.insert(
            (
                index.to_string(),
                normalized.to_string(),
                version.to_string(),
            ),
            parsed.clone(),
        );
        Ok(parse_requires(&parsed.info.requires_dist))
    }

    fn prefetch_projects(&mut self, frames: &[RequirementFrame]) {
        let names: HashSet<String> = frames
            .iter()
            .filter_map(|frame| {
                PepRequirement::from_str(&frame.raw)
                    .ok()
                    .map(|req| normalize_dist_name(req.name.as_ref()))
            })
            .collect();
        for name in names {
            for index in &self.indexes {
                if self
                    .project_cache
                    .contains_key(&(index.to_string(), name.clone()))
                {
                    break;
                }
                let url = format!("{index}/{name}/json");
                if let Ok(resp) = self
                    .client
                    .get(&url)
                    .send()
                    .and_then(|r| r.error_for_status())
                {
                    if let Ok(parsed) = resp.json::<ProjectResponse>() {
                        self.project_cache
                            .insert((index.to_string(), name.clone()), parsed);
                        break;
                    }
                }
            }
        }
    }

    fn files_match_tags(files: &[ReleaseFile], tags: &ResolverTags) -> bool {
        files.iter().any(|file| {
            if file.yanked.unwrap_or(false) {
                return false;
            }
            match file.packagetype.as_str() {
                "bdist_wheel" => {
                    if let Some((py, abi, platform)) = parse_wheel_tags(&file.filename) {
                        wheel_matches(&py, &abi, &platform, tags)
                    } else {
                        false
                    }
                }
                "sdist" => true,
                _ => false,
            }
        })
    }

    fn wheel_tags_match(tags: &(String, String, String), wanted: &ResolverTags) -> bool {
        let (py, abi, platform) = tags;
        wheel_matches(py, abi, platform, wanted)
    }

    fn enqueue_requires(pending: &mut Vec<RequirementFrame>, requires: &[String], direct: bool) {
        for child in requires {
            pending.push(RequirementFrame {
                raw: child.clone(),
                parent_extras: Vec::new(),
                direct,
            });
        }
    }
}

#[derive(Debug, Clone)]
struct ParsedRequirement {
    original_name: String,
    original_spec: String,
    normalized: String,
    extras: Vec<String>,
    marker: Option<String>,
    specifiers: VersionSpecifiers,
    raw: String,
    direct_url: bool,
    pep: PepRequirement,
}

fn build_http_client() -> Result<Client> {
    Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(format!("px-resolver/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("failed to build HTTP client")
}

fn parse_requires(values: &Option<Vec<String>>) -> Vec<(String, Vec<ExtraName>)> {
    let mut out = Vec::new();
    for entry in values.clone().unwrap_or_default() {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(req) = PepRequirement::from_str(trimmed) {
            out.push((trimmed.to_string(), req.extras.clone()));
        }
    }
    out
}

#[derive(Debug, Deserialize, Clone)]
struct ProjectResponse {
    releases: BTreeMap<String, Vec<ReleaseFile>>,
}

#[derive(Debug, Deserialize, Clone)]
struct VersionResponse {
    info: VersionInfo,
}

#[derive(Debug, Deserialize, Clone)]
struct VersionInfo {
    #[serde(default)]
    requires_dist: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Clone)]
struct ReleaseFile {
    filename: String,
    url: String,
    packagetype: String,
    yanked: Option<bool>,
}

pub fn normalize_dist_name(name: &str) -> String {
    name.to_ascii_lowercase().replace(['_', '.'], "-")
}

impl ResolverEnv {
    pub fn to_marker_environment(&self) -> Result<MarkerEnvironment> {
        Ok(MarkerEnvironment {
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

fn normalized_extras(requirement: &PepRequirement) -> Vec<String> {
    let mut extras = requirement
        .extras
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>();
    extras.sort();
    extras.dedup();
    extras
}

fn parse_wheel_tags(filename: &str) -> Option<(String, String, String)> {
    let path = Path::new(filename);
    if !path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("whl"))
    {
        return None;
    }
    let stem = path.file_stem()?.to_str()?;
    let parts: Vec<&str> = stem.split('-').collect();
    if parts.len() < 5 {
        return None;
    }
    Some((
        parts[parts.len() - 3].to_string(),
        parts[parts.len() - 2].to_string(),
        parts[parts.len() - 1].to_string(),
    ))
}

fn parse_wheel_filename(filename: &str) -> Option<(String, String, (String, String, String))> {
    let path = Path::new(filename);
    let stem = path.file_stem()?.to_str()?;
    let parts: Vec<&str> = stem.split('-').collect();
    if parts.len() < 5 {
        return None;
    }
    let name = parts[..parts.len() - 4].join("-");
    let version = parts[parts.len() - 4].to_string();
    let tags = (
        parts[parts.len() - 3].to_string(),
        parts[parts.len() - 2].to_string(),
        parts[parts.len() - 1].to_string(),
    );
    Some((name, version, tags))
}

fn parse_sdist_version(filename: &str) -> Option<String> {
    let stem = Path::new(filename)
        .file_stem()
        .and_then(|name| name.to_str())?;
    let trimmed = stem.strip_suffix(".tar").unwrap_or(stem);
    let mut parts = trimmed.rsplitn(2, '-');
    let version = parts.next()?;
    Some(version.to_string())
}

fn wheel_matches(py: &str, abi: &str, platform: &str, tags: &ResolverTags) -> bool {
    (py.eq_ignore_ascii_case("py3") || matches_any(&tags.python, py))
        && (abi.eq_ignore_ascii_case("none") || matches_any(&tags.abi, abi))
        && (platform.eq_ignore_ascii_case("any") || matches_any(&tags.platform, platform))
}

fn matches_any(values: &[String], candidate: &str) -> bool {
    candidate
        .split('.')
        .any(|part| values.iter().any(|val| part.eq_ignore_ascii_case(val)))
}
