use std::env;
use std::ffi::OsString;

use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::{
    attach_autosync_details, build_pythonpath, python_context_with_mode, CommandContext,
    EnvGuard, ExecutionOutcome, PythonContext,
};

#[derive(Clone, Debug)]
pub enum EnvMode {
    Info,
    Paths,
    Python,
    Unknown(String),
}

#[derive(Clone, Debug)]
pub struct EnvRequest {
    pub mode: EnvMode,
}

struct EnvContextResult {
    ctx: PythonContext,
    sync_report: Option<crate::EnvironmentSyncReport>,
    fallback_hint: Option<String>,
}

pub fn env(ctx: &CommandContext, request: EnvRequest) -> Result<ExecutionOutcome> {
    env_outcome(ctx, request.mode)
}

fn env_outcome(ctx: &CommandContext, mode: EnvMode) -> Result<ExecutionOutcome> {
    match mode {
        EnvMode::Python => {
            let resolved = match resolve_env_context(ctx) {
                Ok(value) => value,
                Err(outcome) => return Ok(outcome),
            };
            let interpreter = resolved.ctx.python.clone();
            let mut details = json!({
                "interpreter": interpreter,
                "project_root": resolved.ctx.project_root.display().to_string(),
                "pythonpath": resolved.ctx.pythonpath,
            });
            apply_fallback_hint(&mut details, resolved.fallback_hint.as_deref());
            let mut outcome = ExecutionOutcome::success(interpreter, details);
            attach_autosync_details(&mut outcome, resolved.sync_report);
            Ok(outcome)
        }
        EnvMode::Info => {
            let resolved = match resolve_env_context(ctx) {
                Ok(value) => value,
                Err(outcome) => return Ok(outcome),
            };
            let mut details = env_details(&resolved.ctx);
            if let Value::Object(ref mut map) = details {
                map.insert("mode".to_string(), Value::String("info".to_string()));
            }
            apply_fallback_hint(&mut details, resolved.fallback_hint.as_deref());
            let mut outcome = ExecutionOutcome::success(
                format!(
                    "interpreter {} • project {}",
                    resolved.ctx.python,
                    resolved.ctx.project_root.display()
                ),
                details,
            );
            attach_autosync_details(&mut outcome, resolved.sync_report);
            Ok(outcome)
        }
        EnvMode::Paths => {
            let resolved = match resolve_env_context(ctx) {
                Ok(value) => value,
                Err(outcome) => return Ok(outcome),
            };
            let mut details = env_details(&resolved.ctx);
            let pythonpath_os = OsString::from(&resolved.ctx.pythonpath);
            let os_paths = env::split_paths(&pythonpath_os)
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>();
            if let Value::Object(ref mut map) = details {
                map.insert("mode".to_string(), Value::String("paths".to_string()));
                map.insert(
                    "paths".to_string(),
                    Value::Array(os_paths.iter().map(|p| Value::String(p.clone())).collect()),
                );
            }
            apply_fallback_hint(&mut details, resolved.fallback_hint.as_deref());
            let mut outcome = ExecutionOutcome::success(
                format!("pythonpath entries: {}", os_paths.len()),
                details,
            );
            attach_autosync_details(&mut outcome, resolved.sync_report);
            Ok(outcome)
        }
        EnvMode::Unknown(other) => bail!("px env mode `{other}` not implemented"),
    }
}

fn env_details(ctx: &crate::PythonContext) -> Value {
    json!({
        "interpreter": ctx.python.clone(),
        "project_root": ctx.project_root.display().to_string(),
        "pythonpath": ctx.pythonpath.clone(),
        "env": {
            "PX_PROJECT_ROOT": ctx.project_root.display().to_string(),
            "PYTHONPATH": ctx.pythonpath.clone(),
        }
    })
}

fn resolve_env_context(ctx: &CommandContext) -> Result<EnvContextResult, ExecutionOutcome> {
    match python_context_with_mode(ctx, EnvGuard::Strict) {
        Ok((py, report)) => Ok(EnvContextResult {
            ctx: py,
            sync_report: report,
            fallback_hint: None,
        }),
        Err(outcome) => {
            if !is_missing_env(&outcome.details) {
                return Err(outcome);
            }
            let fallback_hint = outcome
                .details
                .as_object()
                .and_then(|map| map.get("hint"))
                .and_then(Value::as_str)
                .map(|s| s.to_string())
                .unwrap_or_else(|| "run `px sync` to build the environment".to_string());
            let project_root = ctx.project_root().map_err(|err| {
                ExecutionOutcome::failure(
                    "failed to determine project root",
                    json!({ "error": err.to_string() }),
                )
            })?;
            let python = ctx
                .python_runtime()
                .detect_interpreter()
                .map_err(|err| {
                    ExecutionOutcome::failure(
                        "failed to locate python interpreter",
                        json!({ "error": err.to_string() }),
                    )
                })?;
            let (pythonpath, allowed_paths) = build_pythonpath(ctx.fs(), &project_root).map_err(
                |err| {
                    ExecutionOutcome::failure(
                        "failed to build PYTHONPATH",
                        json!({ "error": err.to_string() }),
                    )
                },
            )?;
            Ok(EnvContextResult {
                ctx: PythonContext {
                    project_root,
                    python,
                    pythonpath,
                    allowed_paths,
                },
                sync_report: None,
                fallback_hint: Some(fallback_hint),
            })
        }
    }
}

fn is_missing_env(details: &Value) -> bool {
    details
        .as_object()
        .and_then(|map| map.get("reason"))
        .and_then(Value::as_str)
        .map(|reason| reason == "missing_env")
        .unwrap_or(false)
}

fn apply_fallback_hint(details: &mut Value, hint: Option<&str>) {
    let Some(text) = hint else { return; };
    if let Value::Object(ref mut map) = details {
        map.entry("warning".to_string())
            .or_insert_with(|| Value::String("project environment missing; showing base interpreter".to_string()));
        map.insert("hint".to_string(), Value::String(text.to_string()));
    }
}
