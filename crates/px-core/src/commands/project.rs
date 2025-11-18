use std::{
    collections::HashSet,
    env, fs,
    path::{Path, PathBuf},
    str::FromStr,
};

use anyhow::{anyhow, Context, Result};
use pep508_rs::{Requirement as PepRequirement, VersionOrUrl};
use serde::Deserialize;
use serde_json::{json, Value};
use toml_edit::{DocumentMut, Item};

use crate::{
    dependency_name, install_snapshot, manifest_snapshot, manifest_snapshot_at,
    refresh_project_site, relative_path_str, CommandContext, ExecutionOutcome, InstallOutcome,
    InstallState, InstallUserError, ManifestSnapshot,
};
use px_project::{
    discover_project_root, infer_package_name, InstallOverride, ManifestEditor, ProjectInitializer,
};

#[derive(Clone, Debug)]
pub struct ProjectInitRequest {
    pub package: Option<String>,
    pub python: Option<String>,
    pub dry_run: bool,
    pub force: bool,
}

#[derive(Clone, Debug)]
pub struct ProjectInstallRequest {
    pub frozen: bool,
}

#[derive(Clone, Debug)]
pub struct ProjectAddRequest {
    pub specs: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct ProjectRemoveRequest {
    pub specs: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct ProjectUpdateRequest {
    pub specs: Vec<String>,
}

pub fn project_init(ctx: &CommandContext, request: ProjectInitRequest) -> Result<ExecutionOutcome> {
    let cwd = env::current_dir().context("unable to determine current directory")?;
    if let Some(existing_root) = discover_project_root()? {
        return existing_pyproject_response(&existing_root.join("pyproject.toml"));
    }
    let root = cwd;
    let pyproject_path = root.join("pyproject.toml");
    let pyproject_preexisting = pyproject_path.exists();

    if pyproject_preexisting {
        if let Some(conflict) = detect_init_conflict(&pyproject_path)? {
            return Ok(conflict.into_outcome(&pyproject_path));
        }
    }

    if !request.force {
        if let Some(changes) = ctx.git().worktree_changes(&root)? {
            if !changes.is_empty() {
                return Ok(dirty_worktree_response(changes));
            }
        }
    }

    let package_arg = request
        .package
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let (package, inferred) = infer_package_name(package_arg, &root)?;
    let python_req = resolve_python_requirement_arg(request.python.as_deref());

    let mut files = ProjectInitializer::scaffold(&root, &package, &python_req)?;
    let snapshot = manifest_snapshot_at(&root)?;
    let actual_name = snapshot.name.clone();
    let lock_existed = snapshot.lock_path.exists();
    match install_snapshot(ctx, &snapshot, false, None) {
        Ok(_) => {}
        Err(err) => match err.downcast::<InstallUserError>() {
            Ok(user) => return Ok(ExecutionOutcome::user_error(user.message, user.details)),
            Err(err) => return Err(err),
        },
    };
    refresh_project_site(&snapshot, ctx.fs())?;
    if !lock_existed {
        files.push(relative_path_str(&snapshot.lock_path, &snapshot.root));
    }

    let mut details = json!({
        "package": actual_name,
        "python": python_req,
        "files_created": files,
        "project_root": root.display().to_string(),
        "lockfile": snapshot.lock_path.display().to_string(),
    });
    if inferred && !pyproject_preexisting {
        details["inferred_package"] = Value::Bool(true);
        details["hint"] = Value::String(
            "Pass --package <name> to override the inferred module name.".to_string(),
        );
    }

    Ok(ExecutionOutcome::success(
        format!("initialized project {actual_name}"),
        details,
    ))
}

pub fn project_install(
    ctx: &CommandContext,
    request: ProjectInstallRequest,
) -> Result<ExecutionOutcome> {
    project_install_outcome(ctx, request.frozen)
}

pub fn project_add(ctx: &CommandContext, request: ProjectAddRequest) -> Result<ExecutionOutcome> {
    if request.specs.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            "provide at least one dependency",
            json!({ "hint": "run `px add name==version`" }),
        ));
    }

    let root = ctx.project_root()?;
    let pyproject_path = root.join("pyproject.toml");
    let cleaned_specs: Vec<String> = request
        .specs
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if cleaned_specs.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            "provide at least one dependency",
            json!({ "hint": "run `px add name==version`" }),
        ));
    }

    let lock_path = root.join("px.lock");
    let backup = ManifestLockBackup::capture(&pyproject_path, &lock_path)?;
    let mut needs_restore = true;

    let outcome = (|| -> Result<ExecutionOutcome> {
        let mut editor = ManifestEditor::open(&pyproject_path)?;
        let report = editor.add_specs(&cleaned_specs)?;

        if report.added.is_empty() && report.updated.is_empty() {
            needs_restore = false;
            return Ok(ExecutionOutcome::success(
                "dependencies already satisfied",
                json!({ "pyproject": pyproject_path.display().to_string() }),
            ));
        }

        let (snapshot, _install) = match sync_manifest_environment(ctx) {
            Ok(result) => result,
            Err(outcome) => return Ok(outcome),
        };
        needs_restore = false;
        let message = format!(
            "updated dependencies (added {}, updated {})",
            report.added.len(),
            report.updated.len()
        );
        Ok(ExecutionOutcome::success(
            message,
            json!({
                "pyproject": pyproject_path.display().to_string(),
                "lockfile": snapshot.lock_path.display().to_string(),
                "added": report.added,
                "updated": report.updated,
            }),
        ))
    })();

    if needs_restore {
        backup.restore()?;
    }

    outcome
}

pub fn project_remove(
    ctx: &CommandContext,
    request: ProjectRemoveRequest,
) -> Result<ExecutionOutcome> {
    if request.specs.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            "provide at least one dependency to remove",
            json!({ "hint": "run `px remove name`" }),
        ));
    }

    let root = ctx.project_root()?;
    let pyproject_path = root.join("pyproject.toml");
    let cleaned_specs: Vec<String> = request
        .specs
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if cleaned_specs.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            "dependencies must contain at least one name",
            json!({ "hint": "use bare names like requests==2.32.3" }),
        ));
    }

    let lock_path = root.join("px.lock");
    let backup = ManifestLockBackup::capture(&pyproject_path, &lock_path)?;
    let mut needs_restore = true;

    let outcome = (|| -> Result<ExecutionOutcome> {
        let mut editor = ManifestEditor::open(&pyproject_path)?;
        let report = editor.remove_specs(&cleaned_specs)?;
        if report.removed.is_empty() {
            let names: Vec<String> = cleaned_specs
                .iter()
                .map(|spec| dependency_name(spec))
                .filter(|name| !name.is_empty())
                .collect();
            needs_restore = false;
            return Ok(ExecutionOutcome::user_error(
                "package is not a direct dependency",
                json!({
                    "packages": names,
                    "hint": "Use `px why <package>` to inspect transitive requirements.",
                }),
            ));
        }

        let (snapshot, _install) = match sync_manifest_environment(ctx) {
            Ok(result) => result,
            Err(outcome) => return Ok(outcome),
        };
        needs_restore = false;
        Ok(ExecutionOutcome::success(
            "removed dependencies",
            json!({
                "pyproject": pyproject_path.display().to_string(),
                "lockfile": snapshot.lock_path.display().to_string(),
                "removed": report.removed,
            }),
        ))
    })();

    if needs_restore {
        backup.restore()?;
    }

    outcome
}

pub fn project_status(ctx: &CommandContext) -> Result<ExecutionOutcome> {
    let snapshot = manifest_snapshot()?;
    let outcome = match install_snapshot(ctx, &snapshot, true, None) {
        Ok(outcome) => outcome,
        Err(err) => match err.downcast::<InstallUserError>() {
            Ok(user) => return Ok(ExecutionOutcome::user_error(user.message, user.details)),
            Err(err) => return Err(err),
        },
    };
    let mut details = json!({
        "pyproject": snapshot.manifest_path.display().to_string(),
        "lockfile": snapshot.lock_path.display().to_string(),
        "project": {
            "root": snapshot.root.display().to_string(),
            "name": snapshot.name.clone(),
            "python_requirement": snapshot.python_requirement.clone(),
        },
    });
    if let Some(runtime) = detect_runtime_details(ctx, &snapshot) {
        details["runtime"] = runtime;
    }
    match outcome.state {
        InstallState::UpToDate => {
            details["status"] = Value::String("in-sync".to_string());
            Ok(ExecutionOutcome::success(
                "Environment is in sync with px.lock",
                details,
            ))
        }
        InstallState::Drift => {
            details["status"] = Value::String("drift".to_string());
            details["issues"] = Value::Array(outcome.drift.into_iter().map(|d| json!(d)).collect());
            details["hint"] = Value::String("Run `px sync` to refresh px.lock".to_string());
            Ok(ExecutionOutcome::user_error(
                "Environment is out of sync with px.lock",
                details,
            ))
        }
        InstallState::MissingLock => {
            details["status"] = Value::String("missing-lock".to_string());
            details["hint"] = Value::String("Run `px sync` to create px.lock".to_string());
            Ok(ExecutionOutcome::user_error("px.lock not found", details))
        }
        _ => Ok(ExecutionOutcome::failure(
            "Unable to determine project status",
            json!({ "status": "unknown" }),
        )),
    }
}

pub fn project_update(
    ctx: &CommandContext,
    request: ProjectUpdateRequest,
) -> Result<ExecutionOutcome> {
    let root = ctx.project_root()?;
    let pyproject_path = root.join("pyproject.toml");
    let lock_path = root.join("px.lock");
    if !lock_path.exists() {
        return Ok(ExecutionOutcome::user_error(
            "px update requires an existing px.lock (run `px sync`)",
            json!({
                "lockfile": lock_path.display().to_string(),
                "hint": "run `px sync` to create px.lock before updating",
            }),
        ));
    }

    let snapshot = manifest_snapshot()?;
    if snapshot.dependencies.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            "px update: no dependencies declared",
            json!({
                "pyproject": pyproject_path.display().to_string(),
                "hint": "add dependencies with `px add` before running update",
            }),
        ));
    }

    let targets: HashSet<String> = request
        .specs
        .iter()
        .map(|spec| dependency_name(spec))
        .filter(|name| !name.is_empty())
        .collect();
    let update_all = targets.is_empty();

    let mut override_specs = snapshot.dependencies.clone();
    let mut ready = Vec::new();
    let mut ready_seen = HashSet::new();
    let mut unsupported = Vec::new();
    let mut unsupported_seen = HashSet::new();
    let mut observed = HashSet::new();

    for spec in override_specs.iter_mut() {
        let name = dependency_name(spec);
        if name.is_empty() {
            continue;
        }
        if !update_all && !targets.contains(&name) {
            continue;
        }
        observed.insert(name.clone());
        match loosen_dependency_spec(spec)? {
            LoosenOutcome::Modified(rewritten) => {
                *spec = rewritten;
                if ready_seen.insert(name.clone()) {
                    ready.push(name.clone());
                }
            }
            LoosenOutcome::AlreadyLoose => {
                if ready_seen.insert(name.clone()) {
                    ready.push(name.clone());
                }
            }
            LoosenOutcome::Unsupported => {
                if unsupported_seen.insert(name.clone()) {
                    unsupported.push(name.clone());
                }
            }
        }
    }

    if !update_all {
        let missing: Vec<String> = targets.difference(&observed).cloned().collect();
        if !missing.is_empty() {
            return Ok(ExecutionOutcome::user_error(
                "package is not a direct dependency",
                json!({
                    "packages": missing,
                    "hint": "use `px add` to declare dependencies before updating",
                }),
            ));
        }
    }

    if ready.is_empty() {
        let mut details = json!({
            "pyproject": pyproject_path.display().to_string(),
        });
        if !unsupported.is_empty() {
            details["unsupported"] = json!(unsupported);
            details["hint"] = json!("Dependencies pinned via direct URLs must be updated manually");
        }
        let message = if update_all {
            "no dependencies eligible for px update"
        } else {
            "px update: requested packages cannot be updated"
        };
        return Ok(ExecutionOutcome::user_error(message, details));
    }

    let override_data = InstallOverride {
        dependencies: override_specs,
        pins: Vec::new(),
    };

    let install_outcome = match install_snapshot(ctx, &snapshot, false, Some(&override_data)) {
        Ok(result) => result,
        Err(err) => match err.downcast::<InstallUserError>() {
            Ok(user) => return Ok(ExecutionOutcome::user_error(user.message, user.details)),
            Err(err) => return Err(err),
        },
    };

    refresh_project_site(&snapshot, ctx.fs())?;

    let updated_count = ready.len();
    let primary_label = ready.first().cloned();
    let mut details = json!({
        "pyproject": pyproject_path.display().to_string(),
        "lockfile": snapshot.lock_path.display().to_string(),
        "targets": ready,
    });
    if !unsupported.is_empty() {
        details["skipped"] = json!(unsupported);
        details["hint"] = json!("Dependencies pinned via direct URLs must be updated manually");
    }

    let message = if update_all {
        "updated project dependencies".to_string()
    } else if updated_count == 1 {
        format!(
            "updated {}",
            primary_label.unwrap_or_else(|| "dependency".to_string())
        )
    } else {
        format!("updated {} dependencies", updated_count)
    };

    match install_outcome.state {
        InstallState::Installed | InstallState::UpToDate => {
            Ok(ExecutionOutcome::success(message, details))
        }
        InstallState::Drift | InstallState::MissingLock => Ok(ExecutionOutcome::failure(
            "px update failed to refresh px.lock",
            json!({ "lockfile": snapshot.lock_path.display().to_string() }),
        )),
    }
}

#[derive(Debug)]
enum LoosenOutcome {
    Modified(String),
    AlreadyLoose,
    Unsupported,
}

fn loosen_dependency_spec(spec: &str) -> Result<LoosenOutcome> {
    let trimmed = crate::strip_wrapping_quotes(spec.trim());
    let requirement = PepRequirement::from_str(trimmed)
        .map_err(|err| anyhow!("unable to parse dependency spec `{}`: {err}", spec))?;
    match requirement.version_or_url {
        Some(VersionOrUrl::VersionSpecifier(_)) => {
            let mut unlocked = requirement.clone();
            unlocked.version_or_url = None;
            Ok(LoosenOutcome::Modified(unlocked.to_string()))
        }
        Some(VersionOrUrl::Url(_)) => Ok(LoosenOutcome::Unsupported),
        None => Ok(LoosenOutcome::AlreadyLoose),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loosen_dependency_spec_removes_pin() {
        match loosen_dependency_spec("requests==2.32.0").expect("parsed") {
            LoosenOutcome::Modified(value) => assert_eq!(value, "requests"),
            other => panic!("unexpected outcome: {:?}", other),
        }
    }

    #[test]
    fn loosen_dependency_spec_detects_direct_urls() {
        let spec = "demo @ https://example.invalid/demo-1.0.0.whl";
        assert!(matches!(
            loosen_dependency_spec(spec).expect("parsed"),
            LoosenOutcome::Unsupported
        ));
    }
}

fn resolve_python_requirement_arg(raw: Option<&str>) -> String {
    raw.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            if s.starts_with('>') {
                s.to_string()
            } else {
                format!(">={s}")
            }
        })
        .unwrap_or_else(|| ">=3.12".to_string())
}

#[derive(Debug)]
enum InitConflict {
    OtherTool(String),
    ExistingDependencies,
}

impl InitConflict {
    fn into_outcome(self, pyproject_path: &Path) -> ExecutionOutcome {
        match self {
            InitConflict::OtherTool(tool) => ExecutionOutcome::user_error(
                format!("pyproject managed by {tool}; run `px migrate --apply` to adopt px"),
                json!({
                    "pyproject": pyproject_path.display().to_string(),
                    "tool": tool,
                    "hint": "Run `px migrate --apply` to convert this project to px.",
                }),
            ),
            InitConflict::ExistingDependencies => ExecutionOutcome::user_error(
                "pyproject already declares dependencies",
                json!({
                    "pyproject": pyproject_path.display().to_string(),
                    "hint": "Run `px migrate --apply` to import existing dependencies into px.",
                }),
            ),
        }
    }
}

fn detect_init_conflict(pyproject_path: &Path) -> Result<Option<InitConflict>> {
    let contents = fs::read_to_string(pyproject_path)?;
    let doc: DocumentMut = contents.parse()?;
    if let Some(tool) = detect_foreign_tool(&doc) {
        return Ok(Some(InitConflict::OtherTool(tool)));
    }
    if project_dependencies_declared(&doc) {
        return Ok(Some(InitConflict::ExistingDependencies));
    }
    Ok(None)
}

fn detect_foreign_tool(doc: &DocumentMut) -> Option<String> {
    let tools = doc
        .get("tool")
        .and_then(Item::as_table)
        .map(|table| {
            table
                .iter()
                .filter_map(|(key, _)| {
                    let name = key.to_string();
                    match name.as_str() {
                        "px" => None,
                        "poetry" | "pdm" | "hatch" | "flit" | "rye" => Some(name),
                        _ => None,
                    }
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    tools.into_iter().next()
}

fn project_dependencies_declared(doc: &DocumentMut) -> bool {
    doc.get("project")
        .and_then(Item::as_table)
        .and_then(|table| table.get("dependencies"))
        .and_then(Item::as_array)
        .map(|array| !array.is_empty())
        .unwrap_or(false)
}

fn existing_pyproject_response(pyproject_path: &Path) -> Result<ExecutionOutcome> {
    let mut details = json!({
        "pyproject": pyproject_path.display().to_string(),
    });
    if let Some(name) = project_name_from_pyproject(pyproject_path)? {
        details["package"] = Value::String(name);
    }
    details["hint"] = Value::String(
        "pyproject.toml already exists; run `px add` or start in an empty directory.".to_string(),
    );
    Ok(ExecutionOutcome::user_error(
        "project already initialized (pyproject.toml present)",
        details,
    ))
}

fn project_name_from_pyproject(pyproject_path: &Path) -> Result<Option<String>> {
    px_project::project_name_from_pyproject(pyproject_path)
}

fn detect_runtime_details(
    ctx: &CommandContext,
    snapshot: &ManifestSnapshot,
) -> Option<Value> {
    let python = ctx.python_runtime().detect_interpreter().ok()?;
    match probe_python_version(ctx, snapshot, &python) {
        Ok(version) => Some(json!({
            "path": python,
            "version": version,
        })),
        Err(err) => Some(json!({
            "path": python,
            "hint": format!("failed to detect python version: {err}"),
        })),
    }
}

fn probe_python_version(
    ctx: &CommandContext,
    snapshot: &ManifestSnapshot,
    python: &str,
) -> Result<String> {
    const SCRIPT: &str =
        "import json, platform; print(json.dumps({'version': platform.python_version()}))";
    let args = vec!["-c".to_string(), SCRIPT.to_string()];
    let output = ctx
        .python_runtime()
        .run_command(python, &args, &[], &snapshot.root)?;
    if output.code != 0 {
        return Err(anyhow!("python exited with {}", output.code));
    }
    let payload: RuntimeProbe =
        serde_json::from_str(output.stdout.trim()).context("invalid runtime probe payload")?;
    Ok(payload.version)
}

#[derive(Deserialize)]
struct RuntimeProbe {
    version: String,
}

fn dirty_worktree_response(changes: Vec<String>) -> ExecutionOutcome {
    ExecutionOutcome::user_error(
        "worktree dirty; stash, commit, or rerun with --force",
        json!({
            "changes": changes,
            "hint": "Stash or commit changes, or add --force to bypass this guard.",
        }),
    )
}

struct ManifestLockBackup {
    pyproject_path: PathBuf,
    lock_path: PathBuf,
    pyproject_contents: String,
    lock_contents: Option<String>,
    lock_preexisting: bool,
}

impl ManifestLockBackup {
    fn capture(pyproject_path: &Path, lock_path: &Path) -> Result<Self> {
        let pyproject_contents = fs::read_to_string(pyproject_path)?;
        let lock_preexisting = lock_path.exists();
        let lock_contents = if lock_preexisting {
            Some(fs::read_to_string(lock_path)?)
        } else {
            None
        };
        Ok(Self {
            pyproject_path: pyproject_path.to_path_buf(),
            lock_path: lock_path.to_path_buf(),
            pyproject_contents,
            lock_contents,
            lock_preexisting,
        })
    }

    fn restore(&self) -> Result<()> {
        fs::write(&self.pyproject_path, &self.pyproject_contents)?;
        match (&self.lock_contents, self.lock_preexisting) {
            (Some(contents), _) => {
                fs::write(&self.lock_path, contents)?;
            }
            (None, false) => {
                if self.lock_path.exists() {
                    fs::remove_file(&self.lock_path)?;
                }
            }
            (None, true) => {
                debug_assert!(
                    false,
                    "lock_preexisting implies lock contents should have been captured"
                );
            }
        }
        Ok(())
    }
}

fn sync_manifest_environment(
    ctx: &CommandContext,
) -> Result<(ManifestSnapshot, InstallOutcome), ExecutionOutcome> {
    let snapshot = match manifest_snapshot() {
        Ok(snapshot) => snapshot,
        Err(err) => {
            return Err(ExecutionOutcome::failure(
                "failed to read project manifest",
                json!({ "error": err.to_string() }),
            ))
        }
    };
    let outcome = match install_snapshot(ctx, &snapshot, false, None) {
        Ok(outcome) => outcome,
        Err(err) => match err.downcast::<InstallUserError>() {
            Ok(user) => return Err(ExecutionOutcome::user_error(user.message, user.details)),
            Err(err) => {
                return Err(ExecutionOutcome::failure(
                    "failed to install dependencies",
                    json!({ "error": err.to_string() }),
                ))
            }
        },
    };
    if let Err(err) = refresh_project_site(&snapshot, ctx.fs()) {
        return Err(ExecutionOutcome::failure(
            "failed to update project environment",
            json!({ "error": err.to_string() }),
        ));
    }
    Ok((snapshot, outcome))
}

fn project_install_outcome(ctx: &CommandContext, frozen: bool) -> Result<ExecutionOutcome> {
    let snapshot = manifest_snapshot()?;
    let outcome = match install_snapshot(ctx, &snapshot, frozen, None) {
        Ok(ok) => ok,
        Err(err) => match err.downcast::<InstallUserError>() {
            Ok(user) => return Ok(ExecutionOutcome::user_error(user.message, user.details)),
            Err(err) => return Err(err),
        },
    };
    let mut details = json!({
        "lockfile": outcome.lockfile,
        "project": snapshot.name,
        "python": snapshot.python_requirement,
    });

    match outcome.state {
        InstallState::Installed => {
            refresh_project_site(&snapshot, ctx.fs())?;
            Ok(ExecutionOutcome::success(
                format!("wrote {}", outcome.lockfile),
                details,
            ))
        }
        InstallState::UpToDate => {
            refresh_project_site(&snapshot, ctx.fs())?;
            let message = if frozen && outcome.verified {
                "lockfile verified".to_string()
            } else {
                "px.lock already up to date".to_string()
            };
            Ok(ExecutionOutcome::success(message, details))
        }
        InstallState::Drift => {
            details["drift"] = Value::Array(outcome.drift.iter().map(|d| json!(d)).collect());
            details["hint"] = Value::String("rerun `px sync` to refresh px.lock".to_string());
            Ok(ExecutionOutcome::user_error(
                "px.lock is out of date",
                details,
            ))
        }
        InstallState::MissingLock => Ok(ExecutionOutcome::user_error(
            "px.lock not found (run `px sync`)",
            json!({
                "lockfile": outcome.lockfile,
                "project": snapshot.name,
                "python": snapshot.python_requirement,
                "hint": "run `px sync` to generate a lockfile",
            }),
        )),
    }
}
