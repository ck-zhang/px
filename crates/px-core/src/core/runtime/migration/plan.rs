use std::collections::HashMap;
use std::fs;

use anyhow::Result;
use toml_edit::{DocumentMut, Item};

use px_domain::{OnboardPackagePlan, PyprojectPlan};

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
    let mut selected = HashMap::new();
    let mut order = Vec::new();
    let mut conflicts = Vec::new();

    for pkg in packages {
        let priority = package_priority(
            pkg,
            requirements_rel,
            dev_rel,
            source_override,
            dev_override,
        );
        let key = (pkg.name.clone(), pkg.scope.clone());
        match selected.get(&key) {
            None => {
                selected.insert(key.clone(), (priority, pkg.clone()));
                order.push(key);
            }
            Some((existing_pri, existing_pkg)) => {
                if *existing_pri > priority {
                    if existing_pkg.requested != pkg.requested {
                        conflicts.push(PackageConflict {
                            name: pkg.name.clone(),
                            scope: pkg.scope.clone(),
                            kept_source: existing_pkg.source.clone(),
                            kept_spec: existing_pkg.requested.clone(),
                            dropped_source: pkg.source.clone(),
                            dropped_spec: pkg.requested.clone(),
                        });
                    }
                } else if *existing_pri < priority {
                    if existing_pkg.requested != pkg.requested {
                        conflicts.push(PackageConflict {
                            name: pkg.name.clone(),
                            scope: pkg.scope.clone(),
                            kept_source: pkg.source.clone(),
                            kept_spec: pkg.requested.clone(),
                            dropped_source: existing_pkg.source.clone(),
                            dropped_spec: existing_pkg.requested.clone(),
                        });
                    }
                    selected.insert(key.clone(), (priority, pkg.clone()));
                    if !order.contains(&key) {
                        order.push(key);
                    }
                } else if existing_pkg.requested != pkg.requested {
                    conflicts.push(PackageConflict {
                        name: pkg.name.clone(),
                        scope: pkg.scope.clone(),
                        kept_source: existing_pkg.source.clone(),
                        kept_spec: existing_pkg.requested.clone(),
                        dropped_source: pkg.source.clone(),
                        dropped_spec: pkg.requested.clone(),
                    });
                }
            }
        }
    }

    let mut final_packages = Vec::new();
    for key in order {
        if let Some((_, pkg)) = selected.remove(&key) {
            final_packages.push(pkg);
        }
    }
    (final_packages, conflicts)
}
