use std::env;
use std::path::Path;

use anyhow::Result;

use super::super::{CommandRunner, EnvPairs};
use crate::PythonContext;

pub(super) fn append_pythonpath(envs: &mut EnvPairs, plugin_dir: &Path) {
    let plugin_entry = plugin_dir.display().to_string();
    if let Some((_, value)) = envs.iter_mut().find(|(key, _)| key == "PYTHONPATH") {
        let mut parts: Vec<_> = env::split_paths(value).collect();
        if !parts.iter().any(|p| p == plugin_dir) {
            parts.insert(0, plugin_dir.to_path_buf());
            if let Ok(joined) = env::join_paths(parts) {
                if let Ok(strval) = joined.into_string() {
                    *value = strval;
                }
            }
        }
    } else {
        envs.push(("PYTHONPATH".into(), plugin_entry));
    }
}

pub(super) fn append_allowed_paths(envs: &mut EnvPairs, path: &Path) {
    if let Some((_, value)) = envs.iter_mut().find(|(key, _)| key == "PX_ALLOWED_PATHS") {
        let mut parts: Vec<_> = env::split_paths(value).collect();
        if !parts.iter().any(|p| p == path) {
            parts.insert(0, path.to_path_buf());
            if let Ok(joined) = env::join_paths(parts) {
                if let Ok(strval) = joined.into_string() {
                    *value = strval;
                }
            }
        }
    }
}

pub(super) fn run_python_command(
    runner: &dyn CommandRunner,
    py_ctx: &PythonContext,
    args: &[String],
    envs: &[(String, String)],
    stream_runner: bool,
    cwd: &Path,
) -> Result<crate::RunOutput> {
    let mut envs = envs.to_vec();
    if let Some(merged) = merged_pythonpath(&envs) {
        envs.retain(|(key, _)| key != "PYTHONPATH");
        envs.push(("PYTHONPATH".into(), merged));
    }
    if stream_runner {
        runner.run_command_streaming(&py_ctx.python, args, &envs, cwd)
    } else {
        runner.run_command(&py_ctx.python, args, &envs, cwd)
    }
}

pub(in crate::core::runtime::run) fn merged_pythonpath(
    envs: &[(String, String)],
) -> Option<String> {
    use std::collections::HashSet;

    let allowed = envs
        .iter()
        .find(|(key, _)| key == "PX_ALLOWED_PATHS")
        .map(|(_, value)| value)?;

    let mut paths = Vec::new();
    let mut seen = HashSet::new();
    let mut push_unique = |path: std::path::PathBuf| {
        if seen.insert(path.clone()) {
            paths.push(path);
        }
    };

    for entry in std::env::split_paths(allowed) {
        push_unique(entry);
    }

    if let Some((_, pythonpath)) = envs.iter().find(|(key, _)| key == "PYTHONPATH") {
        for entry in std::env::split_paths(pythonpath) {
            push_unique(entry);
        }
    }

    std::env::join_paths(paths)
        .ok()
        .and_then(|joined| joined.into_string().ok())
}
