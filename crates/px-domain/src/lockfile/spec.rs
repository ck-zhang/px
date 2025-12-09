use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::str::FromStr;

use crate::project::manifest::canonicalize_package_name;
use anyhow::Result;
use pep508_rs::{MarkerEnvironment, Requirement as PepRequirement};
use sha2::{Digest, Sha256};

pub fn format_specifier(
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

pub fn canonical_extras(extras: &[String]) -> Vec<String> {
    let mut values = extras
        .iter()
        .map(|extra| extra.to_ascii_lowercase())
        .collect::<Vec<_>>();
    values.sort();
    values.dedup();
    values
}

pub(crate) fn parse_spec_metadata(spec: &str) -> (Vec<String>, Option<String>) {
    match PepRequirement::from_str(spec.trim()) {
        Ok(req) => {
            let extras = req
                .extras
                .iter()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>();
            let marker = req.marker.map(|expr| expr.to_string());
            (extras, marker)
        }
        Err(_) => (Vec::new(), None),
    }
}

pub(crate) fn dependency_name(spec: &str) -> String {
    let trimmed = strip_wrapping_quotes(spec.trim());
    let mut end = trimmed.len();
    for (idx, ch) in trimmed.char_indices() {
        if ch.is_ascii_whitespace() || matches!(ch, '<' | '>' | '=' | '!' | '~' | ';') {
            end = idx;
            break;
        }
    }
    let head = &trimmed[..end];
    let base = head.split('[').next().unwrap_or(head);
    canonicalize_package_name(base)
}

pub(crate) fn strip_wrapping_quotes(input: &str) -> &str {
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

pub(crate) fn version_from_specifier(spec: &str) -> Option<&str> {
    spec.trim()
        .split_once("==")
        .map(|(_, version)| version.trim())
}

pub(crate) fn marker_applies(spec: &str, marker_env: &MarkerEnvironment) -> bool {
    let cleaned = strip_wrapping_quotes(spec.trim());
    match PepRequirement::from_str(cleaned) {
        Ok(req) => req.evaluate_markers(marker_env, &[]),
        Err(_) => true,
    }
}

pub(crate) fn spec_map<'a>(
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

pub(crate) fn compute_file_sha256(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

use std::fs::File;
