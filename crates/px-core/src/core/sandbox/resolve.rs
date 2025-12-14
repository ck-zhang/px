use std::{collections::BTreeSet, path::Path};

use anyhow::Result;
use serde_json::json;
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use px_domain::api::{LockSnapshot, SandboxConfig, WorkspaceLock};

use crate::core::system_deps::{
    capability_apt_map, package_capability_rules, read_sys_deps_metadata, resolve_system_deps,
};
use crate::InstallUserError;

use super::errors::sandbox_error;
use super::types::{SandboxBase, SandboxDefinition, SandboxResolution, SBX_VERSION};

const DEFAULT_BASE: &str = "debian-12";

pub(crate) fn resolve_sandbox_definition(
    config: &SandboxConfig,
    lock: Option<&LockSnapshot>,
    workspace_lock: Option<&WorkspaceLock>,
    profile_oid: &str,
    site_packages: Option<&Path>,
) -> Result<SandboxResolution, InstallUserError> {
    let base = resolve_base(config.base.as_deref())?;
    let inferred = if config.auto {
        let mut inferred = BTreeSet::new();
        if let Some(lock) = lock {
            inferred.extend(infer_capabilities_from_lock(lock));
        }
        if let Some(workspace) = workspace_lock {
            inferred.extend(infer_capabilities_from_workspace(workspace));
        }
        if let Some(site) = site_packages {
            inferred.extend(infer_capabilities_from_site(site));
        }
        inferred
    } else {
        BTreeSet::new()
    };
    let capabilities = effective_capabilities(config, &inferred)?;
    validate_capabilities(&base, &capabilities)?;
    let system_deps = resolve_system_deps(&capabilities, site_packages);
    let definition = SandboxDefinition {
        base_os_oid: base.base_os_oid.clone(),
        capabilities,
        system_deps,
        profile_oid: profile_oid.to_string(),
        sbx_version: SBX_VERSION,
    };
    Ok(SandboxResolution { base, definition })
}

fn resolve_base(name: Option<&str>) -> Result<SandboxBase, InstallUserError> {
    let target = name.unwrap_or(DEFAULT_BASE).trim();
    let Some(base) = known_bases()
        .iter()
        .find(|candidate| candidate.name == target)
        .cloned()
    else {
        return Err(sandbox_error(
            "PX900",
            "requested sandbox base is unavailable",
            json!({ "base": target }),
        ));
    };
    Ok(base)
}

fn known_bases() -> &'static [SandboxBase] {
    use std::sync::OnceLock;
    static BASES: OnceLock<Vec<SandboxBase>> = OnceLock::new();
    BASES.get_or_init(|| {
        let capabilities = known_capabilities();
        vec![
            SandboxBase {
                name: DEFAULT_BASE.to_string(),
                base_os_oid: base_os_identity(DEFAULT_BASE),
                supported_capabilities: capabilities.clone(),
            },
            SandboxBase {
                name: "alpine-3.20".to_string(),
                base_os_oid: base_os_identity("alpine-3.20"),
                supported_capabilities: capabilities,
            },
        ]
    })
}

fn base_os_identity(name: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!("base:{name}:v{SBX_VERSION}"));
    format!("{:x}", hasher.finalize())
}

fn known_capabilities() -> BTreeSet<String> {
    let mut caps: BTreeSet<String> = capability_apt_map()
        .iter()
        .map(|(cap, _)| (*cap).to_string())
        .collect();
    for (cap, _) in package_capability_rules() {
        caps.insert((*cap).to_string());
    }
    caps
}

fn infer_capabilities_from_lock(lock: &LockSnapshot) -> BTreeSet<String> {
    let mut caps = BTreeSet::new();
    for spec in &lock.dependencies {
        let name = crate::dependency_name(spec);
        caps.extend(
            crate::core::system_deps::capabilities_from_names([name.as_str()])
                .into_iter()
                .collect::<BTreeSet<_>>(),
        );
    }
    for dep in &lock.resolved {
        let name = dep.name.trim();
        caps.extend(
            crate::core::system_deps::capabilities_from_names([name])
                .into_iter()
                .collect::<BTreeSet<_>>(),
        );
    }
    caps
}

fn infer_capabilities_from_workspace(lock: &WorkspaceLock) -> BTreeSet<String> {
    let mut caps = BTreeSet::new();
    for member in &lock.members {
        for dep in &member.dependencies {
            let name = crate::dependency_name(dep);
            caps.extend(
                crate::core::system_deps::capabilities_from_names([name.as_str()])
                    .into_iter()
                    .collect::<BTreeSet<_>>(),
            );
        }
    }
    caps
}

fn infer_capabilities_from_site(site: &Path) -> BTreeSet<String> {
    let mut caps = BTreeSet::new();
    if !site.exists() {
        return caps;
    }
    let meta = read_sys_deps_metadata(site);
    caps.extend(meta.capabilities);
    for entry in WalkDir::new(site) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        caps.extend(
            crate::core::system_deps::capabilities_from_libraries([name])
                .into_iter()
                .collect::<BTreeSet<_>>(),
        );
    }
    caps
}

fn effective_capabilities(
    config: &SandboxConfig,
    inferred: &BTreeSet<String>,
) -> Result<BTreeSet<String>, InstallUserError> {
    let known = known_capabilities();
    let mut capabilities = BTreeSet::new();
    for (cap, enabled) in &config.capabilities {
        if !known.contains(cap) {
            return Err(sandbox_error(
                "PX901",
                "sandbox capability is not recognized",
                json!({ "capability": cap }),
            ));
        }
        if *enabled {
            capabilities.insert(cap.to_string());
        }
    }
    if config.auto {
        for cap in inferred {
            if *config.capabilities.get(cap).unwrap_or(&true) {
                capabilities.insert(cap.clone());
            }
        }
    }
    Ok(capabilities)
}

fn validate_capabilities(
    base: &SandboxBase,
    capabilities: &BTreeSet<String>,
) -> Result<(), InstallUserError> {
    for cap in capabilities {
        if !base.supported_capabilities.contains(cap) {
            return Err(sandbox_error(
                "PX901",
                "requested sandbox capability is not available for this base",
                json!({
                    "capability": cap,
                    "base": base.name,
                }),
            ));
        }
    }
    Ok(())
}
