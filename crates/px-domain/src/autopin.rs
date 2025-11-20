use std::{collections::HashMap, fs, path::Path};

use anyhow::{anyhow, Result};
use pep508_rs::MarkerEnvironment;
use serde_json::{json, Value};
use toml_edit::DocumentMut;

use crate::manifest::{
    overwrite_dependency_specs, overwrite_dev_dependency_specs, read_dependencies_from_doc,
    read_optional_dependency_group, requirement_display_name,
};
use crate::project_resolver::{
    autopin_pin_key, autopin_spec_key, marker_applies, spec_requires_pin, InstallOverride, PinSpec,
};
use crate::snapshot::ProjectSnapshot;

pub type ResolvePinsFn = dyn Fn(&ProjectSnapshot, &[String]) -> Result<Vec<PinSpec>>;

/// Determine which dependencies require autopinning and plan their updates.
///
/// # Errors
///
/// Returns an error when required files cannot be read, dependency resolution
/// fails, or the resolver omits a pin that is necessary for the plan.
pub fn plan_autopin(
    snapshot: &ProjectSnapshot,
    pyproject_path: &Path,
    lock_only: bool,
    no_autopin: bool,
    resolve_pins: &ResolvePinsFn,
    marker_env: &MarkerEnvironment,
) -> Result<AutopinState> {
    if !pyproject_path.exists() {
        return Ok(AutopinState::NotNeeded);
    }
    let contents = fs::read_to_string(pyproject_path)?;
    let mut doc: DocumentMut = contents.parse()?;
    let prod_specs = read_dependencies_from_doc(&doc);
    let dev_specs = read_optional_dependency_group(&doc, "px-dev");
    let mut autopin_map = collect_autopin_locations(&prod_specs, &dev_specs, marker_env);
    if autopin_map.is_empty() {
        return Ok(AutopinState::NotNeeded);
    }
    if no_autopin {
        let pending = autopin_map
            .values()
            .flat_map(|locs| locs.iter().map(AutopinPending::from))
            .collect();
        return Ok(AutopinState::Disabled { pending });
    }

    let resolver_specs = autopin_map
        .values()
        .filter_map(|locs| locs.first())
        .map(|loc| loc.original.clone())
        .collect::<Vec<_>>();

    let mut resolver_snapshot = snapshot.clone();
    resolver_snapshot.dependencies.clone_from(&resolver_specs);
    let resolved_pin_specs = resolve_pins(&resolver_snapshot, &resolver_specs)?;
    let mut resolved_lookup = HashMap::new();
    for pin in resolved_pin_specs {
        resolved_lookup.insert(autopin_pin_key(&pin), pin);
    }

    let mut prod_specs_final = prod_specs.clone();
    let mut dev_specs_final = dev_specs.clone();
    let mut autopinned = Vec::new();
    let mut prod_override_pins = Vec::new();
    let touches_prod = autopin_map
        .values()
        .any(|locs| locs.iter().any(|loc| loc.scope == AutopinScope::Prod));
    let touches_dev = autopin_map
        .values()
        .any(|locs| locs.iter().any(|loc| loc.scope == AutopinScope::Dev));

    for (key, locations) in autopin_map.drain() {
        let Some(pin) = resolved_lookup.get(&key) else {
            let applies = locations
                .iter()
                .any(|loc| marker_applies(&loc.original, marker_env));
            if !applies {
                continue;
            }
            return Err(anyhow!("resolver missing pin for {key}"));
        };
        for loc in locations {
            let entry = AutopinEntry::new(&loc.name, loc.scope, &loc.original, &pin.specifier);
            match loc.scope {
                AutopinScope::Prod => {
                    if let Some(slot) = prod_specs_final.get_mut(loc.index) {
                        slot.clone_from(&pin.specifier);
                    }
                    prod_override_pins.push(pin.clone());
                }
                AutopinScope::Dev => {
                    if let Some(slot) = dev_specs_final.get_mut(loc.index) {
                        slot.clone_from(&pin.specifier);
                    }
                }
            }
            autopinned.push(entry);
        }
    }

    let mut doc_contents = None;
    if !lock_only {
        let mut changed = false;
        if touches_prod {
            changed |= overwrite_dependency_specs(&mut doc, &prod_specs_final);
        }
        if touches_dev {
            changed |= overwrite_dev_dependency_specs(&mut doc, &dev_specs_final);
        }
        if changed {
            doc_contents = Some(doc.to_string());
        }
    }

    let install_override = if lock_only && touches_prod {
        Some(InstallOverride {
            dependencies: prod_specs_final.clone(),
            pins: prod_override_pins,
        })
    } else {
        None
    };

    Ok(AutopinState::Planned(AutopinPlan {
        doc_contents,
        autopinned,
        install_override,
    }))
}

fn collect_autopin_locations(
    prod_specs: &[String],
    dev_specs: &[String],
    marker_env: &MarkerEnvironment,
) -> HashMap<String, Vec<AutopinLocation>> {
    let mut map = HashMap::new();
    for (idx, spec) in prod_specs.iter().enumerate() {
        push_autopin_location(&mut map, spec, idx, AutopinScope::Prod, marker_env);
    }
    for (idx, spec) in dev_specs.iter().enumerate() {
        push_autopin_location(&mut map, spec, idx, AutopinScope::Dev, marker_env);
    }
    map.retain(|_, locs| !locs.is_empty());
    map
}

fn push_autopin_location(
    map: &mut HashMap<String, Vec<AutopinLocation>>,
    spec: &str,
    index: usize,
    scope: AutopinScope,
    marker_env: &MarkerEnvironment,
) {
    if !spec_requires_pin(spec) {
        return;
    }
    if !marker_applies(spec, marker_env) {
        return;
    }
    let key = autopin_spec_key(spec);
    map.entry(key)
        .or_default()
        .push(AutopinLocation::new(spec, index, scope));
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AutopinScope {
    Prod,
    Dev,
}

impl AutopinScope {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            AutopinScope::Prod => "prod",
            AutopinScope::Dev => "dev",
        }
    }
}

struct AutopinLocation {
    scope: AutopinScope,
    index: usize,
    original: String,
    name: String,
}

impl AutopinLocation {
    fn new(spec: &str, index: usize, scope: AutopinScope) -> Self {
        Self {
            scope,
            index,
            original: spec.to_string(),
            name: requirement_display_name(spec),
        }
    }
}

pub struct AutopinPlan {
    pub doc_contents: Option<String>,
    pub autopinned: Vec<AutopinEntry>,
    pub install_override: Option<InstallOverride>,
}

pub enum AutopinState {
    NotNeeded,
    Disabled { pending: Vec<AutopinPending> },
    Planned(AutopinPlan),
}

#[derive(Clone)]
pub struct AutopinEntry {
    pub name: String,
    pub scope: AutopinScope,
    pub from: String,
    pub to: String,
}

impl AutopinEntry {
    fn new(name: &str, scope: AutopinScope, from: &str, to: &str) -> Self {
        Self {
            name: name.to_string(),
            scope,
            from: from.to_string(),
            to: to.to_string(),
        }
    }

    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "scope": self.scope.as_str(),
            "from": self.from,
            "to": self.to,
        })
    }

    #[must_use]
    pub fn short_label(&self) -> String {
        let version = extract_version_label(&self.to);
        let mut label = format!("{}=={}", self.name, version);
        if self.scope == AutopinScope::Dev {
            label.push_str(" (dev)");
        }
        label
    }
}

#[derive(Clone)]
pub struct AutopinPending {
    pub name: String,
    pub scope: AutopinScope,
    pub requested: String,
}

impl AutopinPending {
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "scope": self.scope.as_str(),
            "requested": self.requested,
        })
    }
}

impl From<&AutopinLocation> for AutopinPending {
    fn from(value: &AutopinLocation) -> Self {
        Self {
            name: value.name.clone(),
            scope: value.scope,
            requested: value.original.clone(),
        }
    }
}

fn extract_version_label(spec: &str) -> String {
    if let Some((_, version)) = spec.split_once("==") {
        let head = version.split(';').next().unwrap_or(version).trim();
        head.to_string()
    } else {
        spec.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pep508_rs::StringVersion;
    use std::str::FromStr;
    use tempfile::tempdir;

    fn marker_env() -> MarkerEnvironment {
        MarkerEnvironment {
            implementation_name: "cpython".into(),
            implementation_version: StringVersion::from_str("3.12.0").expect("impl version"),
            os_name: "posix".into(),
            platform_machine: "x86_64".into(),
            platform_python_implementation: "CPython".into(),
            platform_release: "6.0".into(),
            platform_system: "Linux".into(),
            platform_version: "6.0".into(),
            python_full_version: StringVersion::from_str("3.12.0").expect("full version"),
            python_version: StringVersion::from_str("3.12").expect("python version"),
            sys_platform: "linux".into(),
        }
    }

    #[test]
    fn plans_autopin_changes() -> Result<()> {
        let dir = tempdir()?;
        let root = dir.path();
        let pyproject_path = root.join("pyproject.toml");
        fs::write(
            &pyproject_path,
            r#"[project]
name = "demo"
version = "0.1.0"
requires-python = ">=3.11"
dependencies = ["demo"]
"#,
        )?;

        let snapshot = ProjectSnapshot::read_from(root)?;
        let marker_env = marker_env();
        let resolver = |_: &ProjectSnapshot, specs: &[String]| -> Result<Vec<PinSpec>> {
            assert_eq!(specs, &["demo".to_string()]);
            Ok(vec![PinSpec {
                name: "demo".into(),
                specifier: "demo==1.2.3".into(),
                version: "1.2.3".into(),
                normalized: "demo".into(),
                extras: Vec::new(),
                marker: None,
                direct: true,
                requires: Vec::new(),
            }])
        };

        match plan_autopin(
            &snapshot,
            &pyproject_path,
            false,
            false,
            &resolver,
            &marker_env,
        )? {
            AutopinState::Planned(plan) => {
                assert_eq!(plan.autopinned.len(), 1);
                assert!(plan.doc_contents.is_some());
                assert!(plan.install_override.is_none());
            }
            _ => panic!("unexpected autopin state"),
        }
        Ok(())
    }

    #[test]
    fn rewrites_specs_without_duplication() -> Result<()> {
        let dir = tempdir()?;
        let root = dir.path();
        let pyproject_path = root.join("pyproject.toml");
        fs::write(
            &pyproject_path,
            r#"[project]
name = "demo"
version = "0.1.0"
requires-python = ">=3.11"
dependencies = ["demo>=1.0", "helper==0.5"]
"#,
        )?;

        let snapshot = ProjectSnapshot::read_from(root)?;
        let marker_env = marker_env();
        let resolver = |_: &ProjectSnapshot, specs: &[String]| -> Result<Vec<PinSpec>> {
            assert_eq!(specs, &["demo>=1.0".to_string()]);
            Ok(vec![PinSpec {
                name: "demo".into(),
                specifier: "demo==1.2.3".into(),
                version: "1.2.3".into(),
                normalized: "demo".into(),
                extras: Vec::new(),
                marker: None,
                direct: true,
                requires: Vec::new(),
            }])
        };

        match plan_autopin(
            &snapshot,
            &pyproject_path,
            false,
            false,
            &resolver,
            &marker_env,
        )? {
            AutopinState::Planned(plan) => {
                let contents = plan.doc_contents.expect("pyproject contents");
                let doc: DocumentMut = contents.parse()?;
                let deps = read_dependencies_from_doc(&doc);
                assert_eq!(
                    deps,
                    vec!["demo==1.2.3".to_string(), "helper==0.5".to_string()]
                );
            }
            _ => panic!("unexpected autopin state"),
        }
        Ok(())
    }
}
