use super::super::*;

use std::env;
use std::path::{Path, PathBuf};

use serde_json::json;

use crate::core::runtime::facade::load_project_state;
use crate::core::runtime::runtime_manager;
use crate::core::sandbox::env_root_from_site_packages;

pub(in super::super) fn ephemeral_python_context(
    ctx: &CommandContext,
    snapshot: &ManifestSnapshot,
    runtime: &runtime_manager::RuntimeSelection,
    execution_root: &Path,
) -> Result<PythonContext, ExecutionOutcome> {
    let state = load_project_state(ctx.fs(), &snapshot.root).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to read ephemeral project state",
            json!({ "error": err.to_string() }),
        )
    })?;
    let env_state = state.current_env.as_ref().ok_or_else(|| {
        ExecutionOutcome::user_error(
            "ephemeral environment is missing",
            json!({
                "reason": "missing_env",
                "hint": "rerun without --frozen (or enable PX_ONLINE=1) to populate the cache",
            }),
        )
    })?;

    let env_root = env_state
        .env_path
        .as_ref()
        .map(|path| PathBuf::from(path.trim()))
        .filter(|path| !path.as_os_str().is_empty())
        .or_else(|| {
            let site = PathBuf::from(env_state.site_packages.trim());
            env_root_from_site_packages(&site)
        })
        .ok_or_else(|| {
            ExecutionOutcome::failure(
                "ephemeral environment missing env root",
                json!({ "reason": "missing_env_root" }),
            )
        })?;

    let paths = build_pythonpath(ctx.fs(), execution_root, Some(env_root)).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to assemble PYTHONPATH for ephemeral run",
            json!({ "error": err.to_string() }),
        )
    })?;
    let mut allowed_paths = paths.allowed_paths;
    if snapshot.root != execution_root && !allowed_paths.iter().any(|p| p == &snapshot.root) {
        allowed_paths.push(snapshot.root.clone());
    }
    let python = select_python_from_site(
        &paths.site_bin,
        &runtime.record.path,
        &runtime.record.full_version,
    );

    let profile_oid = env_state
        .profile_oid
        .clone()
        .or_else(|| Some(env_state.id.clone()));
    let pyc_cache_prefix = if env::var_os("PYTHONPYCACHEPREFIX").is_some() {
        None
    } else if let Some(oid) = profile_oid.as_deref() {
        match crate::store::ensure_pyc_cache_prefix(&ctx.cache().path, oid) {
            Ok(prefix) => Some(prefix),
            Err(err) => {
                let prefix = crate::store::pyc_cache_prefix(&ctx.cache().path, oid);
                return Err(ExecutionOutcome::user_error(
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

    Ok(PythonContext {
        project_root: execution_root.to_path_buf(),
        state_root: snapshot.root.clone(),
        project_name: snapshot.name.clone(),
        python,
        pythonpath: paths.pythonpath,
        allowed_paths,
        site_bin: paths.site_bin,
        pep582_bin: paths.pep582_bin,
        pyc_cache_prefix,
        px_options: snapshot.px_options.clone(),
    })
}
