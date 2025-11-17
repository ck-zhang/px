use std::env;
use std::ffi::OsString;

use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::{python_context, CommandContext, ExecutionOutcome};

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

pub fn env(ctx: &CommandContext, request: EnvRequest) -> Result<ExecutionOutcome> {
    env_outcome(ctx, request.mode)
}

fn env_outcome(ctx: &CommandContext, mode: EnvMode) -> Result<ExecutionOutcome> {
    match mode {
        EnvMode::Python => {
            let py = match python_context(ctx) {
                Ok(py) => py,
                Err(outcome) => return Ok(outcome),
            };
            let interpreter = py.python.clone();
            Ok(ExecutionOutcome::success(
                interpreter.clone(),
                json!({
                    "interpreter": interpreter,
                    "project_root": py.project_root.display().to_string(),
                    "pythonpath": py.pythonpath,
                }),
            ))
        }
        EnvMode::Info => {
            let py = match python_context(ctx) {
                Ok(py) => py,
                Err(outcome) => return Ok(outcome),
            };
            let mut details = env_details(&py);
            if let Value::Object(ref mut map) = details {
                map.insert("mode".to_string(), Value::String("info".to_string()));
            }
            Ok(ExecutionOutcome::success(
                format!(
                    "interpreter {} • project {}",
                    py.python,
                    py.project_root.display()
                ),
                details,
            ))
        }
        EnvMode::Paths => {
            let py = match python_context(ctx) {
                Ok(py) => py,
                Err(outcome) => return Ok(outcome),
            };
            let mut details = env_details(&py);
            let pythonpath_os = OsString::from(&py.pythonpath);
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
            Ok(ExecutionOutcome::success(
                format!("pythonpath entries: {}", os_paths.len()),
                details,
            ))
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
