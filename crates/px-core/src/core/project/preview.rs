use std::collections::{BTreeMap, HashMap, HashSet};
use std::str::FromStr;

use pep508_rs::Requirement as PepRequirement;
use serde_json::{json, Value};

use px_domain::api::{autopin_spec_key, canonicalize_package_name, LockSnapshot, PinSpec};

pub(crate) fn dependency_group_changes(group: &str, before: &[String], after: &[String]) -> Value {
    let changes = spec_changes(before, after);
    json!({
        "group": group,
        "added": changes.added,
        "removed": changes.removed,
        "updated": changes.updated,
    })
}

pub(crate) fn lock_preview(lock: Option<&LockSnapshot>, planned_pins: &[PinSpec]) -> Value {
    let planned = planned_pin_map(planned_pins);
    let planned_count = planned.len();

    let current = lock
        .map(lock_pin_map)
        .unwrap_or_default();
    let current_count = current.len();

    let mut added = 0usize;
    let mut updated = 0usize;
    for (name, version) in &planned {
        match current.get(name) {
            None => added += 1,
            Some(existing) if existing != version => updated += 1,
            _ => {}
        }
    }
    let removed = current.keys().filter(|name| !planned.contains_key(*name)).count();
    let would_change = added > 0 || removed > 0 || updated > 0;

    let highlights = lock_highlights(lock, &current, planned_pins);

    json!({
        "path": "px.lock",
        "would_change": would_change,
        "packages": {
            "before": current_count,
            "after": planned_count,
            "added": added,
            "removed": removed,
            "updated": updated,
        },
        "highlights": highlights,
    })
}

pub(crate) fn lock_preview_unresolved(lock: Option<&LockSnapshot>, would_change: bool, note: &str) -> Value {
    let current = lock
        .map(lock_pin_map)
        .unwrap_or_default();
    json!({
        "path": "px.lock",
        "would_change": would_change,
        "packages": {
            "before": current.len(),
            "after": Value::Null,
            "added": Value::Null,
            "removed": Value::Null,
            "updated": Value::Null,
        },
        "highlights": [],
        "note": note,
    })
}

struct SpecChanges {
    added: Vec<String>,
    removed: Vec<String>,
    updated: Vec<Value>,
}

fn spec_changes(before: &[String], after: &[String]) -> SpecChanges {
    let before_map = spec_key_map(before);
    let after_map = spec_key_map(after);

    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut updated = Vec::new();

    for (key, after_spec) in &after_map {
        match before_map.get(key) {
            None => added.push(after_spec.clone()),
            Some(before_spec) if before_spec != after_spec => {
                updated.push(json!({
                    "before": before_spec,
                    "after": after_spec,
                }));
            }
            _ => {}
        }
    }

    for (key, before_spec) in &before_map {
        if !after_map.contains_key(key) {
            removed.push(before_spec.clone());
        }
    }

    SpecChanges {
        added,
        removed,
        updated,
    }
}

fn spec_key_map(specs: &[String]) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for spec in specs {
        let key = dependency_key(spec);
        if key.is_empty() {
            continue;
        }
        map.insert(key, spec.clone());
    }
    map
}

fn dependency_key(spec: &str) -> String {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let marker = trimmed
        .split_once(';')
        .map(|(_, marker)| marker.trim())
        .unwrap_or_default();
    let base = autopin_spec_key(trimmed);
    if marker.is_empty() {
        base
    } else {
        format!("{base};{marker}")
    }
}

fn planned_pin_map(pins: &[PinSpec]) -> HashMap<String, String> {
    let mut map = HashMap::with_capacity(pins.len());
    for pin in pins {
        let name = canonicalize_package_name(&pin.normalized);
        map.insert(name, pin.version.clone());
    }
    map
}

fn lock_pin_map(lock: &LockSnapshot) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for spec in &lock.dependencies {
        let Some((name, version)) = pinned_name_version(spec) else {
            continue;
        };
        map.insert(name, version);
    }
    map
}

fn lock_highlights(
    lock: Option<&LockSnapshot>,
    current: &HashMap<String, String>,
    planned_pins: &[PinSpec],
) -> Vec<Value> {
    let mut planned_direct = HashSet::new();
    for pin in planned_pins.iter().filter(|pin| pin.direct) {
        planned_direct.insert(canonicalize_package_name(&pin.normalized));
    }

    let mut current_direct = HashSet::new();
    if let Some(lock) = lock {
        for dep in lock.resolved.iter().filter(|dep| dep.direct) {
            current_direct.insert(canonicalize_package_name(&dep.name));
        }
    }

    let mut keys = planned_direct
        .into_iter()
        .chain(current_direct)
        .collect::<Vec<_>>();
    keys.sort();
    keys.dedup();

    let planned = planned_pin_map(planned_pins);
    let mut out = Vec::new();
    for name in keys {
        let before = current.get(&name).cloned();
        let after = planned.get(&name).cloned();
        if before == after {
            continue;
        }
        out.push(json!({
            "name": name,
            "from": before,
            "to": after,
        }));
    }
    out
}

fn pinned_name_version(spec: &str) -> Option<(String, String)> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return None;
    }

    let Ok(req) = PepRequirement::from_str(trimmed) else {
        return parse_pinned_name_version_fallback(trimmed);
    };

    let name = canonicalize_package_name(req.name.as_ref());
    let rendered = match req.version_or_url.as_ref() {
        Some(pep508_rs::VersionOrUrl::VersionSpecifier(specifiers)) => specifiers.to_string(),
        _ => String::new(),
    };
    let version = rendered
        .split(',')
        .next()
        .unwrap_or_default()
        .trim()
        .trim_start_matches("===")
        .trim_start_matches("==")
        .to_string();
    if version.is_empty() {
        return None;
    }
    Some((name, version))
}

fn parse_pinned_name_version_fallback(spec: &str) -> Option<(String, String)> {
    let head = spec.split_once(';').map(|(head, _)| head).unwrap_or(spec);
    let (name, version) = head
        .split_once("===")
        .or_else(|| head.split_once("=="))
        .map(|(name, version)| (name.trim(), version.trim()))?;
    if name.is_empty() || version.is_empty() {
        return None;
    }
    Some((canonicalize_package_name(name), version.to_string()))
}
