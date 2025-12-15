use std::env;
use std::fs;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use crate::context::CommandContext;
use crate::outcome::{ExecutionOutcome, InstallUserError};
use crate::tools::disable_proxy_env;

use super::super::{
    is_missing_project_error, load_project_state, manifest_snapshot_at, missing_project_outcome,
    prepare_project_runtime, select_python_from_site,
};
use super::{
    build_pythonpath, ensure_environment_with_guard, ensure_version_file, EnvGuard,
    EnvironmentSyncReport, PythonContext,
};

impl PythonContext {
    fn new_with_guard(
        ctx: &CommandContext,
        guard: EnvGuard,
    ) -> Result<(Self, Option<EnvironmentSyncReport>)> {
        let project_root = ctx.project_root()?;
        let manifest_path = project_root.join("pyproject.toml");
        if !manifest_path.exists() {
            return Err(InstallUserError::new(
                format!("pyproject.toml not found in {}", project_root.display()),
                json!({
                    "pyproject": manifest_path.display().to_string(),
                    "hint": "run `px migrate --apply` or create pyproject.toml first",
                    "reason": "missing_manifest",
                }),
            )
            .into());
        }
        ensure_version_file(&manifest_path)?;
        let snapshot = manifest_snapshot_at(&project_root)?;
        let runtime = prepare_project_runtime(&snapshot)?;
        let sync_report = ensure_environment_with_guard(ctx, &snapshot, guard)?;
        let state = load_project_state(ctx.fs(), &project_root)?;
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
                    return Err(InstallUserError::new(
                        "python bytecode cache directory is not writable",
                        json!({
                            "reason": "pyc_cache_unwritable",
                            "cache_dir": prefix.display().to_string(),
                            "error": err.to_string(),
                            "hint": "ensure the directory is writable or set PX_CACHE_PATH to a writable location",
                        }),
                    )
                    .into());
                }
            }
        } else {
            None
        };
        let paths = build_pythonpath(ctx.fs(), &project_root, None)?;
        let python = select_python_from_site(
            &paths.site_bin,
            &runtime.record.path,
            &runtime.record.full_version,
        );
        Ok((
            Self {
                state_root: project_root.clone(),
                project_root,
                project_name: snapshot.name.clone(),
                python,
                pythonpath: paths.pythonpath,
                allowed_paths: paths.allowed_paths,
                site_bin: paths.site_bin,
                pep582_bin: paths.pep582_bin,
                pyc_cache_prefix,
                px_options: snapshot.px_options.clone(),
            },
            sync_report,
        ))
    }

    pub(crate) fn base_env(&self, command_args: &Value) -> Result<Vec<(String, String)>> {
        let mut allowed_paths = self.allowed_paths.clone();
        if !self.pythonpath.is_empty() {
            for entry in env::split_paths(&self.pythonpath) {
                if !allowed_paths.contains(&entry) {
                    allowed_paths.push(entry);
                }
            }
        }
        let allowed = env::join_paths(&allowed_paths)
            .context("allowed path contains invalid UTF-8")?
            .into_string()
            .map_err(|_| anyhow!("allowed path contains non-utf8 data"))?;
        let mut python_paths: Vec<_> = env::split_paths(&allowed).collect();
        if !self.pythonpath.is_empty() {
            python_paths.extend(env::split_paths(&self.pythonpath));
        }
        let pythonpath = env::join_paths(&python_paths)
            .context("failed to assemble PYTHONPATH")?
            .into_string()
            .map_err(|_| anyhow!("pythonpath contains non-utf8 data"))?;
        let mut envs = vec![
            ("PYTHONPATH".into(), pythonpath),
            ("PYTHONUNBUFFERED".into(), "1".into()),
            ("PYTHONSAFEPATH".into(), "1".into()),
        ];
        if env::var_os("PYTHONPYCACHEPREFIX").is_none() {
            if let Some(prefix) = self.pyc_cache_prefix.as_ref() {
                fs::create_dir_all(prefix).with_context(|| {
                    format!(
                        "failed to create python bytecode cache directory {}",
                        prefix.display()
                    )
                })?;
                envs.push(("PYTHONPYCACHEPREFIX".into(), prefix.display().to_string()));
            }
        }
        envs.push(("PX_ALLOWED_PATHS".into(), allowed));
        envs.push((
            "PX_PROJECT_ROOT".into(),
            self.project_root.display().to_string(),
        ));
        envs.push(("PX_PYTHON".into(), self.python.clone()));
        envs.push(("PX_COMMAND_JSON".into(), command_args.to_string()));
        if let Ok(debug_site) = env::var("PX_DEBUG_SITE_PATHS") {
            envs.push(("PX_DEBUG_SITE_PATHS".into(), debug_site));
        }
        if let Some(alias) = self.px_options.manage_command.as_ref() {
            let trimmed = alias.trim();
            if !trimmed.is_empty() {
                envs.push(("PYAPP_COMMAND_NAME".into(), trimmed.to_string()));
            }
        }
        if let Some(bin) = &self.site_bin {
            if let Some(site_dir) = bin.parent() {
                let virtual_env = site_dir
                    .canonicalize()
                    .unwrap_or_else(|_| site_dir.to_path_buf());
                envs.push(("VIRTUAL_ENV".into(), virtual_env.display().to_string()));
            }
        }
        let mut path_entries = Vec::new();
        if let Some(bin) = &self.site_bin {
            path_entries.push(bin.clone());
        }
        path_entries.extend(self.pep582_bin.iter().cloned());
        if let Some(python_dir) = Path::new(&self.python).parent() {
            path_entries.push(python_dir.to_path_buf());
        }
        if let Ok(existing) = env::var("PATH") {
            path_entries.extend(env::split_paths(&existing));
        }
        let mut unique = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for entry in path_entries.into_iter().filter(|p| p.exists()) {
            if seen.insert(entry.clone()) {
                unique.push(entry);
            }
        }
        if !unique.is_empty() {
            if let Ok(joined) = env::join_paths(&unique) {
                if let Ok(value) = joined.into_string() {
                    envs.push(("PATH".into(), value));
                }
            }
        }
        disable_proxy_env(&mut envs);
        Ok(envs)
    }
}

pub(crate) fn python_context(ctx: &CommandContext) -> Result<PythonContext, ExecutionOutcome> {
    python_context_with_mode(ctx, EnvGuard::Strict).map(|(py, _)| py)
}

pub(crate) fn python_context_with_mode(
    ctx: &CommandContext,
    guard: EnvGuard,
) -> Result<(PythonContext, Option<EnvironmentSyncReport>), ExecutionOutcome> {
    match PythonContext::new_with_guard(ctx, guard) {
        Ok(result) => Ok(result),
        Err(err) => {
            if is_missing_project_error(&err) {
                return Err(missing_project_outcome());
            }
            match err.downcast::<InstallUserError>() {
                Ok(user) => Err(ExecutionOutcome::user_error(user.message, user.details)),
                Err(err) => Err(ExecutionOutcome::failure(
                    "failed to prepare python environment",
                    json!({ "error": err.to_string() }),
                )),
            }
        }
    }
}
