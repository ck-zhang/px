use anyhow::Result;
use pep440_rs::{Version, VersionSpecifiers};
use serde_json::{json, Value};
use std::str::FromStr;

use crate::{
    is_missing_project_error, manifest_snapshot, progress::ProgressReporter, runtime_manager,
    CommandContext, ExecutionOutcome, InstallUserError,
};
use px_domain::api::ManifestEditor;

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
    let runtimes = match runtime_manager::list_runtimes() {
        Ok(runtimes) => runtimes,
        Err(err) => return Ok(runtime_registry_error(err)),
    };
    let details: Vec<Value> = runtimes.iter().map(runtime_to_json).collect();
    if runtimes.is_empty() {
        return Ok(ExecutionOutcome::success(
            "no px runtimes installed",
            json!({ "runtimes": details }),
        ));
    }
    let summary = runtimes
        .iter()
        .map(|rt| format!("{}  {}", rt.version, rt.path))
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
        Err(err) => return Ok(runtime_install_error(err)),
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
    let project_root = ctx.project_root()?;
    let normalized = runtime_manager::normalize_channel(&request.version)?;
    let runtimes = runtime_manager::list_runtimes()?;
    let Some(record) = runtimes.iter().find(|rt| rt.version == normalized) else {
        return Err(InstallUserError::new(
            format!("runtime {normalized} is not installed"),
            json!({
                "version": normalized,
                "hint": format!("run `px python install {normalized}` to register this runtime"),
            }),
        )
        .into());
    };
    let snapshot = manifest_snapshot()?;
    let requirement = &snapshot.python_requirement;
    let specifiers = VersionSpecifiers::from_str(requirement).map_err(|err| {
        InstallUserError::new(
            "project requires-python is invalid",
            json!({
                "error": err.to_string(),
                "requires_python": requirement,
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
    if !specifiers.contains(&runtime_version) {
        return Err(InstallUserError::new(
            format!(
                "runtime {normalized} does not satisfy project requires-python"
            ),
            json!({
                "requested": normalized,
                "runtime_full_version": record.full_version,
                "requires_python": requirement,
                "hint": format!("choose a runtime compatible with `{}` or update [project].requires-python", requirement),
            }),
        )
        .into());
    }
    let pyproject = project_root.join("pyproject.toml");
    let mut editor = ManifestEditor::open(&pyproject)?;
    let changed = editor.set_tool_python(&normalized)?;
    let message = if changed {
        format!(
            "set [tool.px].python = {} ({}). run `px sync` to rebuild the environment",
            normalized, record.path
        )
    } else {
        format!("project already targets Python {normalized}")
    };
    let mut details = runtime_to_json(record);
    if let Value::Object(ref mut map) = details {
        map.insert(
            "pyproject".into(),
            Value::String(pyproject.display().to_string()),
        );
    }
    Ok(ExecutionOutcome::success(message, details))
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
                "project runtime: Python {} at {}",
                selection.record.version, selection.record.path
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
            "no px runtimes registered",
            details,
        ))
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
    let mut details = json!({
        "error": err.to_string(),
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
    let mut details = json!({ "error": err.to_string() });
    if let Some(path) = registry_path_detail() {
        details["registry"] = Value::String(path);
    }
    ExecutionOutcome::user_error("px python install failed", details)
}
