use std::str::FromStr;

use pep508_rs::{MarkerEnvironment, Requirement as PepRequirement};

use crate::manifest::{canonicalize_marker, dependency_name, strip_wrapping_quotes};

#[derive(Clone, Debug)]
pub struct PinSpec {
    pub name: String,
    pub specifier: String,
    pub version: String,
    pub normalized: String,
    pub extras: Vec<String>,
    pub marker: Option<String>,
}

#[derive(Clone, Debug)]
pub struct InstallOverride {
    pub dependencies: Vec<String>,
    pub pins: Vec<PinSpec>,
}

#[derive(Clone, Debug)]
pub struct ResolvedSpecOutput {
    pub specs: Vec<String>,
    pub pins: Vec<PinSpec>,
}

pub fn merge_resolved_dependencies(
    original: &[String],
    resolved: &[String],
    marker_env: &MarkerEnvironment,
) -> Vec<String> {
    let mut merged = Vec::with_capacity(original.len());
    let mut resolved_iter = resolved.iter();
    for spec in original {
        if spec_requires_pin(spec) && marker_applies(spec, marker_env) {
            if let Some(pinned) = resolved_iter.next() {
                merged.push(pinned.clone());
            } else {
                merged.push(spec.clone());
            }
        } else {
            merged.push(spec.clone());
        }
    }
    merged
}

pub fn marker_applies(spec: &str, marker_env: &MarkerEnvironment) -> bool {
    let cleaned = strip_wrapping_quotes(spec.trim());
    match PepRequirement::from_str(cleaned) {
        Ok(req) => req.evaluate_markers(marker_env, &[]),
        Err(_) => true,
    }
}

pub fn spec_requires_pin(spec: &str) -> bool {
    let head = spec.split(';').next().unwrap_or(spec).trim();
    !head.contains("==")
}

pub fn autopin_spec_key(spec: &str) -> String {
    if let Ok(req) = PepRequirement::from_str(spec.trim()) {
        let name = req.name.to_string().to_ascii_lowercase();
        let mut extras = req
            .extras
            .iter()
            .map(|extra| extra.to_string().to_ascii_lowercase())
            .collect::<Vec<_>>();
        extras.sort();
        let extras_part = extras.join(",");
        let marker_part = req
            .marker
            .as_ref()
            .map(|m| canonicalize_marker(&m.to_string()))
            .unwrap_or_default();
        format!("{name}|{extras_part}|{marker_part}")
    } else {
        let name = dependency_name(spec);
        format!("{name}||")
    }
}

pub fn autopin_pin_key(pin: &PinSpec) -> String {
    let mut extras = pin
        .extras
        .iter()
        .map(|extra| extra.to_ascii_lowercase())
        .collect::<Vec<_>>();
    extras.sort();
    let extras_part = extras.join(",");
    let marker_part = pin
        .marker
        .as_deref()
        .map(canonicalize_marker)
        .unwrap_or_default();
    format!("{}|{extras_part}|{marker_part}", pin.normalized)
}
