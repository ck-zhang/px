use anyhow::Result;
use serde_json::{json, Value};

use crate::{
    manifest_snapshot, runtime, CommandContext, ExecutionOutcome, InstallUserError,
    ProgressReporter,
};
use px_domain::ManifestEditor;

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
    let runtimes = runtime::list_runtimes()?;
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
    let record = runtime::install_runtime(
        &request.version,
        request.path.as_deref(),
        request.set_default,
    )?;
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
    let normalized = runtime::normalize_channel(&request.version)?;
    let runtimes = runtime::list_runtimes()?;
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
    let runtimes = runtime::list_runtimes()?;
    let default = runtimes.iter().find(|rt| rt.default).cloned();
    let project_snapshot = manifest_snapshot().ok();
    let project_runtime = project_snapshot.as_ref().and_then(|snapshot| {
        runtime::resolve_runtime(
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

fn runtime_to_json(record: &runtime::RuntimeRecord) -> Value {
    json!({
        "version": record.version,
        "full_version": record.full_version,
        "path": record.path,
        "default": record.default,
    })
}
