use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::{Cursor, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use flate2::read::GzDecoder;
use serde_json::{json, Value};
use tar::Archive;
use tempfile::TempDir;
use tracing::{debug, warn};

use super::cas_env::{ensure_profile_env, project_env_owner_id, workspace_env_owner_id};
use super::script::{detect_inline_script, run_inline_script};
use crate::core::runtime::artifacts::dependency_name;
use crate::core::runtime::facade::{
    build_pythonpath, compute_lock_hash_bytes, detect_runtime_metadata, load_project_state,
    marker_env_for_snapshot, select_python_from_site, ManifestSnapshot,
};
use crate::core::sandbox::{
    default_store_root, detect_container_backend, discover_site_packages, ensure_image_layout,
    ensure_sandbox_image, env_root_from_site_packages, run_container, run_pxapp_bundle,
    sandbox_image_tag, ContainerBackend, ContainerRunArgs, Mount, RunMode, SandboxArtifacts,
    SandboxImageLayout, SandboxStore,
};
use crate::project::evaluate_project_state;
use crate::run_plan::{plan_run_target, RunTargetPlan};
use crate::tooling::{missing_pyproject_outcome, run_target_required_outcome};
use crate::workspace::{prepare_workspace_run_context, WorkspaceStateKind, WorkspaceStateReport};
use crate::{
    attach_autosync_details, is_missing_project_error, manifest_snapshot, missing_project_outcome,
    outcome_from_output, python_context_with_mode, state_guard::guard_for_execution,
    CommandContext, ExecutionOutcome, OwnerId, OwnerType, PythonContext,
};
use px_domain::{detect_lock_drift, load_lockfile_optional, sandbox_config_from_manifest};

#[derive(Clone, Debug)]
pub struct TestRequest {
    pub args: Vec<String>,
    pub frozen: bool,
    pub sandbox: bool,
    pub at: Option<String>,
}

#[derive(Clone, Debug)]
pub struct RunRequest {
    pub entry: Option<String>,
    pub target: Option<String>,
    pub args: Vec<String>,
    pub frozen: bool,
    pub interactive: Option<bool>,
    pub sandbox: bool,
    pub at: Option<String>,
}

type EnvPairs = Vec<(String, String)>;

#[derive(Clone, Debug)]
pub(crate) struct SandboxRunContext {
    store: SandboxStore,
    artifacts: SandboxArtifacts,
}

pub(crate) trait CommandRunner {
    fn run_command(
        &self,
        program: &str,
        args: &[String],
        envs: &[(String, String)],
        cwd: &Path,
    ) -> Result<crate::RunOutput>;

    fn run_command_streaming(
        &self,
        program: &str,
        args: &[String],
        envs: &[(String, String)],
        cwd: &Path,
    ) -> Result<crate::RunOutput>;

    fn run_command_with_stdin(
        &self,
        program: &str,
        args: &[String],
        envs: &[(String, String)],
        cwd: &Path,
        inherit_stdin: bool,
    ) -> Result<crate::RunOutput>;

    fn run_command_passthrough(
        &self,
        program: &str,
        args: &[String],
        envs: &[(String, String)],
        cwd: &Path,
    ) -> Result<crate::RunOutput>;
}

#[derive(Clone, Copy)]
pub(crate) struct HostCommandRunner<'a> {
    ctx: &'a CommandContext<'a>,
}

impl<'a> HostCommandRunner<'a> {
    pub(crate) fn new(ctx: &'a CommandContext<'a>) -> Self {
        Self { ctx }
    }
}

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
    pythonpath: String,
    allowed_paths_env: String,
}

fn invocation_workdir(project_root: &Path) -> PathBuf {
    map_workdir(Some(project_root), project_root)
}

fn map_workdir(invocation_root: Option<&Path>, context_root: &Path) -> PathBuf {
    let cwd = env::current_dir().unwrap_or_else(|_| context_root.to_path_buf());
    if let Some(root) = invocation_root {
        if let Ok(rel) = cwd.strip_prefix(root) {
            return context_root.join(rel);
        }
    }
    if cwd.starts_with(context_root) {
        cwd
    } else {
        context_root.to_path_buf()
    }
}

impl CommandRunner for HostCommandRunner<'_> {
    fn run_command(
        &self,
        program: &str,
        args: &[String],
        envs: &[(String, String)],
        cwd: &Path,
    ) -> Result<crate::RunOutput> {
        self.ctx
            .python_runtime()
            .run_command(program, args, envs, cwd)
    }

    fn run_command_streaming(
        &self,
        program: &str,
        args: &[String],
        envs: &[(String, String)],
        cwd: &Path,
    ) -> Result<crate::RunOutput> {
        self.ctx
            .python_runtime()
            .run_command_streaming(program, args, envs, cwd)
    }

    fn run_command_with_stdin(
        &self,
        program: &str,
        args: &[String],
        envs: &[(String, String)],
        cwd: &Path,
        inherit_stdin: bool,
    ) -> Result<crate::RunOutput> {
        self.ctx
            .python_runtime()
            .run_command_with_stdin(program, args, envs, cwd, inherit_stdin)
    }

    fn run_command_passthrough(
        &self,
        program: &str,
        args: &[String],
        envs: &[(String, String)],
        cwd: &Path,
    ) -> Result<crate::RunOutput> {
        self.ctx
            .python_runtime()
            .run_command_passthrough(program, args, envs, cwd)
    }
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
                "PX_ALLOWED_PATHS" | "PX_PROJECT_ROOT" | "PX_PYTHON" | "VIRTUAL_ENV"
                | "PYTHONHOME" => continue,
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

impl CommandRunner for SandboxCommandRunner {
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

fn map_program_for_container(
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
    sandbox: &mut SandboxRunContext,
    workdir: &Path,
) -> Result<SandboxCommandRunner, ExecutionOutcome> {
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
    Ok(SandboxCommandRunner {
        backend,
        layout,
        sbx_id: sandbox.artifacts.definition.sbx_id(),
        mounts: sandbox_mounts(py_ctx, workdir, &sandbox.artifacts.env_root),
        host_project_root: py_ctx.project_root.clone(),
        host_env_root: sandbox.artifacts.env_root.clone(),
        container_project_root: container_project,
        container_env_root: container_env,
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

fn sandbox_workspace_env_inconsistent(
    root: &Path,
    state: &WorkspaceStateReport,
) -> ExecutionOutcome {
    let reason = state
        .env_issue
        .as_ref()
        .and_then(|issue| issue.get("reason").and_then(serde_json::Value::as_str))
        .unwrap_or("env_outdated");
    ExecutionOutcome::user_error(
        "sandbox requires a consistent workspace environment",
        json!({
            "code": "PX902",
            "reason": reason,
            "hint": "run `px sync` at the workspace root before using --sandbox",
            "state": state.canonical.as_str(),
            "workspace_root": root.display().to_string(),
        }),
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TestReporter {
    Px,
    Pytest,
}

#[derive(Clone, Debug)]
enum TestRunner {
    Pytest,
    Builtin,
    Script(PathBuf),
}

#[derive(Clone, Default)]
struct DependencyContext {
    manifest: HashSet<String>,
    locked: HashSet<String>,
}

impl DependencyContext {
    fn from_sources(manifest_specs: &[String], lock_path: Option<&Path>) -> Self {
        let mut manifest = HashSet::new();
        for spec in manifest_specs {
            let name = dependency_name(spec);
            if !name.is_empty() {
                manifest.insert(name);
            }
        }

        let mut locked = HashSet::new();
        if let Some(path) = lock_path {
            if let Ok(Some(lock)) = load_lockfile_optional(path) {
                for spec in lock.dependencies {
                    let name = dependency_name(&spec);
                    if !name.is_empty() {
                        locked.insert(name);
                    }
                }
                for dep in lock.resolved {
                    let name = dep.name.trim();
                    if !name.is_empty() {
                        locked.insert(name.to_lowercase());
                    }
                }
            }
        }

        Self { manifest, locked }
    }

    fn inject(&self, args: &mut Value) {
        if let Value::Object(map) = args {
            if !self.manifest.is_empty() {
                map.insert(
                    "manifest_deps".into(),
                    serde_json::to_value(sorted_list(&self.manifest)).unwrap_or(Value::Null),
                );
            }
            if !self.locked.is_empty() {
                map.insert(
                    "locked_deps".into(),
                    serde_json::to_value(sorted_list(&self.locked)).unwrap_or(Value::Null),
                );
            }
        }
    }
}

fn sorted_list(values: &HashSet<String>) -> Vec<String> {
    let mut items: Vec<String> = values.iter().cloned().collect();
    items.sort();
    items
}

fn pxapp_path_from_request(request: &RunRequest) -> Option<PathBuf> {
    let entry = request.entry.as_ref()?;
    let path = PathBuf::from(entry);
    if !path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("pxapp"))
        .unwrap_or(false)
    {
        return None;
    }
    if path.exists() {
        Some(path)
    } else {
        None
    }
}

/// Runs the project's tests using a project-provided runner or pytest, with an
/// optional px fallback runner.
///
/// # Errors
/// Returns an error if the Python environment cannot be prepared or test execution fails.
pub fn test_project(ctx: &CommandContext, request: &TestRequest) -> Result<ExecutionOutcome> {
    test_project_outcome(ctx, request)
}

/// Executes a configured px run entry or script.
///
/// # Errors
/// Returns an error if the Python environment cannot be prepared or the entry fails to run.
pub fn run_project(ctx: &CommandContext, request: &RunRequest) -> Result<ExecutionOutcome> {
    run_project_outcome(ctx, request)
}

fn run_project_outcome(ctx: &CommandContext, request: &RunRequest) -> Result<ExecutionOutcome> {
    let strict = request.frozen || ctx.env_flag_enabled("CI");
    let interactive = request.interactive.unwrap_or_else(|| {
        !strict && std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
    });
    if let Some(bundle) = pxapp_path_from_request(request) {
        if request.at.is_some() {
            return Ok(ExecutionOutcome::user_error(
                "px run <bundle.pxapp> does not support --at",
                json!({
                    "code": "PX903",
                    "reason": "pxapp_at_ref_unsupported",
                    "path": bundle.display().to_string(),
                }),
            ));
        }
        return run_pxapp_bundle(ctx, &bundle, &request.args, interactive);
    }
    if let Some(at_ref) = &request.at {
        return run_project_at_ref(ctx, request, at_ref);
    }
    let target = request
        .entry
        .clone()
        .or_else(|| request.target.clone())
        .unwrap_or_default();

    let mut sandbox: Option<SandboxRunContext> = None;

    if !target.trim().is_empty() {
        if let Some(inline) = match detect_inline_script(&target) {
            Ok(result) => result,
            Err(outcome) => return Ok(outcome),
        } {
            let command_args = json!({
                "target": &target,
                "args": &request.args,
            });
            if request.sandbox {
                let snapshot = match manifest_snapshot() {
                    Ok(snapshot) => snapshot,
                    Err(err) => {
                        if is_missing_project_error(&err) {
                            return Ok(missing_project_outcome());
                        }
                        let msg = err.to_string();
                        if msg.contains("pyproject.toml not found") {
                            let root = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                            return Ok(missing_pyproject_outcome("run", &root));
                        }
                        return Err(err);
                    }
                };
                let state_report = match evaluate_project_state(ctx, &snapshot) {
                    Ok(report) => report,
                    Err(err) => {
                        return Ok(ExecutionOutcome::failure(
                            "failed to evaluate project state",
                            json!({ "error": err.to_string() }),
                        ))
                    }
                };
                let guard = match guard_for_execution(strict, &snapshot, &state_report, "run") {
                    Ok(guard) => guard,
                    Err(outcome) => return Ok(outcome),
                };
                if matches!(guard, crate::EnvGuard::AutoSync) {
                    if let Err(outcome) = python_context_with_mode(ctx, guard) {
                        return Ok(outcome);
                    }
                }
                match prepare_project_sandbox(ctx, &snapshot) {
                    Ok(sbx) => sandbox = Some(sbx),
                    Err(outcome) => return Ok(outcome),
                }
            }
            let mut outcome = match run_inline_script(
                ctx,
                sandbox.as_mut(),
                inline,
                &request.args,
                &command_args,
                interactive,
                strict,
            ) {
                Ok(outcome) => outcome,
                Err(outcome) => outcome,
            };
            if let Some(ref sbx) = sandbox {
                attach_sandbox_details(&mut outcome, sbx);
            }
            return Ok(outcome);
        }
    }

    if let Some(ws_ctx) = match prepare_workspace_run_context(ctx, strict, "run", request.sandbox) {
        Ok(result) => result,
        Err(outcome) => return Ok(outcome),
    } {
        if target.trim().is_empty() {
            return Ok(run_target_required_outcome());
        }
        if request.sandbox
            && strict
            && !matches!(
                ws_ctx.state.canonical,
                WorkspaceStateKind::Consistent | WorkspaceStateKind::InitializedEmpty
            )
        {
            return Ok(sandbox_workspace_env_inconsistent(
                &ws_ctx.workspace_root,
                &ws_ctx.state,
            ));
        }
        if request.sandbox {
            match prepare_workspace_sandbox(ctx, &ws_ctx) {
                Ok(sbx) => sandbox = Some(sbx),
                Err(outcome) => return Ok(outcome),
            }
        }
        let workdir = invocation_workdir(&ws_ctx.py_ctx.project_root);
        let deps = DependencyContext::from_sources(&ws_ctx.workspace_deps, Some(&ws_ctx.lock_path));
        let mut command_args = json!({
            "target": &target,
            "args": &request.args,
        });
        deps.inject(&mut command_args);
        let host_runner = HostCommandRunner::new(ctx);
        let sandbox_runner = match sandbox {
            Some(ref mut sbx) => {
                let runner = match sandbox_runner_for_context(&ws_ctx.py_ctx, sbx, &workdir) {
                    Ok(runner) => runner,
                    Err(outcome) => return Ok(outcome),
                };
                Some(runner)
            }
            None => None,
        };
        let runner: &dyn CommandRunner = match sandbox_runner.as_ref() {
            Some(runner) => runner,
            None => &host_runner,
        };
        let plan = plan_run_target(&ws_ctx.py_ctx, &ws_ctx.manifest_path, &target, &workdir)?;
        let mut outcome = match plan {
            RunTargetPlan::Script(path) => run_project_script(
                ctx,
                runner,
                &ws_ctx.py_ctx,
                &path,
                &request.args,
                &command_args,
                &workdir,
                interactive,
                if sandbox.is_some() {
                    "python"
                } else {
                    &ws_ctx.py_ctx.python
                },
            )?,
            RunTargetPlan::Executable(program) => run_executable(
                ctx,
                runner,
                &ws_ctx.py_ctx,
                &program,
                &request.args,
                &command_args,
                &workdir,
                interactive,
            )?,
        };
        attach_autosync_details(&mut outcome, ws_ctx.sync_report);
        if let Some(ref sbx) = sandbox {
            attach_sandbox_details(&mut outcome, sbx);
        }
        return Ok(outcome);
    }

    let snapshot = match manifest_snapshot() {
        Ok(snapshot) => snapshot,
        Err(err) => {
            if is_missing_project_error(&err) {
                return Ok(missing_project_outcome());
            }
            let msg = err.to_string();
            if msg.contains("pyproject.toml not found") {
                let root = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                return Ok(missing_pyproject_outcome("run", &root));
            }
            return Err(err);
        }
    };
    if target.trim().is_empty() {
        return Ok(run_target_required_outcome());
    }
    let state_report = match crate::state_guard::state_or_violation(ctx, &snapshot, "run") {
        Ok(report) => report,
        Err(outcome) => return Ok(outcome),
    };
    let guard = match guard_for_execution(strict, &snapshot, &state_report, "run") {
        Ok(guard) => guard,
        Err(outcome) => return Ok(outcome),
    };
    let (py_ctx, sync_report) = match python_context_with_mode(ctx, guard) {
        Ok(result) => result,
        Err(outcome) => return Ok(outcome),
    };
    if request.sandbox {
        match prepare_project_sandbox(ctx, &snapshot) {
            Ok(sbx) => sandbox = Some(sbx),
            Err(outcome) => return Ok(outcome),
        }
    }
    let manifest = py_ctx.project_root.join("pyproject.toml");
    let deps = DependencyContext::from_sources(&snapshot.requirements, Some(&snapshot.lock_path));
    let mut command_args = json!({
        "target": &target,
        "args": &request.args,
    });
    deps.inject(&mut command_args);
    let workdir = invocation_workdir(&py_ctx.project_root);
    let host_runner = HostCommandRunner::new(ctx);
    let sandbox_runner = match sandbox {
        Some(ref mut sbx) => {
            let runner = match sandbox_runner_for_context(&py_ctx, sbx, &workdir) {
                Ok(runner) => runner,
                Err(outcome) => return Ok(outcome),
            };
            Some(runner)
        }
        None => None,
    };
    let runner: &dyn CommandRunner = match sandbox_runner.as_ref() {
        Some(runner) => runner,
        None => &host_runner,
    };

    if target.trim().is_empty() {
        return Ok(run_target_required_outcome());
    }
    let plan = plan_run_target(&py_ctx, &manifest, &target, &workdir)?;
    let mut outcome = match plan {
        RunTargetPlan::Script(path) => run_project_script(
            ctx,
            runner,
            &py_ctx,
            &path,
            &request.args,
            &command_args,
            &workdir,
            interactive,
            if sandbox.is_some() {
                "python"
            } else {
                &py_ctx.python
            },
        )?,
        RunTargetPlan::Executable(program) => run_executable(
            ctx,
            runner,
            &py_ctx,
            &program,
            &request.args,
            &command_args,
            &workdir,
            interactive,
        )?,
    };
    attach_autosync_details(&mut outcome, sync_report);
    if let Some(ref sbx) = sandbox {
        attach_sandbox_details(&mut outcome, sbx);
    }
    Ok(outcome)
}

struct CommitRunContext {
    py_ctx: PythonContext,
    manifest_path: PathBuf,
    deps: DependencyContext,
    lock: px_domain::LockSnapshot,
    profile_oid: String,
    env_root: PathBuf,
    site_packages: Option<PathBuf>,
    _temp_guard: Option<TempDir>,
}

fn run_project_at_ref(
    ctx: &CommandContext,
    request: &RunRequest,
    git_ref: &str,
) -> Result<ExecutionOutcome> {
    let strict = true;
    let target = request
        .entry
        .clone()
        .or_else(|| request.target.clone())
        .unwrap_or_default();
    let interactive = request
        .interactive
        .unwrap_or_else(|| std::io::stdin().is_terminal() && std::io::stdout().is_terminal());
    let command_args = json!({
        "target": &target,
        "args": &request.args,
        "git_ref": git_ref,
    });

    let commit_ctx = match prepare_commit_python_context(ctx, git_ref) {
        Ok(value) => value,
        Err(outcome) => return Ok(outcome),
    };
    let mut sandbox: Option<SandboxRunContext> = None;
    let invocation_root = ctx.project_root().ok();
    let workdir = map_workdir(invocation_root.as_deref(), &commit_ctx.py_ctx.project_root);
    let mut command_args = command_args;
    commit_ctx.deps.inject(&mut command_args);

    if request.sandbox {
        match prepare_commit_sandbox(
            &commit_ctx.manifest_path,
            &commit_ctx.lock,
            &commit_ctx.profile_oid,
            &commit_ctx.env_root,
            commit_ctx.site_packages.as_deref(),
        ) {
            Ok(sbx) => sandbox = Some(sbx),
            Err(outcome) => return Ok(outcome),
        }
    }
    if target.trim().is_empty() {
        return Ok(run_target_required_outcome());
    }
    if let Some(inline) = match detect_inline_script(&target) {
        Ok(result) => result,
        Err(outcome) => return Ok(outcome),
    } {
        let mut outcome = match run_inline_script(
            ctx,
            sandbox.as_mut(),
            inline,
            &request.args,
            &command_args,
            interactive,
            strict,
        ) {
            Ok(outcome) => outcome,
            Err(outcome) => outcome,
        };
        if let Some(ref sbx) = sandbox {
            attach_sandbox_details(&mut outcome, sbx);
        }
        return Ok(outcome);
    }
    let plan = plan_run_target(
        &commit_ctx.py_ctx,
        &commit_ctx.manifest_path,
        &target,
        &workdir,
    )?;
    let host_runner = HostCommandRunner::new(ctx);
    let sandbox_runner = match sandbox {
        Some(ref mut sbx) => {
            let runner = match sandbox_runner_for_context(&commit_ctx.py_ctx, sbx, &workdir) {
                Ok(runner) => runner,
                Err(outcome) => return Ok(outcome),
            };
            Some(runner)
        }
        None => None,
    };
    let runner: &dyn CommandRunner = match sandbox_runner.as_ref() {
        Some(runner) => runner,
        None => &host_runner,
    };
    let outcome = match plan {
        RunTargetPlan::Script(path) => run_project_script(
            ctx,
            runner,
            &commit_ctx.py_ctx,
            &path,
            &request.args,
            &command_args,
            &workdir,
            interactive,
            if sandbox.is_some() {
                "python"
            } else {
                &commit_ctx.py_ctx.python
            },
        )?,
        RunTargetPlan::Executable(program) => run_executable(
            ctx,
            runner,
            &commit_ctx.py_ctx,
            &program,
            &request.args,
            &command_args,
            &workdir,
            interactive,
        )?,
    };
    let mut outcome = outcome;
    if let Some(ref sbx) = sandbox {
        attach_sandbox_details(&mut outcome, sbx);
    }
    Ok(outcome)
}

fn run_tests_at_ref(
    ctx: &CommandContext,
    request: &TestRequest,
    git_ref: &str,
) -> Result<ExecutionOutcome> {
    let commit_ctx = match prepare_commit_python_context(ctx, git_ref) {
        Ok(value) => value,
        Err(outcome) => return Ok(outcome),
    };
    let _stdlib_guard = commit_stdlib_guard(ctx, git_ref);
    let invocation_root = ctx.project_root().ok();
    let workdir = map_workdir(invocation_root.as_deref(), &commit_ctx.py_ctx.project_root);
    let mut sandbox: Option<SandboxRunContext> = None;
    if request.sandbox {
        match prepare_commit_sandbox(
            &commit_ctx.manifest_path,
            &commit_ctx.lock,
            &commit_ctx.profile_oid,
            &commit_ctx.env_root,
            commit_ctx.site_packages.as_deref(),
        ) {
            Ok(sbx) => sandbox = Some(sbx),
            Err(outcome) => return Ok(outcome),
        }
    }
    let host_runner = HostCommandRunner::new(ctx);
    let sandbox_runner = match sandbox {
        Some(ref mut sbx) => {
            let runner = match sandbox_runner_for_context(&commit_ctx.py_ctx, sbx, &workdir) {
                Ok(runner) => runner,
                Err(outcome) => return Ok(outcome),
            };
            Some(runner)
        }
        None => None,
    };
    let runner: &dyn CommandRunner = match sandbox_runner.as_ref() {
        Some(runner) => runner,
        None => &host_runner,
    };
    let mut outcome =
        run_tests_for_context(ctx, runner, &commit_ctx.py_ctx, request, None, &workdir)?;
    if let Some(ref sbx) = sandbox {
        attach_sandbox_details(&mut outcome, sbx);
    }
    Ok(outcome)
}

fn prepare_commit_python_context(
    ctx: &CommandContext,
    git_ref: &str,
) -> Result<CommitRunContext, ExecutionOutcome> {
    let repo_root = git_repo_root()?;
    let scope = crate::workspace::discover_workspace_scope().map_err(|err| {
        ExecutionOutcome::failure(
            "failed to detect workspace for --at",
            json!({ "error": err.to_string() }),
        )
    })?;
    if let Some(crate::workspace::WorkspaceScope::Member {
        workspace,
        member_root,
    }) = scope
    {
        return commit_workspace_context(
            ctx,
            git_ref,
            &repo_root,
            &workspace.config.root,
            &member_root,
        );
    }
    commit_project_context(ctx, git_ref, &repo_root)
}

fn commit_project_context(
    ctx: &CommandContext,
    git_ref: &str,
    repo_root: &Path,
) -> Result<CommitRunContext, ExecutionOutcome> {
    let project_root = match ctx.project_root() {
        Ok(root) => root,
        Err(err) => {
            if is_missing_project_error(&err) {
                return Err(missing_project_outcome());
            }
            return Err(ExecutionOutcome::failure(
                "failed to resolve project root",
                json!({ "error": err.to_string() }),
            ));
        }
    };
    let rel_root = match project_root.strip_prefix(repo_root) {
        Ok(rel) => rel.to_path_buf(),
        Err(_) => {
            return Err(ExecutionOutcome::user_error(
                "px --at requires running inside a git repository",
                json!({
                    "reason": "project_outside_repo",
                    "project_root": project_root.display().to_string(),
                    "repo_root": repo_root.display().to_string(),
                }),
            ))
        }
    };
    let archive = materialize_ref_tree(repo_root, git_ref)?;
    let project_root_at_ref = archive.path().join(&rel_root);
    let manifest_rel = rel_root.join("pyproject.toml");
    let manifest_path = project_root_at_ref.join("pyproject.toml");
    let manifest_contents = fs::read_to_string(&manifest_path).map_err(|err| {
        ExecutionOutcome::user_error(
            "pyproject.toml not found at the requested git ref",
            json!({
                "git_ref": git_ref,
                "path": manifest_rel.display().to_string(),
                "reason": "pyproject_missing_at_ref",
                "error": err.to_string(),
            }),
        )
    })?;
    let manifest_doc: toml_edit::DocumentMut = manifest_contents
        .parse::<toml_edit::DocumentMut>()
        .map_err(|err| {
            ExecutionOutcome::failure(
                "failed to parse pyproject.toml from git ref",
                json!({
                    "git_ref": git_ref,
                    "path": manifest_rel.display().to_string(),
                    "error": err.to_string(),
                }),
            )
        })?;
    if !manifest_has_px(&manifest_doc) {
        return Err(ExecutionOutcome::user_error(
            "px project not found at the requested git ref",
            json!({
                "git_ref": git_ref,
                "path": manifest_rel.display().to_string(),
                "reason": "missing_px_metadata",
            }),
        ));
    }
    let snapshot = px_domain::ProjectSnapshot::from_document(
        &project_root_at_ref,
        &manifest_path,
        manifest_doc,
    )
    .map_err(|err| {
        ExecutionOutcome::failure(
            "failed to load pyproject.toml from git ref",
            json!({
                "git_ref": git_ref,
                "path": manifest_rel.display().to_string(),
                "error": err.to_string(),
            }),
        )
    })?;
    let lock_rel = rel_root.join("px.lock");
    let lock_path = project_root_at_ref.join("px.lock");
    let lock_contents = fs::read_to_string(&lock_path).map_err(|err| {
        ExecutionOutcome::user_error(
            "px.lock not found at the requested git ref",
            json!({
                "git_ref": git_ref,
                "path": lock_rel.display().to_string(),
                "reason": "px_lock_missing_at_ref",
                "error": err.to_string(),
            }),
        )
    })?;
    let lock = px_domain::parse_lockfile(&lock_contents).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to parse px.lock from git ref",
            json!({
                "git_ref": git_ref,
                "path": lock_rel.display().to_string(),
                "error": err.to_string(),
                "reason": "invalid_lock_at_ref",
            }),
        )
    })?;
    let marker_env = marker_env_for_snapshot(&snapshot);
    let lock_id = validate_lock_for_ref(
        &snapshot,
        &lock,
        &lock_contents,
        git_ref,
        &lock_rel,
        marker_env.as_ref(),
    )?;
    let runtime = detect_runtime_metadata(ctx, &snapshot).map_err(|err| {
        install_error_outcome(err, "python runtime unavailable for git-ref execution")
    })?;
    let owner_id =
        project_env_owner_id(&project_root, &lock_id, &runtime.version).map_err(|err| {
            ExecutionOutcome::failure(
                "failed to compute project environment identity",
                json!({ "error": err.to_string() }),
            )
        })?;
    let env_owner = OwnerId {
        owner_type: OwnerType::ProjectEnv,
        owner_id,
    };
    let cas_profile =
        ensure_profile_env(ctx, &snapshot, &lock, &runtime, &env_owner).map_err(|err| {
            install_error_outcome(err, "failed to prepare environment for git-ref execution")
        })?;
    let env_root = cas_profile.env_path.clone();
    let site_packages = discover_site_packages(&env_root);
    let paths = build_pythonpath(ctx.fs(), &project_root_at_ref, Some(env_root.clone())).map_err(
        |err| {
            ExecutionOutcome::failure(
                "failed to assemble PYTHONPATH for git-ref execution",
                json!({ "error": err.to_string() }),
            )
        },
    )?;
    let runtime_path = cas_profile.runtime_path.display().to_string();
    let python = select_python_from_site(&paths.site_bin, &runtime_path, &runtime.version);
    let deps = DependencyContext::from_sources(&snapshot.requirements, Some(&lock_path));
    Ok(CommitRunContext {
        py_ctx: PythonContext {
            project_root: project_root_at_ref,
            project_name: snapshot.name.clone(),
            python,
            pythonpath: paths.pythonpath,
            allowed_paths: paths.allowed_paths,
            site_bin: paths.site_bin,
            pep582_bin: paths.pep582_bin,
            px_options: snapshot.px_options.clone(),
        },
        manifest_path,
        deps,
        lock: lock.clone(),
        profile_oid: cas_profile.profile_oid.clone(),
        env_root,
        site_packages,
        _temp_guard: Some(archive),
    })
}

fn commit_workspace_context(
    ctx: &CommandContext,
    git_ref: &str,
    repo_root: &Path,
    workspace_root: &Path,
    member_root: &Path,
) -> Result<CommitRunContext, ExecutionOutcome> {
    let workspace_rel = match workspace_root.strip_prefix(repo_root) {
        Ok(rel) => rel.to_path_buf(),
        Err(_) => {
            return Err(ExecutionOutcome::user_error(
                "px --at requires running inside a git repository",
                json!({
                    "reason": "workspace_outside_repo",
                    "workspace_root": workspace_root.display().to_string(),
                    "repo_root": repo_root.display().to_string(),
                }),
            ))
        }
    };
    let archive = materialize_ref_tree(repo_root, git_ref)?;
    let archive_root = archive.path().to_path_buf();
    let workspace_root_at_ref = archive_root.join(&workspace_rel);
    let workspace_manifest_rel = workspace_rel.join("pyproject.toml");
    let workspace_manifest_path = workspace_root_at_ref.join("pyproject.toml");
    let workspace_manifest = fs::read_to_string(&workspace_manifest_path).map_err(|err| {
        ExecutionOutcome::user_error(
            "workspace manifest not found at the requested git ref",
            json!({
                "git_ref": git_ref,
                "path": workspace_manifest_rel.display().to_string(),
                "reason": "workspace_manifest_missing_at_ref",
                "error": err.to_string(),
            }),
        )
    })?;
    let manifest_path = workspace_manifest_path.clone();
    let workspace_doc: toml_edit::DocumentMut = workspace_manifest
        .parse::<toml_edit::DocumentMut>()
        .map_err(|err| {
            ExecutionOutcome::failure(
                "failed to parse workspace manifest from git ref",
                json!({
                    "git_ref": git_ref,
                    "path": workspace_manifest_rel.display().to_string(),
                    "error": err.to_string(),
                }),
            )
        })?;
    if !px_domain::workspace::manifest_has_workspace(&workspace_doc) {
        return Err(ExecutionOutcome::user_error(
            "px workspace not found at the requested git ref",
            json!({
                "git_ref": git_ref,
                "path": workspace_manifest_rel.display().to_string(),
                "reason": "missing_workspace_metadata",
            }),
        ));
    }
    let mut workspace_config = px_domain::workspace::workspace_config_from_doc(
        &workspace_root_at_ref,
        &manifest_path,
        &workspace_doc,
    )
    .map_err(|err| {
        ExecutionOutcome::failure(
            "failed to load workspace manifest from git ref",
            json!({
                "git_ref": git_ref,
                "path": workspace_manifest_rel.display().to_string(),
                "error": err.to_string(),
            }),
        )
    })?;
    let member_rel = member_root
        .strip_prefix(workspace_root)
        .unwrap_or(member_root)
        .to_path_buf();
    let member_root_at_ref = workspace_root_at_ref.join(&member_rel);
    if !workspace_config
        .members
        .iter()
        .any(|member| member == &member_rel)
    {
        return Err(ExecutionOutcome::user_error(
            "current directory is not a workspace member at the requested git ref",
            json!({
                "git_ref": git_ref,
                "member": member_rel.display().to_string(),
                "reason": "workspace_member_not_found",
            }),
        ));
    }

    let mut members = Vec::new();
    for rel in &workspace_config.members {
        let abs_root = workspace_root_at_ref.join(rel);
        let member_manifest_rel = workspace_rel.join(rel).join("pyproject.toml");
        let manifest_path = abs_root.join("pyproject.toml");
        let contents = fs::read_to_string(&manifest_path).map_err(|err| {
            ExecutionOutcome::user_error(
                "workspace member manifest not found at git ref",
                json!({
                    "git_ref": git_ref,
                    "path": member_manifest_rel.display().to_string(),
                    "reason": "member_manifest_missing_at_ref",
                    "error": err.to_string(),
                }),
            )
        })?;
        let doc: toml_edit::DocumentMut =
            contents.parse::<toml_edit::DocumentMut>().map_err(|err| {
                ExecutionOutcome::failure(
                    "failed to parse workspace member manifest from git ref",
                    json!({
                        "git_ref": git_ref,
                        "path": member_manifest_rel.display().to_string(),
                        "error": err.to_string(),
                    }),
                )
            })?;
        if !manifest_has_px(&doc) {
            return Err(ExecutionOutcome::user_error(
                "px project not found in workspace member at git ref",
                json!({
                    "git_ref": git_ref,
                    "path": member_manifest_rel.display().to_string(),
                    "reason": "missing_px_metadata",
                }),
            ));
        }
        let snapshot = px_domain::ProjectSnapshot::from_document(&abs_root, &manifest_path, doc)
            .map_err(|err| {
                ExecutionOutcome::failure(
                    "failed to load workspace member from git ref",
                    json!({
                        "git_ref": git_ref,
                        "path": member_manifest_rel.display().to_string(),
                        "error": err.to_string(),
                    }),
                )
            })?;
        let rel_path = abs_root
            .strip_prefix(&workspace_root_at_ref)
            .unwrap_or(&abs_root)
            .display()
            .to_string();
        members.push(crate::workspace::WorkspaceMember {
            rel_path,
            root: abs_root,
            snapshot,
        });
    }
    if workspace_config.name.is_none() {
        workspace_config.name = workspace_root
            .file_name()
            .and_then(|s| s.to_str())
            .map(std::string::ToString::to_string);
    }
    let member_snapshot = members
        .iter()
        .find(|member| member.root == member_root_at_ref)
        .map(|member| member.snapshot.clone());
    let python_requirement = crate::workspace::derive_workspace_python(&workspace_config, &members)
        .map_err(|err| {
            ExecutionOutcome::failure(
                "workspace python requirement is inconsistent at git ref",
                json!({
                    "git_ref": git_ref,
                    "error": err.to_string(),
                }),
            )
        })?;
    let manifest_fingerprint = px_domain::workspace::workspace_manifest_fingerprint(
        &workspace_config,
        &members
            .iter()
            .map(|m| m.snapshot.clone())
            .collect::<Vec<_>>(),
    )
    .map_err(|err| {
        ExecutionOutcome::failure(
            "failed to fingerprint workspace manifest from git ref",
            json!({
                "git_ref": git_ref,
                "error": err.to_string(),
            }),
        )
    })?;
    let mut dependencies = Vec::new();
    for member in &members {
        dependencies.extend(member.snapshot.requirements.clone());
    }
    dependencies.retain(|dep| !dep.trim().is_empty());
    let workspace_name = workspace_config
        .name
        .clone()
        .or_else(|| {
            workspace_root
                .file_name()
                .and_then(|s| s.to_str())
                .map(std::string::ToString::to_string)
        })
        .or_else(|| {
            workspace_root_at_ref
                .file_name()
                .and_then(|s| s.to_str())
                .map(std::string::ToString::to_string)
        })
        .unwrap_or_else(|| "workspace".to_string());
    let px_options = px_domain::project::manifest::px_options_from_doc(&workspace_doc);
    let workspace_snapshot = crate::workspace::WorkspaceSnapshot {
        config: workspace_config.clone(),
        members,
        manifest_fingerprint,
        lock_path: workspace_root_at_ref.join("px.workspace.lock"),
        python_requirement,
        python_override: workspace_config.python.clone(),
        dependencies,
        name: workspace_name,
        px_options,
    };

    let lock_rel = workspace_rel.join("px.workspace.lock");
    let lock_path = workspace_root_at_ref.join("px.workspace.lock");
    let lock_contents = fs::read_to_string(&lock_path).map_err(|err| {
        ExecutionOutcome::user_error(
            "workspace lockfile not found at the requested git ref",
            json!({
                "git_ref": git_ref,
                "path": lock_rel.display().to_string(),
                "reason": "workspace_lock_missing_at_ref",
                "error": err.to_string(),
            }),
        )
    })?;
    let lock = px_domain::parse_lockfile(&lock_contents).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to parse px.workspace.lock from git ref",
            json!({
                "git_ref": git_ref,
                "path": lock_rel.display().to_string(),
                "error": err.to_string(),
                "reason": "invalid_lock_at_ref",
            }),
        )
    })?;
    let marker_env = ctx.marker_environment().ok();
    let lock_id = validate_lock_for_ref(
        &workspace_snapshot.lock_snapshot(),
        &lock,
        &lock_contents,
        git_ref,
        &lock_rel,
        marker_env.as_ref(),
    )?;
    let runtime =
        detect_runtime_metadata(ctx, &workspace_snapshot.lock_snapshot()).map_err(|err| {
            install_error_outcome(
                err,
                "python runtime unavailable for git-ref workspace execution",
            )
        })?;
    let owner_id =
        workspace_env_owner_id(workspace_root, &lock_id, &runtime.version).map_err(|err| {
            ExecutionOutcome::failure(
                "failed to compute workspace environment identity",
                json!({ "error": err.to_string() }),
            )
        })?;
    let env_owner = OwnerId {
        owner_type: OwnerType::WorkspaceEnv,
        owner_id,
    };
    let cas_profile = ensure_profile_env(
        ctx,
        &workspace_snapshot.lock_snapshot(),
        &lock,
        &runtime,
        &env_owner,
    )
    .map_err(|err| {
        install_error_outcome(
            err,
            "failed to prepare workspace environment for git-ref execution",
        )
    })?;
    let env_root = cas_profile.env_path.clone();
    let site_packages = discover_site_packages(&env_root);
    let paths =
        build_pythonpath(ctx.fs(), &member_root_at_ref, Some(env_root.clone())).map_err(|err| {
            ExecutionOutcome::failure(
                "failed to assemble PYTHONPATH for git-ref execution",
                json!({ "error": err.to_string() }),
            )
        })?;
    let mut combined = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut push_unique = |path: PathBuf| {
        if seen.insert(path.clone()) {
            combined.push(path);
        }
    };
    let current_src = member_root_at_ref.join("src");
    if current_src.exists() {
        push_unique(current_src);
    }
    push_unique(member_root_at_ref.to_path_buf());
    for member in &workspace_snapshot.config.members {
        let abs = workspace_snapshot.config.root.join(member);
        let src = abs.join("src");
        if src.exists() {
            push_unique(src);
        }
        push_unique(abs);
    }
    for path in &paths.allowed_paths {
        push_unique(path.clone());
    }
    let pythonpath = env::join_paths(&combined)
        .map_err(|err| {
            ExecutionOutcome::failure(
                "failed to assemble PYTHONPATH for git-ref execution",
                json!({ "error": err.to_string() }),
            )
        })?
        .into_string()
        .map_err(|_| {
            ExecutionOutcome::failure(
                "failed to assemble PYTHONPATH for git-ref execution",
                json!({ "error": "contains non-utf8 data" }),
            )
        })?;
    let runtime_path = cas_profile.runtime_path.display().to_string();
    let python = select_python_from_site(&paths.site_bin, &runtime_path, &runtime.version);
    let px_options = member_snapshot
        .as_ref()
        .map(|member| member.px_options.clone())
        .unwrap_or_default();
    let project_name = member_snapshot
        .as_ref()
        .map(|member| member.name.clone())
        .unwrap_or_else(|| {
            member_root_at_ref
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_string()
        });
    let deps = DependencyContext::from_sources(&workspace_snapshot.dependencies, Some(&lock_path));
    Ok(CommitRunContext {
        py_ctx: PythonContext {
            project_root: member_root_at_ref.clone(),
            project_name,
            python,
            pythonpath,
            allowed_paths: combined,
            site_bin: paths.site_bin,
            pep582_bin: paths.pep582_bin,
            px_options,
        },
        manifest_path: member_root_at_ref.join("pyproject.toml"),
        deps,
        lock: lock.clone(),
        profile_oid: cas_profile.profile_oid.clone(),
        env_root,
        site_packages,
        _temp_guard: Some(archive),
    })
}

fn validate_lock_for_ref(
    snapshot: &px_domain::ProjectSnapshot,
    lock: &px_domain::LockSnapshot,
    contents: &str,
    git_ref: &str,
    lock_rel: &Path,
    marker_env: Option<&pep508_rs::MarkerEnvironment>,
) -> Result<String, ExecutionOutcome> {
    if lock.manifest_fingerprint.is_none() {
        return Err(ExecutionOutcome::user_error(
            "lockfile at git ref is missing a manifest fingerprint",
            json!({
                "git_ref": git_ref,
                "lock_path": lock_rel.display().to_string(),
                "reason": "lock_missing_fingerprint_at_ref",
            }),
        ));
    }
    let drift = detect_lock_drift(snapshot, lock, marker_env);
    let manifest_match = lock
        .manifest_fingerprint
        .as_deref()
        .is_some_and(|fp| fp == snapshot.manifest_fingerprint);
    if !drift.is_empty() || !manifest_match {
        let mut details = json!({
            "git_ref": git_ref,
            "lock_path": lock_rel.display().to_string(),
            "reason": "lock_drift_at_ref",
            "manifest_fingerprint": snapshot.manifest_fingerprint,
            "lock_fingerprint": lock.manifest_fingerprint,
        });
        if !drift.is_empty() {
            details["drift"] = json!(drift);
        }
        return Err(ExecutionOutcome::user_error(
            "px lockfile is out of sync with the manifest at that git ref",
            details,
        ));
    }

    Ok(lock
        .lock_id
        .clone()
        .unwrap_or_else(|| compute_lock_hash_bytes(contents.as_bytes())))
}

fn git_repo_root() -> Result<PathBuf, ExecutionOutcome> {
    let output = Command::new("git")
        .arg("rev-parse")
        .arg("--show-toplevel")
        .output();
    match output {
        Ok(output) if output.status.success() => {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            Ok(PathBuf::from(path))
        }
        Ok(output) => Err(ExecutionOutcome::user_error(
            "px --at requires a git repository",
            json!({
                "reason": "not_a_git_repo",
                "stderr": String::from_utf8_lossy(&output.stderr),
            }),
        )),
        Err(err) => Err(ExecutionOutcome::failure(
            "failed to invoke git",
            json!({ "error": err.to_string() }),
        )),
    }
}

fn materialize_ref_tree(repo_root: &Path, git_ref: &str) -> Result<TempDir, ExecutionOutcome> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("archive")
        .arg(git_ref)
        .output();
    match output {
        Ok(output) if output.status.success() => {
            let temp = tempfile::tempdir().map_err(|err| {
                ExecutionOutcome::failure(
                    "failed to create temp directory for git-ref execution",
                    json!({ "error": err.to_string() }),
                )
            })?;
            Archive::new(Cursor::new(output.stdout))
                .unpack(temp.path())
                .map_err(|err| {
                    ExecutionOutcome::failure(
                        "failed to extract git ref for --at execution",
                        json!({
                            "git_ref": git_ref,
                            "error": err.to_string(),
                        }),
                    )
                })?;
            populate_submodules(repo_root, git_ref, temp.path())?;
            restore_lfs_pointers(repo_root, git_ref, temp.path())?;
            Ok(temp)
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let reason = if stderr.contains("not a valid object name")
                || stderr.contains("unknown revision")
                || stderr.contains("bad revision")
            {
                "invalid_git_ref"
            } else {
                "archive_failed"
            };
            Err(ExecutionOutcome::user_error(
                format!("failed to read files from git ref {git_ref}"),
                json!({
                    "git_ref": git_ref,
                    "stderr": stderr,
                    "reason": reason,
                }),
            ))
        }
        Err(err) => Err(ExecutionOutcome::failure(
            "failed to invoke git",
            json!({ "error": err.to_string() }),
        )),
    }
}

fn populate_submodules(
    repo_root: &Path,
    git_ref: &str,
    dest_root: &Path,
) -> Result<(), ExecutionOutcome> {
    let submodules = list_submodules(repo_root, git_ref)?;
    if submodules.is_empty() {
        return Ok(());
    }
    let mut missing = Vec::new();
    for (path, sha) in submodules {
        let dest = dest_root.join(&path);
        let worktree_path = repo_root.join(&path);
        let mut reason = None;
        if !worktree_path.exists() {
            reason = Some("submodule not checked out in working tree".to_string());
        } else if !worktree_path.is_dir() {
            reason = Some("submodule path is not a directory in working tree".to_string());
        }
        if reason.is_none() {
            let commit = Command::new("git")
                .arg("-C")
                .arg(&worktree_path)
                .arg("rev-parse")
                .arg("HEAD")
                .output();
            match commit {
                Ok(output) if output.status.success() => {
                    let found = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    if found != sha {
                        reason = Some(format!("submodule checked out at {found}, expected {sha}"));
                    }
                }
                Ok(output) => {
                    reason = Some(format!(
                        "failed to read submodule commit: {}",
                        String::from_utf8_lossy(&output.stderr)
                    ));
                }
                Err(err) => {
                    reason = Some(format!("failed to invoke git for submodule: {err}"));
                }
            }
        }
        if let Some(reason) = reason {
            missing.push(json!({ "path": path.display().to_string(), "reason": reason }));
            continue;
        }
        if let Err(err) = copy_tree(&worktree_path, &dest) {
            missing.push(json!({
                "path": path.display().to_string(),
                "reason": format!("failed to copy submodule: {err}"),
            }));
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(ExecutionOutcome::user_error(
            "submodules from the requested git ref are not available",
            json!({
                "git_ref": git_ref,
                "missing_submodules": missing,
                "hint": "run `git submodule update --init --recursive` to populate them, then retry"
            }),
        ))
    }
}

fn list_submodules(
    repo_root: &Path,
    git_ref: &str,
) -> Result<Vec<(PathBuf, String)>, ExecutionOutcome> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("ls-tree")
        .arg("-rz")
        .arg(git_ref)
        .output();
    let output = match output {
        Ok(output) => output,
        Err(err) => {
            return Err(ExecutionOutcome::failure(
                "failed to invoke git",
                json!({ "error": err.to_string() }),
            ))
        }
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ExecutionOutcome::user_error(
            "failed to list files at git ref",
            json!({
                "git_ref": git_ref,
                "stderr": stderr,
                "reason": "git_ls_tree_failed",
            }),
        ));
    }
    let mut results = Vec::new();
    for record in output.stdout.split(|b| *b == 0) {
        if record.is_empty() {
            continue;
        }
        let Ok(text) = std::str::from_utf8(record) else {
            continue;
        };
        let mut parts = text.splitn(2, '\t');
        let Some(meta) = parts.next() else { continue };
        let Some(path) = parts.next() else { continue };
        let mut fields = meta.split_whitespace();
        let _mode = fields.next();
        let Some(kind) = fields.next() else { continue };
        let Some(sha) = fields.next() else { continue };
        if kind == "commit" {
            results.push((PathBuf::from(path), sha.to_string()));
        }
    }
    Ok(results)
}

fn restore_lfs_pointers(
    repo_root: &Path,
    git_ref: &str,
    root: &Path,
) -> Result<(), ExecutionOutcome> {
    let mut missing = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) => {
                missing.push(json!({
                    "path": dir.display().to_string(),
                    "reason": format!("failed to read directory: {err}"),
                }));
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                stack.push(path);
                continue;
            }
            if !meta.is_file() {
                continue;
            }
            let Ok(bytes) = fs::read(&path) else { continue };
            if !is_lfs_pointer(&bytes) {
                continue;
            }
            match smudge_lfs_pointer(repo_root, &bytes) {
                Ok(contents) => {
                    if let Err(err) = fs::write(&path, contents) {
                        missing.push(json!({
                            "path": path.display().to_string(),
                            "reason": format!("failed to write LFS content: {err}"),
                        }));
                    }
                }
                Err(err) => {
                    missing.push(json!({
                        "path": path.display().to_string(),
                        "reason": err,
                    }));
                }
            }
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(ExecutionOutcome::user_error(
            "git LFS content for the requested ref is unavailable",
            json!({
                "git_ref": git_ref,
                "missing_lfs_objects": missing,
                "hint": "ensure git LFS is installed and fetchable, then retry"
            }),
        ))
    }
}

fn smudge_lfs_pointer(repo_root: &Path, pointer: &[u8]) -> std::result::Result<Vec<u8>, String> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("lfs")
        .arg("smudge")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("failed to invoke git-lfs smudge: {err}"))?;
    if let Some(stdin) = child.stdin.as_mut() {
        if let Err(err) = stdin.write_all(pointer) {
            return Err(format!("failed to write LFS pointer to smudge: {err}"));
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|err| format!("failed to read git-lfs smudge output: {err}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(output.stdout)
}

fn is_lfs_pointer(bytes: &[u8]) -> bool {
    let prefix = b"version https://git-lfs.github.com/spec/v1";
    bytes.starts_with(prefix)
}

fn copy_tree(src: &Path, dest: &Path) -> anyhow::Result<()> {
    if dest.exists() {
        fs::remove_dir_all(dest).ok();
    }
    for entry in walkdir::WalkDir::new(src) {
        let entry = entry?;
        let rel = match entry.path().strip_prefix(src) {
            Ok(rel) => rel,
            Err(_) => continue,
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        if rel.components().any(|c| c.as_os_str() == ".git") {
            // Skip .git contents to avoid nested repository metadata.
            continue;
        }
        let target = dest.join(rel);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)?;
            continue;
        }
        if entry.file_type().is_symlink() {
            let link = fs::read_link(entry.path())?;
            create_symlink(&link, &target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(entry.path(), &target)?;
    }
    Ok(())
}

#[cfg(unix)]
fn create_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn create_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    use std::os::windows::fs as win_fs;
    let metadata = fs::metadata(target)?;
    if metadata.is_dir() {
        win_fs::symlink_dir(target, link)
    } else {
        win_fs::symlink_file(target, link)
    }
}

fn manifest_has_px(doc: &toml_edit::DocumentMut) -> bool {
    doc.get("tool")
        .and_then(toml_edit::Item::as_table)
        .and_then(|tool| tool.get("px"))
        .is_some()
}

fn sanitize_ref_for_path(git_ref: &str) -> String {
    git_ref
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

struct EnvVarGuard {
    key: &'static str,
    previous: Option<String>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: String) -> Self {
        let previous = std::env::var(key).ok();
        std::env::set_var(key, value);
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(prev) => std::env::set_var(self.key, prev),
            None => std::env::remove_var(self.key),
        }
    }
}

fn commit_stdlib_guard(ctx: &CommandContext, git_ref: &str) -> Option<EnvVarGuard> {
    if std::env::var("PX_STDLIB_STAGING_ROOT").is_ok() {
        return None;
    }
    let root = ctx
        .cache()
        .path
        .join("stdlib-tests")
        .join(sanitize_ref_for_path(git_ref));
    Some(EnvVarGuard::set(
        "PX_STDLIB_STAGING_ROOT",
        root.display().to_string(),
    ))
}

fn install_error_outcome(err: anyhow::Error, context: &str) -> ExecutionOutcome {
    match err.downcast::<crate::InstallUserError>() {
        Ok(user) => {
            ExecutionOutcome::user_error(user.message().to_string(), user.details().clone())
        }
        Err(other) => ExecutionOutcome::failure(context, json!({ "error": other.to_string() })),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_pytest_runner(
    ctx: &CommandContext,
    runner: &dyn CommandRunner,
    py_ctx: &PythonContext,
    envs: EnvPairs,
    test_args: &[String],
    stream_runner: bool,
    allow_builtin_fallback: bool,
    workdir: &Path,
) -> Result<ExecutionOutcome> {
    let reporter = test_reporter_from_env();
    let (envs, pytest_cmd) = build_pytest_invocation(ctx, py_ctx, envs, test_args, reporter)?;
    let output = run_python_command(runner, py_ctx, &pytest_cmd, &envs, stream_runner, workdir)?;
    if output.code == 0 {
        let mut outcome = test_success("pytest", output, stream_runner, test_args);
        if let TestReporter::Px = reporter {
            mark_reporter_rendered(&mut outcome);
        }
        return Ok(outcome);
    }
    if missing_pytest(&output.stderr) {
        if ctx.config().test.fallback_builtin || allow_builtin_fallback {
            return run_builtin_tests(ctx, runner, py_ctx, envs, stream_runner, workdir);
        }
        return Ok(missing_pytest_outcome(output, test_args));
    }
    let mut outcome = test_failure("pytest", output, stream_runner, test_args);
    if let TestReporter::Px = reporter {
        mark_reporter_rendered(&mut outcome);
    }
    Ok(outcome)
}

fn mark_reporter_rendered(outcome: &mut ExecutionOutcome) {
    match &mut outcome.details {
        Value::Object(map) => {
            map.insert("reporter_rendered".into(), Value::Bool(true));
        }
        Value::Null => {
            outcome.details = json!({ "reporter_rendered": true });
        }
        other => {
            let prev = other.take();
            outcome.details = json!({ "value": prev, "reporter_rendered": true });
        }
    }
}

fn build_pytest_invocation(
    ctx: &CommandContext,
    py_ctx: &PythonContext,
    mut envs: EnvPairs,
    test_args: &[String],
    reporter: TestReporter,
) -> Result<(EnvPairs, Vec<String>)> {
    let mut defaults = default_pytest_flags(reporter);
    if let TestReporter::Px = reporter {
        let plugin_path = ensure_px_pytest_plugin(ctx, py_ctx)?;
        let plugin_dir = plugin_path
            .parent()
            .unwrap_or(py_ctx.project_root.as_path());
        append_pythonpath(&mut envs, plugin_dir);
        append_allowed_paths(&mut envs, plugin_dir);
        defaults.extend_from_slice(&["-p".to_string(), "px_pytest_plugin".to_string()]);
    }
    let pytest_cmd = build_pytest_command_with_defaults(&py_ctx.project_root, test_args, &defaults);
    Ok((envs, pytest_cmd))
}

fn default_pytest_flags(reporter: TestReporter) -> Vec<String> {
    let mut flags = vec![
        "--color=yes".to_string(),
        "--tb=short".to_string(),
        "--ignore=.px".to_string(),
    ];
    if matches!(reporter, TestReporter::Px | TestReporter::Pytest) {
        flags.push("-q".to_string());
    }
    flags
}

fn test_reporter_from_env() -> TestReporter {
    match std::env::var("PX_TEST_REPORTER")
        .ok()
        .as_deref()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("pytest") => TestReporter::Pytest,
        Some("px") | None => TestReporter::Px,
        _ => TestReporter::Px,
    }
}

fn append_pythonpath(envs: &mut EnvPairs, plugin_dir: &Path) {
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

fn append_allowed_paths(envs: &mut EnvPairs, path: &Path) {
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

fn ensure_px_pytest_plugin(ctx: &CommandContext, py_ctx: &PythonContext) -> Result<PathBuf> {
    let plugin_dir = py_ctx.project_root.join(".px").join("plugins");
    ctx.fs()
        .create_dir_all(&plugin_dir)
        .context("creating px plugin dir")?;
    let plugin_path = plugin_dir.join("px_pytest_plugin.py");
    ctx.fs()
        .write(&plugin_path, PX_PYTEST_PLUGIN.as_bytes())
        .context("writing pytest reporter plugin")?;
    Ok(plugin_path)
}

fn run_python_command(
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

fn merged_pythonpath(envs: &[(String, String)]) -> Option<String> {
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

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_project_script(
    core_ctx: &CommandContext,
    runner: &dyn CommandRunner,
    py_ctx: &PythonContext,
    script: &Path,
    extra_args: &[String],
    command_args: &Value,
    workdir: &Path,
    interactive: bool,
    program: &str,
) -> Result<ExecutionOutcome> {
    let (envs, _) = build_env_with_preflight(core_ctx, py_ctx, command_args)?;
    let mut args = Vec::with_capacity(extra_args.len() + 1);
    args.push(script.display().to_string());
    args.extend(extra_args.iter().cloned());
    let output = if interactive {
        runner.run_command_passthrough(program, &args, &envs, workdir)?
    } else {
        runner.run_command(program, &args, &envs, workdir)?
    };
    let details = json!({
        "mode": "script",
        "script": script.display().to_string(),
        "args": extra_args,
        "interactive": interactive,
    });
    Ok(outcome_from_output(
        "run",
        &script.display().to_string(),
        &output,
        "px run",
        Some(details),
    ))
}

#[allow(clippy::too_many_arguments)]
fn run_executable(
    core_ctx: &CommandContext,
    runner: &dyn CommandRunner,
    py_ctx: &PythonContext,
    program: &str,
    extra_args: &[String],
    command_args: &Value,
    workdir: &Path,
    interactive: bool,
) -> Result<ExecutionOutcome> {
    if let Some(subcommand) = mutating_pip_invocation(program, extra_args, py_ctx) {
        return Ok(pip_mutation_outcome(program, &subcommand, extra_args));
    }
    let (mut envs, _) = build_env_with_preflight(core_ctx, py_ctx, command_args)?;
    let uses_px_python = program_matches_python(program, py_ctx);
    let needs_stdin = uses_px_python && extra_args.first().map(|arg| arg == "-").unwrap_or(false);
    if !uses_px_python {
        envs.retain(|(key, _)| key != "PX_PYTHON");
    }
    let interactive = interactive || needs_stdin;
    let output = if needs_stdin {
        runner.run_command_with_stdin(program, extra_args, &envs, workdir, true)?
    } else if interactive {
        runner.run_command_passthrough(program, extra_args, &envs, workdir)?
    } else {
        runner.run_command(program, extra_args, &envs, workdir)?
    };
    let mut details = json!({
        "mode": if uses_px_python { "passthrough" } else { "executable" },
        "program": program,
        "args": extra_args,
        "interactive": interactive,
    });
    if uses_px_python {
        details["uses_px_python"] = Value::Bool(true);
    }
    Ok(outcome_from_output(
        "run",
        program,
        &output,
        "px run",
        Some(details),
    ))
}

fn program_matches_python(program: &str, py_ctx: &PythonContext) -> bool {
    let program_path = Path::new(program);
    let python_path = Path::new(&py_ctx.python);
    program_path == python_path
        || program_path
            .file_name()
            .and_then(|p| python_path.file_name().filter(|q| q == &p))
            .is_some()
}

fn mutating_pip_invocation(
    program: &str,
    args: &[String],
    py_ctx: &PythonContext,
) -> Option<String> {
    let pip_args = pip_args_for_invocation(program, args, py_ctx)?;
    let subcommand = pip_subcommand(pip_args)?;
    if is_mutating_pip_subcommand(&subcommand) {
        Some(subcommand)
    } else {
        None
    }
}

fn pip_args_for_invocation<'a>(
    program: &'a str,
    args: &'a [String],
    py_ctx: &PythonContext,
) -> Option<&'a [String]> {
    if is_pip_program(Path::new(program)) {
        return Some(args);
    }
    if program_matches_python(program, py_ctx)
        && args.len() >= 2
        && args[0] == "-m"
        && is_pip_module(&args[1])
    {
        return Some(&args[2..]);
    }
    None
}

fn is_pip_program(program: &Path) -> bool {
    program
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| {
            let lower = name.to_ascii_lowercase();
            lower
                .strip_prefix("pip")
                .map(|rest| {
                    rest.is_empty() || rest.chars().all(|ch| ch.is_ascii_digit() || ch == '.')
                })
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

fn is_pip_module(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower == "pip.__main__" {
        return true;
    }
    lower
        .strip_prefix("pip")
        .map(|rest| rest.is_empty() || rest.chars().all(|ch| ch.is_ascii_digit() || ch == '.'))
        .unwrap_or(false)
}

fn pip_subcommand(args: &[String]) -> Option<String> {
    const KNOWN_SUBCOMMANDS: &[&str] = &[
        "install",
        "uninstall",
        "download",
        "freeze",
        "list",
        "show",
        "check",
        "config",
        "search",
        "wheel",
        "hash",
        "completion",
        "debug",
        "help",
        "cache",
        "index",
        "inspect",
    ];
    for arg in args {
        if arg == "--" {
            break;
        }
        if arg.starts_with('-') {
            continue;
        }
        let lower = arg.to_ascii_lowercase();
        if KNOWN_SUBCOMMANDS.contains(&lower.as_str()) {
            return Some(lower);
        }
    }
    None
}

fn is_mutating_pip_subcommand(subcommand: &str) -> bool {
    matches!(subcommand, "install" | "uninstall")
}

fn pip_mutation_outcome(program: &str, subcommand: &str, args: &[String]) -> ExecutionOutcome {
    ExecutionOutcome::user_error(
        "pip cannot modify px-managed environments",
        json!({
            "code": crate::diag_commands::RUN,
            "reason": "pip_mutation_forbidden",
            "program": program,
            "subcommand": subcommand,
            "args": args,
            "hint": "px envs are immutable CAS materializations; use `px add/remove/update/sync` to change dependencies."
        }),
    )
}

fn test_project_outcome(ctx: &CommandContext, request: &TestRequest) -> Result<ExecutionOutcome> {
    if let Some(at_ref) = &request.at {
        return run_tests_at_ref(ctx, request, at_ref);
    }
    let strict = request.frozen || ctx.env_flag_enabled("CI");
    let mut sandbox: Option<SandboxRunContext> = None;

    if let Some(ws_ctx) = match prepare_workspace_run_context(ctx, strict, "test", request.sandbox)
    {
        Ok(result) => result,
        Err(outcome) => return Ok(outcome),
    } {
        if request.sandbox
            && strict
            && !matches!(
                ws_ctx.state.canonical,
                WorkspaceStateKind::Consistent | WorkspaceStateKind::InitializedEmpty
            )
        {
            return Ok(sandbox_workspace_env_inconsistent(
                &ws_ctx.workspace_root,
                &ws_ctx.state,
            ));
        }
        if request.sandbox {
            match prepare_workspace_sandbox(ctx, &ws_ctx) {
                Ok(sbx) => sandbox = Some(sbx),
                Err(outcome) => return Ok(outcome),
            }
        }
        let workdir = invocation_workdir(&ws_ctx.py_ctx.project_root);
        let host_runner = HostCommandRunner::new(ctx);
        let sandbox_runner = match sandbox {
            Some(ref mut sbx) => {
                let runner = match sandbox_runner_for_context(&ws_ctx.py_ctx, sbx, &workdir) {
                    Ok(runner) => runner,
                    Err(outcome) => return Ok(outcome),
                };
                Some(runner)
            }
            None => None,
        };
        let runner: &dyn CommandRunner = match sandbox_runner.as_ref() {
            Some(runner) => runner,
            None => &host_runner,
        };
        let mut outcome = run_tests_for_context(
            ctx,
            runner,
            &ws_ctx.py_ctx,
            request,
            ws_ctx.sync_report,
            &workdir,
        )?;
        if let Some(ref sbx) = sandbox {
            attach_sandbox_details(&mut outcome, sbx);
        }
        return Ok(outcome);
    }

    let snapshot = match manifest_snapshot() {
        Ok(snapshot) => snapshot,
        Err(err) => {
            if is_missing_project_error(&err) {
                return Ok(missing_project_outcome());
            }
            let msg = err.to_string();
            if msg.contains("pyproject.toml not found") {
                let root = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                return Ok(missing_pyproject_outcome("test", &root));
            }
            return Err(err);
        }
    };
    let state_report = match crate::state_guard::state_or_violation(ctx, &snapshot, "test") {
        Ok(report) => report,
        Err(outcome) => return Ok(outcome),
    };
    let guard = match guard_for_execution(strict, &snapshot, &state_report, "test") {
        Ok(guard) => guard,
        Err(outcome) => return Ok(outcome),
    };
    let (py_ctx, sync_report) = match python_context_with_mode(ctx, guard) {
        Ok(result) => result,
        Err(outcome) => return Ok(outcome),
    };
    if request.sandbox {
        match prepare_project_sandbox(ctx, &snapshot) {
            Ok(sbx) => sandbox = Some(sbx),
            Err(outcome) => return Ok(outcome),
        }
    }
    let workdir = invocation_workdir(&py_ctx.project_root);
    let host_runner = HostCommandRunner::new(ctx);
    let sandbox_runner = match sandbox {
        Some(ref mut sbx) => {
            let runner = match sandbox_runner_for_context(&py_ctx, sbx, &workdir) {
                Ok(runner) => runner,
                Err(outcome) => return Ok(outcome),
            };
            Some(runner)
        }
        None => None,
    };
    let runner: &dyn CommandRunner = match sandbox_runner.as_ref() {
        Some(runner) => runner,
        None => &host_runner,
    };
    let mut outcome = run_tests_for_context(ctx, runner, &py_ctx, request, sync_report, &workdir)?;
    if let Some(ref sbx) = sandbox {
        attach_sandbox_details(&mut outcome, sbx);
    }
    Ok(outcome)
}

fn select_test_runner(ctx: &CommandContext, py_ctx: &PythonContext) -> TestRunner {
    if ctx.config().test.fallback_builtin {
        return TestRunner::Builtin;
    }
    if let Some(script) = find_runtests_script(&py_ctx.project_root) {
        return TestRunner::Script(script);
    }
    TestRunner::Pytest
}

fn find_runtests_script(project_root: &Path) -> Option<PathBuf> {
    ["tests/runtests.py", "runtests.py"]
        .iter()
        .map(|rel| project_root.join(rel))
        .find(|candidate| candidate.is_file())
}

fn run_tests_for_context(
    ctx: &CommandContext,
    runner: &dyn CommandRunner,
    py_ctx: &PythonContext,
    request: &TestRequest,
    sync_report: Option<crate::EnvironmentSyncReport>,
    workdir: &Path,
) -> Result<ExecutionOutcome> {
    let command_args = json!({ "test_args": request.args });
    let (mut envs, _preflight) = build_env_with_preflight(ctx, py_ctx, &command_args)?;
    let stream_runner = !ctx.global.json;
    let allow_missing_pytest_fallback = sync_report
        .as_ref()
        .map(|report| report.action() == "env-recreate")
        .unwrap_or(false);

    let mut outcome = match select_test_runner(ctx, py_ctx) {
        TestRunner::Builtin => {
            run_builtin_tests(ctx, runner, py_ctx, envs, stream_runner, workdir)?
        }
        TestRunner::Script(script) => run_script_runner(
            ctx,
            runner,
            py_ctx,
            envs,
            &script,
            &request.args,
            stream_runner,
            workdir,
        )?,
        TestRunner::Pytest => {
            envs.push(("PX_TEST_RUNNER".into(), "pytest".into()));
            run_pytest_runner(
                ctx,
                runner,
                py_ctx,
                envs,
                &request.args,
                stream_runner,
                allow_missing_pytest_fallback,
                workdir,
            )?
        }
    };
    attach_autosync_details(&mut outcome, sync_report);
    Ok(outcome)
}

#[allow(clippy::too_many_arguments)]
fn run_script_runner(
    _ctx: &CommandContext,
    runner: &dyn CommandRunner,
    py_ctx: &PythonContext,
    mut envs: EnvPairs,
    script: &Path,
    args: &[String],
    stream_runner: bool,
    workdir: &Path,
) -> Result<ExecutionOutcome> {
    let runner_label = script
        .strip_prefix(&py_ctx.project_root)
        .unwrap_or(script)
        .display()
        .to_string();
    envs.push(("PX_TEST_RUNNER".into(), runner_label.clone()));
    let mut cmd_args = vec![script.display().to_string()];
    cmd_args.extend_from_slice(args);
    let output = run_python_command(runner, py_ctx, &cmd_args, &envs, stream_runner, workdir)?;
    if output.code == 0 {
        Ok(test_success(&runner_label, output, stream_runner, args))
    } else {
        Ok(test_failure(&runner_label, output, stream_runner, args))
    }
}

fn run_builtin_tests(
    _core_ctx: &CommandContext,
    runner: &dyn CommandRunner,
    ctx: &PythonContext,
    mut envs: Vec<(String, String)>,
    stream_runner: bool,
    workdir: &Path,
) -> Result<ExecutionOutcome> {
    if let Some(path) = ensure_stdlib_tests_available(ctx)? {
        append_pythonpath(&mut envs, &path);
    }
    envs.push(("PX_TEST_RUNNER".into(), "builtin".into()));
    let script = "from sample_px_app import cli\nassert cli.greet() == 'Hello, World!'\nprint('px fallback test passed')";
    let args = vec!["-c".to_string(), script.to_string()];
    let output = run_python_command(runner, ctx, &args, &envs, stream_runner, workdir)?;
    let runner_args: Vec<String> = Vec::new();
    Ok(test_success("builtin", output, stream_runner, &runner_args))
}

fn test_success(
    runner: &str,
    output: crate::RunOutput,
    stream_runner: bool,
    args: &[String],
) -> ExecutionOutcome {
    ExecutionOutcome::success(
        format!("{runner} ok"),
        test_details(runner, output, stream_runner, args, None),
    )
}

fn ensure_stdlib_tests_available(py_ctx: &PythonContext) -> Result<Option<PathBuf>> {
    const DISCOVER_SCRIPT: &str =
        "import json, sys, sysconfig; print(json.dumps({'version': sys.version.split()[0], 'stdlib': sysconfig.get_path('stdlib')}))";
    let output = Command::new(&py_ctx.python)
        .arg("-c")
        .arg(DISCOVER_SCRIPT)
        .output()
        .context("probing python stdlib path")?;
    if !output.status.success() {
        bail!(
            "python exited with {} while probing stdlib",
            output.status.code().unwrap_or(-1)
        );
    }
    let payload: Value =
        serde_json::from_slice(&output.stdout).context("invalid stdlib probe payload")?;
    let runtime_version = payload
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let Some((major, minor)) = parse_python_version(&runtime_version) else {
        return Ok(None);
    };
    let stdlib = payload
        .get("stdlib")
        .and_then(Value::as_str)
        .context("python stdlib path unavailable")?;
    let tests_dir = PathBuf::from(stdlib).join("test");
    if tests_dir.exists() {
        return Ok(None);
    }

    // Avoid mutating the system stdlib; stage tests under the project .px directory.
    let staging_base = env::var_os("PX_STDLIB_STAGING_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| py_ctx.project_root.join(".px").join("stdlib-tests"));
    let staged_root = staging_base.join(format!("{major}.{minor}"));
    let staged_tests = staged_root.join("test");
    if staged_tests.exists() {
        return Ok(Some(staged_root));
    }

    if let Some((host_python, source_tests)) = host_stdlib_tests(&major, &minor, &runtime_version) {
        if copy_stdlib_tests(&source_tests, &staged_tests, &host_python).is_ok() {
            return Ok(Some(staged_root));
        }
    }
    if download_stdlib_tests(&runtime_version, &staged_tests)? {
        return Ok(Some(staged_root));
    }

    warn!(
        runtime = %runtime_version,
        tests_dir = %tests_dir.display(),
        "stdlib test suite missing; proceeding without staging tests"
    );
    Ok(None)
}

fn host_stdlib_tests(
    major: &str,
    minor: &str,
    runtime_version: &str,
) -> Option<(PathBuf, PathBuf)> {
    let candidates = [
        format!("python{major}.{minor}"),
        format!("python{major}"),
        "python".to_string(),
    ];
    for candidate in candidates {
        let output = Command::new(&candidate)
            .arg("-c")
            .arg(
                "import json, sys, sysconfig; print(json.dumps({'stdlib': sysconfig.get_path('stdlib'), 'version': sys.version.split()[0], 'executable': sys.executable}))",
            )
            .output();
        let Ok(output) = output else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        let Ok(payload) = serde_json::from_slice::<Value>(&output.stdout) else {
            continue;
        };
        let detected_version = payload
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if !detected_version.starts_with(&format!("{major}.{minor}")) {
            continue;
        }
        let stdlib = payload
            .get("stdlib")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if stdlib.is_empty() {
            continue;
        }
        let tests = PathBuf::from(stdlib).join("test");
        if !tests.exists() {
            continue;
        }
        let exe = payload
            .get("executable")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(candidate.clone()));
        debug!(
            version = %runtime_version,
            source = %exe.display(),
            tests = %tests.display(),
            "found host stdlib tests"
        );
        return Some((exe, tests));
    }
    None
}

fn download_stdlib_tests(version: &str, dest: &Path) -> Result<bool> {
    let url = format!("https://www.python.org/ftp/python/{version}/Python-{version}.tgz");
    let client = crate::core::runtime::build_http_client()?;
    let response = match client.get(&url).send() {
        Ok(resp) => resp,
        Err(err) => {
            debug!(error = %err, url = %url, "failed to download cpython sources");
            return Ok(false);
        }
    };
    if !response.status().is_success() {
        debug!(
            status = %response.status(),
            url = %url,
            "cpython source archive unavailable for stdlib tests"
        );
        return Ok(false);
    }
    let bytes = match response.bytes() {
        Ok(bytes) => bytes,
        Err(err) => {
            debug!(%err, url = %url, "failed to read cpython source archive for stdlib tests");
            return Ok(false);
        }
    };
    let mut archive = Archive::new(GzDecoder::new(Cursor::new(bytes)));
    if dest.exists() {
        fs::remove_dir_all(dest)
            .with_context(|| format!("clearing existing stdlib tests at {}", dest.display()))?;
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating stdlib parent {}", parent.display()))?;
    }
    let prefix = PathBuf::from(format!("Python-{version}/Lib/test"));
    let mut extracted = false;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?;
        let Ok(rel) = path.strip_prefix(&prefix) else {
            continue;
        };
        let dest_path = dest.join(rel);
        if let Some(parent) = dest_path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        entry.unpack(&dest_path)?;
        extracted = true;
    }
    Ok(extracted)
}

fn copy_stdlib_tests(source: &Path, dest: &Path, python: &Path) -> Result<()> {
    let script = r#"
import shutil
import sys
from pathlib import Path

src = Path(sys.argv[1])
dest = Path(sys.argv[2])
shutil.copytree(src, dest, dirs_exist_ok=True, symlinks=True)
"#;
    if dest.exists() {
        fs::remove_dir_all(dest)
            .with_context(|| format!("removing previous stdlib tests at {}", dest.display()))?;
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating stdlib parent {}", parent.display()))?;
    }
    let status = Command::new(python)
        .arg("-c")
        .arg(script)
        .arg(source.as_os_str())
        .arg(dest.as_os_str())
        .status()
        .with_context(|| format!("copying stdlib tests using {}", python.display()))?;
    if !status.success() {
        bail!(
            "python exited with {} while copying stdlib tests",
            status.code().unwrap_or(-1)
        );
    }
    Ok(())
}

fn parse_python_version(version: &str) -> Option<(String, String)> {
    let mut parts = version.split('.');
    let major = parts.next()?.to_string();
    let minor = parts.next().unwrap_or_default().to_string();
    if major.is_empty() || minor.is_empty() {
        return None;
    }
    Some((major, minor))
}

fn test_failure(
    runner: &str,
    output: crate::RunOutput,
    stream_runner: bool,
    args: &[String],
) -> ExecutionOutcome {
    let code = output.code;
    let mut details = test_details(runner, output, stream_runner, args, Some("tests_failed"));
    if let Value::Object(map) = &mut details {
        map.insert("suppress_cli_frame".into(), Value::Bool(true));
    }
    ExecutionOutcome::failure(format!("{runner} failed (exit {code})"), details)
}

fn missing_pytest_outcome(output: crate::RunOutput, args: &[String]) -> ExecutionOutcome {
    ExecutionOutcome::user_error(
        "pytest is not available in the project environment",
        json!({
            "stdout": output.stdout,
            "stderr": output.stderr,
            "hint": "Add pytest to your project with `px add pytest`, then rerun `px test`.",
            "reason": "missing_pytest",
            "code": crate::diag_commands::TEST,
            "runner": "pytest",
            "args": args,
        }),
    )
}

fn test_details(
    runner: &str,
    output: crate::RunOutput,
    stream_runner: bool,
    args: &[String],
    reason: Option<&str>,
) -> serde_json::Value {
    let mut details = json!({
        "runner": runner,
        "stdout": output.stdout,
        "stderr": output.stderr,
        "code": output.code,
        "args": args,
        "streamed": stream_runner,
    });
    if let Some(reason) = reason {
        if let Some(map) = details.as_object_mut() {
            map.insert("reason".to_string(), json!(reason));
        }
    }
    details
}

fn missing_pytest(stderr: &str) -> bool {
    let lowered = stderr.to_ascii_lowercase();
    if !lowered.contains("no module named") {
        return false;
    }
    lowered.contains("no module named 'pytest'")
        || lowered.contains("no module named \"pytest\"")
        || lowered
            .split_once("no module named")
            .map(|(_, rest)| rest.trim_start().starts_with("pytest"))
            .unwrap_or(false)
}

#[cfg(test)]
fn build_pytest_command(project_root: &Path, extra_args: &[String]) -> Vec<String> {
    build_pytest_command_with_defaults(project_root, extra_args, &[])
}

fn build_pytest_command_with_defaults(
    project_root: &Path,
    extra_args: &[String],
    defaults: &[String],
) -> Vec<String> {
    let mut pytest_cmd = vec!["-m".to_string(), "pytest".to_string()];
    pytest_cmd.extend(defaults.iter().cloned());
    if extra_args.is_empty() {
        for candidate in ["tests", "test"] {
            if project_root.join(candidate).exists() {
                pytest_cmd.push(candidate.to_string());
                break;
            }
        }
    }
    pytest_cmd.extend(extra_args.iter().cloned());
    pytest_cmd
}

fn build_env_with_preflight(
    ctx: &CommandContext,
    py_ctx: &PythonContext,
    command_args: &Value,
) -> Result<(EnvPairs, Option<bool>)> {
    let mut envs = py_ctx.base_env(command_args)?;
    if std::env::var("PX_PYTEST_PERF_BASELINE").is_err() {
        if let Some(baseline) = pytest_perf_baseline(py_ctx) {
            envs.push(("PX_PYTEST_PERF_BASELINE".into(), baseline));
        }
    }
    let preflight = preflight_plugins(ctx, py_ctx, &envs)?;
    if let Some(ok) = preflight {
        envs.push((
            "PX_PLUGIN_PREFLIGHT".into(),
            if ok { "1".into() } else { "0".into() },
        ));
    }
    Ok((envs, preflight))
}

fn pytest_perf_baseline(py_ctx: &PythonContext) -> Option<String> {
    let canonical_root = py_ctx.project_root.canonicalize().ok()?;
    Some(format!(
        "{}{{extras}}@{}",
        py_ctx.project_name,
        canonical_root.to_string_lossy()
    ))
}

fn preflight_plugins(
    ctx: &CommandContext,
    py_ctx: &PythonContext,
    envs: &[(String, String)],
) -> Result<Option<bool>> {
    if py_ctx.px_options.plugin_imports.is_empty() {
        return Ok(None);
    }
    let imports = py_ctx
        .px_options
        .plugin_imports
        .iter()
        .map(|name| format!("{name:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let script = format!(
        "import importlib.util, sys\nmissing=[name for name in [{imports}] if importlib.util.find_spec(name) is None]\nsys.exit(1 if missing else 0)"
    );
    let args = vec!["-c".to_string(), script];
    match ctx
        .python_runtime()
        .run_command(&py_ctx.python, &args, envs, &py_ctx.project_root)
    {
        Ok(output) => Ok(Some(output.code == 0)),
        Err(err) => {
            debug!(error = ?err, "plugin preflight failed");
            Ok(Some(false))
        }
    }
}

const PX_PYTEST_PLUGIN: &str = r#"import os
import sys
import time
import pytest
from _pytest._io.terminalwriter import TerminalWriter


class PxTerminalReporter:
    def __init__(self, config):
        self.config = config
        self._tw = TerminalWriter(file=sys.stdout)
        self._tw.hasmarkup = True
        self.session_start = time.time()
        self.collection_start = None
        self.collection_duration = 0.0
        self.collected = 0
        self.files = []
        self._current_file = None
        self.failures = []
        self.collection_errors = []
        self.stats = {"passed": 0, "failed": 0, "skipped": 0, "error": 0, "xfailed": 0, "xpassed": 0}
        self.exitstatus = 0
        self.spinner_frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]
        self.spinner_index = 0
        self.last_progress_len = 0
        self._spinner_active = True

    def pytest_sessionstart(self, session):
        import platform

        py_ver = platform.python_version()
        root = str(self.config.rootpath)
        cfg = self.config.inifile or "auto-detected"
        self._tw.line(f"px test  •  Python {py_ver}  •  pytest {pytest.__version__}", cyan=True, bold=True)
        self._tw.line(f"root:   {root}")
        self._tw.line(f"config: {cfg}")
        self.collection_start = time.time()
        self._render_progress(note="collecting", force=True)

    def pytest_collection_finish(self, session):
        self.collected = len(session.items)
        files = {str(item.fspath) for item in session.items}
        self.files = sorted(files)
        self.collection_duration = time.time() - (self.collection_start or self.session_start)
        label = "tests" if self.collected != 1 else "test"
        file_label = "files" if len(self.files) != 1 else "file"
        self._spinner_active = False
        self._clear_spinner(newline=True)
        self._tw.line(f"collected {self.collected} {label} from {len(self.files)} {file_label} in {self.collection_duration:.2f}s")
        self._tw.line("")

    def pytest_collectreport(self, report):
        if report.failed:
            self.stats["error"] += 1
            summary = getattr(report, "longreprtext", "") or getattr(report, "longrepr", "")
            self.collection_errors.append((str(report.fspath), str(summary)))
        self._render_progress(note="collecting")

    def pytest_runtest_logreport(self, report):
        if report.when not in ("setup", "call", "teardown"):
            return
        status = None
        if report.passed and report.when == "call":
            status = "passed"
            self.stats["passed"] += 1
        elif report.skipped:
            status = "skipped"
            self.stats["skipped"] += 1
        elif report.failed:
            status = "failed" if report.when == "call" else "error"
            self.stats[status] += 1

        if status:
            file_path = str(report.location[0])
            name = report.location[2]
            duration = getattr(report, "duration", 0.0)
            self._print_test_result(file_path, name, status, duration)

        if report.failed:
            self.failures.append(report)

    def pytest_sessionfinish(self, session, exitstatus):
        self.exitstatus = exitstatus
        self._spinner_active = False
        self._clear_spinner(newline=True)
        if self.failures:
            self._render_failures()
        if self.collection_errors:
            self._render_collection_errors()
        self._render_summary(exitstatus)

    # --- rendering helpers ---
    def _render_progress(self, note="", force=False):
        if not force and not self._spinner_active:
            return
        total = self.collected or "?"
        completed = (
            self.stats["passed"]
            + self.stats["failed"]
            + self.stats["skipped"]
            + self.stats["error"]
            + self.stats["xfailed"]
            + self.stats["xpassed"]
        )
        frame = self.spinner_frames[self.spinner_index % len(self.spinner_frames)]
        self.spinner_index += 1
        line = f"\r{frame} {completed}/{total} • pass:{self.stats['passed']} fail:{self.stats['failed']} skip:{self.stats['skipped']} err:{self.stats['error']}"
        if note:
            line += f" • {note}"
        padding = max(self.last_progress_len - len(line), 0)
        sys.stdout.write(line + (" " * padding))
        sys.stdout.flush()
        self.last_progress_len = len(line)

    def _clear_spinner(self, newline: bool = False):
        if self.last_progress_len:
            end = "\n" if newline else "\r"
            sys.stdout.write("\r" + " " * self.last_progress_len + end)
            sys.stdout.flush()
            self.last_progress_len = 0

    def _print_test_result(self, file_path, name, status, duration):
        if self._current_file != file_path:
            self._current_file = file_path
            self._tw.line("")
            self._tw.line(file_path)
        icon, color = self._status_icon(status)
        dur = f"{duration:.2f}s"
        line = f"  {icon} {name}  {dur}"
        self._tw.line(line, **color)

    def _render_failures(self):
        self._tw.line(f"FAILURES ({len(self.failures)})", red=True, bold=True)
        self._tw.line("-" * 11)
        for idx, report in enumerate(self.failures, start=1):
            self._render_single_failure(idx, report)

    def _render_collection_errors(self):
        self._tw.line(f"COLLECTION ERRORS ({len(self.collection_errors)})", red=True, bold=True)
        self._tw.line("-" * 19)
        for idx, (path, summary) in enumerate(self.collection_errors, start=1):
            self._tw.line("")
            self._tw.line(f"{idx}) {path}", bold=True)
            if summary:
                for line in str(summary).splitlines():
                    self._tw.line(f"   {line}", red=True)

    def _render_single_failure(self, idx, report):
        path, lineno = self._failure_lineno(report)
        self._tw.line("")
        self._tw.line(f"{idx}) {report.nodeid}", bold=True)
        self._tw.line("")
        message = self._failure_message(report)
        if message:
            self._tw.line(f"   {message}", red=True)
            self._tw.line("")
        snippet = self._load_snippet(path, lineno)
        if snippet:
            file_line = f"   {path}:{lineno}"
            self._tw.line(file_line)
            for i, text in snippet:
                pointer = "→" if i == lineno else " "
                self._tw.line(f"  {pointer}{i:>4}  {text}")
            self._tw.line("")
        explanation = self._assertion_explanation(report)
        if explanation:
            self._tw.line("   Explanation:")
            for line in explanation:
                self._tw.line(f"     {line}")

    def _render_summary(self, exitstatus):
        total = sum(self.stats.values())
        duration = time.time() - self.session_start
        status_label = "✓ PASSED" if exitstatus == 0 else "✗ FAILED"
        status_color = {"green": exitstatus == 0, "red": exitstatus != 0, "bold": True}
        self._tw.line("")
        self._tw.line(f"RESULT   {status_label} (exit code {exitstatus})", **status_color)
        self._tw.line(f"TOTAL    {total} tests in {duration:.2f}s")
        self._tw.line(f"PASSED   {self.stats['passed']}")
        self._tw.line(f"FAILED   {self.stats['failed']}")
        self._tw.line(f"SKIPPED  {self.stats['skipped']}")
        self._tw.line(f"ERRORS   {self.stats['error']}")

    # --- utility helpers ---
    def _status_icon(self, status):
        if status in ("passed", "xpassed"):
            return "✓", {"green": True}
        if status in ("skipped", "xfailed"):
            return "∙", {"yellow": True}
        return "✗", {"red": True, "bold": True}

    def _failure_message(self, report):
        longrepr = getattr(report, "longrepr", None)
        if hasattr(longrepr, "reprcrash") and longrepr.reprcrash:
            return longrepr.reprcrash.message
        if hasattr(report, "longreprtext"):
            return report.longreprtext.splitlines()[0]
        return str(longrepr) if longrepr else "test failed"

    def _load_snippet(self, path, lineno, context=2):
        path = str(path)
        try:
            with open(path, "r", encoding="utf-8") as f:
                lines = f.readlines()
        except OSError:
            return None
        start = max(0, lineno - context - 1)
        end = min(len(lines), lineno + context)
        snippet = []
        for idx in range(start, end):
            text = lines[idx].rstrip("\n")
            snippet.append((idx + 1, text))
        return snippet

    def _failure_lineno(self, report):
        longrepr = getattr(report, "longrepr", None)
        if hasattr(longrepr, "reprcrash") and longrepr.reprcrash:
            return str(longrepr.reprcrash.path), longrepr.reprcrash.lineno
        path, lineno, _ = report.location
        return str(path), lineno + 1

    def _assertion_explanation(self, report):
        longrepr = getattr(report, "longrepr", None)
        summary = None
        if hasattr(longrepr, "reprcrash") and longrepr.reprcrash:
            summary = longrepr.reprcrash.message or ""
        if summary:
            lowered = summary.lower()
            if "did not raise" in lowered:
                expected = summary.split("DID NOT RAISE")[-1].strip()
                expected = expected or "expected exception"
                summary = f"Expected {expected} to be raised, but none was."
            elif "assert" in lowered and "==" in summary:
                parts = summary.split("==", 1)
                left = parts[0].replace("AssertionError:", "").replace("assert", "", 1).strip()
                right = parts[1].strip()
                summary = f"Expected: {right}"
                if left:
                    summary += f"\n     Actual:   {left}"
            else:
                summary = summary.replace("AssertionError:", "").strip()
        if not summary:
            return None
        parts = summary.split("\n")
        return [part for part in parts if part.strip()]


def pytest_configure(config):
    config.option.color = "yes"
    pm = config.pluginmanager
    reporter = PxTerminalReporter(config)
    default = pm.getplugin("terminalreporter")
    if default:
        pm.unregister(default)
        pm.register(reporter, "terminalreporter")
        config._px_reporter_registered = True
    else:
        config._px_reporter_registered = False
    config._px_reporter = reporter


def pytest_sessionstart(session):
    config = session.config
    reporter = getattr(config, "_px_reporter", None)
    if reporter is None:
        return
    if not getattr(config, "_px_reporter_registered", False):
        pm = config.pluginmanager
        default = pm.getplugin("terminalreporter")
        if default and default is not reporter:
            pm.unregister(default)
        pm.register(reporter, "terminalreporter")
        config._px_reporter_registered = True
reporter.pytest_sessionstart(session)
"#;

fn prepare_project_sandbox(
    ctx: &CommandContext,
    snapshot: &ManifestSnapshot,
) -> Result<SandboxRunContext, ExecutionOutcome> {
    let store = sandbox_store()?;
    let state = load_project_state(ctx.fs(), &snapshot.root).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to read project state for sandbox",
            json!({ "error": err.to_string(), "code": "PX903" }),
        )
    })?;
    let env = state.current_env.ok_or_else(|| {
        ExecutionOutcome::user_error(
            "project environment missing for sandbox execution",
            json!({
                "code": "PX902",
                "reason": "missing_env",
                "hint": "run `px sync` before using --sandbox",
            }),
        )
    })?;
    let profile_oid = env
        .profile_oid
        .as_deref()
        .unwrap_or(&env.id)
        .trim()
        .to_string();
    if profile_oid.is_empty() {
        return Err(ExecutionOutcome::user_error(
            "sandbox requires an environment profile",
            json!({
                "code": "PX904",
                "reason": "missing_profile_oid",
            }),
        ));
    }
    let site_packages = if env.site_packages.trim().is_empty() {
        None
    } else {
        Some(PathBuf::from(&env.site_packages))
    };
    let env_root = env.env_path.as_ref().map(PathBuf::from).or_else(|| {
        site_packages
            .as_ref()
            .and_then(|site| env_root_from_site_packages(site))
    });
    let env_root = match env_root {
        Some(root) => root,
        None => {
            return Err(ExecutionOutcome::user_error(
                "project environment missing for sandbox execution",
                json!({
                    "code": "PX902",
                    "reason": "missing_env",
                    "hint": "run `px sync` before using --sandbox",
                }),
            ))
        }
    };
    let lock = match load_lockfile_optional(&snapshot.lock_path) {
        Ok(lock) => lock,
        Err(err) => {
            return Err(ExecutionOutcome::failure(
                "failed to read px.lock",
                json!({ "error": err.to_string(), "code": "PX900" }),
            ))
        }
    };
    let Some(lock) = lock.as_ref() else {
        return Err(ExecutionOutcome::user_error(
            "px.lock not found for sandbox execution",
            json!({ "code": "PX900", "reason": "missing_lock" }),
        ));
    };
    let config = sandbox_config_from_manifest(&snapshot.manifest_path).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to read sandbox configuration",
            json!({ "error": err.to_string() }),
        )
    })?;
    let artifacts = ensure_sandbox_image(
        &store,
        &config,
        Some(lock),
        None,
        &profile_oid,
        &env_root,
        site_packages.as_deref(),
    )
    .map_err(|err| ExecutionOutcome::user_error(err.message, err.details))?;
    Ok(SandboxRunContext { store, artifacts })
}

fn prepare_workspace_sandbox(
    _ctx: &CommandContext,
    ws_ctx: &crate::workspace::WorkspaceRunContext,
) -> Result<SandboxRunContext, ExecutionOutcome> {
    let store = sandbox_store()?;
    let profile_oid = ws_ctx
        .profile_oid
        .as_deref()
        .unwrap_or("")
        .trim()
        .to_string();
    if profile_oid.is_empty() {
        return Err(ExecutionOutcome::user_error(
            "workspace environment missing for sandbox execution",
            json!({
                "code": "PX902",
                "reason": "missing_env",
                "hint": "run `px sync` at the workspace root before using --sandbox",
            }),
        ));
    }
    let lock = match load_lockfile_optional(&ws_ctx.lock_path) {
        Ok(lock) => lock,
        Err(err) => {
            return Err(ExecutionOutcome::failure(
                "failed to read workspace lockfile",
                json!({ "error": err.to_string(), "code": "PX900" }),
            ))
        }
    };
    let Some(lock) = lock.as_ref() else {
        return Err(ExecutionOutcome::user_error(
            "workspace lockfile missing for sandbox execution",
            json!({ "code": "PX900", "reason": "missing_lock" }),
        ));
    };
    let env_root = env_root_from_site_packages(&ws_ctx.site_packages).ok_or_else(|| {
        ExecutionOutcome::user_error(
            "workspace environment missing for sandbox execution",
            json!({
                "code": "PX902",
                "reason": "missing_env",
                "hint": "run `px sync` at the workspace root before using --sandbox",
            }),
        )
    })?;
    let workspace_lock = lock.workspace.as_ref();
    let config = sandbox_config_from_manifest(&ws_ctx.workspace_manifest).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to read workspace sandbox configuration",
            json!({ "error": err.to_string() }),
        )
    })?;
    let artifacts = ensure_sandbox_image(
        &store,
        &config,
        Some(lock),
        workspace_lock,
        &profile_oid,
        &env_root,
        Some(ws_ctx.site_packages.as_path()),
    )
    .map_err(|err| ExecutionOutcome::user_error(err.message, err.details))?;
    Ok(SandboxRunContext { store, artifacts })
}

fn prepare_commit_sandbox(
    manifest_path: &Path,
    lock: &px_domain::LockSnapshot,
    profile_oid: &str,
    env_root: &Path,
    site_packages: Option<&Path>,
) -> Result<SandboxRunContext, ExecutionOutcome> {
    let store = sandbox_store()?;
    let config = sandbox_config_from_manifest(manifest_path).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to read sandbox configuration at git ref",
            json!({ "error": err.to_string() }),
        )
    })?;
    let artifacts = ensure_sandbox_image(
        &store,
        &config,
        Some(lock),
        lock.workspace.as_ref(),
        profile_oid,
        env_root,
        site_packages,
    )
    .map_err(|err| ExecutionOutcome::user_error(err.message, err.details))?;
    Ok(SandboxRunContext { store, artifacts })
}

fn sandbox_store() -> Result<SandboxStore, ExecutionOutcome> {
    default_store_root().map(SandboxStore::new).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to resolve sandbox store",
            json!({ "error": err.to_string(), "code": "PX903" }),
        )
    })
}

fn attach_sandbox_details(outcome: &mut ExecutionOutcome, sandbox: &SandboxRunContext) {
    let details = json!({
        "sbx_id": sandbox.artifacts.definition.sbx_id(),
        "base": sandbox.artifacts.base.name,
        "base_os_oid": sandbox.artifacts.base.base_os_oid,
        "capabilities": sandbox.artifacts.definition.capabilities,
        "profile_oid": sandbox.artifacts.definition.profile_oid,
        "image_digest": sandbox.artifacts.manifest.image_digest,
    });
    match outcome.details {
        Value::Object(ref mut map) => {
            map.insert("sandbox".to_string(), details);
        }
        Value::Null => {
            outcome.details = json!({ "sandbox": details });
        }
        ref mut other => {
            let prev = other.take();
            outcome.details = json!({
                "value": prev,
                "sandbox": details,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CommandStatus, GlobalOptions, SystemEffects};
    use px_domain::PxOptions;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use tempfile::tempdir;

    fn ctx_with_defaults() -> CommandContext<'static> {
        static GLOBAL: GlobalOptions = GlobalOptions {
            quiet: false,
            verbose: 0,
            trace: false,
            debug: false,
            json: false,
        };
        CommandContext::new(&GLOBAL, Arc::new(SystemEffects::new())).expect("ctx")
    }

    #[test]
    fn maps_program_path_into_container_roots() {
        let mapped_env = map_program_for_container(
            "/home/user/.px/envs/demo/bin/pythonproject",
            Path::new("/home/user/project"),
            Path::new("/app"),
            Path::new("/home/user/.px/envs/demo"),
            Path::new("/px/env"),
        );
        assert_eq!(mapped_env, "/px/env/bin/pythonproject");

        let mapped_project = map_program_for_container(
            "/home/user/project/scripts/run.py",
            Path::new("/home/user/project"),
            Path::new("/app"),
            Path::new("/home/user/.px/envs/demo"),
            Path::new("/px/env"),
        );
        assert_eq!(mapped_project, "/app/scripts/run.py");

        let passthrough = map_program_for_container(
            "pythonproject",
            Path::new("/home/user/project"),
            Path::new("/app"),
            Path::new("/home/user/.px/envs/demo"),
            Path::new("/px/env"),
        );
        assert_eq!(passthrough, "pythonproject");
    }

    #[test]
    fn detects_mutating_pip_invocation_for_install() {
        let temp = tempdir().expect("tempdir");
        let py_ctx = PythonContext {
            project_root: temp.path().to_path_buf(),
            project_name: "demo".into(),
            python: "/usr/bin/python".into(),
            pythonpath: String::new(),
            allowed_paths: vec![temp.path().to_path_buf()],
            site_bin: None,
            pep582_bin: Vec::new(),
            px_options: PxOptions::default(),
        };

        let args = vec!["install".to_string(), "demo".to_string()];
        let subcommand = mutating_pip_invocation("pip", &args, &py_ctx);
        assert_eq!(subcommand.as_deref(), Some("install"));
    }

    #[test]
    fn detects_mutating_python_dash_m_pip_invocation() {
        let temp = tempdir().expect("tempdir");
        let py_ctx = PythonContext {
            project_root: temp.path().to_path_buf(),
            project_name: "demo".into(),
            python: "/usr/bin/python".into(),
            pythonpath: String::new(),
            allowed_paths: vec![temp.path().to_path_buf()],
            site_bin: None,
            pep582_bin: Vec::new(),
            px_options: PxOptions::default(),
        };

        let args = vec![
            "-m".to_string(),
            "pip".to_string(),
            "uninstall".to_string(),
            "demo".to_string(),
        ];
        let subcommand = mutating_pip_invocation(&py_ctx.python, &args, &py_ctx);
        assert_eq!(subcommand.as_deref(), Some("uninstall"));
    }

    #[test]
    fn read_only_pip_commands_are_allowed() {
        let temp = tempdir().expect("tempdir");
        let py_ctx = PythonContext {
            project_root: temp.path().to_path_buf(),
            project_name: "demo".into(),
            python: "/usr/bin/python".into(),
            pythonpath: String::new(),
            allowed_paths: vec![temp.path().to_path_buf()],
            site_bin: None,
            pep582_bin: Vec::new(),
            px_options: PxOptions::default(),
        };

        let list_args = vec!["list".to_string()];
        assert!(mutating_pip_invocation("pip3", &list_args, &py_ctx).is_none());

        let help_args = vec!["help".to_string(), "install".to_string()];
        assert!(mutating_pip_invocation("pip", &help_args, &py_ctx).is_none());

        let version_args = vec!["--version".to_string()];
        assert!(mutating_pip_invocation("pip", &version_args, &py_ctx).is_none());
    }

    #[test]
    fn detects_lfs_pointer_format() {
        let pointer = b"version https://git-lfs.github.com/spec/v1\n\
oid sha256:1234567890abcdef\nsize 12\n";
        assert!(is_lfs_pointer(pointer));
        assert!(!is_lfs_pointer(b"plain file"));
    }

    #[test]
    fn copy_tree_skips_git_metadata() {
        let src = tempdir().expect("src");
        let dest = tempdir().expect("dest");
        let normal = src.path().join("data.txt");
        fs::write(&normal, b"hello").expect("write");
        let git_dir = src.path().join(".git");
        fs::create_dir_all(&git_dir).expect("git dir");
        fs::write(git_dir.join("config"), b"ignore").expect("git config");

        copy_tree(src.path(), dest.path()).expect("copy");
        assert!(dest.path().join("data.txt").is_file());
        assert!(!dest.path().join(".git").exists());
    }

    #[test]
    fn list_submodules_reports_commits() -> Result<()> {
        let workspace = tempdir()?;
        let root = workspace.path();

        // Create a tiny submodule repo.
        let subrepo = root.join("subrepo");
        fs::create_dir_all(&subrepo)?;
        git(&subrepo, &["init"])?;
        fs::write(subrepo.join("data.txt"), "demo")?;
        git(&subrepo, &["add", "data.txt"])?;
        git(&subrepo, &["commit", "-m", "init"])?;
        let sub_head = git(&subrepo, &["rev-parse", "HEAD"])?;
        let sub_head = sub_head.trim().to_string();

        // Main repo with submodule.
        git(root, &["init"])?;
        fs::write(root.join("pyproject.toml"), "")?;
        fs::write(root.join("px.lock"), "")?;
        git(
            root,
            &["submodule", "add", subrepo.to_str().unwrap(), "libs/data"],
        )?;
        git(root, &["commit", "-am", "add submodule"])?;

        let subs = list_submodules(root, "HEAD").expect("list submodules");
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].0, PathBuf::from("libs/data"));
        assert_eq!(subs[0].1, sub_head);
        Ok(())
    }

    #[test]
    fn restore_lfs_pointers_smudges_with_git_lfs() -> Result<()> {
        let workspace = tempdir()?;
        let root = workspace.path();

        git(root, &["init"])?;
        fs::write(root.join("pyproject.toml"), "")?;
        fs::write(root.join("px.lock"), "")?;
        fs::create_dir_all(root.join("assets"))?;
        let pointer = root.join("assets").join("file.bin");
        fs::write(
            &pointer,
            "version https://git-lfs.github.com/spec/v1\n\
oid sha256:deadbeef\nsize 4\n",
        )?;
        git(root, &["add", "."])?;
        git(root, &["commit", "-m", "add lfs pointer"])?;

        // Fake git-lfs subcommand on PATH.
        let fake_bin = workspace.path().join("bin");
        fs::create_dir_all(&fake_bin)?;
        let fake = fake_bin.join("git-lfs");
        fs::write(
            &fake,
            "#!/bin/sh\nif [ \"$1\" = \"smudge\" ]; then cat >/dev/null; echo \"SMUDGED\"; else exit 1; fi\n",
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&fake)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&fake, perms)?;
        }
        let _path_guard = EnvVarGuard::set(
            "PATH",
            format!(
                "{}:{}",
                fake_bin.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        );

        restore_lfs_pointers(root, "HEAD", root).expect("smudge lfs pointers");
        let contents = fs::read_to_string(&pointer)?;
        assert_eq!(contents.trim(), "SMUDGED");

        Ok(())
    }

    #[test]
    fn run_executable_blocks_mutating_pip_commands() -> Result<()> {
        let ctx = ctx_with_defaults();
        let temp = tempdir()?;
        let py_ctx = PythonContext {
            project_root: temp.path().to_path_buf(),
            project_name: "demo".into(),
            python: "/usr/bin/python".into(),
            pythonpath: String::new(),
            allowed_paths: vec![temp.path().to_path_buf()],
            site_bin: None,
            pep582_bin: Vec::new(),
            px_options: PxOptions::default(),
        };

        let args = vec!["install".to_string(), "demo".to_string()];
        let runner = HostCommandRunner::new(&ctx);
        let outcome = run_executable(
            &ctx,
            &runner,
            &py_ctx,
            "pip",
            &args,
            &json!({}),
            &py_ctx.project_root,
            false,
        )?;

        assert_eq!(outcome.status, CommandStatus::UserError);
        assert_eq!(
            outcome
                .details
                .get("reason")
                .and_then(|value| value.as_str()),
            Some("pip_mutation_forbidden")
        );
        assert_eq!(
            outcome
                .details
                .get("subcommand")
                .and_then(|value| value.as_str()),
            Some("install")
        );
        assert_eq!(
            outcome
                .details
                .get("program")
                .and_then(|value| value.as_str()),
            Some("pip")
        );
        Ok(())
    }

    #[test]
    fn run_executable_uses_workdir() -> Result<()> {
        let ctx = ctx_with_defaults();
        let temp = tempdir()?;
        let workdir = temp.path().join("nested");
        fs::create_dir_all(&workdir)?;
        let py_ctx = PythonContext {
            project_root: temp.path().to_path_buf(),
            project_name: "demo".into(),
            python: "/usr/bin/python".into(),
            pythonpath: String::new(),
            allowed_paths: vec![temp.path().to_path_buf()],
            site_bin: None,
            pep582_bin: Vec::new(),
            px_options: PxOptions::default(),
        };

        let runner = HostCommandRunner::new(&ctx);
        let outcome = run_executable(
            &ctx,
            &runner,
            &py_ctx,
            "pwd",
            &[],
            &json!({}),
            &workdir,
            false,
        )?;

        let stdout = outcome
            .details
            .get("stdout")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .trim();
        assert_eq!(PathBuf::from(stdout), workdir);
        Ok(())
    }

    #[test]
    fn build_env_marks_available_plugins() -> Result<()> {
        let ctx = ctx_with_defaults();
        let python = match ctx.python_runtime().detect_interpreter() {
            Ok(path) => path,
            Err(_) => return Ok(()),
        };
        let temp = tempdir()?;
        let py_ctx = PythonContext {
            project_root: temp.path().to_path_buf(),
            project_name: "demo".into(),
            python,
            pythonpath: String::new(),
            allowed_paths: vec![temp.path().to_path_buf()],
            site_bin: None,
            pep582_bin: Vec::new(),
            px_options: PxOptions {
                manage_command: Some("self".into()),
                plugin_imports: vec!["json".into()],
                env_vars: BTreeMap::new(),
            },
        };

        let (envs, preflight) = build_env_with_preflight(&ctx, &py_ctx, &json!({}))?;
        assert_eq!(preflight, Some(true));
        assert!(envs
            .iter()
            .any(|(key, value)| key == "PYAPP_COMMAND_NAME" && value == "self"));
        assert!(envs
            .iter()
            .any(|(key, value)| key == "PX_PLUGIN_PREFLIGHT" && value == "1"));
        Ok(())
    }

    #[test]
    fn build_env_marks_missing_plugins() -> Result<()> {
        let ctx = ctx_with_defaults();
        let python = match ctx.python_runtime().detect_interpreter() {
            Ok(path) => path,
            Err(_) => return Ok(()),
        };
        let temp = tempdir()?;
        let py_ctx = PythonContext {
            project_root: temp.path().to_path_buf(),
            project_name: "demo".into(),
            python,
            pythonpath: String::new(),
            allowed_paths: vec![temp.path().to_path_buf()],
            site_bin: None,
            pep582_bin: Vec::new(),
            px_options: PxOptions {
                manage_command: None,
                plugin_imports: vec!["px_missing_plugin_mod".into()],
                env_vars: BTreeMap::new(),
            },
        };

        let (envs, preflight) = build_env_with_preflight(&ctx, &py_ctx, &json!({}))?;
        assert_eq!(preflight, Some(false));
        assert!(envs
            .iter()
            .any(|(key, value)| key == "PX_PLUGIN_PREFLIGHT" && value == "0"));
        Ok(())
    }

    #[test]
    fn pytest_plugin_path_is_on_env_vars() -> Result<()> {
        let ctx = ctx_with_defaults();
        let python = match ctx.python_runtime().detect_interpreter() {
            Ok(path) => path,
            Err(_) => return Ok(()),
        };
        let temp = tempdir()?;
        let py_ctx = PythonContext {
            project_root: temp.path().to_path_buf(),
            project_name: "demo".into(),
            python,
            pythonpath: temp.path().display().to_string(),
            allowed_paths: vec![temp.path().to_path_buf()],
            site_bin: None,
            pep582_bin: Vec::new(),
            px_options: PxOptions::default(),
        };
        let envs = py_ctx.base_env(&json!({}))?;
        let (envs, _cmd) = build_pytest_invocation(&ctx, &py_ctx, envs, &[], TestReporter::Px)?;
        let plugin_dir = temp.path().join(".px").join("plugins");
        let pythonpath = envs
            .iter()
            .find(|(k, _)| k == "PYTHONPATH")
            .map(|(_, v)| v)
            .cloned()
            .unwrap_or_default();
        let allowed = envs
            .iter()
            .find(|(k, _)| k == "PX_ALLOWED_PATHS")
            .map(|(_, v)| v)
            .cloned()
            .unwrap_or_default();
        let py_entries: Vec<_> = std::env::split_paths(&pythonpath).collect();
        let allowed_entries: Vec<_> = std::env::split_paths(&allowed).collect();
        assert!(
            py_entries.iter().any(|entry| entry == &plugin_dir),
            "PYTHONPATH should include the px pytest plugin dir"
        );
        assert!(
            allowed_entries.iter().any(|entry| entry == &plugin_dir),
            "PX_ALLOWED_PATHS should include the px pytest plugin dir"
        );
        Ok(())
    }

    #[test]
    fn missing_pytest_detection_targets_pytest_module_only() {
        assert!(missing_pytest(
            "ModuleNotFoundError: No module named 'pytest'\n"
        ));
        assert!(missing_pytest("ImportError: No module named pytest"));
        assert!(!missing_pytest(
            "ModuleNotFoundError: No module named 'px_pytest_plugin'\n"
        ));
    }

    #[test]
    fn pytest_command_prefers_tests_dir() -> Result<()> {
        let temp = tempdir()?;
        fs::create_dir_all(temp.path().join("tests"))?;

        let cmd = build_pytest_command(temp.path(), &[]);
        assert_eq!(cmd, vec!["-m", "pytest", "tests"]);
        Ok(())
    }

    #[test]
    fn default_pytest_flags_keep_warnings_enabled() {
        let flags = default_pytest_flags(TestReporter::Px);
        assert_eq!(
            flags,
            vec!["--color=yes", "--tb=short", "--ignore=.px", "-q"]
        );
    }

    #[test]
    fn default_pytest_flags_pytest_reporter_matches() {
        let flags = default_pytest_flags(TestReporter::Pytest);
        assert_eq!(
            flags,
            vec!["--color=yes", "--tb=short", "--ignore=.px", "-q"]
        );
    }

    #[test]
    fn pytest_command_falls_back_to_test_dir() -> Result<()> {
        let temp = tempdir()?;
        fs::create_dir_all(temp.path().join("test"))?;

        let cmd = build_pytest_command(temp.path(), &[]);
        assert_eq!(cmd, vec!["-m", "pytest", "test"]);
        Ok(())
    }

    #[test]
    fn pytest_command_respects_user_args() {
        let temp = tempdir().expect("tempdir");
        fs::create_dir_all(temp.path().join("test")).expect("create test dir");

        let cmd = build_pytest_command(
            temp.path(),
            &["-k".to_string(), "unit".to_string(), "extra".to_string()],
        );
        assert_eq!(cmd, vec!["-m", "pytest", "-k", "unit", "extra"]);
    }

    #[test]
    fn prefers_tests_runtests_script() -> Result<()> {
        let temp = tempdir()?;
        let root = temp.path();
        fs::write(root.join("runtests.py"), "print('root')")?;
        fs::create_dir_all(root.join("tests"))?;
        fs::write(root.join("tests/runtests.py"), "print('tests')")?;

        let detected = find_runtests_script(root).expect("script detected");
        assert_eq!(
            detected,
            root.join("tests").join("runtests.py"),
            "tests/runtests.py should be preferred over root runtests.py"
        );
        Ok(())
    }

    #[test]
    fn merged_pythonpath_keeps_extra_entries() {
        let envs = vec![
            ("PX_ALLOWED_PATHS".into(), "/a:/b".into()),
            ("PYTHONPATH".into(), "/extra:/b".into()),
        ];
        let merged = merged_pythonpath(&envs).expect("merged path");
        let entries: Vec<_> = std::env::split_paths(&merged)
            .map(|p| p.display().to_string())
            .collect();
        assert_eq!(entries, vec!["/a", "/b", "/extra"]);
    }

    fn git(cwd: &Path, args: &[&str]) -> Result<String> {
        let output = Command::new("git").args(args).current_dir(cwd).output()?;
        if !output.status.success() {
            bail!(
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }
}
