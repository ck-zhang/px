use anyhow::Result;
use pep440_rs::{Version, VersionSpecifiers};
use serde_json::{json, Value};
use std::cmp::Ordering;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::core::runtime::runtime_manager;
use crate::project::{evaluate_project_state, project_sync, ProjectSyncRequest};
use crate::workspace::{
    evaluate_workspace_state, load_workspace_snapshot, workspace_sync, WorkspaceScope,
    WorkspaceStateKind, WorkspaceSyncRequest,
};
use crate::{
    is_missing_project_error, manifest_snapshot, progress::ProgressReporter, CommandContext,
    ExecutionOutcome, InstallUserError,
};
use px_domain::api::{
    discover_workspace_root, read_workspace_config, ManifestEditor, ProjectSnapshot,
};

pub struct PythonListRequest;

#[derive(Clone, Debug)]
pub struct PythonInstallRequest {
    pub version: String,
    pub path: Option<String>,
    pub set_default: bool,
}

#[derive(Clone, Debug)]
pub struct PythonUseRequest {
    pub version: String,
}

pub struct PythonInfoRequest;

/// Lists registered px-managed Python runtimes.
///
/// # Errors
/// Returns an error if the runtime registry cannot be read.
pub fn python_list(
    _ctx: &CommandContext,
    _request: &PythonListRequest,
) -> Result<ExecutionOutcome> {
    let mut runtimes = match runtime_manager::list_runtimes() {
        Ok(runtimes) => runtimes,
        Err(err) => return Ok(runtime_registry_error(err)),
    };
    runtimes.sort_by(|left, right| {
        let left_version = Version::from_str(&left.full_version).ok();
        let right_version = Version::from_str(&right.full_version).ok();
        match (left_version, right_version) {
            (Some(left), Some(right)) => right.cmp(&left),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => right.full_version.cmp(&left.full_version),
        }
        .then_with(|| right.version.cmp(&left.version))
        .then_with(|| left.path.cmp(&right.path))
    });
    let details: Vec<Value> = runtimes.iter().map(runtime_to_json).collect();
    if runtimes.is_empty() {
        return Ok(ExecutionOutcome::success(
            "no px runtimes installed",
            json!({ "runtimes": details }),
        ));
    }
    let summary = runtimes
        .iter()
        .map(|rt| {
            if rt.default {
                format!("{}  {}  (default)", rt.version, rt.path)
            } else {
                format!("{}  {}", rt.version, rt.path)
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    Ok(ExecutionOutcome::success(
        format!("registered runtimes:\n{summary}"),
        json!({ "runtimes": details }),
    ))
}

/// Registers a Python runtime for px to use.
///
/// # Errors
/// Returns an error if the runtime cannot be inspected or persisted.
pub fn python_install(
    _ctx: &CommandContext,
    request: &PythonInstallRequest,
) -> Result<ExecutionOutcome> {
    let spinner = ProgressReporter::spinner(format!("Installing Python {}", request.version));
    let record = match runtime_manager::install_runtime(
        &request.version,
        request.path.as_deref(),
        request.set_default,
    ) {
        Ok(record) => record,
        Err(err) => match err.downcast::<InstallUserError>() {
            Ok(user) => return Ok(ExecutionOutcome::user_error(user.message, user.details)),
            Err(err) => return Ok(runtime_install_error(err)),
        },
    };
    spinner.finish(format!(
        "Installed Python {} at {}",
        record.version, record.path
    ));
    Ok(ExecutionOutcome::success(
        format!("registered Python {} at {}", record.version, record.path),
        runtime_to_json(&record),
    ))
}

/// Sets the Python runtime requirement for the current project.
///
/// # Errors
/// Returns an error if the runtime is unavailable or the manifest cannot be edited.
pub fn python_use(ctx: &CommandContext, request: &PythonUseRequest) -> Result<ExecutionOutcome> {
    let strict = ctx.env_flag_enabled("CI");
    let normalized = runtime_manager::normalize_channel(&request.version)?;
    let runtimes = runtime_manager::list_runtimes()?;
    let record = runtimes
        .iter()
        .find(|rt| rt.version == normalized)
        .cloned()
        .ok_or_else(|| {
            InstallUserError::new(
            format!("runtime {normalized} is not installed"),
            json!({
                "version": normalized,
                "hint": format!("run `px python install {normalized}` to register this runtime"),
            }),
            )
        })?;

    let runtime_version = Version::from_str(&record.full_version).map_err(|err| {
        InstallUserError::new(
            "unable to parse runtime version",
            json!({
                "runtime": record.full_version,
                "error": err.to_string(),
            }),
        )
    })?;

    if let Some(workspace_root) = discover_workspace_root()? {
        return python_use_workspace(
            ctx,
            &record,
            &normalized,
            &runtime_version,
            &workspace_root,
            strict,
        );
    }

    let snapshot = manifest_snapshot()?;
    assert_runtime_satisfies_requirement(
        &runtime_version,
        &snapshot.python_requirement,
        "project requires-python is invalid",
        |requires_python| {
            InstallUserError::new(
                format!("runtime {normalized} does not satisfy project requires-python"),
                json!({
                    "requested": normalized,
                    "runtime_full_version": record.full_version,
                    "requires_python": requires_python,
                    "hint": format!("choose a runtime compatible with `{}` or update [project].requires-python", requires_python),
                }),
            )
        },
    )?;

    if strict {
        if snapshot.python_override.as_deref() != Some(normalized.as_str()) {
            return Err(InstallUserError::new(
                format!("project runtime is not set to Python {normalized}"),
                json!({
                    "requested": normalized,
                    "hint": format!("run `px python use {normalized}` locally (CI=0) to update and sync the project"),
                }),
            )
            .into());
        }
        let state = evaluate_project_state(ctx, &snapshot)?;
        if !matches!(
            state.canonical,
            px_domain::api::ProjectStateKind::Consistent
        ) {
            return Err(InstallUserError::new(
                "project is not consistent under CI=1",
                json!({
                    "hint": "run `px sync` (or re-run `px python use` locally) to rebuild the lock/env before CI",
                }),
            )
            .into());
        }
        let mut details = runtime_to_json(&record);
        if let Value::Object(ref mut map) = details {
            map.insert(
                "pyproject".into(),
                Value::String(snapshot.manifest_path.display().to_string()),
            );
        }
        return Ok(ExecutionOutcome::success(
            format!("project already targets Python {normalized} (CI=1)"),
            details,
        ));
    }

    let pyproject = snapshot.root.join("pyproject.toml");
    let backup = ManifestLockBackup::capture(&pyproject, &snapshot.lock_path)?;
    let mut needs_restore = true;
    let outcome = (|| -> Result<ExecutionOutcome> {
        let mut editor = ManifestEditor::open(&pyproject)?;
        let changed = editor.set_tool_python(&normalized)?;
        let sync_outcome = project_sync(
            ctx,
            &ProjectSyncRequest {
                frozen: false,
                dry_run: false,
            },
        )?;
        needs_restore = false;
        let message = if changed {
            format!(
                "set project runtime to Python {normalized} ({}) and synced lock/env",
                record.path
            )
        } else {
            format!("project already targets Python {normalized}; synced lock/env")
        };
        Ok(python_use_outcome(
            message,
            &record,
            &pyproject,
            None,
            sync_outcome,
        ))
    })();
    if needs_restore {
        backup.restore()?;
    }
    outcome
}

fn python_use_workspace(
    ctx: &CommandContext,
    record: &runtime_manager::RuntimeRecord,
    normalized: &str,
    runtime_version: &Version,
    workspace_root: &Path,
    strict: bool,
) -> Result<ExecutionOutcome> {
    let workspace_config = read_workspace_config(workspace_root)?;
    let mut violations = Vec::new();
    let root_snapshot = match ProjectSnapshot::read_from(workspace_root) {
        Ok(snapshot) => Some(snapshot),
        Err(err) => {
            let msg = err.to_string();
            if msg.contains("[project] must be a table")
                || msg.contains("pyproject missing [project].name")
            {
                None
            } else {
                return Err(err);
            }
        }
    };
    if let Some(snapshot) = &root_snapshot {
        if !requirement_allows_runtime_at(
            &snapshot.manifest_path,
            &snapshot.python_requirement,
            runtime_version,
        )? {
            violations.push(json!({
                "path": snapshot.manifest_path.display().to_string(),
                "requires_python": snapshot.python_requirement,
            }));
        }
    }
    for member_rel in &workspace_config.members {
        let member_root = workspace_config.root.join(member_rel);
        let snapshot = ProjectSnapshot::read_from(&member_root)?;
        if !requirement_allows_runtime_at(
            &snapshot.manifest_path,
            &snapshot.python_requirement,
            runtime_version,
        )? {
            violations.push(json!({
                "path": snapshot.manifest_path.display().to_string(),
                "requires_python": snapshot.python_requirement,
            }));
        }
    }
    if !violations.is_empty() {
        return Err(InstallUserError::new(
            format!("runtime {normalized} does not satisfy workspace member requires-python"),
            json!({
                "requested": normalized,
                "runtime_full_version": record.full_version,
                "violations": violations,
                "hint": "choose a runtime compatible with all workspace members (or update members' [project].requires-python).",
            }),
        )
        .into());
    }

    if strict {
        if workspace_config.python.as_deref() != Some(normalized) {
            return Err(InstallUserError::new(
                format!("workspace runtime is not set to Python {normalized}"),
                json!({
                    "requested": normalized,
                    "hint": format!("run `px python use {normalized}` locally (CI=0) to update and sync the workspace"),
                }),
            )
            .into());
        }
        let workspace = load_workspace_snapshot(workspace_root)?;
        let state = evaluate_workspace_state(ctx, &workspace)?;
        if !matches!(state.canonical, WorkspaceStateKind::Consistent) {
            return Err(InstallUserError::new(
                "workspace is not consistent under CI=1",
                json!({
                    "hint": "run `px sync` (or re-run `px python use` locally) to rebuild the lock/env before CI",
                }),
            )
            .into());
        }
        let mut details = runtime_to_json(record);
        if let Value::Object(ref mut map) = details {
            map.insert(
                "pyproject".into(),
                Value::String(workspace_root.join("pyproject.toml").display().to_string()),
            );
            map.insert(
                "workspace".into(),
                Value::String(workspace_root.display().to_string()),
            );
        }
        return Ok(ExecutionOutcome::success(
            format!("workspace already targets Python {normalized} (CI=1)"),
            details,
        ));
    }

    let pyproject = workspace_root.join("pyproject.toml");
    let lock_path = workspace_root.join("px.workspace.lock");
    let backup = ManifestLockBackup::capture(&pyproject, &lock_path)?;
    let mut needs_restore = true;
    let outcome = (|| -> Result<ExecutionOutcome> {
        let mut editor = ManifestEditor::open(&pyproject)?;
        let changed = editor.set_workspace_python(normalized)?;
        let sync_outcome = workspace_sync(
            ctx,
            WorkspaceScope::Root(load_workspace_snapshot(workspace_root)?),
            &WorkspaceSyncRequest {
                frozen: false,
                dry_run: false,
                force_resolve: false,
            },
        )?;
        needs_restore = false;
        let message = if changed {
            format!(
                "set workspace runtime to Python {normalized} ({}) and synced lock/env",
                record.path
            )
        } else {
            format!("workspace already targets Python {normalized}; synced lock/env")
        };
        Ok(python_use_outcome(
            message,
            record,
            &pyproject,
            Some(workspace_root),
            sync_outcome,
        ))
    })();
    if needs_restore {
        backup.restore()?;
    }
    outcome
}

fn assert_runtime_satisfies_requirement(
    runtime_version: &Version,
    requirement: &str,
    invalid_message: &'static str,
    mismatch_error: impl FnOnce(&str) -> InstallUserError,
) -> Result<()> {
    let specifiers = requirement_specifiers(requirement).map_err(|err| {
        InstallUserError::new(
            invalid_message,
            json!({
                "error": err,
                "requires_python": requirement,
            }),
        )
    })?;
    if specifiers.contains(runtime_version) {
        Ok(())
    } else {
        Err(mismatch_error(requirement).into())
    }
}

fn requirement_allows_runtime_at(
    manifest_path: &Path,
    requirement: &str,
    runtime_version: &Version,
) -> Result<bool> {
    let specifiers = requirement_specifiers(requirement).map_err(|err| {
        InstallUserError::new(
            "requires-python is invalid",
            json!({
                "error": err,
                "requires_python": requirement,
                "pyproject": manifest_path.display().to_string(),
            }),
        )
    })?;
    Ok(specifiers.contains(runtime_version))
}

fn requirement_specifiers(requirement: &str) -> std::result::Result<VersionSpecifiers, String> {
    match VersionSpecifiers::from_str(requirement) {
        Ok(specifiers) => Ok(specifiers),
        Err(err) => {
            if let Ok(channel) = runtime_manager::normalize_channel(requirement) {
                let spec = format!("=={channel}.*");
                if let Ok(specifiers) = VersionSpecifiers::from_str(&spec) {
                    return Ok(specifiers);
                }
            }
            Err(err.to_string())
        }
    }
}

fn python_use_outcome(
    message: String,
    record: &runtime_manager::RuntimeRecord,
    pyproject: &Path,
    workspace_root: Option<&Path>,
    sync_outcome: ExecutionOutcome,
) -> ExecutionOutcome {
    let mut details = runtime_to_json(record);
    if let Value::Object(ref mut map) = details {
        map.insert(
            "pyproject".into(),
            Value::String(pyproject.display().to_string()),
        );
        if let Some(root) = workspace_root {
            map.insert(
                "workspace".into(),
                Value::String(root.display().to_string()),
            );
        }
        map.insert("sync".into(), sync_outcome.details);
    }
    ExecutionOutcome::success(message, details)
}

/// Shows details about installed runtimes and the current project selection.
///
/// # Errors
/// Returns an error if runtime metadata or the manifest snapshot cannot be read.
pub fn python_info(
    _ctx: &CommandContext,
    _request: &PythonInfoRequest,
) -> Result<ExecutionOutcome> {
    let runtimes = match runtime_manager::list_runtimes() {
        Ok(runtimes) => runtimes,
        Err(err) => return Ok(runtime_registry_error(err)),
    };
    let default = runtimes.iter().find(|rt| rt.default).cloned();
    let default_human = default
        .as_ref()
        .map(|record| format!("default: Python {} at {}", record.version, record.path))
        .unwrap_or_else(|| "default: <none>".to_string());
    if let Some(workspace_root) = discover_workspace_root()? {
        let workspace = load_workspace_snapshot(&workspace_root)?;
        let workspace_runtime = runtime_manager::resolve_runtime(
            workspace.python_override.as_deref(),
            &workspace.python_requirement,
        )
        .ok();
        let mut details = json!({
            "default": default.as_ref().map(runtime_to_json),
        });
        if let Some(selection) = workspace_runtime {
            let info = json!({
                "version": selection.record.version,
                "path": selection.record.path,
                "full_version": selection.record.full_version,
                "source": format!("{:?}", selection.source),
            });
            details["workspace"] = Value::String(workspace_root.display().to_string());
            details["project"] = info;
            return Ok(ExecutionOutcome::success(
                format!(
                    "workspace runtime: Python {} at {} ({default_human})",
                    selection.record.version, selection.record.path,
                ),
                details,
            ));
        }
        details["workspace"] = Value::String(workspace_root.display().to_string());
        details["project"] = Value::Null;
        return Ok(ExecutionOutcome::success(
            format!("workspace runtime unavailable ({default_human})"),
            details,
        ));
    }
    let project_snapshot = match manifest_snapshot() {
        Ok(snapshot) => Some(snapshot),
        Err(err) => {
            if is_missing_project_error(&err) {
                None
            } else {
                return Err(err);
            }
        }
    };
    let project_runtime = project_snapshot.as_ref().and_then(|snapshot| {
        runtime_manager::resolve_runtime(
            snapshot.python_override.as_deref(),
            &snapshot.python_requirement,
        )
        .ok()
    });
    let mut details = json!({
        "default": default.as_ref().map(runtime_to_json),
    });
    if let Some(selection) = project_runtime {
        let info = json!({
            "version": selection.record.version,
            "path": selection.record.path,
            "full_version": selection.record.full_version,
            "source": format!("{:?}", selection.source),
        });
        if let Value::Object(ref mut map) = details {
            map.insert("project".to_string(), info);
        }
        Ok(ExecutionOutcome::success(
            format!(
                "project runtime: Python {} at {} ({default_human})",
                selection.record.version, selection.record.path,
            ),
            details,
        ))
    } else if let Some(record) = default {
        details["project"] = Value::Null;
        Ok(ExecutionOutcome::success(
            format!(
                "default runtime: Python {} at {}",
                record.version, record.path
            ),
            details,
        ))
    } else {
        details["project"] = Value::Null;
        Ok(ExecutionOutcome::success(
            "no px runtimes registered (default: <none>)",
            details,
        ))
    }
}

struct ManifestLockBackup {
    pyproject_path: PathBuf,
    lock_path: PathBuf,
    pyproject_contents: String,
    lock_contents: Option<String>,
    lock_preexisting: bool,
    pyproject_permissions: fs::Permissions,
    lock_permissions: Option<fs::Permissions>,
}

impl ManifestLockBackup {
    fn capture(pyproject_path: &Path, lock_path: &Path) -> Result<Self> {
        let pyproject_contents = fs::read_to_string(pyproject_path)?;
        let pyproject_permissions = fs::metadata(pyproject_path)?.permissions();
        let lock_preexisting = lock_path.exists();
        let lock_contents = if lock_preexisting {
            Some(fs::read_to_string(lock_path)?)
        } else {
            None
        };
        let lock_permissions = if lock_preexisting {
            Some(fs::metadata(lock_path)?.permissions())
        } else {
            None
        };
        Ok(Self {
            pyproject_path: pyproject_path.to_path_buf(),
            lock_path: lock_path.to_path_buf(),
            pyproject_contents,
            lock_contents,
            lock_preexisting,
            pyproject_permissions,
            lock_permissions,
        })
    }

    fn restore(&self) -> Result<()> {
        self.write_with_permissions(
            &self.pyproject_path,
            &self.pyproject_contents,
            &self.pyproject_permissions,
        )?;
        match (&self.lock_contents, self.lock_preexisting) {
            (Some(contents), _) => {
                let permissions = if let Some(perms) = &self.lock_permissions {
                    perms.clone()
                } else {
                    fs::metadata(&self.lock_path)?.permissions()
                };
                self.write_with_permissions(&self.lock_path, contents, &permissions)?;
            }
            (None, false) => {
                if self.lock_path.exists() {
                    self.remove_with_permissions(&self.lock_path)?;
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

    fn write_with_permissions(
        &self,
        path: &Path,
        contents: &str,
        original: &fs::Permissions,
    ) -> Result<()> {
        let writable = Self::writable_permissions(original);
        fs::set_permissions(path, writable)?;
        fs::write(path, contents)?;
        fs::set_permissions(path, original.clone())?;
        Ok(())
    }

    fn remove_with_permissions(&self, path: &Path) -> Result<()> {
        let original = fs::metadata(path)?.permissions();
        let writable = Self::writable_permissions(&original);
        fs::set_permissions(path, writable)?;
        fs::remove_file(path)?;
        Ok(())
    }

    #[cfg(unix)]
    fn writable_permissions(original: &fs::Permissions) -> fs::Permissions {
        use std::os::unix::fs::PermissionsExt;
        fs::Permissions::from_mode(original.mode() | 0o200)
    }

    #[cfg(not(unix))]
    fn writable_permissions(original: &fs::Permissions) -> fs::Permissions {
        let mut perms = original.clone();
        perms.set_readonly(false);
        perms
    }
}

fn runtime_to_json(record: &runtime_manager::RuntimeRecord) -> Value {
    json!({
        "version": record.version,
        "full_version": record.full_version,
        "path": record.path,
        "default": record.default,
    })
}

fn registry_path_detail() -> Option<String> {
    runtime_manager::registry_path()
        .ok()
        .map(|path| path.display().to_string())
}

fn runtime_registry_error(err: anyhow::Error) -> ExecutionOutcome {
    let issues: Vec<String> = err.chain().map(std::string::ToString::to_string).collect();
    let mut details = json!({
        "error": err.to_string(),
        "issues": issues,
    });
    if let Some(path) = registry_path_detail() {
        details["registry"] = Value::String(path);
        details["hint"] = Value::String(
            "Repair or delete the px runtime registry file, then rerun the command.".to_string(),
        );
    }
    ExecutionOutcome::user_error("unable to read px runtime registry", details)
}

fn runtime_install_error(err: anyhow::Error) -> ExecutionOutcome {
    let issues: Vec<String> = err.chain().map(std::string::ToString::to_string).collect();
    let mut details = json!({
        "error": err.to_string(),
        "issues": issues,
    });
    if let Some(path) = registry_path_detail() {
        details["registry"] = Value::String(path);
    }
    ExecutionOutcome::user_error("px python install failed", details)
}
