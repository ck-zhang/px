use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use anyhow::{anyhow, Context, Result};
use pep440_rs::{Version, VersionSpecifiers};
use serde_json::json;
use toml_edit::{Array, DocumentMut, Item, Table, Value as TomlValue};

use crate::core::runtime::runtime_manager;
use crate::{
    compute_lock_hash, install_snapshot, manifest_snapshot_at, persist_resolved_dependencies,
    refresh_project_site, resolve_dependencies_with_effects, CommandContext, ExecutionOutcome,
    InstallOverride, InstallUserError,
};
use px_domain::api::{load_lockfile_optional, merge_resolved_dependencies, ManifestEditor};

use super::metadata::{timestamp_string, write_metadata, ToolMetadata, MIN_PYTHON_REQUIREMENT};
use super::paths::{
    copy_dir_contents, find_env_site, normalize_tool_name, tool_root_dir, tools_env_store_root,
};

#[derive(Clone, Debug)]
pub struct ToolInstallRequest {
    pub name: String,
    pub spec: Option<String>,
    pub python: Option<String>,
    pub entry: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ToolUpgradeRequest {
    pub name: String,
    pub python: Option<String>,
}

/// Installs or upgrades a px-managed tool inside the shared tools directory.
///
/// # Errors
/// Returns an error if files cannot be written or dependencies fail to install.
pub fn tool_install(
    ctx: &CommandContext,
    request: &ToolInstallRequest,
) -> Result<ExecutionOutcome> {
    if name_looks_like_requirement(&request.name) {
        let clean = request
            .name
            .split(|ch: char| "=<>~! ".contains(ch))
            .next()
            .unwrap_or(&request.name);
        return Ok(ExecutionOutcome::user_error(
            "tool name looks like a requirement spec",
            json!({
                "name": request.name,
                "hint": format!("Use `px tool install {clean} {}`", request.name),
            }),
        ));
    }
    let normalized = normalize_tool_name(&request.name);
    if normalized.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            "tool name must contain at least one alphanumeric character",
            json!({ "hint": "pass names like black or ruff" }),
        ));
    }
    let spec = request.spec.clone().unwrap_or_else(|| request.name.clone());
    let entry = request.entry.clone().unwrap_or_else(|| normalized.clone());
    let runtime_selection = resolve_runtime(request.python.as_deref())?;
    let tool_root = tool_root_dir(&normalized)?;
    fs::create_dir_all(&tool_root)?;
    scaffold_tool_pyproject(&tool_root, &normalized)?;
    env::set_var("PX_RUNTIME_PYTHON", &runtime_selection.record.path);

    let pyproject = tool_root.join("pyproject.toml");
    let mut editor = ManifestEditor::open(&pyproject)?;
    editor.write_dependencies(std::slice::from_ref(&spec))?;
    editor.set_tool_python(&runtime_selection.record.version)?;

    let snapshot = px_domain::api::ProjectSnapshot::read_from(&tool_root)?;
    let resolved = match resolve_dependencies_with_effects(ctx.effects(), &snapshot, true) {
        Ok(resolved) => resolved,
        Err(err) => match err.downcast::<InstallUserError>() {
            Ok(user) => return Ok(ExecutionOutcome::user_error(user.message, user.details)),
            Err(other) => return Err(other),
        },
    };
    let marker_env = ctx
        .marker_environment()
        .context("failed to detect marker environment")?;
    let merged = merge_resolved_dependencies(&snapshot.dependencies, &resolved.specs, &marker_env);
    persist_resolved_dependencies(&snapshot, &merged)?;
    let updated_snapshot = manifest_snapshot_at(&tool_root)?;
    let install_override = InstallOverride {
        dependencies: resolved.specs.clone(),
        pins: resolved.pins.clone(),
    };
    let install_outcome =
        match install_snapshot(ctx, &updated_snapshot, false, Some(&install_override)) {
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
    let metadata = load_installed_tool(&root, &normalized)?;
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

pub(crate) fn resolve_runtime(explicit: Option<&str>) -> Result<runtime_manager::RuntimeSelection> {
    let requirement = MIN_PYTHON_REQUIREMENT.to_string();
    if let Some(value) = explicit {
        if looks_like_python_path(value) {
            return resolve_runtime_from_path(value, &requirement);
        }
    }
    runtime_manager::resolve_runtime(explicit, &requirement).map_err(|err| {
        anyhow!(InstallUserError::new(
            "python runtime unavailable",
            json!({ "hint": err.to_string() }),
        ))
    })
}

fn looks_like_python_path(value: &str) -> bool {
    runtime_manager::normalize_channel(value).is_err()
}

fn resolve_runtime_from_path(
    value: &str,
    requirement: &str,
) -> Result<runtime_manager::RuntimeSelection> {
    let details = runtime_manager::inspect_python(Path::new(value)).map_err(|err| {
        anyhow!(InstallUserError::new(
            "python runtime unavailable",
            json!({
                "python": value,
                "hint": err.to_string(),
            }),
        ))
    })?;
    let specs = VersionSpecifiers::from_str(requirement).map_err(|err| {
        anyhow!(InstallUserError::new(
            "python runtime unavailable",
            json!({ "hint": err.to_string() }),
        ))
    })?;
    let version = Version::from_str(&details.full_version).map_err(|err| {
        anyhow!(InstallUserError::new(
            "python runtime unavailable",
            json!({ "hint": err.to_string() }),
        ))
    })?;
    if !specs.contains(&version) {
        return Err(anyhow!(InstallUserError::new(
            "python runtime unavailable",
            json!({
                "python": details.executable,
                "version": details.full_version,
                "requires_python": requirement,
                "hint": format!(
                    "Python {} does not satisfy requires-python {}",
                    details.full_version, requirement
                ),
            }),
        )));
    }
    let channel = runtime_manager::format_channel(&details.full_version).map_err(|err| {
        anyhow!(InstallUserError::new(
            "python runtime unavailable",
            json!({ "hint": err.to_string() }),
        ))
    })?;
    Ok(runtime_manager::RuntimeSelection {
        record: runtime_manager::RuntimeRecord {
            version: channel,
            full_version: details.full_version,
            path: details.executable,
            default: false,
        },
        source: runtime_manager::RuntimeSource::Explicit,
    })
}

fn scaffold_tool_pyproject(root: &Path, name: &str) -> Result<()> {
    let path = root.join("pyproject.toml");
    let desired_name = format!("px-tool-{name}");
    if path.exists() {
        let contents = fs::read_to_string(&path)?;
        let mut doc: DocumentMut = contents.parse()?;
        let table = doc
            .entry("project")
            .or_insert(Item::Table(Table::new()))
            .as_table_mut()
            .ok_or_else(|| anyhow!("[project] must be a table"))?;
        let current = table.get("name").and_then(Item::as_str).unwrap_or("");
        if current.is_empty() || current == name {
            table.insert("name", Item::Value(TomlValue::from(desired_name)));
            fs::write(&path, doc.to_string())?;
        }
        return Ok(());
    }
    let mut doc = DocumentMut::new();
    let mut project = Table::new();
    project.insert("name", Item::Value(TomlValue::from(desired_name)));
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

fn finalize_tool_environment(
    root: &Path,
    snapshot: &px_domain::api::ProjectSnapshot,
    runtime: &runtime_manager::RuntimeSelection,
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
    let lock_id = match lock.lock_id.clone() {
        Some(value) => value,
        None => compute_lock_hash(&snapshot.lock_path)?,
    };
    let env_id = format!(
        "tool-{}-{}-{}",
        normalize_tool_name(&snapshot.name),
        runtime.record.version.replace('.', "_"),
        &lock_id[..lock_id.len().min(12)]
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

fn collect_console_scripts(site_dir: &Path) -> Result<std::collections::BTreeMap<String, String>> {
    let mut scripts = std::collections::BTreeMap::new();
    if !site_dir.exists() {
        return Ok(scripts);
    }
    let pth = site_dir.join("px.pth");
    if !pth.exists() {
        return Ok(scripts);
    }
    let contents = fs::read_to_string(&pth)?;
    let mut visited = std::collections::HashSet::new();
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
    visited: &mut std::collections::HashSet<PathBuf>,
    scripts: &mut std::collections::BTreeMap<String, String>,
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

fn parse_entry_points(
    path: &Path,
    scripts: &mut std::collections::BTreeMap<String, String>,
) -> Result<()> {
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

fn update_tool_state(root: &Path, site_path: &Path, env_id: &str) -> Result<()> {
    let path = root.join(".px").join("state.json");
    let contents = fs::read_to_string(&path)?;
    let mut value: serde_json::Value = serde_json::from_str(&contents)?;
    if let Some(env) = value
        .as_object_mut()
        .and_then(|map| map.get_mut("current_env"))
        .and_then(serde_json::Value::as_object_mut)
    {
        env.insert(
            "site_packages".into(),
            serde_json::Value::String(site_path.display().to_string()),
        );
        env.insert("id".into(), serde_json::Value::String(env_id.to_string()));
        env.insert("env_path".into(), serde_json::Value::String(String::new()));
    }
    let mut buf = serde_json::to_vec_pretty(&value)?;
    buf.push(b'\n');
    fs::write(path, buf)?;
    Ok(())
}

fn write_console_shims(
    root: &Path,
    metadata: &ToolMetadata,
    scripts: &std::collections::BTreeMap<String, String>,
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
        #[cfg(not(windows))]
        {
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

fn name_looks_like_requirement(raw: &str) -> bool {
    raw.contains("==")
        || raw.contains(">=")
        || raw.contains("<=")
        || raw.contains("~=")
        || raw.contains("!=")
        || raw.contains('<')
        || raw.contains('>')
        || raw.contains('=')
        || raw.contains(' ')
}

fn load_installed_tool(root: &Path, normalized: &str) -> Result<ToolMetadata, InstallUserError> {
    match super::metadata::read_metadata(root) {
        Ok(meta) => Ok(meta),
        Err(err) => Err(InstallUserError::new(
            format!("tool '{normalized}' is not installed"),
            json!({ "error": err.to_string(), "hint": format!("run `px tool install {normalized}` first") }),
        )),
    }
}
