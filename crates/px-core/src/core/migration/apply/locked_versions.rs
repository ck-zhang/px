use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result};
use pep440_rs::{Version, VersionSpecifiers};
use pep508_rs::{MarkerEnvironment, Requirement as PepRequirement};
use toml_edit::{DocumentMut, Item};

use px_domain::api::{autopin_pin_key, format_specifier, normalize_dist_name, PinSpec};

fn marker_expression_matches(marker_env: &MarkerEnvironment, expression: &str) -> bool {
    let requirement = format!("__px_marker_only__; {}", expression.trim());
    PepRequirement::from_str(&requirement)
        .map(|req| req.evaluate_markers(marker_env, &[]))
        .unwrap_or(false)
}

pub(super) fn uv_lock_versions(
    root: &Path,
    marker_env: &MarkerEnvironment,
) -> Result<Option<HashMap<String, String>>> {
    let path = root.join("uv.lock");
    if !path.exists() {
        return Ok(None);
    }
    let contents = fs::read_to_string(&path)
        .with_context(|| format!("failed to read uv.lock at {}", path.display()))?;
    let doc: DocumentMut = contents
        .parse()
        .with_context(|| format!("failed to parse uv.lock at {}", path.display()))?;
    let Some(packages) = doc.get("package").and_then(Item::as_array_of_tables) else {
        return Ok(None);
    };
    let mut versions: HashMap<String, (String, usize)> = HashMap::new();
    for package in packages.iter() {
        let Some(name) = package.get("name").and_then(Item::as_str) else {
            continue;
        };
        let Some(version) = package.get("version").and_then(Item::as_str) else {
            continue;
        };
        let resolution_markers = package
            .get("resolution-markers")
            .and_then(Item::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|value| value.as_str().map(std::string::ToString::to_string))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if !resolution_markers.is_empty()
            && !resolution_markers
                .iter()
                .any(|expr| marker_expression_matches(marker_env, expr))
        {
            continue;
        }
        let normalized = normalize_dist_name(name);
        let specificity = resolution_markers.len();
        match versions.get(&normalized) {
            Some((_, existing_specificity)) if *existing_specificity >= specificity => {}
            _ => {
                versions.insert(normalized, (version.to_string(), specificity));
            }
        }
    }
    if versions.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        versions
            .into_iter()
            .map(|(name, (version, _))| (name, version))
            .collect(),
    ))
}

pub(super) fn poetry_lock_versions(root: &Path) -> Result<Option<HashMap<String, String>>> {
    let path = root.join("poetry.lock");
    if !path.exists() {
        return Ok(None);
    }
    let contents = fs::read_to_string(&path)
        .with_context(|| format!("failed to read poetry.lock at {}", path.display()))?;
    let doc: DocumentMut = contents
        .parse()
        .with_context(|| format!("failed to parse poetry.lock at {}", path.display()))?;
    let Some(packages) = doc.get("package").and_then(Item::as_array_of_tables) else {
        return Ok(None);
    };
    let mut versions = HashMap::new();
    for package in packages.iter() {
        let Some(name) = package.get("name").and_then(Item::as_str) else {
            continue;
        };
        let Some(version) = package.get("version").and_then(Item::as_str) else {
            continue;
        };
        versions
            .entry(normalize_dist_name(name))
            .or_insert_with(|| version.to_string());
    }
    Ok(Some(versions))
}

pub(super) fn merge_pin_sets(existing: &mut Vec<PinSpec>, extra: Vec<PinSpec>) {
    let mut seen: HashSet<String> = existing.iter().map(autopin_pin_key).collect();
    for pin in extra {
        let key = autopin_pin_key(&pin);
        if seen.insert(key) {
            existing.push(pin);
        }
    }
}

pub(super) enum LockPinChoice {
    Reuse { pin: PinSpec, source: String },
    Skip(String),
}

pub(super) fn pin_from_locked_versions(
    spec: &str,
    versions: &HashMap<String, String>,
    marker_env: &MarkerEnvironment,
    source_label: &str,
) -> LockPinChoice {
    let requirement = match PepRequirement::from_str(spec.trim()) {
        Ok(req) => req,
        Err(err) => return LockPinChoice::Skip(err.to_string()),
    };
    if !requirement.evaluate_markers(marker_env, &[]) {
        return LockPinChoice::Skip("markers_do_not_apply".to_string());
    }
    let name = requirement.name.to_string();
    let normalized = normalize_dist_name(&name);
    let Some(version) = versions.get(&normalized) else {
        return LockPinChoice::Skip(format!("not_in_{source_label}"));
    };

    // Ensure the locked version satisfies the current specifiers.
    let satisfies = match &requirement.version_or_url {
        Some(pep508_rs::VersionOrUrl::VersionSpecifier(specifiers)) => {
            VersionSpecifiers::from_str(&specifiers.to_string())
                .ok()
                .and_then(|specs| {
                    Version::from_str(version)
                        .ok()
                        .map(|ver| specs.contains(&ver))
                })
                .unwrap_or(false)
        }
        Some(pep508_rs::VersionOrUrl::Url(_)) => false,
        None => false,
    };
    if !satisfies {
        return LockPinChoice::Skip(format!(
            "{source_label} has {version} which does not satisfy current spec"
        ));
    }

    let extras = px_domain::api::canonical_extras(
        &requirement
            .extras
            .iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>(),
    );
    let marker = requirement.marker.as_ref().map(ToString::to_string);
    let specifier = format_specifier(&normalized, &extras, version, marker.as_deref());
    let normalized_label = normalized.clone();
    LockPinChoice::Reuse {
        pin: PinSpec {
            name,
            specifier,
            version: version.clone(),
            normalized,
            extras,
            marker,
            direct: true,
            requires: Vec::new(),
        },
        source: format!("{normalized_label}=={version} ({source_label})"),
    }
}
