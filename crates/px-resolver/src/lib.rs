


use std::{collections::BTreeMap, str::FromStr};

use anyhow::{anyhow, bail, Context, Result};
use pep440_rs::{Operator, Version, VersionSpecifiers};
use pep508_rs::{MarkerEnvironment, Requirement as PepRequirement, StringVersion, VersionOrUrl};
use reqwest::blocking::Client;
use serde::Deserialize;

const PYPI_BASE: &str = "https://pypi.org/pypi";


#[derive(Debug, Clone)]
pub struct ResolveRequest {
    pub project: String,
    pub requirements: Vec<String>,
    pub tags: ResolverTags,
    pub env: ResolverEnv,
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
}

pub fn resolve(request: ResolveRequest) -> Result<Vec<ResolvedSpecifier>> {
    if request.requirements.is_empty() {
        return Ok(Vec::new());
    }

    let client = build_http_client()?;
    let marker_env = request.env.to_marker_environment()?;
    request
        .requirements
        .iter()
        .filter_map(
            |req| match resolve_requirement(&client, req, &request.tags, &marker_env) {
                Ok(Some(spec)) => Some(Ok(spec)),
                Ok(None) => None,
                Err(err) => Some(Err(err)),
            },
        )
        .collect()
}

fn resolve_requirement(
    client: &Client,
    requirement_str: &str,
    tags: &ResolverTags,
    marker_env: &MarkerEnvironment,
) -> Result<Option<ResolvedSpecifier>> {
    let requirement = PepRequirement::from_str(requirement_str)
        .map_err(|err| anyhow!("failed to parse requirement `{}`: {err}", requirement_str))?;

    if !requirement.evaluate_markers(marker_env, &[]) {
        return Ok(None);
    }

    let normalized = normalize_dist_name(requirement.name.as_ref());
    let extras = normalized_extras(&requirement);
    let marker = requirement.marker.as_ref().map(|m| m.to_string());

    if let Some(version) = pinned_version(&requirement) {
        return Ok(Some(ResolvedSpecifier {
            name: requirement.name.to_string(),
            specifier: requirement.to_string(),
            normalized,
            selected_version: version,
            extras,
            marker,
        }));
    }

    let specifiers = match requirement.version_or_url.as_ref() {
        Some(VersionOrUrl::VersionSpecifier(spec)) => {
            VersionSpecifiers::from_str(&spec.to_string())
                .map_err(|err| anyhow!("failed to parse specifiers `{}`: {err}", spec))?
        }
        Some(VersionOrUrl::Url(_)) => {
            bail!(
                "URL requirements are not supported by the resolver yet: `{}`",
                requirement_str
            )
        }
        None => VersionSpecifiers::from_iter(std::iter::empty()),
    };

    let project = fetch_project(client, &normalized)?;
    let Some(selected) = select_version(&project.releases, &specifiers, tags) else {
        bail!(
            "unable to resolve `{}`: no compatible release found",
            requirement_str
        );
    };

    Ok(Some(ResolvedSpecifier {
        name: requirement.name.to_string(),
        specifier: requirement.to_string(),
        normalized,
        selected_version: selected,
        extras,
        marker,
    }))
}

fn build_http_client() -> Result<Client> {
    Client::builder()
        .user_agent(format!("px-resolver/{}", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(30))
        .no_proxy()
        .build()
        .context("failed to build HTTP client")
}

fn fetch_project(client: &Client, name: &str) -> Result<ProjectResponse> {
    let url = format!("{PYPI_BASE}/{name}/json");
    client
        .get(&url)
        .send()
        .map_err(|err| anyhow!("failed to query PyPI for {name}: {err}"))?
        .error_for_status()
        .map_err(|err| anyhow!("PyPI error for {name}: {err}"))?
        .json::<ProjectResponse>()
        .map_err(|err| anyhow!("invalid JSON from PyPI for {name}: {err}"))
}

#[derive(Debug, Deserialize)]
struct ProjectResponse {
    releases: BTreeMap<String, Vec<ReleaseFile>>,
}

#[derive(Debug, Deserialize)]
struct ReleaseFile {
    filename: String,
    packagetype: String,
    yanked: Option<bool>,
}

fn normalize_dist_name(name: &str) -> String {
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
    StringVersion::from_str(value).map_err(|err| {
        anyhow!(
            "`{}` is not a valid PEP 440 version for `{}`: {}",
            value,
            field,
            err
        )
    })
}

fn normalized_extras(requirement: &PepRequirement) -> Vec<String> {
    let mut extras = requirement
        .extras
        .iter()
        .map(|extra| extra.to_string())
        .collect::<Vec<_>>();
    extras.sort();
    extras.dedup();
    extras
}

fn pinned_version(requirement: &PepRequirement) -> Option<String> {
    let version_or_url = requirement.version_or_url.as_ref()?;
    let VersionOrUrl::VersionSpecifier(specifiers) = version_or_url else {
        return None;
    };
    let parsed = VersionSpecifiers::from_str(&specifiers.to_string()).ok()?;
    let mut iter = parsed.iter();
    let first = iter.next()?;
    if iter.next().is_some() {
        return None;
    }
    match first.operator() {
        Operator::Equal | Operator::ExactEqual => Some(first.version().to_string()),
        _ => None,
    }
}

fn select_version(
    releases: &BTreeMap<String, Vec<ReleaseFile>>,
    specifiers: &VersionSpecifiers,
    tags: &ResolverTags,
) -> Option<String> {
    let mut best: Option<(Version, u8, String)> = None;
    for (version_str, files) in releases {
        let Ok(candidate) = Version::from_str(version_str) else {
            continue;
        };
        if !specifiers.contains(&candidate) {
            continue;
        }
        if let Some(score) = release_score(files, tags) {
            let replace = match &best {
                Some((best_version, best_score, _)) => {
                    candidate > *best_version || (candidate == *best_version && score > *best_score)
                }
                None => true,
            };
            if replace {
                best = Some((candidate.clone(), score, version_str.clone()));
            }
        }
    }
    best.map(|(_, _, version)| version)
}

fn release_score(files: &[ReleaseFile], tags: &ResolverTags) -> Option<u8> {
    let mut score = None;
    for file in files {
        if file.packagetype != "bdist_wheel" || file.yanked.unwrap_or(false) {
            continue;
        }
        if let Some((python_tag, abi_tag, platform_tag)) = parse_wheel_tags(&file.filename) {
            if wheel_matches(&python_tag, &abi_tag, &platform_tag, tags) {
                return Some(2);
            }
            if python_tag.eq_ignore_ascii_case("py3")
                && abi_tag.eq_ignore_ascii_case("none")
                && platform_tag.eq_ignore_ascii_case("any")
            {
                score = score.max(Some(1));
            }
        }
    }
    if score.is_some() {
        return score;
    }
    if files
        .iter()
        .any(|file| file.packagetype == "sdist" && !file.yanked.unwrap_or(false))
    {
        return Some(0);
    }
    None
}

fn parse_wheel_tags(filename: &str) -> Option<(String, String, String)> {
    if !filename.ends_with(".whl") {
        return None;
    }
    let trimmed = filename.trim_end_matches(".whl");
    let parts: Vec<&str> = trimmed.split('-').collect();
    if parts.len() < 5 {
        return None;
    }
    Some((
        parts[parts.len() - 3].to_string(),
        parts[parts.len() - 2].to_string(),
        parts[parts.len() - 1].to_string(),
    ))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn sample_env() -> ResolverEnv {
        ResolverEnv {
            implementation_name: "cpython".into(),
            implementation_version: "3.12.0".into(),
            os_name: "posix".into(),
            platform_machine: "x86_64".into(),
            platform_python_implementation: "CPython".into(),
            platform_release: "6.0".into(),
            platform_system: "Linux".into(),
            platform_version: "6.0".into(),
            python_full_version: "3.12.0".into(),
            python_version: "3.12".into(),
            sys_platform: "linux".into(),
        }
    }

    fn sample_tags() -> ResolverTags {
        ResolverTags {
            python: vec!["cp312".into(), "py312".into(), "py3".into()],
            abi: vec!["cp312".into(), "abi3".into(), "none".into()],
            platform: vec!["manylinux_2_17_x86_64".into(), "any".into()],
        }
    }

    #[test]
    fn resolves_packaging_range() -> Result<()> {
        if std::env::var("PX_ONLINE").ok().as_deref() != Some("1") {
            eprintln!("skipping resolves_packaging_range (PX_ONLINE!=1)");
            return Ok(());
        }

        let request = ResolveRequest {
            project: "demo".into(),
            requirements: vec!["packaging>=24,<25".into()],
            tags: ResolverTags::default(),
            env: sample_env(),
        };
        let resolved = resolve(request)?;
        assert_eq!(resolved.len(), 1);
        let spec = &resolved[0];
        assert_eq!(spec.normalized, "packaging");
        assert!(spec.selected_version.starts_with("24."));
        Ok(())
    }

    #[test]
    fn normalized_extras_are_sorted() {
        let req = PepRequirement::from_str(r#"demo[tests,security,security]==1.0"#).unwrap();
        let extras = normalized_extras(&req);
        assert_eq!(extras, vec!["security", "tests"]);
    }

    #[test]
    fn markers_follow_python_version() {
        let env = sample_env().to_marker_environment().unwrap();
        let req = PepRequirement::from_str(r#"demo>=1 ; python_version >= "3.11""#).unwrap();
        assert!(req.evaluate_markers(&env, &[]));
        let req = PepRequirement::from_str(r#"demo>=1 ; python_version < "3.0""#).unwrap();
        assert!(!req.evaluate_markers(&env, &[]));
    }

    #[test]
    fn select_version_prefers_matching_tags() {
        let mut releases: BTreeMap<String, Vec<ReleaseFile>> = BTreeMap::new();
        releases.insert(
            "1.0.0".into(),
            vec![ReleaseFile {
                filename: "demo-1.0.0-py3-none-any.whl".into(),
                packagetype: "bdist_wheel".into(),
                yanked: Some(false),
            }],
        );
        releases.insert(
            "1.1.0".into(),
            vec![ReleaseFile {
                filename: "demo-1.1.0-cp312-cp312-manylinux_2_17_x86_64.whl".into(),
                packagetype: "bdist_wheel".into(),
                yanked: Some(false),
            }],
        );
        let tags = sample_tags();
        let specifiers = VersionSpecifiers::from_str(">=1.0").unwrap();
        let selected = select_version(&releases, &specifiers, &tags);
        assert_eq!(selected.as_deref(), Some("1.1.0"));
    }
}
