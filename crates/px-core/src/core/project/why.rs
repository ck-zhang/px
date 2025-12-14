use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::Result;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    dependency_name, manifest_snapshot, marker_env_for_snapshot, python_context_with_mode,
    CommandContext, EnvGuard, ExecutionOutcome, ManifestSnapshot, PythonContext,
};
use px_domain::api::{collect_resolved_dependencies, detect_lock_drift, load_lockfile_optional};

use super::{evaluate_project_state, issue_id_for};

#[derive(Clone, Debug)]
pub struct ProjectWhyRequest {
    pub package: Option<String>,
    pub issue: Option<String>,
}

/// Explains why a dependency or issue exists in the project.
///
/// # Errors
/// Returns an error if px.lock cannot be read or dependency graphs are unavailable.
/// # Panics
/// Panics if the resolver returns inconsistent dependency data.
pub fn project_why(ctx: &CommandContext, request: &ProjectWhyRequest) -> Result<ExecutionOutcome> {
    if let Some(issue) = request.issue.as_deref() {
        return explain_issue(ctx, issue);
    }
    let package = match request.package.as_deref() {
        Some(pkg) if !pkg.trim().is_empty() => pkg.trim().to_string(),
        _ => {
            return Ok(ExecutionOutcome::user_error(
                "px why requires a package name",
                json!({ "hint": "run `px why <package>` to inspect dependencies" }),
            ))
        }
    };
    let snapshot = manifest_snapshot()?;
    let state_report = evaluate_project_state(ctx, &snapshot)?;
    if !state_report.manifest_clean {
        return Ok(ExecutionOutcome::user_error(
            "Project manifest has changed since px.lock was created",
            json!({
                "pyproject": snapshot.manifest_path.display().to_string(),
                "lockfile": snapshot.lock_path.display().to_string(),
                "hint": "Run `px sync` to update px.lock and the environment.",
            }),
        ));
    }
    if !state_report.lock_exists {
        return Ok(ExecutionOutcome::user_error(
            "px.lock not found",
            json!({
                "lockfile": snapshot.lock_path.display().to_string(),
                "hint": "Run `px sync` to create px.lock before inspecting dependencies.",
            }),
        ));
    }
    let target = dependency_name(&package);
    if target.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            "unable to normalize package name",
            json!({
                "package": package,
                "hint": "use names like `rich` or `requests`",
            }),
        ));
    }
    let roots: HashSet<String> = snapshot
        .dependencies
        .iter()
        .map(|spec| dependency_name(spec))
        .filter(|name| !name.is_empty())
        .collect();
    let graph = if state_report.env_clean {
        match python_context_with_mode(ctx, EnvGuard::Strict) {
            Ok((py_ctx, _)) => match collect_dependency_graph(ctx, &py_ctx) {
                Ok(graph) => graph,
                Err(outcome) => return Ok(outcome),
            },
            Err(_) => match collect_dependency_graph_from_lock(&snapshot) {
                Ok(graph) => graph,
                Err(outcome) => return Ok(outcome),
            },
        }
    } else {
        match collect_dependency_graph_from_lock(&snapshot) {
            Ok(graph) => graph,
            Err(outcome) => return Ok(outcome),
        }
    };
    let implicit_base = matches!(target.as_str(), "pip" | "setuptools");
    let entry = graph.packages.get(&target);
    if entry.is_none() {
        if implicit_base {
            let reason = if target == "pip" {
                "provided by the selected Python runtime (ensurepip)"
            } else {
                "seeded by px as a deterministic base layer"
            };
            return Ok(ExecutionOutcome::success(
                format!("{package} is an implicit base package ({reason})"),
                json!({
                    "package": package,
                    "normalized": target,
                    "version": Value::Null,
                    "direct": false,
                    "chains": Vec::<Vec<String>>::new(),
                    "implicit": true,
                    "implicit_reason": reason,
                    "hint": "Run `px sync` to materialize the environment, then retry to see the installed version.",
                }),
            ));
        }
        let mut details = json!({
            "package": package,
            "hint": "run `px sync` to refresh the environment, then retry",
        });
        if !state_report.env_clean {
            if let Some(issue) = state_report.env_issue {
                details["environment_issue"] = issue;
            }
        }
        return Ok(ExecutionOutcome::user_error(
            format!("{package} is not installed in this project"),
            details,
        ));
    }
    let entry = entry.unwrap();
    let chains = find_dependency_chains(&graph.reverse, &roots, &target, 5);
    let direct = roots.contains(&target);
    let version = entry.version.clone();
    let (message, implicit_reason) = if implicit_base && !direct && chains.is_empty() {
        let reason = if target == "pip" {
            "provided by the selected Python runtime (ensurepip)"
        } else {
            "seeded by px as a deterministic base layer"
        };
        (
            format!(
                "{}=={} is an implicit base package ({reason})",
                entry.name, version
            ),
            Some(reason),
        )
    } else if direct {
        (
            format!("{}=={} is declared in pyproject.toml", entry.name, version),
            None,
        )
    } else if chains.is_empty() {
        (
            format!(
                "{}=={} is present but no dependency chain was found",
                entry.name, version
            ),
            None,
        )
    } else {
        let chain = chains
            .first()
            .map_or_else(|| entry.name.clone(), |path| path.join(" -> "));
        (
            format!("{}=={} is required by {chain}", entry.name, version),
            None,
        )
    };
    let mut details = json!({
        "package": entry.name,
        "normalized": target,
        "version": version,
        "direct": direct,
        "chains": chains,
    });
    if let Some(reason) = implicit_reason {
        details["implicit"] = json!(true);
        details["implicit_reason"] = json!(reason);
    }
    if !state_report.env_clean {
        details["hint"] =
            json!("Environment is out of sync with px.lock; run `px sync` to rebuild it.");
    }
    Ok(ExecutionOutcome::success(message, details))
}

fn explain_issue(_ctx: &CommandContext, issue_id: &str) -> Result<ExecutionOutcome> {
    let trimmed = issue_id.trim();
    if trimmed.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            "px why --issue requires an ID",
            json!({ "hint": "Run `px status` to list current issue IDs." }),
        ));
    }
    let snapshot = manifest_snapshot()?;
    let Some(lock) = load_lockfile_optional(&snapshot.lock_path)? else {
        return Ok(ExecutionOutcome::user_error(
            "px why --issue: px.lock not found",
            json!({
                "lockfile": snapshot.lock_path.display().to_string(),
                "hint": "Run `px sync` to create px.lock before inspecting issues.",
            }),
        ));
    };
    let marker_env = marker_env_for_snapshot(&snapshot);
    let drift = detect_lock_drift(&snapshot, &lock, marker_env.as_ref());
    let mut normalized = trimmed.to_string();
    normalized.make_ascii_uppercase();
    for message in drift {
        let id = issue_id_for(&message);
        if id.eq_ignore_ascii_case(&normalized) {
            let summary = format!("Issue {id}: {message}");
            let details = json!({
                "id": id,
                "message": message,
                "pyproject": snapshot.manifest_path.display().to_string(),
                "lockfile": snapshot.lock_path.display().to_string(),
            });
            return Ok(ExecutionOutcome::success(summary, details));
        }
    }
    Ok(ExecutionOutcome::user_error(
        format!("issue {issue_id} not found"),
        json!({
            "issue": issue_id,
            "hint": "Run `px status` to list current issue IDs before retrying.",
        }),
    ))
}

fn collect_dependency_graph(
    ctx: &CommandContext,
    py_ctx: &PythonContext,
) -> Result<WhyGraph, ExecutionOutcome> {
    let envs = py_ctx
        .base_env(&json!({ "reason": "why" }))
        .map_err(|err| {
            ExecutionOutcome::failure(
                "failed to prepare project environment",
                json!({ "error": err.to_string() }),
            )
        })?;
    let args = vec!["-c".to_string(), WHY_GRAPH_SCRIPT.to_string()];
    let output = ctx
        .python_runtime()
        .run_command(&py_ctx.python, &args, &envs, &py_ctx.project_root)
        .map_err(|err| {
            ExecutionOutcome::failure(
                "failed to inspect dependencies",
                json!({ "error": err.to_string() }),
            )
        })?;
    if output.code != 0 {
        return Err(ExecutionOutcome::failure(
            "python exited with errors while reading metadata",
            json!({
                "stderr": output.stderr,
                "status": output.code,
            }),
        ));
    }
    let payload: WhyGraphPayload = serde_json::from_str(output.stdout.trim()).map_err(|err| {
        ExecutionOutcome::failure(
            "invalid dependency metadata payload",
            json!({ "error": err.to_string() }),
        )
    })?;
    let mut packages = HashMap::new();
    let mut reverse: HashMap<String, Vec<String>> = HashMap::new();
    for package in payload.packages {
        for dep in &package.requires {
            let parents = reverse.entry(dep.clone()).or_default();
            if !parents.iter().any(|p| p == &package.normalized) {
                parents.push(package.normalized.clone());
            }
        }
        packages.insert(package.normalized.clone(), package);
    }

    let lock_path = py_ctx.project_root.join("px.lock");
    if lock_path.exists() {
        let lock = load_lockfile_optional(&lock_path).map_err(|err| {
            ExecutionOutcome::failure(
                "failed to read px.lock",
                json!({ "error": err.to_string(), "lockfile": lock_path.display().to_string() }),
            )
        })?;
        if let Some(lock) = lock {
            for dep in collect_resolved_dependencies(&lock) {
                let normalized = dependency_name(&dep.specifier);
                if normalized.is_empty() {
                    continue;
                }
                let version = version_from_spec(&dep.specifier);
                packages
                    .entry(normalized.clone())
                    .or_insert_with(|| WhyPackage {
                        name: dep.name.clone(),
                        normalized: normalized.clone(),
                        version: version.clone(),
                        requires: dep.requires.clone(),
                    });
                if let Some(pkg) = packages.get_mut(&normalized) {
                    if pkg.version.is_empty() {
                        pkg.version = version.clone();
                    }
                    if pkg.requires.is_empty() && !dep.requires.is_empty() {
                        pkg.requires = dep.requires.clone();
                    }
                }
                for parent in dep.requires {
                    if parent.is_empty() {
                        continue;
                    }
                    let parents = reverse.entry(parent).or_default();
                    if !parents.iter().any(|p| p == &normalized) {
                        parents.push(normalized.clone());
                    }
                }
            }
        }
    }

    Ok(WhyGraph { packages, reverse })
}

pub fn collect_dependency_graph_from_lock(
    snapshot: &ManifestSnapshot,
) -> Result<WhyGraph, ExecutionOutcome> {
    let lock = match load_lockfile_optional(&snapshot.lock_path) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            return Err(ExecutionOutcome::user_error(
                "px.lock not found",
                json!({
                    "lockfile": snapshot.lock_path.display().to_string(),
                    "hint": "Run `px sync` to create px.lock before inspecting dependencies.",
                }),
            ))
        }
        Err(err) => {
            return Err(ExecutionOutcome::failure(
                "failed to read px.lock",
                json!({ "error": err.to_string(), "lockfile": snapshot.lock_path.display().to_string() }),
            ))
        }
    };

    let mut packages = HashMap::new();
    let mut reverse: HashMap<String, Vec<String>> = HashMap::new();
    for dep in collect_resolved_dependencies(&lock) {
        let normalized = dependency_name(&dep.specifier);
        if normalized.is_empty() {
            continue;
        }
        let version = version_from_spec(&dep.specifier);
        packages
            .entry(normalized.clone())
            .or_insert_with(|| WhyPackage {
                name: dep.name.clone(),
                normalized: normalized.clone(),
                version: version.clone(),
                requires: dep.requires.clone(),
            });
        if let Some(pkg) = packages.get_mut(&normalized) {
            if pkg.version.is_empty() {
                pkg.version = version.clone();
            }
            if pkg.requires.is_empty() && !dep.requires.is_empty() {
                pkg.requires = dep.requires.clone();
            }
        }
        for parent in dep.requires {
            if parent.is_empty() {
                continue;
            }
            let parents = reverse.entry(parent).or_default();
            if !parents.iter().any(|p| p == &normalized) {
                parents.push(normalized.clone());
            }
        }
    }

    Ok(WhyGraph { packages, reverse })
}

fn find_dependency_chains(
    reverse: &HashMap<String, Vec<String>>,
    roots: &HashSet<String>,
    target: &str,
    limit: usize,
) -> Vec<Vec<String>> {
    if limit == 0 {
        return Vec::new();
    }
    let mut results = Vec::new();
    let mut queue = VecDeque::new();
    queue.push_back(vec![target.to_string()]);

    while let Some(path) = queue.pop_front() {
        let current = path.last().cloned().unwrap_or_else(|| target.to_string());
        if roots.contains(&current) {
            let mut chain = path.clone();
            chain.reverse();
            results.push(chain);
            if results.len() >= limit {
                break;
            }
        }
        if let Some(parents) = reverse.get(&current) {
            for parent in parents {
                if path.iter().any(|node| node == parent) {
                    continue;
                }
                let mut next = path.clone();
                next.push(parent.clone());
                queue.push_back(next);
            }
        }
    }
    results
}

fn version_from_spec(spec: &str) -> String {
    let trimmed = spec.trim();
    let head = trimmed.split(';').next().unwrap_or(trimmed);
    if let Some((_, rest)) = head.split_once("==") {
        rest.trim().to_string()
    } else {
        String::new()
    }
}

const WHY_GRAPH_SCRIPT: &str = r"
import importlib.metadata as im
import json

def normalize(name: str) -> str:
    return name.strip().lower()

packages = []
for dist in im.distributions():
    name = dist.metadata.get('Name') or dist.name
    if not name:
        continue
    normalized = normalize(name)
    requires = []
    if dist.requires:
        for req in dist.requires:
            head = req.split(';', 1)[0].strip()
            if not head:
                continue
            token = head.split()[0]
            if not token:
                continue
            base = token.split('[', 1)[0].strip()
            if base:
                requires.append(normalize(base))
    packages.append({
        'name': name,
        'normalized': normalized,
        'version': dist.version or '',
        'requires': requires,
    })

print(json.dumps({'packages': packages}))
";

pub(crate) struct WhyGraph {
    packages: HashMap<String, WhyPackage>,
    reverse: HashMap<String, Vec<String>>,
}

#[derive(Deserialize)]
struct WhyGraphPayload {
    packages: Vec<WhyPackage>,
}

#[derive(Clone, Deserialize)]
struct WhyPackage {
    name: String,
    normalized: String,
    version: String,
    requires: Vec<String>,
}
