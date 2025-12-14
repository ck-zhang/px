use std::collections::HashMap;
use std::fs;
use std::str::FromStr;

use anyhow::Result;
use pep508_rs::{MarkerEnvironment, Requirement as PepRequirement, StringVersion};
use toml_edit::{DocumentMut, Item};

use px_domain::api::{OnboardPackagePlan, PyprojectPlan};

#[derive(Clone, Debug)]
pub(crate) struct PackageConflict {
    pub name: String,
    pub scope: String,
    pub kept_source: String,
    pub kept_spec: String,
    pub dropped_source: String,
    pub dropped_spec: String,
}

pub(crate) fn apply_python_override(plan: &PyprojectPlan, python: &str) -> Result<DocumentMut> {
    let mut doc: DocumentMut = if let Some(contents) = &plan.contents {
        contents.parse()?
    } else {
        fs::read_to_string(&plan.path)?.parse()?
    };
    let tool = doc
        .entry("tool")
        .or_insert(Item::Table(toml_edit::Table::new()))
        .as_table_mut()
        .expect("tool table");
    let px = tool
        .entry("px")
        .or_insert(Item::Table(toml_edit::Table::new()))
        .as_table_mut()
        .expect("px table");
    px.insert("python", toml_edit::value(python));
    Ok(doc)
}

pub(crate) fn package_priority(
    pkg: &OnboardPackagePlan,
    requirements_rel: Option<&String>,
    dev_rel: Option<&String>,
    source_override: &Option<String>,
    dev_override: &Option<String>,
) -> u8 {
    if requirements_rel
        .map(|rel| rel == &pkg.source)
        .unwrap_or(false)
        && source_override.is_some()
    {
        return 3;
    }
    if dev_rel.map(|rel| rel == &pkg.source).unwrap_or(false) && dev_override.is_some() {
        return 3;
    }
    if pkg.source.ends_with("pyproject.toml") {
        2
    } else {
        1
    }
}

pub(crate) fn apply_precedence(
    packages: &[OnboardPackagePlan],
    requirements_rel: Option<&String>,
    dev_rel: Option<&String>,
    source_override: &Option<String>,
    dev_override: &Option<String>,
) -> (Vec<OnboardPackagePlan>, Vec<PackageConflict>) {
    type PackageKey = (String, String);
    type PackageEntry = (usize, OnboardPackagePlan, u8);
    type PackageBuckets = HashMap<PackageKey, Vec<PackageEntry>>;

    let mut grouped: PackageBuckets = HashMap::new();
    let mut order = Vec::new();
    let mut conflicts = Vec::new();

    for (idx, pkg) in packages.iter().enumerate() {
        let priority = package_priority(
            pkg,
            requirements_rel,
            dev_rel,
            source_override,
            dev_override,
        );
        let key = (pkg.name.clone(), pkg.scope.clone());
        if !grouped.contains_key(&key) {
            order.push(key.clone());
        }
        grouped
            .entry(key)
            .or_default()
            .push((idx, pkg.clone(), priority));
    }

    let mut final_packages = Vec::new();
    for key in order {
        let Some(entries) = grouped.remove(&key) else {
            continue;
        };
        let max_priority = entries.iter().map(|(_, _, pri)| *pri).max().unwrap_or(0);

        let mut kept: Vec<(usize, OnboardPackagePlan)> = Vec::new();
        for (idx, pkg, _) in entries
            .iter()
            .filter(|(_, _, pri)| *pri == max_priority)
            .cloned()
            .collect::<Vec<_>>()
        {
            let mut duplicate = false;
            let mut conflict_with: Option<OnboardPackagePlan> = None;
            for (_, existing) in &kept {
                if !requirements_overlap(&pkg, existing) {
                    continue;
                }
                if pkg.requested == existing.requested {
                    duplicate = true;
                    break;
                } else {
                    conflict_with = Some(existing.clone());
                    break;
                }
            }
            if duplicate {
                continue;
            }
            if let Some(conflict) = conflict_with {
                conflicts.push(PackageConflict {
                    name: pkg.name.clone(),
                    scope: pkg.scope.clone(),
                    kept_source: conflict.source.clone(),
                    kept_spec: conflict.requested.clone(),
                    dropped_source: pkg.source.clone(),
                    dropped_spec: pkg.requested.clone(),
                });
                continue;
            }
            kept.push((idx, pkg));
        }

        for (_, pkg, _) in entries.iter().filter(|(_, _, pri)| *pri < max_priority) {
            for (_, existing) in &kept {
                if requirements_overlap(pkg, existing) && pkg.requested != existing.requested {
                    conflicts.push(PackageConflict {
                        name: pkg.name.clone(),
                        scope: pkg.scope.clone(),
                        kept_source: existing.source.clone(),
                        kept_spec: existing.requested.clone(),
                        dropped_source: pkg.source.clone(),
                        dropped_spec: pkg.requested.clone(),
                    });
                    break;
                }
            }
        }

        kept.sort_by_key(|(idx, _)| *idx);
        for (_, pkg) in kept {
            final_packages.push(pkg);
        }
    }
    (final_packages, conflicts)
}

fn requirements_overlap(a: &OnboardPackagePlan, b: &OnboardPackagePlan) -> bool {
    if a.source == b.source && a.requested == b.requested {
        return true;
    }
    let req_a = PepRequirement::from_str(a.requested.trim());
    let req_b = PepRequirement::from_str(b.requested.trim());
    let (Ok(req_a), Ok(req_b)) = (req_a, req_b) else {
        return true;
    };

    markers_overlap(&req_a, &req_b)
}

fn markers_overlap(a: &PepRequirement, b: &PepRequirement) -> bool {
    let marker_a = a
        .marker
        .as_ref()
        .map(|m| canonicalize_marker(&m.to_string()));
    let marker_b = b
        .marker
        .as_ref()
        .map(|m| canonicalize_marker(&m.to_string()));

    if marker_a.is_none() || marker_b.is_none() {
        return true;
    }
    if marker_a == marker_b {
        return true;
    }
    if marker_a.as_ref().is_some_and(|m| m.contains("extra"))
        || marker_b.as_ref().is_some_and(|m| m.contains("extra"))
    {
        return true;
    }

    for env in marker_scenarios() {
        if a.evaluate_markers(&env, &[]) && b.evaluate_markers(&env, &[]) {
            return true;
        }
    }
    false
}

fn marker_scenarios() -> Vec<MarkerEnvironment> {
    let mut scenarios = Vec::new();
    let matrix = [
        ("3.11", "linux", "posix", "Linux"),
        ("3.12", "linux", "posix", "Linux"),
        ("3.13", "linux", "posix", "Linux"),
        ("3.12", "win32", "nt", "Windows"),
        ("3.12", "darwin", "posix", "Darwin"),
    ];
    for (python_version, sys_platform, os_name, platform_system) in matrix {
        let python_full = format!("{python_version}.0");
        scenarios.push(MarkerEnvironment {
            implementation_name: "cpython".into(),
            implementation_version: StringVersion::from_str(&python_full)
                .expect("valid implementation version"),
            os_name: os_name.into(),
            platform_machine: "x86_64".into(),
            platform_python_implementation: "CPython".into(),
            platform_release: "6.0".into(),
            platform_system: platform_system.into(),
            platform_version: "6.0".into(),
            python_full_version: StringVersion::from_str(&python_full)
                .expect("valid python_full_version"),
            python_version: StringVersion::from_str(python_version).expect("valid python_version"),
            sys_platform: sys_platform.into(),
        });
    }
    scenarios
}

fn canonicalize_marker(raw: &str) -> String {
    raw.split_whitespace()
        .collect::<String>()
        .to_ascii_lowercase()
}
