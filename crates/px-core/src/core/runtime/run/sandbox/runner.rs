use std::collections::HashSet;
use std::env;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::json;

use crate::core::sandbox::{
    detect_container_backend, ensure_image_layout, run_container, sandbox_image_tag,
    ContainerBackend, ContainerRunArgs, Mount, RunMode, SandboxImageLayout,
};
use crate::{ExecutionOutcome, PythonContext};

#[derive(Clone)]
pub(crate) struct SandboxCommandRunner {
    backend: ContainerBackend,
    layout: SandboxImageLayout,
    sbx_id: String,
    mounts: Vec<Mount>,
    host_project_root: PathBuf,
    host_env_root: PathBuf,
    container_project_root: PathBuf,
    container_env_root: PathBuf,
    container_pyc_cache_prefix: Option<PathBuf>,
    pythonpath: String,
    allowed_paths_env: String,
}

impl SandboxCommandRunner {
    fn run_in_container(
        &self,
        program: &str,
        args: &[String],
        envs: &[(String, String)],
        cwd: &Path,
        mode: RunMode,
        inherit_stdin: bool,
    ) -> Result<crate::RunOutput> {
        let mut env = Vec::new();
        let mut base_path: Option<String> = None;
        let mut rewritten_pythonpath: Option<String> = None;
        let mut allowed_entries: Vec<PathBuf> =
            std::env::split_paths(&self.allowed_paths_env).collect();
        let mut ld_entries = Vec::new();
        let mut ld_value: Option<String> = None;
        for (key, value) in envs {
            match key.as_str() {
                "PX_ALLOWED_PATHS"
                | "PX_PROJECT_ROOT"
                | "PX_PYTHON"
                | "VIRTUAL_ENV"
                | "PYTHONHOME"
                | "PYTHONPYCACHEPREFIX" => continue,
                "PATH" => {
                    base_path = Some(value.clone());
                }
                "LD_LIBRARY_PATH" => {
                    let rewritten = rewrite_env_value(
                        value,
                        &self.host_project_root,
                        &self.container_project_root,
                        &self.host_env_root,
                        &self.container_env_root,
                    );
                    ld_value = Some(rewritten.clone());
                    for entry in std::env::split_paths(&rewritten) {
                        ld_entries.push(entry);
                    }
                }
                "PYTHONPATH" => {
                    rewritten_pythonpath = Some(rewrite_env_value(
                        value,
                        &self.host_project_root,
                        &self.container_project_root,
                        &self.host_env_root,
                        &self.container_env_root,
                    ));
                }
                _ => {
                    env.push((
                        key.clone(),
                        rewrite_env_value(
                            value,
                            &self.host_project_root,
                            &self.container_project_root,
                            &self.host_env_root,
                            &self.container_env_root,
                        ),
                    ));
                }
            }
        }
        let pythonpath_value = rewritten_pythonpath.unwrap_or_else(|| self.pythonpath.clone());
        allowed_entries.extend(std::env::split_paths(&pythonpath_value));
        for path in [
            self.container_env_root.join("lib"),
            self.container_env_root.join("lib64"),
            self.container_env_root.join("site-packages"),
            self.container_env_root
                .join("site-packages")
                .join("sys-libs"),
            self.container_env_root.join("sys-libs"),
        ] {
            ld_entries.push(path);
        }
        let mut seen_ld = HashSet::new();
        ld_entries.retain(|entry| seen_ld.insert(entry.clone()));
        let ld_library_path = if ld_entries.is_empty() {
            ld_value
        } else {
            Some(
                std::env::join_paths(&ld_entries)
                    .map_err(|err| anyhow::anyhow!(err.to_string()))?
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("non-utf8 ld_library_path entry"))?,
            )
        };
        if let Some(ref value) = ld_library_path {
            allowed_entries.extend(std::env::split_paths(value));
        }
        let mut seen_allowed = HashSet::new();
        allowed_entries.retain(|entry| seen_allowed.insert(entry.clone()));
        let allowed_paths_env = std::env::join_paths(allowed_entries)
            .map_err(|err| anyhow::anyhow!(err.to_string()))?
            .into_string()
            .map_err(|_| anyhow::anyhow!("non-utf8 allowed path entry"))?;
        env.push(("PX_ALLOWED_PATHS".into(), allowed_paths_env));
        env.push(("PYTHONPATH".into(), pythonpath_value));
        if let Some(prefix) = self.container_pyc_cache_prefix.as_ref() {
            env.push(("PYTHONPYCACHEPREFIX".into(), prefix.display().to_string()));
        }
        if let Some(ld_paths) = ld_library_path {
            env.push(("LD_LIBRARY_PATH".into(), ld_paths));
        }
        env.push((
            "PX_PROJECT_ROOT".into(),
            self.container_project_root.display().to_string(),
        ));
        env.push(("PX_PYTHON".into(), "/px/runtime/bin/python".into()));
        env.push((
            "VIRTUAL_ENV".into(),
            self.container_env_root.display().to_string(),
        ));
        env.push((
            "PYTHONHOME".into(),
            self.container_env_root.display().to_string(),
        ));
        let default_path = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/bin".to_string();
        let base_path = base_path.unwrap_or(default_path);
        env.push((
            "PATH".into(),
            format!("{}/bin:{base_path}", self.container_env_root.display()),
        ));
        env.push(("PX_SANDBOX".into(), "1".into()));
        env.push(("PX_SANDBOX_ID".into(), self.sbx_id.clone()));
        let workdir = map_workdir_container(
            cwd,
            &self.host_project_root,
            &self.container_project_root,
            &self.host_env_root,
            &self.container_env_root,
        );
        let original_program = program.to_string();
        let mut program = map_program_for_container(
            program,
            &self.host_project_root,
            &self.container_project_root,
            &self.host_env_root,
            &self.container_env_root,
        );
        let mut args: Vec<String> = args
            .iter()
            .map(|arg| {
                map_arg_for_container(
                    arg,
                    &self.host_project_root,
                    &self.container_project_root,
                    &self.host_env_root,
                    &self.container_env_root,
                )
            })
            .collect();
        let host_program = PathBuf::from(original_program);
        if host_program.is_absolute() {
            let program_canon = host_program
                .canonicalize()
                .unwrap_or_else(|_| host_program.clone());
            let is_python_binary = program_canon
                .file_name()
                .and_then(|n| n.to_str())
                .map(|name| {
                    let lower = name.to_ascii_lowercase();
                    lower == "python"
                        || lower.starts_with("python3")
                        || lower.starts_with("python2")
                })
                .unwrap_or(false);
            if !is_python_binary {
                if let Some(env_root) = program_canon
                    .parent()
                    .and_then(|p| p.parent())
                    .and_then(|p| p.canonicalize().ok())
                    .filter(|root| root.join("pyvenv.cfg").exists())
                {
                    if let Ok(rel) = program_canon.strip_prefix(&env_root) {
                        let script = self.container_env_root.join(rel).display().to_string();
                        let mut rewritten = vec![script];
                        rewritten.extend(args);
                        args = rewritten;
                        program = self
                            .container_env_root
                            .join("bin/python")
                            .display()
                            .to_string();
                    }
                }
            }
        }
        let opts = ContainerRunArgs {
            env,
            mounts: self.mounts.clone(),
            workdir,
            program,
            args,
        };
        let mode = match mode {
            RunMode::WithStdin(_) => RunMode::WithStdin(inherit_stdin),
            other => other,
        };
        run_container(&self.backend, &self.layout, &opts, mode).map_err(anyhow::Error::new)
    }
}

impl super::super::CommandRunner for SandboxCommandRunner {
    fn run_command(
        &self,
        program: &str,
        args: &[String],
        envs: &[(String, String)],
        cwd: &Path,
    ) -> Result<crate::RunOutput> {
        self.run_in_container(program, args, envs, cwd, RunMode::Capture, false)
    }

    fn run_command_streaming(
        &self,
        program: &str,
        args: &[String],
        envs: &[(String, String)],
        cwd: &Path,
    ) -> Result<crate::RunOutput> {
        self.run_in_container(program, args, envs, cwd, RunMode::Streaming, false)
    }

    fn run_command_with_stdin(
        &self,
        program: &str,
        args: &[String],
        envs: &[(String, String)],
        cwd: &Path,
        inherit_stdin: bool,
    ) -> Result<crate::RunOutput> {
        self.run_in_container(
            program,
            args,
            envs,
            cwd,
            RunMode::WithStdin(inherit_stdin),
            inherit_stdin,
        )
    }

    fn run_command_passthrough(
        &self,
        program: &str,
        args: &[String],
        envs: &[(String, String)],
        cwd: &Path,
    ) -> Result<crate::RunOutput> {
        self.run_in_container(program, args, envs, cwd, RunMode::Passthrough, true)
    }
}

fn canonical_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn map_allowed_paths_for_container(
    allowed_paths: &[PathBuf],
    project_root: &Path,
    env_root: &Path,
    container_project: &Path,
    container_env: &Path,
) -> Vec<PathBuf> {
    let mut mapped = Vec::new();
    for path in allowed_paths {
        let mapped_path = map_path_for_container(
            path,
            project_root,
            env_root,
            container_project,
            container_env,
        );
        if !mapped.iter().any(|p| p == &mapped_path) {
            mapped.push(mapped_path);
        }
    }
    if !mapped.iter().any(|p| p == container_project) {
        mapped.insert(0, container_project.to_path_buf());
    }
    mapped
}

fn map_path_for_container(
    path: &Path,
    project_root: &Path,
    env_root: &Path,
    container_project: &Path,
    container_env: &Path,
) -> PathBuf {
    if path.starts_with(project_root) {
        return container_project
            .join(
                path.strip_prefix(project_root)
                    .unwrap_or_else(|_| Path::new("")),
            )
            .to_path_buf();
    }
    if path.starts_with(env_root) {
        return container_env
            .join(
                path.strip_prefix(env_root)
                    .unwrap_or_else(|_| Path::new("")),
            )
            .to_path_buf();
    }
    path.to_path_buf()
}

fn map_workdir_container(
    cwd: &Path,
    host_project_root: &Path,
    container_project_root: &Path,
    host_env_root: &Path,
    container_env_root: &Path,
) -> PathBuf {
    if cwd.starts_with(host_project_root) {
        return container_project_root.join(
            cwd.strip_prefix(host_project_root)
                .unwrap_or_else(|_| Path::new("")),
        );
    }
    if cwd.starts_with(host_env_root) {
        return container_env_root.join(
            cwd.strip_prefix(host_env_root)
                .unwrap_or_else(|_| Path::new("")),
        );
    }
    canonical_path(cwd)
}

pub(in super::super) fn map_program_for_container(
    program: &str,
    host_project_root: &Path,
    container_project_root: &Path,
    host_env_root: &Path,
    container_env_root: &Path,
) -> String {
    let path = Path::new(program);
    if !path.is_absolute() {
        return program.to_string();
    }

    let program_canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let host_project_canon = host_project_root
        .canonicalize()
        .unwrap_or_else(|_| host_project_root.to_path_buf());
    let host_env_canon = host_env_root
        .canonicalize()
        .unwrap_or_else(|_| host_env_root.to_path_buf());

    if let Ok(rel) = program_canon.strip_prefix(&host_project_canon) {
        return container_project_root.join(rel).display().to_string();
    }
    if let Ok(rel) = program_canon.strip_prefix(&host_env_canon) {
        return container_env_root.join(rel).display().to_string();
    }

    // Fall back to the original path when no mapping applies.
    program.to_string()
}

fn map_arg_for_container(
    arg: &str,
    host_project_root: &Path,
    container_project_root: &Path,
    host_env_root: &Path,
    container_env_root: &Path,
) -> String {
    let path = Path::new(arg);
    if !path.is_absolute() {
        return arg.to_string();
    }
    let arg_canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let host_project_canon = host_project_root
        .canonicalize()
        .unwrap_or_else(|_| host_project_root.to_path_buf());
    if let Ok(rel) = arg_canon.strip_prefix(&host_project_canon) {
        return container_project_root.join(rel).display().to_string();
    }
    let host_env_canon = host_env_root
        .canonicalize()
        .unwrap_or_else(|_| host_env_root.to_path_buf());
    if let Ok(rel) = arg_canon.strip_prefix(&host_env_canon) {
        return container_env_root.join(rel).display().to_string();
    }
    arg.to_string()
}

fn rewrite_env_value(
    value: &str,
    host_project_root: &Path,
    container_project_root: &Path,
    host_env_root: &Path,
    container_env_root: &Path,
) -> String {
    let mut rewritten = value.to_string();
    if let (Some(host), Some(container)) =
        (host_project_root.to_str(), container_project_root.to_str())
    {
        rewritten = rewritten.replace(host, container);
    }
    if let (Some(host), Some(container)) = (host_env_root.to_str(), container_env_root.to_str()) {
        rewritten = rewritten.replace(host, container);
    }
    rewritten
}

pub(crate) fn sandbox_runner_for_context(
    py_ctx: &PythonContext,
    sandbox: &mut super::SandboxRunContext,
    workdir: &Path,
) -> Result<super::SandboxCommandRunner, ExecutionOutcome> {
    let backend = detect_container_backend().map_err(|err| {
        ExecutionOutcome::user_error(err.message().to_string(), err.details().clone())
    })?;
    let tag = sandbox_image_tag(&sandbox.artifacts.definition.sbx_id());
    let layout = ensure_image_layout(
        &mut sandbox.artifacts,
        &sandbox.store,
        &py_ctx.project_root,
        &py_ctx.allowed_paths,
        &tag,
    )
    .map_err(|err| ExecutionOutcome::user_error(err.message, err.details))?;
    let container_project = PathBuf::from("/app");
    let container_env = PathBuf::from("/px/env");
    let mapped_allowed = map_allowed_paths_for_container(
        &py_ctx.allowed_paths,
        &py_ctx.project_root,
        &sandbox.artifacts.env_root,
        &container_project,
        &container_env,
    );
    let mapped_pythonpath: Vec<PathBuf> = env::split_paths(&py_ctx.pythonpath)
        .map(|path| {
            map_path_for_container(
                &path,
                &py_ctx.project_root,
                &sandbox.artifacts.env_root,
                &container_project,
                &container_env,
            )
        })
        .collect();
    let mut allowed_union = mapped_allowed.clone();
    for entry in &mapped_pythonpath {
        if !allowed_union.iter().any(|p| p == entry) {
            allowed_union.push(entry.clone());
        }
    }
    let allowed_env = env::join_paths(&allowed_union)
        .map_err(|err| {
            ExecutionOutcome::failure(
                "failed to assemble sandbox allowed paths",
                json!({ "error": err.to_string() }),
            )
        })?
        .into_string()
        .map_err(|_| {
            ExecutionOutcome::failure(
                "failed to assemble sandbox allowed paths",
                json!({ "error": "non-utf8 path entry" }),
            )
        })?;
    let pythonpath = env::join_paths(&mapped_pythonpath)
        .map_err(|err| {
            ExecutionOutcome::failure(
                "failed to assemble sandbox python path",
                json!({ "error": err.to_string() }),
            )
        })?
        .into_string()
        .map_err(|_| {
            ExecutionOutcome::failure(
                "failed to assemble sandbox python path",
                json!({ "error": "non-utf8 path entry" }),
            )
        })?;
    let profile_oid = sandbox.artifacts.definition.profile_oid.clone();
    let cache_root = crate::store::resolve_cache_store_path().map_err(|err| {
        ExecutionOutcome::failure(
            "failed to resolve px cache directory",
            json!({ "error": err.to_string() }),
        )
    })?;
    let host_pyc_cache_prefix = match crate::store::ensure_pyc_cache_prefix(
        &cache_root.path,
        &profile_oid,
    ) {
        Ok(prefix) => prefix,
        Err(err) => {
            let prefix = crate::store::pyc_cache_prefix(&cache_root.path, &profile_oid);
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
    };
    let container_pyc_cache_prefix = PathBuf::from("/px/cache/pyc").join(&profile_oid);
    let mut mounts = sandbox_mounts(py_ctx, workdir, &sandbox.artifacts.env_root);
    if let Ok(envs_root) = crate::core::runtime::cas_env::default_envs_root() {
        let envs_root = canonical_path(envs_root.as_path());
        mounts.push(Mount {
            host: envs_root.clone(),
            guest: envs_root,
            read_only: true,
        });
    }
    if let Ok(tools_root) = crate::core::tools::tools_root() {
        let tools_root = canonical_path(tools_root.as_path());
        mounts.push(Mount {
            host: tools_root.clone(),
            guest: tools_root,
            read_only: true,
        });
    }
    let store_root = canonical_path(crate::store::cas::global_store().root());
    mounts.push(Mount {
        host: store_root.clone(),
        guest: store_root,
        read_only: true,
    });
    mounts.push(Mount {
        host: canonical_path(&host_pyc_cache_prefix),
        guest: container_pyc_cache_prefix.clone(),
        read_only: false,
    });
    let mut seen = HashSet::new();
    mounts.retain(|m| seen.insert((m.host.clone(), m.guest.clone())));
    Ok(SandboxCommandRunner {
        backend,
        layout,
        sbx_id: sandbox.artifacts.definition.sbx_id(),
        mounts,
        host_project_root: py_ctx.project_root.clone(),
        host_env_root: sandbox.artifacts.env_root.clone(),
        container_project_root: container_project,
        container_env_root: container_env,
        container_pyc_cache_prefix: Some(container_pyc_cache_prefix),
        pythonpath,
        allowed_paths_env: allowed_env,
    })
}

fn sandbox_mounts(py_ctx: &PythonContext, workdir: &Path, env_root: &Path) -> Vec<Mount> {
    let container_project = PathBuf::from("/app");
    let mut mounts = vec![Mount {
        host: canonical_path(&py_ctx.project_root),
        guest: container_project,
        read_only: false,
    }];
    let workdir = canonical_path(workdir);
    if !workdir.starts_with(&py_ctx.project_root) {
        mounts.push(Mount {
            host: workdir.clone(),
            guest: workdir.clone(),
            read_only: false,
        });
    }
    for path in &py_ctx.allowed_paths {
        if path.starts_with(&py_ctx.project_root) || path.starts_with(env_root) {
            continue;
        }
        let host = if path.is_file() {
            path.parent()
                .map(PathBuf::from)
                .unwrap_or_else(|| path.clone())
        } else {
            path.clone()
        };
        let host = canonical_path(&host);
        let guest = host.clone();
        mounts.push(Mount {
            host,
            guest,
            read_only: false,
        });
    }
    let mut seen = HashSet::new();
    mounts
        .into_iter()
        .filter(|m| seen.insert((m.host.clone(), m.guest.clone())))
        .collect()
}
