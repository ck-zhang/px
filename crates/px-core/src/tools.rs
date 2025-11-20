use std::{
    collections::{BTreeMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use anyhow::{anyhow, Context, Result};
use dirs_next::home_dir;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use time::OffsetDateTime;
use toml_edit::{Array, DocumentMut, Item, Table, Value as TomlValue};

use crate::{
    build_pythonpath, compute_lock_hash, ensure_project_environment_synced, install_snapshot,
    manifest_snapshot_at, outcome_from_output, persist_resolved_dependencies, refresh_project_site,
    resolve_dependencies_with_effects, runtime, CommandContext, ExecutionOutcome, InstallUserError,
};
use px_domain::{load_lockfile_optional, ManifestEditor};

const TOOLS_DIR_ENV: &str = "PX_TOOLS_DIR";
const TOOL_STORE_ENV: &str = "PX_TOOL_STORE";
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
    pub console: Option<String>,
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
    console_scripts: BTreeMap<String, String>,
    runtime_version: String,
    runtime_full_version: String,
    runtime_path: String,
    installed_spec: String,
    created_at: String,
    updated_at: String,
}

/// Installs or upgrades a px-managed tool inside the shared tools directory.
///
/// # Errors
/// Returns an error if files cannot be written or dependencies fail to install.
pub fn tool_install(
    ctx: &CommandContext,
    request: &ToolInstallRequest,
) -> Result<ExecutionOutcome> {
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

    let snapshot = px_domain::ProjectSnapshot::read_from(&tool_root)?;
    let resolved = match resolve_dependencies_with_effects(ctx.effects(), &snapshot) {
        Ok(resolved) => resolved,
        Err(err) => match err.downcast::<InstallUserError>() {
            Ok(user) => return Ok(ExecutionOutcome::user_error(user.message, user.details)),
            Err(other) => return Err(other),
        },
    };
    persist_resolved_dependencies(&snapshot, &resolved.specs)?;
    let updated_snapshot = manifest_snapshot_at(&tool_root)?;
    let install_outcome = match install_snapshot(ctx, &updated_snapshot, false, None) {
        Ok(outcome) => outcome,
        Err(err) => match err.downcast::<InstallUserError>() {
            Ok(user) => return Ok(ExecutionOutcome::user_error(user.message, user.details)),
            Err(other) => return Err(other),
        },
    };
    if matches!(install_outcome.state, crate::InstallState::MissingLock) {
        return Ok(ExecutionOutcome::failure(
            "px tool install could not write px.lock",
            json!({ "tool": normalized }),
        ));
    }
    refresh_project_site(&updated_snapshot, ctx)?;
    let store_site = finalize_tool_environment(&tool_root, &updated_snapshot, &runtime_selection)?;

    let pinned = ManifestEditor::open(&pyproject)?.dependencies();
    let installed_spec = pinned.first().cloned().unwrap_or(spec.clone());
    let timestamp = timestamp_string()?;
    let console_scripts = collect_console_scripts(&store_site)?;
    let metadata = ToolMetadata {
        name: normalized.clone(),
        spec,
        entry,
        console_scripts: console_scripts.clone(),
        runtime_version: runtime_selection.record.version.clone(),
        runtime_full_version: runtime_selection.record.full_version.clone(),
        runtime_path: runtime_selection.record.path.clone(),
        installed_spec,
        created_at: timestamp.clone(),
        updated_at: timestamp,
    };
    write_metadata(&tool_root, &metadata)?;
    write_console_shims(&tool_root, &metadata, &console_scripts)?;
    Ok(ExecutionOutcome::success(
        format!(
            "installed tool {} (Python {} via {})",
            metadata.name, metadata.runtime_version, metadata.runtime_full_version
        ),
        json!({
            "tool": metadata.name,
            "entry": metadata.entry,
            "console_scripts": metadata.console_scripts.keys().collect::<Vec<_>>(),
            "spec": metadata.installed_spec,
            "runtime": {
                "version": metadata.runtime_version,
                "full_version": metadata.runtime_full_version,
                "path": metadata.runtime_path,
            }
        }),
    ))
}

/// Runs a px-managed tool from its cached environment.
///
/// # Errors
/// Returns an error if the tool metadata is missing or the invocation fails.
pub fn tool_run(ctx: &CommandContext, request: &ToolRunRequest) -> Result<ExecutionOutcome> {
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
            format!("tool '{normalized}' is not installed"),
            json!({
                "error": err.to_string(),
                "hint": format!("run `px tool install {normalized}` first"),
            }),
        )
    })?;
    let snapshot = px_domain::ProjectSnapshot::read_from(&tool_root)?;
    let runtime_selection = resolve_runtime(Some(&metadata.runtime_version))?;
    env::set_var("PX_RUNTIME_PYTHON", &runtime_selection.record.path);
    if let Err(err) = ensure_project_environment_synced(ctx, &snapshot) {
        return match err.downcast::<InstallUserError>() {
            Ok(user) => Ok(ExecutionOutcome::user_error(user.message, user.details)),
            Err(other) => Err(other),
        };
    }
    let mut script_name = request.console.clone();
    if script_name.is_none() && metadata.console_scripts.contains_key(&metadata.name) {
        script_name = Some(metadata.name.clone());
    }
    let script_target = script_name.as_deref();
    let (pythonpath, allowed_paths) = build_pythonpath(ctx.fs(), &tool_root)?;
    let mut args = if let Some(script) = script_target {
        match metadata.console_scripts.get(script) {
            Some(entrypoint) => vec!["-c".to_string(), console_entry_invoke(script, entrypoint)?],
            None => {
                return Ok(ExecutionOutcome::user_error(
                    format!("tool '{normalized}' has no console script `{script}`"),
                    json!({
                        "tool": metadata.name,
                        "script": script,
                        "hint": "run `px tool list` to view available scripts",
                    }),
                ))
            }
        }
    } else {
        vec!["-m".to_string(), metadata.entry.clone()]
    };
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
    let passthrough = ctx.env_flag_enabled("PX_TOOL_PASSTHROUGH") || request.args.is_empty();
    let output = if passthrough {
        px_domain::run_command_passthrough(&runtime_selection.record.path, &args, &envs, &cwd)?
    } else {
        px_domain::run_command(&runtime_selection.record.path, &args, &envs, &cwd)?
    };
    let details = json!({
        "tool": metadata.name,
        "entry": metadata.entry,
        "console_script": script_target,
        "runtime": runtime_selection.record.full_version,
        "args": args,
    });
    Ok(outcome_from_output(
        "tool",
        &metadata.name,
        &output,
        "px tool",
        Some(details),
    ))
}

/// Lists all installed px-managed tools.
///
/// # Errors
/// Returns an error if the tools directory cannot be read.
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
            if let Ok(meta) = read_metadata(&path) {
                rows.push(meta);
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
                "console_scripts": meta.console_scripts.keys().collect::<Vec<_>>(),
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

/// Removes an installed px-managed tool.
///
/// # Errors
/// Returns an error if the tool directory cannot be deleted.
pub fn tool_remove(_ctx: &CommandContext, request: &ToolRemoveRequest) -> Result<ExecutionOutcome> {
    let normalized = normalize_tool_name(&request.name);
    let root = tool_root_dir(&normalized)?;
    if !root.exists() {
        return Ok(ExecutionOutcome::user_error(
            format!("tool '{normalized}' is not installed"),
            json!({ "hint": format!("run `px tool install {normalized}` first") }),
        ));
    }
    fs::remove_dir_all(&root).with_context(|| format!("removing {}", root.display()))?;
    Ok(ExecutionOutcome::success(
        format!("removed tool {normalized}"),
        json!({ "tool": normalized }),
    ))
}

/// Upgrades an installed tool to the latest compatible version.
///
/// # Errors
/// Returns an error if the tool metadata cannot be read or the upgrade fails.
pub fn tool_upgrade(
    ctx: &CommandContext,
    request: &ToolUpgradeRequest,
) -> Result<ExecutionOutcome> {
    let normalized = normalize_tool_name(&request.name);
    let root = tool_root_dir(&normalized)?;
    let metadata = read_metadata(&root).map_err(|err| {
        InstallUserError::new(
            format!("tool '{normalized}' is not installed"),
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
    tool_install(ctx, &install_request)
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

fn console_entry_invoke(script: &str, entry: &str) -> Result<String> {
    let (module, target) = entry
        .split_once(':')
        .ok_or_else(|| anyhow!("invalid console entry `{entry}`"))?;
    let call = target.trim();
    let module_name = module.trim();
    Ok(format!(
        "import importlib, sys; sys.argv[0] = {script:?}; sys.exit(getattr(importlib.import_module({module_name:?}), {call:?})())"
    ))
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
    serde_json::from_str(&contents).context("invalid tool metadata")
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

fn finalize_tool_environment(
    root: &Path,
    snapshot: &px_domain::ProjectSnapshot,
    runtime: &runtime::RuntimeSelection,
) -> Result<PathBuf> {
    let state_path = root.join(".px").join("state.json");
    let initial_site = fs::read_to_string(&state_path)
        .ok()
        .and_then(|contents| serde_json::from_str::<serde_json::Value>(&contents).ok())
        .and_then(|value| {
            value
                .get("current_env")
                .and_then(|env| env.get("site_packages"))
                .and_then(|path| path.as_str())
                .map(PathBuf::from)
        });
    let source_site = initial_site
        .clone()
        .filter(|path| path.exists())
        .or_else(|| find_env_site(root));

    let lock = load_lockfile_optional(&snapshot.lock_path)?.ok_or_else(|| {
        anyhow!(
            "px sync: lockfile missing at {}",
            snapshot.lock_path.display()
        )
    })?;
    let source_site = source_site.ok_or_else(|| {
        anyhow!(
            "tool environment missing for {}; reinstall with `px tool install {}`",
            snapshot.name,
            snapshot.name
        )
    })?;
    let lock_hash = match lock.lock_id.clone() {
        Some(value) => value,
        None => compute_lock_hash(&snapshot.lock_path)?,
    };
    let env_id = format!(
        "tool-{}-{}-{}",
        normalize_tool_name(&snapshot.name),
        runtime.record.version.replace('.', "_"),
        &lock_hash[..lock_hash.len().min(12)]
    );
    let env_root = tools_env_store_root()?.join(&env_id);
    let site_path = env_root.join("site");
    if site_path.exists() {
        fs::remove_dir_all(&site_path)?;
    }
    copy_dir_contents(&source_site, &site_path)?;
    update_tool_state(root, &site_path, &env_id)?;
    let _ = fs::remove_dir_all(root.join(".px").join("site"));
    Ok(site_path)
}

fn find_env_site(root: &Path) -> Option<PathBuf> {
    let envs_root = root.join(".px").join("envs");
    let entries = fs::read_dir(envs_root).ok()?;
    for entry in entries.flatten() {
        let site = entry.path().join("site");
        if site.exists() {
            return Some(site);
        }
    }
    None
}

fn collect_console_scripts(site_dir: &Path) -> Result<BTreeMap<String, String>> {
    let mut scripts = BTreeMap::new();
    if !site_dir.exists() {
        return Ok(scripts);
    }
    let pth = site_dir.join("px.pth");
    if !pth.exists() {
        return Ok(scripts);
    }
    let contents = fs::read_to_string(&pth)?;
    let mut visited = HashSet::new();
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let entry = PathBuf::from(trimmed);
        let parent = entry.parent().unwrap_or(&entry);
        scan_dist_infos(parent, &mut visited, &mut scripts)?;
    }
    Ok(scripts)
}

fn scan_dist_infos(
    base: &Path,
    visited: &mut HashSet<PathBuf>,
    scripts: &mut BTreeMap<String, String>,
) -> Result<()> {
    if !base.exists() {
        return Ok(());
    }
    if is_dist_info(base) {
        let canonical = base.canonicalize().unwrap_or(base.to_path_buf());
        if visited.insert(canonical.clone()) {
            parse_entry_points(&canonical.join("entry_points.txt"), scripts)?;
        }
        return Ok(());
    }
    for entry in fs::read_dir(base)? {
        let entry = entry?;
        let path = entry.path();
        if is_dist_info(&path) {
            let canonical = path.canonicalize().unwrap_or(path);
            if visited.insert(canonical.clone()) {
                parse_entry_points(&canonical.join("entry_points.txt"), scripts)?;
            }
        }
    }
    Ok(())
}

fn is_dist_info(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.ends_with(".dist-info"))
}

fn parse_entry_points(path: &Path, scripts: &mut BTreeMap<String, String>) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let contents = fs::read_to_string(path)?;
    let mut in_section = false;
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_section = trimmed.eq_ignore_ascii_case("[console_scripts]");
            continue;
        }
        if !in_section || trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some((name, entry)) = trimmed.split_once('=') {
            let script = name.trim().to_string();
            let target = entry.trim().to_string();
            scripts.entry(script).or_insert(target);
        }
    }
    Ok(())
}

fn tools_env_store_root() -> Result<PathBuf> {
    let base = if let Some(dir) = env::var_os(TOOL_STORE_ENV) {
        PathBuf::from(dir)
    } else {
        let home = home_dir().ok_or_else(|| anyhow!("home directory not found"))?;
        home.join(".px").join("tools").join("store")
    };
    fs::create_dir_all(&base)?;
    let envs = base.join("envs");
    fs::create_dir_all(&envs)?;
    Ok(envs)
}

fn copy_dir_contents(src: &Path, dst: &Path) -> Result<()> {
    if !src.exists() {
        return Ok(());
    }
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let target = dst.join(entry.file_name());
        if path.is_dir() {
            copy_dir_contents(&path, &target)?;
        } else {
            fs::copy(&path, &target)
                .with_context(|| format!("copying {} to {}", path.display(), target.display()))?;
        }
    }
    Ok(())
}

fn update_tool_state(root: &Path, site_path: &Path, env_id: &str) -> Result<()> {
    let path = root.join(".px").join("state.json");
    let contents = fs::read_to_string(&path)?;
    let mut value: serde_json::Value = serde_json::from_str(&contents)?;
    if let Some(env) = value
        .as_object_mut()
        .and_then(|map| map.get_mut("current_env"))
        .and_then(Value::as_object_mut)
    {
        env.insert(
            "site_packages".into(),
            Value::String(site_path.display().to_string()),
        );
        env.insert("id".into(), Value::String(env_id.to_string()));
    }
    let mut buf = serde_json::to_vec_pretty(&value)?;
    buf.push(b'\n');
    fs::write(path, buf)?;
    Ok(())
}

fn write_console_shims(
    root: &Path,
    metadata: &ToolMetadata,
    scripts: &BTreeMap<String, String>,
) -> Result<()> {
    let bin_dir = root.join("bin");
    if bin_dir.exists() {
        for entry in fs::read_dir(&bin_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() {
                let _ = fs::remove_file(&path);
            }
        }
    }
    if scripts.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(&bin_dir)?;
    for name in scripts.keys() {
        let shim = bin_dir.join(name);
        let contents = format!(
            "#!/usr/bin/env sh\nexec px tool run {} --console {} \"$@\"\n",
            metadata.name, name
        );
        fs::write(&shim, contents)?;
        #[cfg(unix)]
        {
            let mut perms = fs::metadata(&shim)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&shim, perms)?;
        }
        #[cfg(windows)]
        {
            let cmd = bin_dir.join(format!("{}.cmd", name));
            let cmd_contents = format!(
                "@echo off\r\npx tool run {} --console {} %*\r\n",
                metadata.name, name
            );
            fs::write(cmd, cmd_contents)?;
        }
    }
    Ok(())
}
