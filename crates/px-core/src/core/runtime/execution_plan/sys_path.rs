use std::collections::HashSet;

use anyhow::{bail, Result};

use super::types::SysPathSummary;
use crate::core::store::cas::{global_store, MATERIALIZED_PKG_BUILDS_DIR};

const SYS_PATH_SUMMARY_PREFIX: usize = 5;

pub(super) fn sys_path_for_profile(profile_oid: &str) -> Result<Vec<String>> {
    let store = global_store();
    let loaded = store.load(profile_oid)?;
    let crate::LoadedObject::Profile { header, .. } = loaded else {
        bail!("CAS object {profile_oid} is not a profile");
    };
    let ordered: Vec<String> = if header.sys_path_order.is_empty() {
        header
            .packages
            .iter()
            .map(|pkg| pkg.pkg_build_oid.clone())
            .collect()
    } else {
        header.sys_path_order.clone()
    };
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    for oid in ordered {
        if seen.insert(oid.clone()) {
            entries.push(
                store
                    .root()
                    .join(MATERIALIZED_PKG_BUILDS_DIR)
                    .join(&oid)
                    .join("site-packages")
                    .display()
                    .to_string(),
            );
        }
    }
    for pkg in &header.packages {
        if seen.insert(pkg.pkg_build_oid.clone()) {
            entries.push(
                store
                    .root()
                    .join(MATERIALIZED_PKG_BUILDS_DIR)
                    .join(&pkg.pkg_build_oid)
                    .join("site-packages")
                    .display()
                    .to_string(),
            );
        }
    }
    Ok(entries)
}

pub(super) fn summarize_sys_path(entries: &[String]) -> SysPathSummary {
    SysPathSummary {
        first: entries
            .iter()
            .take(SYS_PATH_SUMMARY_PREFIX)
            .cloned()
            .collect(),
        count: entries.len(),
    }
}
