use std::env;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::outcome::InstallUserError;
use anyhow::Result;
use pep440_rs::{Operator, Version, VersionSpecifiers};
use pep508_rs::{Requirement as PepRequirement, VersionOrUrl};
use serde_json::json;

#[derive(Default)]
pub(super) struct SysEnvGuard {
    saved: Vec<(String, Option<String>)>,
}

impl SysEnvGuard {
    fn set_var(&mut self, key: &str, value: String) {
        let prev = env::var(key).ok();
        env::set_var(key, &value);
        self.saved.push((key.to_string(), prev));
    }

    fn prepend_paths(&mut self, key: &str, entries: &[PathBuf]) {
        let mut parts: Vec<PathBuf> = entries
            .iter()
            .filter(|path| path.exists())
            .cloned()
            .collect();
        if let Some(existing) = env::var_os(key) {
            parts.extend(env::split_paths(&existing));
        }
        if parts.is_empty() {
            return;
        }
        if let Ok(joined) = env::join_paths(&parts) {
            self.set_var(key, joined.to_string_lossy().to_string());
        }
    }

    pub(super) fn apply(&mut self, root: &Path) {
        let path_entries = [
            root.join("usr/bin"),
            root.join("bin"),
            root.join("usr/sbin"),
        ];
        self.prepend_paths("PATH", &path_entries);

        let lib_paths = [
            root.join("lib"),
            root.join("lib/x86_64-linux-gnu"),
            root.join("usr/lib"),
            root.join("usr/lib/x86_64-linux-gnu"),
        ];
        self.prepend_paths("LD_LIBRARY_PATH", &lib_paths);
        self.prepend_paths("LIBRARY_PATH", &lib_paths);

        let include_paths = [
            root.join("usr/include"),
            root.join("usr/include/gdal"),
            root.join("usr/local/include"),
        ];
        self.prepend_paths("CPATH", &include_paths);

        let pkg_paths = [
            root.join("usr/lib/pkgconfig"),
            root.join("usr/lib/x86_64-linux-gnu/pkgconfig"),
        ];
        self.prepend_paths("PKG_CONFIG_PATH", &pkg_paths);

        let gdal_data = root.join("usr/share/gdal");
        if gdal_data.exists() {
            self.set_var("GDAL_DATA", gdal_data.display().to_string());
        }
        let proj_lib = root.join("usr/share/proj");
        if proj_lib.exists() {
            self.set_var("PROJ_LIB", proj_lib.display().to_string());
        }
        if env::var_os("CC").is_none() {
            let cc = root.join("usr/bin/gcc");
            if cc.exists() {
                self.set_var("CC", cc.display().to_string());
            }
        }
        if env::var_os("CXX").is_none() {
            let cxx = root.join("usr/bin/g++");
            if cxx.exists() {
                self.set_var("CXX", cxx.display().to_string());
            }
        }
    }
}

impl Drop for SysEnvGuard {
    fn drop(&mut self) {
        for (key, value) in self.saved.drain(..).rev() {
            match value {
                Some(val) => env::set_var(&key, val),
                None => env::remove_var(&key),
            }
        }
    }
}

fn parse_apt_numeric_version(raw: &str) -> Option<Version> {
    let trimmed = raw.trim();
    let without_epoch = trimmed.rsplit(':').next().unwrap_or(trimmed);
    let mut buf = String::new();
    for ch in without_epoch.chars() {
        if ch.is_ascii_digit() || ch == '.' {
            buf.push(ch);
        } else {
            break;
        }
    }
    let numeric = buf.trim_end_matches('.');
    if numeric.is_empty() {
        return None;
    }
    Version::from_str(numeric).ok()
}

fn min_requested_version(spec_str: &str) -> Option<Version> {
    let Ok(specifiers) = VersionSpecifiers::from_str(spec_str) else {
        return None;
    };
    let mut min: Option<Version> = None;
    for spec in specifiers.iter() {
        let operator = spec.operator();
        let version = spec.version();
        match *operator {
            Operator::GreaterThan
            | Operator::GreaterThanEqual
            | Operator::Equal
            | Operator::EqualStar
            | Operator::ExactEqual
            | Operator::TildeEqual => {
                if min.as_ref().is_none_or(|existing| version > existing) {
                    min = Some(version.clone());
                }
            }
            _ => {}
        }
    }
    min
}

fn has_compatible_upper_bound(spec_str: &str, max: &Version) -> bool {
    let Ok(specifiers) = VersionSpecifiers::from_str(spec_str) else {
        return false;
    };
    for spec in specifiers.iter() {
        let operator = spec.operator();
        let version = spec.version();
        match *operator {
            Operator::LessThan | Operator::LessThanEqual => {
                if version <= max {
                    return true;
                }
            }
            Operator::Equal | Operator::ExactEqual => {
                if version <= max {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

fn format_requirement_with_cap(req: &PepRequirement, cap: &Version) -> String {
    let mut out = req.name.to_string();
    if !req.extras.is_empty() {
        out.push('[');
        out.push_str(
            &req.extras
                .iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join(","),
        );
        out.push(']');
    }
    match &req.version_or_url {
        None => {
            out.push_str(&format!("<={cap}"));
        }
        Some(VersionOrUrl::VersionSpecifier(specifiers)) => {
            let existing = specifiers.to_string();
            if existing.is_empty() {
                out.push_str(&format!("<={cap}"));
            } else {
                out.push_str(&existing);
                out.push_str(&format!(",<={cap}"));
            }
        }
        _ => {
            out.push_str(&format!("<={cap}"));
        }
    }
    if let Some(marker) = req.marker.as_ref() {
        out.push_str(" ; ");
        out.push_str(&marker.to_string());
    }
    out
}

pub(super) fn apply_system_lib_compatibility(
    requirements: Vec<String>,
    system_deps: &crate::core::system_deps::SystemDeps,
) -> Result<Vec<String>> {
    let Some(libgdal_raw) = system_deps.apt_versions.get("libgdal-dev") else {
        return Ok(requirements);
    };
    let Some(libgdal_ver) = parse_apt_numeric_version(libgdal_raw) else {
        return Ok(requirements);
    };
    let mut rewritten = Vec::with_capacity(requirements.len());
    for spec in requirements {
        let Ok(req) = PepRequirement::from_str(&spec) else {
            rewritten.push(spec);
            continue;
        };
        if req.name.to_string().eq_ignore_ascii_case("gdal") {
            match &req.version_or_url {
                None => {
                    rewritten.push(format_requirement_with_cap(&req, &libgdal_ver));
                    continue;
                }
                Some(VersionOrUrl::VersionSpecifier(specifiers)) => {
                    let spec_str = specifiers.to_string();
                    if let Some(min) = min_requested_version(&spec_str) {
                        if min > libgdal_ver {
                            let hint = format!(
                                "base provides libgdal {libgdal_ver}; requested {spec} needs >= {min}; \
either unpin or choose a different version/base."
                            );
                            return Err(InstallUserError::new(
                                "dependency resolution failed",
                                json!({
                                    "code": "PX110",
                                    "reason": "incompatible_system_library",
                                    "package": "gdal",
                                    "requested": spec,
                                    "lib": "libgdal-dev",
                                    "lib_version": libgdal_ver.to_string(),
                                    "hint": hint,
                                }),
                            )
                            .into());
                        }
                    }
                    if !has_compatible_upper_bound(&spec_str, &libgdal_ver) {
                        rewritten.push(format_requirement_with_cap(&req, &libgdal_ver));
                        continue;
                    }
                }
                _ => {}
            }
        }
        rewritten.push(spec);
    }
    Ok(rewritten)
}
