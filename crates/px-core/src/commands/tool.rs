use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Context, Result};
use dirs_next::home_dir;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use time::OffsetDateTime;
use toml_edit::{Array, DocumentMut, Item, Table, Value as TomlValue};

use crate::{
    build_pythonpath, ensure_project_environment_synced, install_snapshot, outcome_from_output,
    refresh_project_site, runtime, CommandContext, ExecutionOutcome, InstallUserError,
};
use px_project::manifest::ManifestEditor;

const TOOLS_DIR_ENV: &str = "PX_TOOLS_DIR";
const MIN_PYTHON_REQUIREMENT: &str = ">=3.8";

#[derive(Clone, Debug)]
pub struct ToolInstallRequest {
    pub name: String,
    pub spec: Option<String>,
    pub python: Option<String>,
    pub entry: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ToolRunRequest {
    pub name: String,
    pub args: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct ToolRemoveRequest {
    pub name: String,
}

#[derive(Clone, Debug)]
pub struct ToolUpgradeRequest {
    pub name: String,
    pub python: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ToolListRequest;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ToolMetadata {
    name: String,
    spec: String,
    entry: String,
    runtime_version: String,
    runtime_full_version: String,
    runtime_path: String,
    installed_spec: String,
    created_at: String,
    updated_at: String,
}

pub fn tool_install(ctx: &CommandContext, request: ToolInstallRequest) -> Result<ExecutionOutcome> {
    let normalized = normalize_tool_name(&request.name);
    if normalized.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            "tool name must contain at least one alphanumeric character",
            json!({ "hint": "pass names like black or ruff" }),
        ));
    }
    let spec = request.spec.clone().unwrap_or_else(|| request.name.clone());
    let entry = request.entry.clone().unwrap_or_else(|| normalized.clone());
    let tool_root = tool_root_dir(&normalized)?;
    fs::create_dir_all(&tool_root)?;
    scaffold_tool_pyproject(&tool_root, &normalized)?;
    let runtime_selection = resolve_runtime(request.python.as_deref())?;
    env::set_var("PX_RUNTIME_PYTHON", &runtime_selection.record.path);

    let pyproject = tool_root.join("pyproject.toml");
    let mut editor = ManifestEditor::open(&pyproject)?;
    editor.write_dependencies(&[spec.clone()])?;
    editor.set_tool_python(&runtime_selection.record.version)?;

    let snapshot = px_project::ProjectSnapshot::read_from(&tool_root)?;
    install_snapshot(ctx, &snapshot, false, None)?;
    refresh_project_site(&snapshot, ctx)?;

    let pinned = ManifestEditor::open(&pyproject)?.dependencies();
    let installed_spec = pinned.first().cloned().unwrap_or(spec.clone());
    let timestamp = timestamp_string()?;
    let metadata = ToolMetadata {
        name: normalized.clone(),
        spec,
        entry,
        runtime_version: runtime_selection.record.version.clone(),
        runtime_full_version: runtime_selection.record.full_version.clone(),
        runtime_path: runtime_selection.record.path.clone(),
        installed_spec,
        created_at: timestamp.clone(),
        updated_at: timestamp,
    };
    write_metadata(&tool_root, &metadata)?;
    Ok(ExecutionOutcome::success(
        format!(
            "installed tool {} (Python {} via {})",
            metadata.name, metadata.runtime_version, metadata.runtime_full_version
        ),
        json!({
            "tool": metadata.name,
            "entry": metadata.entry,
            "spec": metadata.installed_spec,
            "runtime": {
                "version": metadata.runtime_version,
                "full_version": metadata.runtime_full_version,
                "path": metadata.runtime_path,
            }
        }),
    ))
}

pub fn tool_run(ctx: &CommandContext, request: ToolRunRequest) -> Result<ExecutionOutcome> {
    let normalized = normalize_tool_name(&request.name);
    if normalized.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            "tool name must contain at least one alphanumeric character",
            json!({ "hint": "run commands like `px tool run black`" }),
        ));
    }
    let tool_root = tool_root_dir(&normalized)?;
    let metadata = read_metadata(&tool_root).map_err(|err| {
        InstallUserError::new(
            format!("tool '{}' is not installed", normalized),
            json!({
                "error": err.to_string(),
                "hint": format!("run `px tool install {normalized}` first"),
            }),
        )
    })?;
    let snapshot = px_project::ProjectSnapshot::read_from(&tool_root)?;
    let runtime_selection = resolve_runtime(Some(&metadata.runtime_version))?;
    env::set_var("PX_RUNTIME_PYTHON", &runtime_selection.record.path);
    if let Err(err) = ensure_project_environment_synced(ctx, &snapshot) {
        return match err.downcast::<InstallUserError>() {
            Ok(user) => Ok(ExecutionOutcome::user_error(user.message, user.details)),
            Err(other) => Err(other),
        };
    }
    let (pythonpath, allowed_paths) = build_pythonpath(ctx.fs(), &tool_root)?;
    let mut args = vec!["-m".to_string(), metadata.entry.clone()];
    args.extend(request.args.clone());
    let allowed = env::join_paths(&allowed_paths)
        .context("allowed path contains invalid UTF-8")?
        .into_string()
        .map_err(|_| anyhow!("allowed path contains non-utf8 data"))?;
    let cwd = env::current_dir().unwrap_or(tool_root.clone());
    let env_payload = json!({
        "tool": metadata.name,
        "args": request.args,
    });
    let envs = vec![
        ("PYTHONPATH".into(), pythonpath),
        ("PYTHONUNBUFFERED".into(), "1".into()),
        ("PX_ALLOWED_PATHS".into(), allowed),
        ("PX_TOOL_ROOT".into(), tool_root.display().to_string()),
        ("PX_COMMAND_JSON".into(), env_payload.to_string()),
    ];
    let output =
        crate::px_runtime::run_command(&runtime_selection.record.path, &args, &envs, &cwd)?;
    let details = json!({
        "tool": metadata.name,
        "entry": metadata.entry,
        "runtime": runtime_selection.record.full_version,
        "args": args,
    });
    Ok(outcome_from_output(
        "tool",
        &metadata.name,
        output,
        "px tool",
        Some(details),
    ))
}

pub fn tool_list(_ctx: &CommandContext, _request: ToolListRequest) -> Result<ExecutionOutcome> {
    let root = tools_root()?;
    if !root.exists() {
        return Ok(ExecutionOutcome::success(
            "no tools installed",
            json!({ "tools": Vec::<Value>::new() }),
        ));
    }
    let mut rows = Vec::new();
    if let Ok(entries) = fs::read_dir(&root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            match read_metadata(&path) {
                Ok(meta) => rows.push(meta),
                Err(_) => continue,
            }
        }
    }
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    let details: Vec<Value> = rows
        .iter()
        .map(|meta| {
            json!({
                "name": meta.name,
                "spec": meta.installed_spec,
                "runtime": meta.runtime_version,
                "entry": meta.entry,
            })
        })
        .collect();
    if rows.is_empty() {
        return Ok(ExecutionOutcome::success(
            "no tools installed",
            json!({ "tools": details }),
        ));
    }
    let mut lines = Vec::new();
    for meta in &rows {
        lines.push(format!(
            "{}  {}  (Python {})",
            meta.name, meta.installed_spec, meta.runtime_version
        ));
    }
    Ok(ExecutionOutcome::success(
        lines.join("\n"),
        json!({ "tools": details }),
    ))
}

pub fn tool_remove(_ctx: &CommandContext, request: ToolRemoveRequest) -> Result<ExecutionOutcome> {
    let normalized = normalize_tool_name(&request.name);
    let root = tool_root_dir(&normalized)?;
    if !root.exists() {
        return Ok(ExecutionOutcome::user_error(
            format!("tool '{}' is not installed", normalized),
            json!({ "hint": format!("run `px tool install {normalized}` first") }),
        ));
    }
    fs::remove_dir_all(&root).with_context(|| format!("removing {}", root.display()))?;
    Ok(ExecutionOutcome::success(
        format!("removed tool {}", normalized),
        json!({ "tool": normalized }),
    ))
}

pub fn tool_upgrade(ctx: &CommandContext, request: ToolUpgradeRequest) -> Result<ExecutionOutcome> {
    let normalized = normalize_tool_name(&request.name);
    let root = tool_root_dir(&normalized)?;
    let metadata = read_metadata(&root).map_err(|err| {
        InstallUserError::new(
            format!("tool '{}' is not installed", normalized),
            json!({ "error": err.to_string(), "hint": format!("run `px tool install {normalized}` first") }),
        )
    })?;
    let install_request = ToolInstallRequest {
        name: normalized.clone(),
        spec: Some(metadata.spec.clone()),
        python: request
            .python
            .clone()
            .or_else(|| Some(metadata.runtime_version.clone())),
        entry: Some(metadata.entry.clone()),
    };
    tool_install(ctx, install_request)
}

fn resolve_runtime(explicit: Option<&str>) -> Result<runtime::RuntimeSelection> {
    let requirement = MIN_PYTHON_REQUIREMENT.to_string();
    runtime::resolve_runtime(explicit, &requirement).map_err(|err| {
        anyhow!(InstallUserError::new(
            "python runtime unavailable",
            json!({ "hint": err.to_string() }),
        ))
    })
}

fn tools_root() -> Result<PathBuf> {
    if let Some(dir) = env::var_os(TOOLS_DIR_ENV) {
        let path = PathBuf::from(dir);
        fs::create_dir_all(&path)?;
        return Ok(path);
    }
    let home = home_dir().ok_or_else(|| anyhow!("home directory not found"))?;
    let path = home.join(".px").join("tools");
    fs::create_dir_all(&path)?;
    Ok(path)
}

fn tool_root_dir(name: &str) -> Result<PathBuf> {
    Ok(tools_root()?.join(name))
}

fn scaffold_tool_pyproject(root: &Path, name: &str) -> Result<()> {
    let path = root.join("pyproject.toml");
    if path.exists() {
        return Ok(());
    }
    let mut doc = DocumentMut::new();
    let mut project = Table::new();
    project.insert("name", Item::Value(TomlValue::from(name)));
    project.insert("version", Item::Value(TomlValue::from("0.0.0")));
    project.insert(
        "requires-python",
        Item::Value(TomlValue::from(MIN_PYTHON_REQUIREMENT)),
    );
    project.insert("dependencies", Item::Value(TomlValue::Array(Array::new())));
    doc.insert("project", Item::Table(project));
    let mut tool = Table::new();
    tool.insert("px", Item::Table(Table::new()));
    doc.insert("tool", Item::Table(tool));
    let mut build = Table::new();
    let mut requires = Array::new();
    requires.push("setuptools>=70");
    requires.push("wheel");
    build.insert("requires", Item::Value(TomlValue::Array(requires)));
    build.insert(
        "build-backend",
        Item::Value(TomlValue::from("setuptools.build_meta")),
    );
    doc.insert("build-system", Item::Table(build));
    fs::write(&path, doc.to_string())?;
    Ok(())
}

fn write_metadata(root: &Path, metadata: &ToolMetadata) -> Result<()> {
    let path = root.join("tool.json");
    let mut json = serde_json::to_vec_pretty(metadata)?;
    json.push(b'\n');
    fs::write(path, json)?;
    Ok(())
}

fn read_metadata(root: &Path) -> Result<ToolMetadata> {
    let path = root.join("tool.json");
    let contents =
        fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    Ok(serde_json::from_str(&contents).context("invalid tool metadata")?)
}

fn normalize_tool_name(raw: &str) -> String {
    raw.chars()
        .filter(|ch| ch.is_alphanumeric() || *ch == '_' || *ch == '-')
        .collect::<String>()
        .to_lowercase()
}

fn timestamp_string() -> Result<String> {
    let now = OffsetDateTime::now_utc();
    Ok(now.format(&time::format_description::well_known::Rfc3339)?)
}
