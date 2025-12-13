use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::env;
use std::io::{self, Write};

use crate::process::{run_command, run_command_passthrough};
use crate::{
    build_pythonpath, ensure_project_environment_synced, load_project_state, outcome_from_output,
    CommandContext, ExecutionOutcome, InstallUserError,
};
use px_domain::ProjectSnapshot;

use super::install::resolve_runtime;
use super::metadata::read_metadata;
use super::paths::{normalize_tool_name, tool_root_dir};

#[derive(Clone, Debug)]
pub struct ToolRunRequest {
    pub name: String,
    pub args: Vec<String>,
    pub console: Option<String>,
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
    let metadata = match read_metadata(&tool_root) {
        Ok(meta) => meta,
        Err(err) => {
            return Ok(ExecutionOutcome::user_error(
                format!("tool '{normalized}' is not installed"),
                json!({
                    "error": err.to_string(),
                    "hint": format!("run `px tool install {normalized}` first"),
                }),
            ))
        }
    };
    let snapshot = match ProjectSnapshot::read_from(&tool_root) {
        Ok(snapshot) => snapshot,
        Err(err) => {
            return Ok(ExecutionOutcome::user_error(
                format!("tool '{normalized}' metadata is missing or corrupted"),
                json!({
                    "error": err.to_string(),
                    "hint": format!("reinstall with `px tool install {normalized}`"),
                }),
            ))
        }
    };
    let runtime_selection = resolve_runtime(Some(&metadata.runtime_version))?;
    env::set_var("PX_RUNTIME_PYTHON", &runtime_selection.record.path);
    if let Err(err) = ensure_project_environment_synced(ctx, &snapshot) {
        return match err.downcast::<InstallUserError>() {
            Ok(user) => {
                let mut details = match user.details {
                    Value::Object(map) => map,
                    other => {
                        let mut map = serde_json::Map::new();
                        map.insert(
                            "reason".into(),
                            Value::String("tool_env_unavailable".into()),
                        );
                        map.insert("details".into(), other);
                        map
                    }
                };
                details.insert("tool".into(), Value::String(normalized.clone()));
                details.insert(
                    "hint".into(),
                    Value::String(format!(
                        "run `px tool install {normalized}` to rebuild the tool environment"
                    )),
                );
                Ok(ExecutionOutcome::user_error(
                    format!("tool '{normalized}' is not ready"),
                    Value::Object(details),
                ))
            }
            Err(other) => Err(other),
        };
    }
    let state = load_project_state(ctx.fs(), &tool_root)?;
    let profile_oid = state
        .current_env
        .as_ref()
        .and_then(|env| env.profile_oid.clone().or_else(|| Some(env.id.clone())));
    let pyc_cache_prefix = if env::var_os("PYTHONPYCACHEPREFIX").is_some() {
        None
    } else if let Some(oid) = profile_oid.as_deref() {
        match crate::store::ensure_pyc_cache_prefix(&ctx.cache().path, oid) {
            Ok(prefix) => Some(prefix),
            Err(err) => {
                let prefix = crate::store::pyc_cache_prefix(&ctx.cache().path, oid);
                return Ok(ExecutionOutcome::user_error(
                    "python bytecode cache directory is not writable",
                    json!({
                        "reason": "pyc_cache_unwritable",
                        "cache_dir": prefix.display().to_string(),
                        "error": err.to_string(),
                        "hint": "ensure the directory is writable or set PX_CACHE_PATH to a writable location",
                    }),
                ));
            }
        }
    } else {
        None
    };
    let mut script_name = request.console.clone();
    if script_name.is_none() && metadata.console_scripts.contains_key(&metadata.name) {
        script_name = Some(metadata.name.clone());
    }
    let script_target = script_name.as_deref();
    let paths = build_pythonpath(ctx.fs(), &tool_root, None)?;
    let allowed_paths = paths.allowed_paths;
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
    let passthrough = ctx.env_flag_enabled("PX_TOOL_PASSTHROUGH") || request.args.is_empty();
    let cwd = env::current_dir().unwrap_or(tool_root.clone());
    if passthrough {
        if metadata.name == "grip" {
            let port = infer_grip_port(&request.args).unwrap_or(6419);
            println!(
                "px tool {}: serving from {} on http://localhost:{port} (Ctrl+C to stop)",
                metadata.name,
                cwd.display()
            );
        } else {
            println!(
                "px tool {}: launching in {} (Ctrl+C to stop)",
                metadata.name,
                cwd.display()
            );
        }
        io::stdout().flush().ok();
    }
    let env_payload = json!({
        "tool": metadata.name,
        "args": request.args,
    });
    let mut envs = vec![
        ("PYTHONPATH".into(), paths.pythonpath),
        ("PYTHONUNBUFFERED".into(), "1".into()),
        ("PX_ALLOWED_PATHS".into(), allowed),
        ("PX_TOOL_ROOT".into(), tool_root.display().to_string()),
        ("PX_COMMAND_JSON".into(), env_payload.to_string()),
    ];
    if let Ok(existing) = env::var("PYTHONPYCACHEPREFIX") {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            envs.push(("PYTHONPYCACHEPREFIX".into(), trimmed.to_string()));
        }
    } else if let Some(prefix) = pyc_cache_prefix.as_ref() {
        envs.push((
            "PYTHONPYCACHEPREFIX".into(),
            prefix.display().to_string(),
        ));
    }
    if let Some(alias) = snapshot.px_options.manage_command.as_ref() {
        let trimmed = alias.trim();
        if !trimmed.is_empty() {
            envs.push(("PYAPP_COMMAND_NAME".into(), trimmed.to_string()));
        }
    }
    disable_proxy_env(&mut envs);
    let output = if passthrough {
        run_command_passthrough(&runtime_selection.record.path, &args, &envs, &cwd)?
    } else {
        run_command(&runtime_selection.record.path, &args, &envs, &cwd)?
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

fn infer_grip_port(args: &[String]) -> Option<u16> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-p" | "--port" => {
                if let Some(value) = iter.next() {
                    if let Ok(port) = value.parse::<u16>() {
                        return Some(port);
                    }
                }
            }
            "--address" => {
                if let Some(value) = iter.next() {
                    if let Some(port) = value.rsplit(':').next() {
                        if let Ok(port) = port.parse::<u16>() {
                            return Some(port);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    None
}

pub(crate) fn disable_proxy_env(envs: &mut Vec<(String, String)>) {
    const PROXY_VARS: [&str; 8] = [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "NO_PROXY",
        "no_proxy",
    ];
    for key in PROXY_VARS {
        envs.push((key.to_string(), String::new()));
    }
}
