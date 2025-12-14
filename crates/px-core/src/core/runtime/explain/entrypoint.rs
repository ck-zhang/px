use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result as AnyhowResult;
use serde_json::json;

use super::super::execution_plan;
use crate::{CommandContext, ExecutionOutcome};

pub fn explain_entrypoint(ctx: &CommandContext, name: &str) -> AnyhowResult<ExecutionOutcome> {
    let name = name.trim();
    if name.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            "entrypoint name is required",
            json!({
                "reason": "missing_entrypoint_name",
                "hint": "provide a console script name (example: `px explain entrypoint ruff`)",
            }),
        ));
    }

    let strict = ctx.env_flag_enabled("CI");
    let plan = match execution_plan::plan_run_execution(ctx, strict, false, "python", &[]) {
        Ok(plan) => plan,
        Err(outcome) => return Ok(outcome),
    };
    if plan.sys_path.entries.is_empty() {
        let mut details = json!({
            "reason": "missing_profile",
            "entrypoint": name,
        });
        if plan.would_repair_env {
            details["hint"] =
                json!("run `px sync` to build the environment before resolving entrypoints");
        }
        return Ok(ExecutionOutcome::user_error(
            "environment profile is not available",
            details,
        ));
    }

    #[derive(Clone, Debug)]
    struct Candidate {
        dist: String,
        version: Option<String>,
        entry_point: String,
        pkg_build_oid: Option<String>,
    }

    fn pkg_build_oid_from_sys_path(path: &Path) -> Option<String> {
        const PKG_BUILDS_DIR: &str = "pkg-builds";
        let mut iter = path.components().peekable();
        while let Some(comp) = iter.next() {
            let part = comp.as_os_str().to_string_lossy();
            if part == PKG_BUILDS_DIR {
                if let Some(next) = iter.next() {
                    let oid = next.as_os_str().to_string_lossy().to_string();
                    if !oid.is_empty() {
                        return Some(oid);
                    }
                }
            }
        }
        None
    }

    fn parse_console_scripts(contents: &str) -> Vec<(String, String)> {
        let mut in_console_scripts = false;
        let mut scripts = Vec::new();
        for line in contents.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
                continue;
            }
            if trimmed.starts_with('[') && trimmed.ends_with(']') {
                let section = &trimmed[1..trimmed.len() - 1];
                in_console_scripts = section.trim() == "console_scripts";
                continue;
            }
            if !in_console_scripts {
                continue;
            }
            let Some((key, value)) = trimmed.split_once('=') else {
                continue;
            };
            let key = key.trim();
            let value = value.trim();
            if key.is_empty() || value.is_empty() {
                continue;
            }
            scripts.push((key.to_string(), value.to_string()));
        }
        scripts
    }

    fn read_dist_metadata_name_version(dist_info: &Path) -> (String, Option<String>) {
        let metadata = dist_info.join("METADATA");
        let mut name = None;
        let mut version = None;
        if let Ok(contents) = fs::read_to_string(&metadata) {
            for line in contents.lines() {
                if name.is_none() {
                    if let Some(value) = line.strip_prefix("Name:") {
                        let trimmed = value.trim();
                        if !trimmed.is_empty() {
                            name = Some(trimmed.to_string());
                        }
                    }
                }
                if version.is_none() {
                    if let Some(value) = line.strip_prefix("Version:") {
                        let trimmed = value.trim();
                        if !trimmed.is_empty() {
                            version = Some(trimmed.to_string());
                        }
                    }
                }
                if name.is_some() && version.is_some() {
                    break;
                }
            }
        }
        let fallback = dist_info
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_string();
        (name.unwrap_or(fallback), version)
    }

    let mut candidates = Vec::<Candidate>::new();
    for entry in &plan.sys_path.entries {
        let sys_path = PathBuf::from(entry);
        if !sys_path.is_dir() {
            continue;
        }
        let pkg_build_oid = pkg_build_oid_from_sys_path(&sys_path);
        let Ok(entries) = fs::read_dir(&sys_path) else {
            continue;
        };
        let mut dist_infos = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("dist-info"))
                && path.is_dir()
            {
                dist_infos.push(path);
            }
        }
        dist_infos.sort();
        for dist_info in dist_infos {
            let entry_points = dist_info.join("entry_points.txt");
            if !entry_points.exists() {
                continue;
            }
            let contents = match fs::read_to_string(&entry_points) {
                Ok(contents) => contents,
                Err(_) => continue,
            };
            for (script, value) in parse_console_scripts(&contents) {
                if script != name {
                    continue;
                }
                let (dist, version) = read_dist_metadata_name_version(&dist_info);
                candidates.push(Candidate {
                    dist,
                    version,
                    entry_point: value,
                    pkg_build_oid: pkg_build_oid.clone(),
                });
            }
        }
    }

    candidates.sort_by(|a, b| {
        a.dist
            .cmp(&b.dist)
            .then(a.version.cmp(&b.version))
            .then(a.pkg_build_oid.cmp(&b.pkg_build_oid))
            .then(a.entry_point.cmp(&b.entry_point))
    });

    if candidates.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            format!("console script `{name}` not found in the current environment"),
            json!({
                "schema_version": 1,
                "reason": "entrypoint_not_found",
                "entrypoint": name,
                "hint": "check the entrypoint name, or run `px sync` to ensure the environment is up to date",
            }),
        ));
    }

    if candidates.len() > 1 {
        let rendered = candidates
            .iter()
            .map(|candidate| {
                json!({
                    "distribution": &candidate.dist,
                    "version": candidate.version.as_deref(),
                    "entry_point": &candidate.entry_point,
                    "pkg_build_oid": candidate.pkg_build_oid.as_deref(),
                })
            })
            .collect::<Vec<_>>();
        return Ok(ExecutionOutcome::user_error(
            format!("console script `{name}` is provided by multiple distributions"),
            json!({
                "schema_version": 1,
                "reason": "ambiguous_console_script",
                "entrypoint": name,
                "candidates": rendered,
                "hint": "Remove one of the distributions providing this script, or run a module directly via `px run python -m <module>`.",
            }),
        ));
    }

    let candidate = candidates.first().expect("single candidate").clone();
    let details = json!({
        "schema_version": 1,
        "entrypoint": name,
        "provider": {
            "distribution": candidate.dist,
            "version": candidate.version,
            "pkg_build_oid": candidate.pkg_build_oid,
        },
        "target": {
            "entry_point": candidate.entry_point,
        }
    });
    let dist = details["provider"]["distribution"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let version = details["provider"]["version"]
        .as_str()
        .map(|value| value.to_string());
    let pkg_build_oid = details["provider"]["pkg_build_oid"]
        .as_str()
        .map(|value| value.to_string());
    let entry_point = details["target"]["entry_point"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let version_display = version
        .as_deref()
        .map(|version| format!(" {version}"))
        .unwrap_or_default();
    let oid_display = pkg_build_oid
        .as_deref()
        .map(|oid| format!(" (pkg_build_oid={oid})"))
        .unwrap_or_default();
    let message = format!(
        "entrypoint: {name}\nprovider: {dist}{version_display}{oid_display}\ntarget: {entry_point}",
    );
    Ok(ExecutionOutcome::success(message, details))
}
