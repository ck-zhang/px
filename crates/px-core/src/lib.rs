use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    env,
    ffi::OsString,
    fmt,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    process::Command,
    str::FromStr,
    time::Duration,
};

use anyhow::{anyhow, bail, Context, Result};
use dirs_next::home_dir;
use pep440_rs::Version;
use pep508_rs::Requirement as PepRequirement;
use px_python;
use px_resolver::{ResolveRequest as ResolverRequest, ResolverEnv, ResolverTags};
use px_runtime::{self, RunOutput};
use px_store::{
    cache_wheel, ensure_sdist_build, prefetch_artifacts, ArtifactRequest,
    PrefetchOptions as StorePrefetchOptions, PrefetchSpec as StorePrefetchSpec,
    PrefetchSummary as StorePrefetchSummary, SdistRequest,
};
use reqwest::{blocking::Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, Value as TomlValue};

const LOCK_VERSION: i64 = 1;
const LOCK_MODE_PINNED: &str = "p0-pinned";
const PYPI_BASE_URL: &str = "https://pypi.org/pypi";
const PX_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GlobalOptions {
    pub quiet: bool,
    pub verbose: u8,
    pub trace: bool,
    pub json: bool,
    pub config: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CommandGroup {
    Project,
    Workflow,
    Quality,
    Output,
    Infra,
    Lock,
    Workspace,
    Store,
}

impl fmt::Display for CommandGroup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            CommandGroup::Project => "project",
            CommandGroup::Workflow => "workflow",
            CommandGroup::Quality => "quality",
            CommandGroup::Output => "output",
            CommandGroup::Infra => "infra",
            CommandGroup::Lock => "lock",
            CommandGroup::Workspace => "workspace",
            CommandGroup::Store => "store",
        };
        f.write_str(name)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PxCommand {
    pub group: CommandGroup,
    pub name: String,
    #[serde(default)]
    pub specs: Vec<String>,
    #[serde(default)]
    pub args: Value,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub force: bool,
}

impl PxCommand {
    pub fn new(
        group: CommandGroup,
        name: impl Into<String>,
        specs: Vec<String>,
        args: Value,
        dry_run: bool,
        force: bool,
    ) -> Self {
        Self {
            group,
            name: name.into(),
            specs,
            args,
            dry_run,
            force,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionOutcome {
    pub status: CommandStatus,
    pub message: String,
    #[serde(default)]
    pub details: Value,
}

impl ExecutionOutcome {
    pub fn success(message: impl Into<String>, details: Value) -> Self {
        Self {
            status: CommandStatus::Ok,
            message: message.into(),
            details,
        }
    }

    pub fn failure(message: impl Into<String>, details: Value) -> Self {
        Self {
            status: CommandStatus::Failure,
            message: message.into(),
            details,
        }
    }

    pub fn user_error(message: impl Into<String>, details: Value) -> Self {
        Self {
            status: CommandStatus::UserError,
            message: message.into(),
            details,
        }
    }
}

#[derive(thiserror::Error, Debug)]
#[error("{message}")]
struct InstallUserError {
    message: String,
    details: Value,
}

impl InstallUserError {
    fn new(message: impl Into<String>, details: Value) -> Self {
        Self {
            message: message.into(),
            details,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CommandStatus {
    Ok,
    UserError,
    Failure,
}

pub fn execute(_global: &GlobalOptions, command: &PxCommand) -> Result<ExecutionOutcome> {
    match (command.group, command.name.as_str()) {
        (CommandGroup::Infra, "env") => handle_env(command),
        (CommandGroup::Infra, "cache") => handle_cache(command),
        (CommandGroup::Workflow, "run") => handle_run(command),
        (CommandGroup::Workflow, "test") => handle_test(command),
        (CommandGroup::Project, "init") => handle_project_init(command),
        (CommandGroup::Project, "add") => handle_project_add(command),
        (CommandGroup::Project, "remove") => handle_project_remove(command),
        (CommandGroup::Project, "install") => handle_project_install(command),
        (CommandGroup::Quality, "tidy") => handle_tidy(command),
        (CommandGroup::Lock, "diff") => handle_lock_diff(command),
        (CommandGroup::Lock, "upgrade") => handle_lock_upgrade(command),
        (CommandGroup::Workspace, "list") => handle_workspace_list(command),
        (CommandGroup::Workspace, "verify") => handle_workspace_verify(command),
        (CommandGroup::Workspace, "install") => handle_workspace_install(command),
        (CommandGroup::Workspace, "tidy") => handle_workspace_tidy(command),
        (CommandGroup::Store, "prefetch") => handle_store_prefetch(command),
        _ => Ok(default_outcome(command)),
    }
}

fn handle_env(command: &PxCommand) -> Result<ExecutionOutcome> {
    let mode = command
        .args
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("info")
        .to_lowercase();
    match mode.as_str() {
        "python" => {
            let interpreter = px_python::detect_interpreter()?;
            Ok(ExecutionOutcome::success(
                interpreter.clone(),
                json!({ "interpreter": interpreter }),
            ))
        }
        "info" => {
            let ctx = PythonContext::new()?;
            let details = env_details(&ctx);
            Ok(ExecutionOutcome::success(
                format!(
                    "interpreter: {}\nproject root: {}",
                    ctx.python,
                    ctx.project_root.display()
                ),
                details,
            ))
        }
        "paths" => {
            let ctx = PythonContext::new()?;
            let details = env_details(&ctx);
            let mut lines = vec![
                format!("Interpreter: {}", ctx.python),
                format!("Project root: {}", ctx.project_root.display()),
                "PYTHONPATH:".to_string(),
            ];
            let pythonpath_os = OsString::from(&ctx.pythonpath);
            let os_paths = env::split_paths(&pythonpath_os)
                .map(|p| format!("  {}", p.display()))
                .collect::<Vec<_>>();
            lines.extend(os_paths);
            Ok(ExecutionOutcome::success(lines.join("\n"), details))
        }
        other => bail!("px env mode `{other}` not implemented"),
    }
}

fn handle_run(command: &PxCommand) -> Result<ExecutionOutcome> {
    let ctx = PythonContext::new()?;
    let entry = command
        .args
        .get("entry")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("sample_px_app.cli");
    let extra_args = array_arg(command, "args");
    let mut python_args = vec!["-m".to_string(), entry.to_string()];
    python_args.extend(extra_args);

    let mut envs = ctx.base_env(command)?;
    envs.push(("PX_RUN_ENTRY".into(), entry.to_string()));

    let output = px_runtime::run_command(&ctx.python, &python_args, &envs, &ctx.project_root)?;
    Ok(outcome_from_output(&command.name, entry, output, "px run"))
}

fn handle_test(command: &PxCommand) -> Result<ExecutionOutcome> {
    let ctx = PythonContext::new()?;
    let mut envs = ctx.base_env(command)?;
    envs.push(("PX_TEST_RUNNER".into(), "pytest".into()));

    if env::var("PX_TEST_FALLBACK_STD").is_ok() {
        return run_builtin_tests(command, ctx, envs);
    }

    let mut pytest_args = vec!["-m".to_string(), "pytest".to_string(), "tests".to_string()];
    pytest_args.extend(array_arg(command, "pytest_args"));

    let output = px_runtime::run_command(&ctx.python, &pytest_args, &envs, &ctx.project_root)?;
    if output.code == 0 {
        return Ok(outcome_from_output(
            &command.name,
            "pytest",
            output,
            "px test",
        ));
    }

    if missing_pytest(&output.stderr) {
        return run_builtin_tests(command, ctx, envs);
    }

    Ok(ExecutionOutcome::failure(
        format!("px test failed (exit {})", output.code),
        json!({
            "stdout": output.stdout,
            "stderr": output.stderr,
            "code": output.code,
        }),
    ))
}

fn run_builtin_tests(
    command: &PxCommand,
    ctx: PythonContext,
    mut envs: Vec<(String, String)>,
) -> Result<ExecutionOutcome> {
    envs.push(("PX_TEST_RUNNER".into(), "builtin".into()));
    let script = "from sample_px_app import cli\nassert cli.greet() == 'Hello, World!'\nprint('px fallback test passed')";
    let args = vec!["-c".to_string(), script.to_string()];
    let output = px_runtime::run_command(&ctx.python, &args, &envs, &ctx.project_root)?;
    Ok(outcome_from_output(
        &command.name,
        "builtin",
        output,
        "px test",
    ))
}

fn default_outcome(command: &PxCommand) -> ExecutionOutcome {
    let details = json!({
        "specs": command.specs.clone(),
        "args": command.args.clone(),
        "dry_run": command.dry_run,
        "force": command.force,
    });
    ExecutionOutcome::success(
        format!("stubbed {} {}", command.group, command.name),
        details,
    )
}

fn env_details(ctx: &PythonContext) -> Value {
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

fn handle_cache(command: &PxCommand) -> Result<ExecutionOutcome> {
    let mode = command
        .args
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("path")
        .to_lowercase();
    match mode.as_str() {
        "path" => cache_path_outcome(),
        "stats" => cache_stats_outcome(),
        "prune" => {
            let all = command
                .args
                .get("all")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let dry_run = command
                .args
                .get("dry_run")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            cache_prune_outcome(all, dry_run)
        }
        other => bail!("px cache mode `{other}` not implemented"),
    }
}

fn handle_store_prefetch(command: &PxCommand) -> Result<ExecutionOutcome> {
    let dry_run = command.dry_run;
    if !dry_run && env::var("PX_ONLINE").ok().as_deref() != Some("1") {
        return Ok(ExecutionOutcome::user_error(
            "px store prefetch requires PX_ONLINE=1",
            json!({
                "dry_run": dry_run,
                "hint": "export PX_ONLINE=1 or add --dry-run to inspect work without downloading",
            }),
        ));
    }

    let workspace_mode = command
        .args
        .get("workspace")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    if workspace_mode {
        handle_workspace_prefetch(dry_run)
    } else {
        handle_project_prefetch(dry_run)
    }
}

fn handle_project_prefetch(dry_run: bool) -> Result<ExecutionOutcome> {
    let snapshot = manifest_snapshot()?;
    let lock = match maybe_load_lock_snapshot(&snapshot.lock_path)? {
        Some(lock) => lock,
        None => {
            return Ok(ExecutionOutcome::user_error(
                "px.lock not found (run `px install`)",
                json!({
                    "lockfile": snapshot.lock_path.display().to_string(),
                    "hint": "run `px install` to regenerate the lockfile",
                }),
            ))
        }
    };

    let specs = match prefetch_specs_from_lock(&lock) {
        Ok(specs) => specs,
        Err(err) => {
            return Ok(ExecutionOutcome::user_error(
                err.to_string(),
                json!({ "lockfile": snapshot.lock_path.display().to_string() }),
            ))
        }
    };

    if specs.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            "px.lock does not contain artifact metadata",
            json!({ "lockfile": snapshot.lock_path.display().to_string() }),
        ));
    }

    let cache = resolve_cache_store_path()?;
    let store_specs: Vec<_> = specs.iter().map(|spec| spec.as_px_spec()).collect();
    let summary = prefetch_artifacts(
        &cache.path,
        &store_specs,
        StorePrefetchOptions {
            dry_run,
            parallel: 4,
        },
    )?;

    let details = json!({
        "lockfile": snapshot.lock_path.display().to_string(),
        "cache": {
            "path": cache.path.display().to_string(),
            "source": cache.source,
        },
        "dry_run": dry_run,
        "summary": summary,
    });

    if summary.failed > 0 {
        return Ok(ExecutionOutcome::user_error(
            "px store prefetch encountered errors",
            details,
        ));
    }

    let message = if dry_run {
        format!(
            "prefetch dry-run: {} artifacts ({} hit)",
            summary.requested, summary.hit
        )
    } else {
        format!(
            "prefetched {} artifacts ({} hit, {} fetched)",
            summary.requested, summary.hit, summary.fetched
        )
    };

    Ok(ExecutionOutcome::success(message, details))
}

fn handle_workspace_prefetch(dry_run: bool) -> Result<ExecutionOutcome> {
    let workspace = read_workspace_definition()?;
    if workspace.members.is_empty() {
        return Ok(workspace_missing_members_outcome(&workspace));
    }

    let cache = resolve_cache_store_path()?;
    let mut totals = StorePrefetchSummary::default();
    let mut members = Vec::new();
    let mut had_error = false;

    for member in &workspace.members {
        let lockfile = member.abs_path.join("px.lock").display().to_string();
        let mut status = "ok".to_string();
        let mut error = None;
        let mut summary = StorePrefetchSummary::default();

        if !member.exists {
            status = "missing-manifest".to_string();
            error = Some(format!(
                "manifest not found at {}",
                member.manifest_path.display()
            ));
            had_error = true;
        } else {
            match manifest_snapshot_at(&member.abs_path) {
                Ok(snapshot) => match maybe_load_lock_snapshot(&snapshot.lock_path)? {
                    Some(lock) => match prefetch_specs_from_lock(&lock) {
                        Ok(specs) => {
                            if specs.is_empty() {
                                status = "missing-artifacts".to_string();
                                error =
                                    Some("px.lock does not contain artifact metadata".to_string());
                                had_error = true;
                            } else {
                                let store_specs: Vec<_> =
                                    specs.iter().map(|spec| spec.as_px_spec()).collect();
                                match prefetch_artifacts(
                                    &cache.path,
                                    &store_specs,
                                    StorePrefetchOptions {
                                        dry_run,
                                        parallel: 4,
                                    },
                                ) {
                                    Ok(result) => {
                                        summary = result;
                                        if summary.failed > 0 {
                                            status = "prefetch-error".to_string();
                                            error = summary.errors.first().cloned();
                                            had_error = true;
                                        }
                                    }
                                    Err(err) => {
                                        status = "prefetch-error".to_string();
                                        summary.requested = store_specs.len();
                                        summary.failed = store_specs.len();
                                        summary.errors.push(err.to_string());
                                        error = Some(err.to_string());
                                        had_error = true;
                                    }
                                }
                            }
                        }
                        Err(err) => {
                            status = "lock-error".to_string();
                            error = Some(err.to_string());
                            had_error = true;
                        }
                    },
                    None => {
                        status = "missing-lock".to_string();
                        error = Some("px.lock not found (run `px install`)".to_string());
                        had_error = true;
                    }
                },
                Err(err) => {
                    status = "manifest-error".to_string();
                    error = Some(err.to_string());
                    had_error = true;
                }
            }
        }

        accumulate_prefetch_summary(&mut totals, &summary);
        members.push(PrefetchWorkspaceMember {
            name: member.name.clone(),
            path: member.rel_path.clone(),
            lockfile: Some(lockfile),
            status,
            summary: summary.clone(),
            error,
        });
    }

    let names = workspace
        .members
        .iter()
        .map(|member| member.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");

    let message = if dry_run {
        format!(
            "workspace prefetch dry-run ({names}): {} artifacts ({} hit)",
            totals.requested, totals.hit
        )
    } else {
        format!(
            "workspace prefetch ({names}): {} artifacts ({} hit, {} fetched)",
            totals.requested, totals.hit, totals.fetched
        )
    };

    let details = json!({
        "cache": {
            "path": cache.path.display().to_string(),
            "source": cache.source,
        },
        "dry_run": dry_run,
        "workspace": {
            "root": workspace.root.display().to_string(),
            "members": members,
            "totals": totals,
        }
    });

    if had_error {
        Ok(ExecutionOutcome::user_error(
            "workspace prefetch encountered errors",
            details,
        ))
    } else {
        Ok(ExecutionOutcome::success(message, details))
    }
}

fn cache_path_outcome() -> Result<ExecutionOutcome> {
    let cache = resolve_cache_store_path()?;
    fs::create_dir_all(&cache.path).context("unable to create cache directory")?;
    let canonical = fs::canonicalize(&cache.path).unwrap_or(cache.path.clone());
    let message = canonical.display().to_string();
    Ok(ExecutionOutcome::success(
        message.clone(),
        json!({
            "path": message,
            "source": cache.source,
        }),
    ))
}

fn cache_stats_outcome() -> Result<ExecutionOutcome> {
    let cache = resolve_cache_store_path()?;
    let usage = compute_cache_usage(&cache.path)?;
    let message = if usage.exists {
        format!(
            "cache stats: {} files, {} bytes",
            usage.total_entries, usage.total_size_bytes
        )
    } else {
        format!("cache directory not found at {}", cache.path.display())
    };
    Ok(ExecutionOutcome::success(
        message,
        json!({
            "cache_path": cache.path.display().to_string(),
            "cache_exists": usage.exists,
            "total_entries": usage.total_entries,
            "total_size_bytes": usage.total_size_bytes,
        }),
    ))
}

fn cache_prune_outcome(all: bool, dry_run: bool) -> Result<ExecutionOutcome> {
    let cache = resolve_cache_store_path()?;
    if !all {
        return Ok(ExecutionOutcome::user_error(
            "px cache prune currently requires --all",
            json!({
                "cache_path": cache.path.display().to_string(),
                "dry_run": dry_run,
                "hint": "rerun with --all to prune every cached artifact",
            }),
        ));
    }

    if !cache.path.exists() {
        return Ok(ExecutionOutcome::success(
            format!("cache directory not found at {}", cache.path.display()),
            json!({
                "cache_path": cache.path.display().to_string(),
                "cache_exists": false,
                "dry_run": dry_run,
                "candidate_entries": 0,
                "candidate_size_bytes": 0,
                "deleted_entries": 0,
                "deleted_size_bytes": 0,
                "errors": [],
                "status": "no-cache",
            }),
        ));
    }

    let walk = collect_cache_walk(&cache.path)?;
    let candidate_entries = walk.files.len() as u64;
    let candidate_size_bytes = walk.total_bytes;

    if candidate_entries == 0 {
        return Ok(ExecutionOutcome::success(
            format!(
                "cache prune: nothing to remove under {}",
                cache.path.display()
            ),
            json!({
                "cache_path": cache.path.display().to_string(),
                "cache_exists": true,
                "dry_run": dry_run,
                "candidate_entries": 0,
                "candidate_size_bytes": 0,
                "deleted_entries": 0,
                "deleted_size_bytes": 0,
                "errors": [],
                "status": if dry_run { "dry-run" } else { "success" },
            }),
        ));
    }

    if dry_run {
        return Ok(ExecutionOutcome::success(
            format!(
                "cache prune (dry-run): would remove {} files ({candidate_size_bytes} bytes)",
                candidate_entries
            ),
            json!({
                "cache_path": cache.path.display().to_string(),
                "cache_exists": true,
                "dry_run": true,
                "candidate_entries": candidate_entries,
                "candidate_size_bytes": candidate_size_bytes,
                "deleted_entries": 0,
                "deleted_size_bytes": 0,
                "errors": [],
                "status": "dry-run",
            }),
        ));
    }

    let mut deleted_entries = 0u64;
    let mut deleted_size_bytes = 0u64;
    let mut errors = Vec::new();
    for entry in &walk.files {
        match fs::remove_file(&entry.path) {
            Ok(_) => {
                deleted_entries += 1;
                deleted_size_bytes += entry.size;
            }
            Err(err) => errors.push(json!({
                "path": entry.path.display().to_string(),
                "error": err.to_string(),
            })),
        }
    }

    for dir in walk.dirs.iter().rev() {
        let _ = fs::remove_dir(dir);
    }

    let error_count = errors.len();
    let details = json!({
        "cache_path": cache.path.display().to_string(),
        "cache_exists": true,
        "dry_run": false,
        "candidate_entries": candidate_entries,
        "candidate_size_bytes": candidate_size_bytes,
        "deleted_entries": deleted_entries,
        "deleted_size_bytes": deleted_size_bytes,
        "errors": errors,
        "status": if error_count == 0 { "success" } else { "partial" },
    });

    if error_count == 0 {
        Ok(ExecutionOutcome::success(
            format!(
                "cache prune: removed {} files ({deleted_size_bytes} bytes)",
                deleted_entries
            ),
            details,
        ))
    } else {
        Ok(ExecutionOutcome::failure(
            format!(
                "cache prune: removed {} files but {} errors occurred",
                deleted_entries, error_count
            ),
            details,
        ))
    }
}

struct CacheUsage {
    exists: bool,
    total_entries: u64,
    total_size_bytes: u64,
}

fn compute_cache_usage(path: &Path) -> Result<CacheUsage> {
    if !path.exists() {
        return Ok(CacheUsage {
            exists: false,
            total_entries: 0,
            total_size_bytes: 0,
        });
    }

    let mut total_entries = 0u64;
    let mut total_size_bytes = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let entry_path = entry.path();
            let metadata = entry.metadata()?;
            if metadata.is_dir() {
                stack.push(entry_path);
            } else if metadata.is_file() {
                total_entries += 1;
                total_size_bytes += metadata.len();
            }
        }
    }

    Ok(CacheUsage {
        exists: true,
        total_entries,
        total_size_bytes,
    })
}

#[derive(Default)]
struct CacheWalk {
    files: Vec<CacheEntry>,
    dirs: Vec<PathBuf>,
    total_bytes: u64,
}

#[derive(Clone)]
struct CacheEntry {
    path: PathBuf,
    size: u64,
}

fn collect_cache_walk(path: &Path) -> Result<CacheWalk> {
    let mut walk = CacheWalk::default();
    if !path.exists() {
        return Ok(walk);
    }

    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let entry_path = entry.path();
            let metadata = entry.metadata()?;
            if metadata.is_dir() {
                stack.push(entry_path.clone());
                if entry_path != path {
                    walk.dirs.push(entry_path);
                }
            } else if metadata.is_file() {
                let size = metadata.len();
                walk.total_bytes += size;
                walk.files.push(CacheEntry {
                    path: entry_path,
                    size,
                });
            }
        }
    }

    walk.files.sort_by(|a, b| a.path.cmp(&b.path));
    walk.dirs.sort();
    Ok(walk)
}

fn workspace_missing_members_outcome(workspace: &WorkspaceDefinition) -> ExecutionOutcome {
    ExecutionOutcome::user_error(
        "no [tool.px.workspace] members declared",
        json!({
            "workspace": {
                "root": workspace.root.display().to_string(),
                "members": Vec::<Value>::new(),
            },
            "hint": "add [tool.px.workspace].members entries in pyproject.toml",
        }),
    )
}

fn finalize_workspace_outcome(
    label: &str,
    workspace: WorkspaceDefinition,
    reports: Vec<WorkspaceMemberReport>,
    stats: WorkspaceStats,
) -> Result<ExecutionOutcome> {
    let total = reports.len();
    let details = workspace_details(&workspace, &reports, &stats);
    let summary = workspace_summary(label, &stats, total);
    if stats.has_error() {
        Ok(ExecutionOutcome::user_error(summary, details))
    } else {
        Ok(ExecutionOutcome::success(summary, details))
    }
}

fn workspace_details(
    workspace: &WorkspaceDefinition,
    reports: &[WorkspaceMemberReport],
    stats: &WorkspaceStats,
) -> Value {
    json!({
        "workspace": {
            "root": workspace.root.display().to_string(),
            "counts": stats.counts_value(reports.len()),
            "members": reports.iter().map(|r| r.to_json()).collect::<Vec<_>>(),
        }
    })
}

fn workspace_summary(label: &str, stats: &WorkspaceStats, total: usize) -> String {
    if stats.has_error() {
        format!(
            "{label}: {}/{} clean, {} drifted, {} failed",
            stats.ok, total, stats.drifted, stats.failed
        )
    } else {
        format!("{label}: all {total} members clean")
    }
}

fn read_workspace_definition() -> Result<WorkspaceDefinition> {
    let root = current_project_root()?;
    let manifest_path = root.join("pyproject.toml");
    ensure_pyproject_exists(&manifest_path)?;
    let contents = fs::read_to_string(&manifest_path)?;
    let doc: DocumentMut = contents.parse()?;

    let members_item = doc
        .get("tool")
        .and_then(Item::as_table)
        .and_then(|tool| tool.get("px"))
        .and_then(Item::as_table)
        .and_then(|px| px.get("workspace"))
        .and_then(Item::as_table)
        .and_then(|workspace| workspace.get("members"));

    let mut members = Vec::new();
    if let Some(item) = members_item {
        if let Some(array) = item.as_array() {
            for value in array.iter() {
                if let Some(rel) = value.as_str() {
                    let rel_path = rel.to_string();
                    let abs_path = root.join(rel);
                    let member_manifest = abs_path.join("pyproject.toml");
                    let exists = member_manifest.exists();
                    let name = if exists {
                        discover_project_name(&member_manifest).unwrap_or_else(|| rel_path.clone())
                    } else {
                        rel_path.clone()
                    };
                    let lock_exists = abs_path.join("px.lock").exists();
                    members.push(WorkspaceMember {
                        name,
                        rel_path,
                        abs_path,
                        manifest_path: member_manifest,
                        exists,
                        lock_exists,
                    });
                }
            }
        }
    }

    Ok(WorkspaceDefinition { root, members })
}

fn discover_project_name(manifest_path: &Path) -> Option<String> {
    let contents = fs::read_to_string(manifest_path).ok()?;
    let doc: DocumentMut = contents.parse().ok()?;
    doc.get("project")
        .and_then(Item::as_table)
        .and_then(|table| table.get("name"))
        .and_then(Item::as_str)
        .map(|s| s.to_string())
}

fn analyze_lock_diff(snapshot: &ManifestSnapshot, lock: &LockSnapshot) -> LockDiffReport {
    let mut report = LockDiffReport::default();
    let manifest_map = spec_map(&snapshot.dependencies);
    let lock_map = spec_map(&lock.dependencies);

    for (name, spec) in &manifest_map {
        match lock_map.get(name) {
            Some(lock_spec) => {
                if *lock_spec != *spec {
                    report.changed.push(ChangedEntry {
                        name: name.clone(),
                        from: (*lock_spec).clone(),
                        to: (*spec).clone(),
                    });
                }
            }
            None => report.added.push(DiffEntry {
                name: name.clone(),
                specifier: (*spec).clone(),
                source: "pyproject",
            }),
        }
    }

    for (name, spec) in &lock_map {
        if !manifest_map.contains_key(name) {
            report.removed.push(DiffEntry {
                name: name.clone(),
                specifier: (*spec).clone(),
                source: "px.lock",
            });
        }
    }

    match lock.project_name.as_deref() {
        Some(name) if name == snapshot.name => {}
        Some(name) => {
            report.project_mismatch = Some(ProjectMismatch {
                manifest: snapshot.name.clone(),
                lock: Some(name.to_string()),
            })
        }
        None => {
            report.project_mismatch = Some(ProjectMismatch {
                manifest: snapshot.name.clone(),
                lock: None,
            })
        }
    }

    match lock.python_requirement.as_ref() {
        Some(req) if req == &snapshot.python_requirement => {}
        Some(req) => {
            report.python_mismatch = Some(PythonMismatch {
                manifest: snapshot.python_requirement.clone(),
                lock: Some(req.clone()),
            })
        }
        None => {
            report.python_mismatch = Some(PythonMismatch {
                manifest: snapshot.python_requirement.clone(),
                lock: None,
            })
        }
    }

    if lock.version != LOCK_VERSION && lock.version != 2 {
        report.version_mismatch = Some(VersionMismatch {
            expected: LOCK_VERSION,
            found: lock.version,
        });
    }

    if lock.mode.as_deref() != Some(LOCK_MODE_PINNED) {
        report.mode_mismatch = Some(ModeMismatch {
            expected: LOCK_MODE_PINNED,
            found: lock.mode.clone(),
        });
    }

    report
}

fn spec_map<'a>(specs: &'a [String]) -> HashMap<String, &'a String> {
    let mut map = HashMap::new();
    for spec in specs {
        map.insert(dependency_name(spec), spec);
    }
    map
}

#[derive(Default)]
struct LockDiffReport {
    added: Vec<DiffEntry>,
    removed: Vec<DiffEntry>,
    changed: Vec<ChangedEntry>,
    python_mismatch: Option<PythonMismatch>,
    version_mismatch: Option<VersionMismatch>,
    mode_mismatch: Option<ModeMismatch>,
    project_mismatch: Option<ProjectMismatch>,
}

#[derive(Clone)]
struct DiffEntry {
    name: String,
    specifier: String,
    source: &'static str,
}

#[derive(Clone)]
struct ChangedEntry {
    name: String,
    from: String,
    to: String,
}

struct PythonMismatch {
    manifest: String,
    lock: Option<String>,
}

struct VersionMismatch {
    expected: i64,
    found: i64,
}

struct ModeMismatch {
    expected: &'static str,
    found: Option<String>,
}

struct ProjectMismatch {
    manifest: String,
    lock: Option<String>,
}

struct WorkspaceDefinition {
    root: PathBuf,
    members: Vec<WorkspaceMember>,
}

struct WorkspaceMember {
    name: String,
    rel_path: String,
    abs_path: PathBuf,
    manifest_path: PathBuf,
    exists: bool,
    lock_exists: bool,
}

enum WorkspaceMemberStatus {
    Installed,
    UpToDate,
    Verified,
    Tidied,
    Drift,
    MissingLock,
    MissingManifest,
    ManifestError,
    InstallError,
}

impl WorkspaceMemberStatus {
    fn as_str(&self) -> &'static str {
        match self {
            WorkspaceMemberStatus::Installed => "installed",
            WorkspaceMemberStatus::UpToDate => "up-to-date",
            WorkspaceMemberStatus::Verified => "verified",
            WorkspaceMemberStatus::Tidied => "tidied",
            WorkspaceMemberStatus::Drift => "drift",
            WorkspaceMemberStatus::MissingLock => "missing-lock",
            WorkspaceMemberStatus::MissingManifest => "missing-manifest",
            WorkspaceMemberStatus::ManifestError => "manifest-error",
            WorkspaceMemberStatus::InstallError => "install-error",
        }
    }

    fn is_ok(&self) -> bool {
        matches!(
            self,
            WorkspaceMemberStatus::Installed
                | WorkspaceMemberStatus::UpToDate
                | WorkspaceMemberStatus::Verified
                | WorkspaceMemberStatus::Tidied
        )
    }

    fn is_drift(&self) -> bool {
        matches!(
            self,
            WorkspaceMemberStatus::Drift | WorkspaceMemberStatus::MissingLock
        )
    }
}

struct WorkspaceMemberReport {
    name: String,
    path: String,
    status: WorkspaceMemberStatus,
    lockfile: Option<String>,
    drift: Vec<String>,
    error: Option<String>,
}

impl WorkspaceMemberReport {
    fn new(member: &WorkspaceMember) -> Self {
        Self {
            name: member.name.clone(),
            path: member.rel_path.clone(),
            status: WorkspaceMemberStatus::UpToDate,
            lockfile: None,
            drift: Vec::new(),
            error: None,
        }
    }

    fn with_status(mut self, status: WorkspaceMemberStatus) -> Self {
        self.status = status;
        self
    }

    fn lockfile(mut self, path: impl Into<String>) -> Self {
        self.lockfile = Some(path.into());
        self
    }

    fn drift(mut self, drift: Vec<String>) -> Self {
        self.drift = drift;
        self
    }

    fn error(mut self, err: impl Into<String>) -> Self {
        self.error = Some(err.into());
        self
    }

    fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "path": self.path,
            "status": self.status.as_str(),
            "lockfile": self.lockfile,
            "drift": self.drift,
            "error": self.error,
        })
    }
}

#[derive(Serialize)]
struct PrefetchWorkspaceMember {
    name: String,
    path: String,
    lockfile: Option<String>,
    status: String,
    summary: StorePrefetchSummary,
    error: Option<String>,
}

fn accumulate_prefetch_summary(target: &mut StorePrefetchSummary, addition: &StorePrefetchSummary) {
    target.requested += addition.requested;
    target.hit += addition.hit;
    target.fetched += addition.fetched;
    target.failed += addition.failed;
    target.bytes_fetched += addition.bytes_fetched;
    if !addition.errors.is_empty() {
        target.errors.extend(addition.errors.iter().cloned());
    }
}

#[derive(Default)]
struct WorkspaceStats {
    ok: usize,
    drifted: usize,
    failed: usize,
}

impl WorkspaceStats {
    fn update(&mut self, status: &WorkspaceMemberStatus) {
        if status.is_ok() {
            self.ok += 1;
        } else if status.is_drift() {
            self.drifted += 1;
        } else {
            self.failed += 1;
        }
    }

    fn has_error(&self) -> bool {
        self.drifted > 0 || self.failed > 0
    }

    fn counts_value(&self, total: usize) -> Value {
        json!({
            "total": total,
            "ok": self.ok,
            "drifted": self.drifted,
            "failed": self.failed,
        })
    }
}

struct InstallOutcome {
    state: InstallState,
    lockfile: String,
    drift: Vec<String>,
    verified: bool,
}

enum InstallState {
    Installed,
    UpToDate,
    Drift,
    MissingLock,
}

struct TidyOutcome {
    state: TidyState,
    lockfile: String,
    drift: Vec<String>,
}

enum TidyState {
    Clean,
    Drift,
    MissingLock,
}

impl LockDiffReport {
    fn is_clean(&self) -> bool {
        self.added.is_empty()
            && self.removed.is_empty()
            && self.changed.is_empty()
            && self.python_mismatch.is_none()
            && self.version_mismatch.is_none()
            && self.mode_mismatch.is_none()
    }

    fn to_json(&self, snapshot: &ManifestSnapshot) -> Value {
        json!({
            "status": if self.is_clean() { "clean" } else { "drift" },
            "pyproject": snapshot.manifest_path.display().to_string(),
            "lockfile": snapshot.lock_path.display().to_string(),
            "added": self
                .added
                .iter()
                .map(|entry| json!({
                    "name": entry.name,
                    "specifier": entry.specifier,
                    "source": entry.source,
                }))
                .collect::<Vec<_>>(),
            "removed": self
                .removed
                .iter()
                .map(|entry| json!({
                    "name": entry.name,
                    "specifier": entry.specifier,
                    "source": entry.source,
                }))
                .collect::<Vec<_>>(),
            "changed": self
                .changed
                .iter()
                .map(|entry| json!({
                    "name": entry.name,
                    "from": entry.from,
                    "to": entry.to,
                }))
                .collect::<Vec<_>>(),
            "python_mismatch": self.python_mismatch.as_ref().map(|m| json!({
                "manifest": m.manifest,
                "lock": m.lock,
            })),
            "version_mismatch": self.version_mismatch.as_ref().map(|m| json!({
                "expected": m.expected,
                "found": m.found,
            })),
            "mode_mismatch": self.mode_mismatch.as_ref().map(|m| json!({
                "expected": m.expected,
                "found": m.found,
            })),
            "project_mismatch": self.project_mismatch.as_ref().map(|m| json!({
                "manifest": m.manifest,
                "lock": m.lock,
            })),
        })
    }

    fn summary(&self) -> String {
        if self.is_clean() {
            return "px lock diff: clean".to_string();
        }

        let mut chunks = Vec::new();
        if !self.added.is_empty() {
            chunks.push(format!("{} added", self.added.len()));
        }
        if !self.removed.is_empty() {
            chunks.push(format!("{} removed", self.removed.len()));
        }
        if !self.changed.is_empty() {
            chunks.push(format!("{} changed", self.changed.len()));
        }
        if self.python_mismatch.is_some() {
            chunks.push("python mismatch".to_string());
        }
        if self.version_mismatch.is_some() {
            chunks.push("lock version mismatch".to_string());
        }
        if self.mode_mismatch.is_some() {
            chunks.push("mode mismatch".to_string());
        }
        if self.project_mismatch.is_some() {
            chunks.push("project mismatch".to_string());
        }
        if chunks.is_empty() {
            "px lock diff: drift".to_string()
        } else {
            format!("px lock diff: drift ({})", chunks.join(", "))
        }
    }

    fn to_messages(&self) -> Vec<String> {
        let mut msgs = Vec::new();
        for entry in &self.added {
            msgs.push(format!(
                "dependency `{}` present in pyproject but missing from px.lock",
                entry.name
            ));
        }
        for entry in &self.removed {
            msgs.push(format!(
                "dependency `{}` present in px.lock but missing from pyproject",
                entry.name
            ));
        }
        for entry in &self.changed {
            msgs.push(format!(
                "dependency `{}` differs (lock={}, manifest={})",
                entry.name, entry.from, entry.to
            ));
        }
        if let Some(mismatch) = &self.python_mismatch {
            match &mismatch.lock {
                Some(lock_req) => msgs.push(format!(
                    "python requirement differs (lock={}, manifest={})",
                    lock_req, mismatch.manifest
                )),
                None => msgs.push(format!(
                    "python requirement missing in lock (manifest={})",
                    mismatch.manifest
                )),
            }
        }
        if let Some(mismatch) = &self.version_mismatch {
            msgs.push(format!(
                "lock version {} does not match expected {}",
                mismatch.found, mismatch.expected
            ));
        }
        if let Some(mismatch) = &self.mode_mismatch {
            match &mismatch.found {
                Some(found) => msgs.push(format!(
                    "lock metadata mode `{}` does not match expected `{}`",
                    found, mismatch.expected
                )),
                None => msgs.push(format!(
                    "lock metadata mode missing (expected `{}`)",
                    mismatch.expected
                )),
            }
        }
        if let Some(mismatch) = &self.project_mismatch {
            match &mismatch.lock {
                Some(lock_name) => msgs.push(format!(
                    "project name differs (lock={}, manifest={})",
                    lock_name, mismatch.manifest
                )),
                None => msgs.push(format!(
                    "project name missing in lock (manifest={})",
                    mismatch.manifest
                )),
            }
        }
        msgs
    }
}

fn handle_project_init(command: &PxCommand) -> Result<ExecutionOutcome> {
    let package = command
        .args
        .get("package")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("--package <name> is required"))?
        .to_string();
    validate_package_name(&package)?;

    let python_req = command
        .args
        .get("python")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            if s.starts_with('>') {
                s.to_string()
            } else {
                format!(">={s}")
            }
        })
        .unwrap_or_else(|| ">=3.12".to_string());

    let root = current_project_root()?;
    let pyproject_path = root.join("pyproject.toml");
    if pyproject_path.exists() {
        bail!("pyproject.toml already exists in {}", root.display());
    }

    let files = scaffold_project(&root, &package, &python_req)?;
    let message = format!("Initialized project {package}");
    Ok(ExecutionOutcome::success(
        message,
        json!({
            "package": package,
            "python": python_req,
            "files_created": files,
        }),
    ))
}

fn handle_project_add(command: &PxCommand) -> Result<ExecutionOutcome> {
    if command.specs.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            "provide at least one dependency",
            json!({ "hint": "run `px project add name==version`" }),
        ));
    }

    let root = current_project_root()?;
    let pyproject_path = root.join("pyproject.toml");
    ensure_pyproject_exists(&pyproject_path)?;

    let mut doc: DocumentMut = fs::read_to_string(&pyproject_path)?.parse()?;
    let mut deps = read_dependencies(&doc)?;
    let mut added = Vec::new();
    let mut updated = Vec::new();

    for spec in &command.specs {
        let spec = spec.trim();
        if spec.is_empty() {
            continue;
        }
        match upsert_dependency(&mut deps, spec) {
            InsertOutcome::Added(name) => added.push(name),
            InsertOutcome::Updated(name) => updated.push(name),
            InsertOutcome::Unchanged => {}
        }
    }

    sort_and_dedupe(&mut deps);
    if added.is_empty() && updated.is_empty() {
        return Ok(ExecutionOutcome::success(
            "dependencies already satisfied",
            json!({ "pyproject": pyproject_path.display().to_string() }),
        ));
    }

    write_dependencies(&mut doc, &deps)?;
    fs::write(&pyproject_path, doc.to_string())?;

    let message = format!(
        "updated dependencies (added {}, updated {})",
        added.len(),
        updated.len()
    );
    Ok(ExecutionOutcome::success(
        message,
        json!({
            "pyproject": pyproject_path.display().to_string(),
            "added": added,
            "updated": updated,
        }),
    ))
}

fn handle_project_remove(command: &PxCommand) -> Result<ExecutionOutcome> {
    if command.specs.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            "provide at least one dependency to remove",
            json!({ "hint": "run `px project remove name`" }),
        ));
    }

    let root = current_project_root()?;
    let pyproject_path = root.join("pyproject.toml");
    ensure_pyproject_exists(&pyproject_path)?;

    let mut doc: DocumentMut = fs::read_to_string(&pyproject_path)?.parse()?;
    let mut deps = read_dependencies(&doc)?;
    let targets: HashSet<String> = command
        .specs
        .iter()
        .map(|s| dependency_name(s))
        .filter(|s| !s.is_empty())
        .collect();
    if targets.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            "dependencies must contain at least one name",
            json!({ "hint": "use bare names like requests==2.32.3" }),
        ));
    }

    let before = deps.len();
    deps.retain(|spec| !targets.contains(&dependency_name(spec)));
    if deps.len() == before {
        return Ok(ExecutionOutcome::success(
            "no matching dependencies found",
            json!({ "removed": [] }),
        ));
    }

    sort_and_dedupe(&mut deps);
    write_dependencies(&mut doc, &deps)?;
    fs::write(&pyproject_path, doc.to_string())?;

    Ok(ExecutionOutcome::success(
        "removed dependencies",
        json!({
            "pyproject": pyproject_path.display().to_string(),
            "removed": targets,
        }),
    ))
}

fn handle_project_install(command: &PxCommand) -> Result<ExecutionOutcome> {
    let frozen = command
        .args
        .get("frozen")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let snapshot = manifest_snapshot()?;
    let outcome = match install_snapshot(&snapshot, frozen) {
        Ok(ok) => ok,
        Err(err) => match err.downcast::<InstallUserError>() {
            Ok(user) => return Ok(ExecutionOutcome::user_error(user.message, user.details)),
            Err(err) => return Err(err),
        },
    };
    let mut details = json!({
        "lockfile": outcome.lockfile,
        "project": snapshot.name,
        "python": snapshot.python_requirement,
    });

    match outcome.state {
        InstallState::Installed => Ok(ExecutionOutcome::success(
            format!("wrote {}", outcome.lockfile),
            details,
        )),
        InstallState::UpToDate => {
            let message = if frozen && outcome.verified {
                "lockfile verified".to_string()
            } else {
                "px.lock already up to date".to_string()
            };
            Ok(ExecutionOutcome::success(message, details))
        }
        InstallState::Drift => {
            details["drift"] = Value::Array(outcome.drift.iter().map(|d| json!(d)).collect());
            details["hint"] = Value::String("rerun `px install` to refresh px.lock".to_string());
            Ok(ExecutionOutcome::user_error(
                "px.lock is out of date",
                details,
            ))
        }
        InstallState::MissingLock => Ok(ExecutionOutcome::user_error(
            "px.lock not found (run `px install`)",
            json!({
                "lockfile": outcome.lockfile,
                "project": snapshot.name,
                "python": snapshot.python_requirement,
                "hint": "run `px install` to generate a lockfile",
            }),
        )),
    }
}

fn handle_tidy(_command: &PxCommand) -> Result<ExecutionOutcome> {
    let snapshot = manifest_snapshot()?;

    let lock = match maybe_load_lock_snapshot(&snapshot.lock_path)? {
        Some(lock) => lock,
        None => {
            return Ok(ExecutionOutcome::user_error(
                "px.lock not found (run `px install`)",
                json!({ "lockfile": snapshot.lock_path.display().to_string() }),
            ))
        }
    };

    let drift = detect_lock_drift(&snapshot, &lock);
    if drift.is_empty() {
        Ok(ExecutionOutcome::success(
            "workspace tidy",
            json!({ "lockfile": snapshot.lock_path.display().to_string() }),
        ))
    } else {
        Ok(ExecutionOutcome::user_error(
            "px.lock is out of date (run `px install`)",
            json!({
                "lockfile": snapshot.lock_path.display().to_string(),
                "drift": drift,
                "hint": "rerun `px install` to refresh the lockfile",
            }),
        ))
    }
}

fn handle_lock_diff(_command: &PxCommand) -> Result<ExecutionOutcome> {
    let snapshot = manifest_snapshot()?;
    match maybe_load_lock_snapshot(&snapshot.lock_path)? {
        Some(lock) => {
            let report = analyze_lock_diff(&snapshot, &lock);
            let details = report.to_json(&snapshot);
            if report.is_clean() {
                Ok(ExecutionOutcome::success(report.summary(), details))
            } else {
                Ok(ExecutionOutcome::user_error(report.summary(), details))
            }
        }
        None => {
            let details = json!({
                "status": "missing_lock",
                "pyproject": snapshot.manifest_path.display().to_string(),
                "lockfile": snapshot.lock_path.display().to_string(),
                "added": [],
                "removed": [],
                "changed": [],
                "version_mismatch": Value::Null,
                "python_mismatch": Value::Null,
                "mode_mismatch": Value::Null,
                "hint": "run `px install` to generate px.lock before diffing",
            });
            Ok(ExecutionOutcome::user_error(
                format!(
                    "px lock diff: missing px.lock at {} (run `px install`)",
                    snapshot.lock_path.display()
                ),
                details,
            ))
        }
    }
}

fn handle_lock_upgrade(_command: &PxCommand) -> Result<ExecutionOutcome> {
    let snapshot = manifest_snapshot()?;
    let lock_path = snapshot.lock_path.clone();
    let lock = match maybe_load_lock_snapshot(&lock_path)? {
        Some(lock) => lock,
        None => {
            return Ok(ExecutionOutcome::user_error(
                "px.lock not found (run `px install`)",
                json!({
                    "lockfile": lock_path.display().to_string(),
                    "hint": "run `px install` to create a lock before upgrading",
                }),
            ))
        }
    };

    if lock.version >= 2 {
        return Ok(ExecutionOutcome::success(
            "px.lock already at version 2",
            json!({
                "lockfile": lock_path.display().to_string(),
                "version": lock.version,
            }),
        ));
    }

    let upgraded = render_lockfile_v2(&snapshot, &lock)?;
    fs::write(&lock_path, upgraded)?;

    Ok(ExecutionOutcome::success(
        "upgraded px.lock to version 2",
        json!({
            "lockfile": lock_path.display().to_string(),
            "version": 2,
        }),
    ))
}

fn handle_workspace_list(_command: &PxCommand) -> Result<ExecutionOutcome> {
    let workspace = read_workspace_definition()?;
    if workspace.members.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            "no [tool.px.workspace] members declared",
            json!({
                "workspace": {
                    "root": workspace.root.display().to_string(),
                    "members": Vec::<Value>::new(),
                },
                "hint": "add [tool.px.workspace].members to pyproject.toml",
            }),
        ));
    }

    let details = json!({
        "workspace": {
            "root": workspace.root.display().to_string(),
            "members": workspace
                .members
                .iter()
                .map(|member| json!({
                    "name": member.name,
                    "path": member.rel_path,
                    "manifest": member.manifest_path.display().to_string(),
                    "manifest_exists": member.exists,
                    "lock_exists": member.lock_exists,
                }))
                .collect::<Vec<_>>(),
        },
    });

    let names = workspace
        .members
        .iter()
        .map(|m| m.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    Ok(ExecutionOutcome::success(
        format!("workspace members: {names}"),
        details,
    ))
}

fn handle_workspace_verify(_command: &PxCommand) -> Result<ExecutionOutcome> {
    let workspace = read_workspace_definition()?;
    if workspace.members.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            "no [tool.px.workspace] members declared",
            json!({
                "workspace": {
                    "root": workspace.root.display().to_string(),
                    "members": Vec::<Value>::new(),
                }
            }),
        ));
    }

    let mut member_reports = Vec::new();
    let mut has_drift = false;

    for member in &workspace.members {
        if !member.exists {
            has_drift = true;
            member_reports.push(json!({
                "name": member.name,
                "path": member.rel_path,
                "status": "missing-manifest",
                "message": format!("manifest not found at {}", member.manifest_path.display()),
            }));
            continue;
        }

        let snapshot = match manifest_snapshot_at(&member.abs_path) {
            Ok(snapshot) => snapshot,
            Err(err) => {
                has_drift = true;
                member_reports.push(json!({
                    "name": member.name,
                    "path": member.rel_path,
                    "status": "manifest-error",
                    "message": err.to_string(),
                }));
                continue;
            }
        };

        match maybe_load_lock_snapshot(&snapshot.lock_path)? {
            Some(lock) => {
                let report = analyze_lock_diff(&snapshot, &lock);
                if report.is_clean() {
                    member_reports.push(json!({
                        "name": member.name,
                        "path": member.rel_path,
                        "status": "ok",
                        "lockfile": snapshot.lock_path.display().to_string(),
                    }));
                } else {
                    has_drift = true;
                    member_reports.push(json!({
                        "name": member.name,
                        "path": member.rel_path,
                        "status": "drift",
                        "lockfile": snapshot.lock_path.display().to_string(),
                        "drift": report.to_messages(),
                    }));
                }
            }
            None => {
                has_drift = true;
                member_reports.push(json!({
                    "name": member.name,
                    "path": member.rel_path,
                    "status": "missing-lock",
                    "lockfile": snapshot.lock_path.display().to_string(),
                }));
            }
        }
    }

    let mut details = json!({
        "workspace": {
            "root": workspace.root.display().to_string(),
            "members": member_reports,
        }
    });

    if has_drift {
        details["hint"] = Value::String(
            "run `px workspace install` or `px install` inside drifted members".to_string(),
        );
        Ok(ExecutionOutcome::user_error(
            "workspace drift detected (run `px install` inside each member)",
            details,
        ))
    } else {
        Ok(ExecutionOutcome::success("workspace verified", details))
    }
}

fn handle_workspace_install(command: &PxCommand) -> Result<ExecutionOutcome> {
    let frozen = command
        .args
        .get("frozen")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let workspace = read_workspace_definition()?;
    if workspace.members.is_empty() {
        return Ok(workspace_missing_members_outcome(&workspace));
    }

    let mut reports = Vec::new();
    let mut stats = WorkspaceStats::default();

    for member in &workspace.members {
        let mut report = WorkspaceMemberReport::new(member);
        if !member.exists {
            report = report
                .with_status(WorkspaceMemberStatus::MissingManifest)
                .error(format!(
                    "manifest not found at {}",
                    member.manifest_path.display()
                ));
            stats.update(&report.status);
            reports.push(report);
            continue;
        }

        let snapshot = match manifest_snapshot_at(&member.abs_path) {
            Ok(snapshot) => snapshot,
            Err(err) => {
                report = report
                    .with_status(WorkspaceMemberStatus::ManifestError)
                    .error(err.to_string());
                stats.update(&report.status);
                reports.push(report);
                continue;
            }
        };

        match install_snapshot(&snapshot, frozen) {
            Ok(result) => {
                report = report.lockfile(result.lockfile.clone());
                report = match result.state {
                    InstallState::Installed => report.with_status(WorkspaceMemberStatus::Installed),
                    InstallState::UpToDate => {
                        if frozen && result.verified {
                            report.with_status(WorkspaceMemberStatus::Verified)
                        } else {
                            report.with_status(WorkspaceMemberStatus::UpToDate)
                        }
                    }
                    InstallState::Drift => report
                        .with_status(WorkspaceMemberStatus::Drift)
                        .drift(result.drift),
                    InstallState::MissingLock => {
                        report.with_status(WorkspaceMemberStatus::MissingLock)
                    }
                };
            }
            Err(err) => match err.downcast::<InstallUserError>() {
                Ok(user) => {
                    report = report
                        .with_status(WorkspaceMemberStatus::InstallError)
                        .error(user.message);
                }
                Err(err) => {
                    report = report
                        .with_status(WorkspaceMemberStatus::InstallError)
                        .error(err.to_string());
                }
            },
        }

        stats.update(&report.status);
        reports.push(report);
    }

    finalize_workspace_outcome(
        if frozen {
            "workspace install --frozen"
        } else {
            "workspace install"
        },
        workspace,
        reports,
        stats,
    )
}

fn handle_workspace_tidy(_command: &PxCommand) -> Result<ExecutionOutcome> {
    let workspace = read_workspace_definition()?;
    if workspace.members.is_empty() {
        return Ok(workspace_missing_members_outcome(&workspace));
    }

    let mut reports = Vec::new();
    let mut stats = WorkspaceStats::default();

    for member in &workspace.members {
        let mut report = WorkspaceMemberReport::new(member);
        if !member.exists {
            report = report
                .with_status(WorkspaceMemberStatus::MissingManifest)
                .error(format!(
                    "manifest not found at {}",
                    member.manifest_path.display()
                ));
            stats.update(&report.status);
            reports.push(report);
            continue;
        }

        let snapshot = match manifest_snapshot_at(&member.abs_path) {
            Ok(snapshot) => snapshot,
            Err(err) => {
                report = report
                    .with_status(WorkspaceMemberStatus::ManifestError)
                    .error(err.to_string());
                stats.update(&report.status);
                reports.push(report);
                continue;
            }
        };

        match tidy_snapshot(&snapshot) {
            Ok(result) => {
                report = report.lockfile(result.lockfile.clone());
                report = match result.state {
                    TidyState::Clean => report.with_status(WorkspaceMemberStatus::Tidied),
                    TidyState::Drift => report
                        .with_status(WorkspaceMemberStatus::Drift)
                        .drift(result.drift),
                    TidyState::MissingLock => {
                        report.with_status(WorkspaceMemberStatus::MissingLock)
                    }
                };
            }
            Err(err) => {
                report = report
                    .with_status(WorkspaceMemberStatus::InstallError)
                    .error(err.to_string());
            }
        }

        stats.update(&report.status);
        reports.push(report);
    }

    finalize_workspace_outcome("workspace tidy", workspace, reports, stats)
}

fn lock_is_fresh(snapshot: &ManifestSnapshot) -> Result<bool> {
    match maybe_load_lock_snapshot(&snapshot.lock_path)? {
        Some(lock) => Ok(detect_lock_drift(snapshot, &lock).is_empty()),
        None => Ok(false),
    }
}

fn manifest_snapshot() -> Result<ManifestSnapshot> {
    let root = current_project_root()?;
    manifest_snapshot_at(&root)
}

fn manifest_snapshot_at(root: &Path) -> Result<ManifestSnapshot> {
    let manifest_path = root.join("pyproject.toml");
    ensure_pyproject_exists(&manifest_path)?;
    let contents = fs::read_to_string(&manifest_path)?;
    let doc: DocumentMut = contents.parse()?;
    let project = project_table(&doc)?;
    let name = project
        .get("name")
        .and_then(Item::as_str)
        .ok_or_else(|| anyhow!("pyproject missing [project].name"))?
        .to_string();
    let python_requirement = project
        .get("requires-python")
        .and_then(Item::as_str)
        .map(|s| s.to_string())
        .unwrap_or_else(|| ">=3.12".to_string());
    let dependencies = read_dependencies(&doc)?;
    Ok(ManifestSnapshot {
        root: root.to_path_buf(),
        manifest_path,
        lock_path: root.join("px.lock"),
        name,
        python_requirement,
        dependencies,
    })
}

fn install_snapshot(snapshot: &ManifestSnapshot, frozen: bool) -> Result<InstallOutcome> {
    let lockfile = snapshot.lock_path.display().to_string();

    if frozen {
        return verify_lock(snapshot);
    }

    if lock_is_fresh(snapshot)? {
        Ok(InstallOutcome {
            state: InstallState::UpToDate,
            lockfile,
            drift: Vec::new(),
            verified: false,
        })
    } else {
        if let Some(parent) = snapshot.lock_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut dependencies = snapshot.dependencies.clone();
        let mut resolved_override = None;
        if dependencies_require_resolution(&dependencies) && resolver_enabled() {
            let resolved = resolve_dependencies(snapshot)?;
            dependencies = resolved.specs.clone();
            resolved_override = Some(resolved.pins);
            persist_resolved_dependencies(snapshot, &dependencies)?;
        }
        let pins = match resolved_override {
            Some(pins) => pins,
            None => ensure_exact_pins(&dependencies)?,
        };
        let resolved = resolve_pins(&pins)?;
        let contents = render_lockfile(snapshot, &resolved)?;
        fs::write(&snapshot.lock_path, contents)?;
        Ok(InstallOutcome {
            state: InstallState::Installed,
            lockfile,
            drift: Vec::new(),
            verified: false,
        })
    }
}

fn verify_lock(snapshot: &ManifestSnapshot) -> Result<InstallOutcome> {
    let lockfile = snapshot.lock_path.display().to_string();
    match maybe_load_lock_snapshot(&snapshot.lock_path)? {
        Some(lock) => {
            let report = analyze_lock_diff(snapshot, &lock);
            let mut drift = report.to_messages();
            if drift.is_empty() {
                drift = verify_locked_artifacts(&lock);
            }
            if drift.is_empty() {
                Ok(InstallOutcome {
                    state: InstallState::UpToDate,
                    lockfile,
                    drift,
                    verified: true,
                })
            } else {
                Ok(InstallOutcome {
                    state: InstallState::Drift,
                    lockfile,
                    drift,
                    verified: true,
                })
            }
        }
        None => Ok(InstallOutcome {
            state: InstallState::MissingLock,
            lockfile,
            drift: Vec::new(),
            verified: true,
        }),
    }
}

fn verify_locked_artifacts(lock: &LockSnapshot) -> Vec<String> {
    let mut issues = Vec::new();
    for dep in &lock.resolved {
        let Some(artifact) = &dep.artifact else {
            continue;
        };
        if artifact.cached_path.is_empty() {
            issues.push(format!(
                "dependency `{}` missing cached_path in lock",
                dep.name
            ));
            continue;
        }
        let path = PathBuf::from(&artifact.cached_path);
        if !path.exists() {
            issues.push(format!(
                "artifact for `{}` missing at {}",
                dep.name,
                path.display()
            ));
            continue;
        }
        match compute_file_sha256(&path) {
            Ok(actual) if actual == artifact.sha256 => {}
            Ok(actual) => {
                issues.push(format!(
                    "artifact for `{}` has sha256 {} but lock expects {}",
                    dep.name, actual, artifact.sha256
                ));
                continue;
            }
            Err(err) => {
                issues.push(format!(
                    "unable to hash `{}` at {}: {}",
                    dep.name,
                    path.display(),
                    err
                ));
                continue;
            }
        }

        if let Ok(meta) = fs::metadata(&path) {
            if meta.len() != artifact.size {
                issues.push(format!(
                    "artifact for `{}` size mismatch (have {}, lock {})",
                    dep.name,
                    meta.len(),
                    artifact.size
                ));
            }
        }
    }
    issues
}

fn compute_file_sha256(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn dependencies_require_resolution(specs: &[String]) -> bool {
    specs.iter().any(|spec| !spec.trim().contains("=="))
}

fn resolver_enabled() -> bool {
    matches!(env::var("PX_RESOLVER").ok().as_deref(), Some("1"))
}

fn force_sdist_build() -> bool {
    matches!(env::var("PX_FORCE_SDIST").ok().as_deref(), Some("1"))
}

struct ResolvedSpecOutput {
    specs: Vec<String>,
    pins: Vec<PinSpec>,
}

fn resolve_dependencies(snapshot: &ManifestSnapshot) -> Result<ResolvedSpecOutput> {
    let python = px_python::detect_interpreter()?;
    let tags = detect_interpreter_tags_with(&python)?;
    let marker_env = detect_marker_environment_with(&python)?;
    let request = ResolverRequest {
        project: snapshot.name.clone(),
        requirements: snapshot.dependencies.clone(),
        tags: ResolverTags {
            python: tags.python.clone(),
            abi: tags.abi.clone(),
            platform: tags.platform.clone(),
        },
        env: marker_env,
    };
    let resolved = px_resolver::resolve(request).map_err(|err| {
        InstallUserError::new(
            format!("resolver failed: {err}"),
            json!({ "error": err.to_string() }),
        )
    })?;
    let mut specs = Vec::new();
    let mut pins = Vec::new();
    for spec in resolved {
        let formatted = format_specifier(
            &spec.normalized,
            &spec.extras,
            &spec.selected_version,
            spec.marker.as_deref(),
        );
        specs.push(formatted.clone());
        pins.push(PinSpec {
            name: spec.name,
            specifier: formatted,
            version: spec.selected_version,
            normalized: spec.normalized,
            extras: spec.extras,
            marker: spec.marker,
        });
    }
    Ok(ResolvedSpecOutput { specs, pins })
}

fn persist_resolved_dependencies(snapshot: &ManifestSnapshot, specs: &[String]) -> Result<()> {
    let contents = fs::read_to_string(&snapshot.manifest_path)?;
    let mut doc: DocumentMut = contents.parse()?;
    write_dependencies(&mut doc, specs)?;
    fs::write(&snapshot.manifest_path, doc.to_string())?;
    Ok(())
}

fn prefetch_specs_from_lock(lock: &LockSnapshot) -> Result<Vec<PrefetchArtifactSpec>> {
    let mut spec_map = HashMap::new();
    for spec in &lock.dependencies {
        spec_map.insert(dependency_name(spec), spec.clone());
    }

    let mut specs = Vec::new();
    for dep in &lock.resolved {
        let Some(artifact) = &dep.artifact else {
            continue;
        };
        let Some(specifier) = spec_map.get(&dep.name) else {
            bail!("lock entry `{}` missing from dependencies list", dep.name);
        };
        let Some(version) = version_from_specifier(specifier) else {
            bail!("lock entry `{}` is missing a pinned version", dep.name);
        };
        if artifact.filename.is_empty() || artifact.url.is_empty() || artifact.sha256.is_empty() {
            bail!("lock entry `{}` is missing artifact metadata", dep.name);
        }
        specs.push(PrefetchArtifactSpec {
            name: dep.name.clone(),
            version: version.to_string(),
            filename: artifact.filename.clone(),
            url: artifact.url.clone(),
            sha256: artifact.sha256.clone(),
        });
    }
    Ok(specs)
}

fn version_from_specifier(spec: &str) -> Option<&str> {
    spec.trim()
        .split_once("==")
        .map(|(_, version)| version.trim())
}

fn ensure_exact_pins(specs: &[String]) -> Result<Vec<PinSpec>> {
    let mut pins = Vec::new();
    for spec in specs {
        pins.push(parse_exact_pin(spec)?);
    }
    Ok(pins)
}

fn parse_exact_pin(spec: &str) -> Result<PinSpec> {
    let trimmed = spec.trim().trim_matches(|c| c == '\"' || c == '\'');
    if trimmed.is_empty() {
        return Err(InstallUserError::new(
            "dependency specifier cannot be empty",
            json!({ "specifier": spec }),
        )
        .into());
    }
    if trimmed.contains(';') {
        return Err(InstallUserError::new(
            "environment markers are not supported in pinned installs",
            json!({ "specifier": trimmed }),
        )
        .into());
    }
    if trimmed.contains('[') {
        return Err(InstallUserError::new(
            "extras are not supported in pinned installs",
            json!({ "specifier": trimmed }),
        )
        .into());
    }

    let Some((name_part, version_part)) = trimmed.split_once("==") else {
        return Err(InstallUserError::new(
            "px install currently requires `name==version` pins",
            json!({ "specifier": trimmed }),
        )
        .into());
    };

    let version_str = version_part.trim();
    if version_str.is_empty() {
        return Err(InstallUserError::new(
            "version after `==` cannot be empty",
            json!({ "specifier": trimmed }),
        )
        .into());
    }
    Version::from_str(version_str).map_err(|_| {
        InstallUserError::new(
            "version must follow PEP 440",
            json!({ "specifier": trimmed, "version": version_str }),
        )
    })?;

    let name = dependency_name(name_part);
    if name.is_empty() {
        return Err(InstallUserError::new(
            "dependency name missing before `==`",
            json!({ "specifier": trimmed }),
        )
        .into());
    }

    let normalized = normalize_dist_name(&name);
    Ok(PinSpec {
        name,
        specifier: format!("{normalized}=={version_str}"),
        version: version_str.to_string(),
        normalized,
        extras: Vec::new(),
        marker: None,
    })
}

#[derive(Clone)]
struct PinSpec {
    name: String,
    specifier: String,
    version: String,
    normalized: String,
    extras: Vec<String>,
    marker: Option<String>,
}

struct PrefetchArtifactSpec {
    name: String,
    version: String,
    filename: String,
    url: String,
    sha256: String,
}

impl PrefetchArtifactSpec {
    fn as_px_spec(&self) -> StorePrefetchSpec<'_> {
        StorePrefetchSpec {
            name: &self.name,
            version: &self.version,
            filename: &self.filename,
            url: &self.url,
            sha256: &self.sha256,
        }
    }
}

fn resolve_pins(pins: &[PinSpec]) -> Result<Vec<ResolvedDependency>> {
    if pins.is_empty() {
        return Ok(Vec::new());
    }

    let cache = resolve_cache_store_path()?;
    let client = build_http_client()?;
    let python = px_python::detect_interpreter()?;
    let tags = detect_interpreter_tags_with(&python)?;
    let mut resolved = Vec::new();
    let force_sdist = force_sdist_build();
    for pin in pins {
        let release = fetch_release(&client, &pin.normalized, &pin.version, &pin.specifier)?;
        let artifact = if force_sdist {
            build_wheel_via_sdist(&cache, &release, pin, &python)?
        } else {
            match select_wheel(&release.urls, &tags, &pin.specifier) {
                Ok(wheel) => {
                    let request = ArtifactRequest {
                        name: &pin.normalized,
                        version: &pin.version,
                        filename: &wheel.filename,
                        url: &wheel.url,
                        sha256: &wheel.sha256,
                    };
                    let cached = cache_wheel(&cache.path, &request)?;
                    LockedArtifact {
                        filename: wheel.filename.clone(),
                        url: wheel.url.clone(),
                        sha256: wheel.sha256.clone(),
                        size: cached.size,
                        cached_path: cached.path.display().to_string(),
                        python_tag: wheel.python_tag.clone(),
                        abi_tag: wheel.abi_tag.clone(),
                        platform_tag: wheel.platform_tag.clone(),
                    }
                }
                Err(err) => match build_wheel_via_sdist(&cache, &release, pin, &python) {
                    Ok(artifact) => artifact,
                    Err(build_err) => {
                        return Err(err.context(format!("sdist fallback failed: {build_err}")))
                    }
                },
            }
        };
        resolved.push(ResolvedDependency {
            name: pin.name.clone(),
            specifier: pin.specifier.clone(),
            extras: pin.extras.clone(),
            marker: pin.marker.clone(),
            artifact,
        });
    }

    Ok(resolved)
}

fn build_wheel_via_sdist(
    cache: &CacheLocation,
    release: &PypiReleaseResponse,
    pin: &PinSpec,
    python: &str,
) -> Result<LockedArtifact> {
    let sdist = select_sdist(&release.urls, &pin.specifier)?;
    let built = ensure_sdist_build(
        &cache.path,
        &SdistRequest {
            normalized_name: &pin.normalized,
            version: &pin.version,
            filename: &sdist.filename,
            url: &sdist.url,
            sha256: Some(&sdist.digests.sha256),
            python_path: python,
        },
    )?;
    Ok(LockedArtifact {
        filename: built.filename,
        url: built.url,
        sha256: built.sha256,
        size: built.size,
        cached_path: built.cached_path.display().to_string(),
        python_tag: built.python_tag,
        abi_tag: built.abi_tag,
        platform_tag: built.platform_tag,
    })
}

fn select_sdist<'a>(files: &'a [PypiFile], specifier: &str) -> Result<&'a PypiFile> {
    files
        .iter()
        .find(|file| file.packagetype == "sdist" && !file.yanked.unwrap_or(false))
        .ok_or_else(|| {
            InstallUserError::new(
                format!("PyPI does not provide an sdist for {specifier}"),
                json!({ "specifier": specifier }),
            )
            .into()
        })
}

fn build_http_client() -> Result<Client> {
    Client::builder()
        .user_agent(format!("px/{PX_VERSION}"))
        .timeout(Duration::from_secs(60))
        .no_proxy()
        .build()
        .context("failed to build HTTP client")
}

fn fetch_release(
    client: &Client,
    normalized: &str,
    version: &str,
    specifier: &str,
) -> Result<PypiReleaseResponse> {
    let url = format!("{PYPI_BASE_URL}/{normalized}/{version}/json");
    let response = client
        .get(&url)
        .send()
        .map_err(|err| anyhow!("failed to query PyPI for {specifier}: {err}"))?;
    if response.status() == StatusCode::NOT_FOUND {
        return Err(InstallUserError::new(
            format!("PyPI does not provide {specifier}"),
            json!({ "specifier": specifier }),
        )
        .into());
    }
    let response = response
        .error_for_status()
        .map_err(|err| anyhow!("PyPI returned an error for {specifier}: {err}"))?;
    response
        .json::<PypiReleaseResponse>()
        .map_err(|err| anyhow!("invalid JSON for {specifier}: {err}"))
}

fn select_wheel(
    files: &[PypiFile],
    tags: &InterpreterTags,
    specifier: &str,
) -> Result<WheelCandidate> {
    let mut candidates = Vec::new();
    for file in files {
        if file.packagetype != "bdist_wheel" || file.yanked.unwrap_or(false) {
            continue;
        }
        let Some((python_tag, abi_tag, platform_tag)) = parse_wheel_tags(&file.filename) else {
            continue;
        };
        candidates.push(WheelCandidate {
            filename: file.filename.clone(),
            url: file.url.clone(),
            sha256: file.digests.sha256.clone(),
            python_tag,
            abi_tag,
            platform_tag,
        });
    }

    if let Some(universal) = candidates
        .iter()
        .find(|c| c.python_tag == "py3" && c.abi_tag == "none" && c.platform_tag == "any")
    {
        return Ok(universal.clone());
    }

    let mut best: Option<(i32, WheelCandidate)> = None;
    for candidate in candidates {
        let score = score_candidate(&candidate, tags);
        match &mut best {
            Some((best_score, best_candidate)) => match score.cmp(best_score) {
                Ordering::Greater => {
                    *best_score = score;
                    *best_candidate = candidate;
                }
                Ordering::Equal => {
                    if candidate.filename < best_candidate.filename {
                        *best_candidate = candidate;
                    }
                }
                Ordering::Less => {}
            },
            None => best = Some((score, candidate)),
        }
    }

    best.map(|(_, candidate)| candidate).ok_or_else(|| {
        InstallUserError::new(
            format!("PyPI did not provide any wheels for {specifier}"),
            json!({ "specifier": specifier }),
        )
        .into()
    })
}

fn score_candidate(candidate: &WheelCandidate, tags: &InterpreterTags) -> i32 {
    let mut score = 0;
    if matches_any(&tags.python, &candidate.python_tag) {
        score += 100;
    } else if candidate.python_tag.starts_with("py3") {
        score += 50;
    }

    if matches_any(&tags.abi, &candidate.abi_tag) {
        score += 40;
    } else if candidate.abi_tag == "none" {
        score += 20;
    }

    if candidate.platform_tag == "any" {
        score += 30;
    } else if matches_any(&tags.platform, &candidate.platform_tag) {
        score += 25;
    }

    score
}

fn matches_any(values: &[String], candidate: &str) -> bool {
    let split = candidate.split('.');
    for part in split {
        if values.iter().any(|val| part.eq_ignore_ascii_case(val)) {
            return true;
        }
    }
    false
}

fn parse_wheel_tags(filename: &str) -> Option<(String, String, String)> {
    if !filename.ends_with(".whl") {
        return None;
    }
    let trimmed = filename.trim_end_matches(".whl");
    let parts: Vec<&str> = trimmed.split('-').collect();
    if parts.len() < 5 {
        return None;
    }
    let python_tag = parts[parts.len() - 3].to_string();
    let abi_tag = parts[parts.len() - 2].to_string();
    let platform_tag = parts[parts.len() - 1].to_string();
    Some((python_tag, abi_tag, platform_tag))
}

fn detect_interpreter_tags_with(python: &str) -> Result<InterpreterTags> {
    let script = r#"import json, sys, sysconfig
major = sys.version_info[0]
minor = sys.version_info[1]
py = [f"cp{major}{minor}", f"py{major}{minor}", f"py{major}", "py3"]
abi = [f"cp{major}{minor}", "abi3", "none"]
plat = sysconfig.get_platform().lower().replace("-", "_").replace(".", "_")
print(json.dumps({"python": py, "abi": abi, "platform": [plat, "any"]}))
"#;
    let cmd = Command::new(python)
        .arg("-c")
        .arg(script)
        .output()
        .with_context(|| format!("failed to interrogate interpreter tags via {python}"))?;
    if !cmd.status.success() {
        let stderr = String::from_utf8_lossy(&cmd.stderr);
        bail!("python tag probe failed: {stderr}");
    }
    let payload: InterpreterTagsPayload =
        serde_json::from_slice(&cmd.stdout).context("invalid interpreter tag payload")?;
    Ok(InterpreterTags {
        python: payload.python,
        abi: payload.abi,
        platform: payload.platform,
    })
}

fn detect_marker_environment_with(python: &str) -> Result<ResolverEnv> {
    let script = r#"import json, os, platform, sys
impl_name = getattr(sys.implementation, "name", "cpython")
impl_version = platform.python_version()
python_full = platform.python_version()
python_short = f"{sys.version_info[0]}.{sys.version_info[1]}"
data = {
    "implementation_name": impl_name,
    "implementation_version": impl_version,
    "os_name": os.name,
    "platform_machine": platform.machine(),
    "platform_python_implementation": platform.python_implementation(),
    "platform_release": platform.release(),
    "platform_system": platform.system(),
    "platform_version": platform.version(),
    "python_full_version": python_full,
    "python_version": python_short,
    "sys_platform": sys.platform,
}
print(json.dumps(data))
"#;
    let cmd = Command::new(python)
        .arg("-c")
        .arg(script)
        .output()
        .with_context(|| format!("failed to probe marker environment via {python}"))?;
    if !cmd.status.success() {
        let stderr = String::from_utf8_lossy(&cmd.stderr);
        bail!("python marker probe failed: {stderr}");
    }
    let payload: MarkerEnvPayload =
        serde_json::from_slice(&cmd.stdout).context("invalid marker env payload")?;
    Ok(ResolverEnv {
        implementation_name: payload.implementation_name,
        implementation_version: payload.implementation_version,
        os_name: payload.os_name,
        platform_machine: payload.platform_machine,
        platform_python_implementation: payload.platform_python_implementation,
        platform_release: payload.platform_release,
        platform_system: payload.platform_system,
        platform_version: payload.platform_version,
        python_full_version: payload.python_full_version,
        python_version: payload.python_version,
        sys_platform: payload.sys_platform,
    })
}

#[derive(Deserialize)]
struct MarkerEnvPayload {
    implementation_name: String,
    implementation_version: String,
    os_name: String,
    platform_machine: String,
    platform_python_implementation: String,
    platform_release: String,
    platform_system: String,
    platform_version: String,
    python_full_version: String,
    python_version: String,
    sys_platform: String,
}

fn normalize_dist_name(name: &str) -> String {
    name.to_ascii_lowercase().replace(['_', '.'], "-")
}

fn format_specifier(
    normalized: &str,
    extras: &[String],
    version: &str,
    marker: Option<&str>,
) -> String {
    let mut spec = normalized.to_string();
    let extras = canonical_extras(extras);
    if !extras.is_empty() {
        spec.push('[');
        spec.push_str(&extras.join(","));
        spec.push(']');
    }
    spec.push_str("==");
    spec.push_str(version);
    if let Some(marker) = marker.and_then(|m| {
        let trimmed = m.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    }) {
        spec.push_str(" ; ");
        spec.push_str(marker);
    }
    spec
}

fn canonical_extras(extras: &[String]) -> Vec<String> {
    let mut values = extras
        .iter()
        .map(|extra| extra.to_ascii_lowercase())
        .collect::<Vec<_>>();
    values.sort();
    values.dedup();
    values
}

fn parse_spec_metadata(spec: &str) -> (Vec<String>, Option<String>) {
    match PepRequirement::from_str(spec.trim()) {
        Ok(req) => {
            let extras = canonical_extras(
                &req.extras
                    .iter()
                    .map(|extra| extra.to_string())
                    .collect::<Vec<_>>(),
            );
            let marker = req.marker.as_ref().map(|m| m.to_string());
            (extras, marker)
        }
        Err(_) => (Vec::new(), None),
    }
}

#[derive(Deserialize)]
struct PypiReleaseResponse {
    urls: Vec<PypiFile>,
}

#[derive(Clone, Deserialize)]
struct PypiFile {
    filename: String,
    url: String,
    packagetype: String,
    yanked: Option<bool>,
    digests: PypiDigests,
}

#[derive(Clone, Deserialize)]
struct PypiDigests {
    sha256: String,
}

#[derive(Clone)]
struct WheelCandidate {
    filename: String,
    url: String,
    sha256: String,
    python_tag: String,
    abi_tag: String,
    platform_tag: String,
}

struct InterpreterTags {
    python: Vec<String>,
    abi: Vec<String>,
    platform: Vec<String>,
}

#[derive(Deserialize)]
struct InterpreterTagsPayload {
    python: Vec<String>,
    abi: Vec<String>,
    platform: Vec<String>,
}

fn tidy_snapshot(snapshot: &ManifestSnapshot) -> Result<TidyOutcome> {
    let lockfile = snapshot.lock_path.display().to_string();
    match maybe_load_lock_snapshot(&snapshot.lock_path)? {
        Some(lock) => {
            let report = analyze_lock_diff(snapshot, &lock);
            if report.is_clean() {
                Ok(TidyOutcome {
                    state: TidyState::Clean,
                    lockfile,
                    drift: Vec::new(),
                })
            } else {
                Ok(TidyOutcome {
                    state: TidyState::Drift,
                    lockfile,
                    drift: report.to_messages(),
                })
            }
        }
        None => Ok(TidyOutcome {
            state: TidyState::MissingLock,
            lockfile,
            drift: Vec::new(),
        }),
    }
}

fn render_lockfile(snapshot: &ManifestSnapshot, resolved: &[ResolvedDependency]) -> Result<String> {
    let mut doc = DocumentMut::new();
    doc.insert("version", Item::Value(TomlValue::from(LOCK_VERSION)));

    let mut metadata = Table::new();
    metadata.insert("px_version", Item::Value(TomlValue::from(PX_VERSION)));
    metadata.insert(
        "created_at",
        Item::Value(TomlValue::from(current_timestamp()?)),
    );
    metadata.insert("mode", Item::Value(TomlValue::from(LOCK_MODE_PINNED)));
    doc.insert("metadata", Item::Table(metadata));

    let mut project = Table::new();
    project.insert("name", Item::Value(TomlValue::from(snapshot.name.clone())));
    doc.insert("project", Item::Table(project));

    let mut python = Table::new();
    python.insert(
        "requirement",
        Item::Value(TomlValue::from(snapshot.python_requirement.clone())),
    );
    doc.insert("python", Item::Table(python));

    let mut ordered = resolved.to_vec();
    ordered.sort_by(|a, b| a.name.cmp(&b.name).then(a.specifier.cmp(&b.specifier)));
    let mut deps = ArrayOfTables::new();
    for dep in ordered {
        let mut table = Table::new();
        table.insert("name", Item::Value(TomlValue::from(dep.name.clone())));
        table.insert(
            "specifier",
            Item::Value(TomlValue::from(dep.specifier.clone())),
        );
        if !dep.extras.is_empty() {
            let mut extras = Array::new();
            for extra in &dep.extras {
                extras.push(TomlValue::from(extra.as_str()));
            }
            table.insert("extras", Item::Value(TomlValue::Array(extras)));
        }
        if let Some(marker) = &dep.marker {
            table.insert("marker", Item::Value(TomlValue::from(marker.clone())));
        }

        let mut artifact = Table::new();
        artifact.insert(
            "filename",
            Item::Value(TomlValue::from(dep.artifact.filename.clone())),
        );
        artifact.insert(
            "url",
            Item::Value(TomlValue::from(dep.artifact.url.clone())),
        );
        artifact.insert(
            "sha256",
            Item::Value(TomlValue::from(dep.artifact.sha256.clone())),
        );
        artifact.insert(
            "size",
            Item::Value(TomlValue::from(dep.artifact.size as i64)),
        );
        artifact.insert(
            "cached_path",
            Item::Value(TomlValue::from(dep.artifact.cached_path.clone())),
        );
        artifact.insert(
            "python_tag",
            Item::Value(TomlValue::from(dep.artifact.python_tag.clone())),
        );
        artifact.insert(
            "abi_tag",
            Item::Value(TomlValue::from(dep.artifact.abi_tag.clone())),
        );
        artifact.insert(
            "platform_tag",
            Item::Value(TomlValue::from(dep.artifact.platform_tag.clone())),
        );
        table.insert("artifact", Item::Table(artifact));
        deps.push(table);
    }
    doc.insert("dependencies", Item::ArrayOfTables(deps));

    Ok(doc.to_string())
}

fn render_lockfile_v2(snapshot: &ManifestSnapshot, lock: &LockSnapshot) -> Result<String> {
    let mut doc = DocumentMut::new();
    doc.insert("version", Item::Value(TomlValue::from(2)));

    let mut metadata = Table::new();
    metadata.insert("px_version", Item::Value(TomlValue::from(PX_VERSION)));
    metadata.insert(
        "created_at",
        Item::Value(TomlValue::from(current_timestamp()?)),
    );
    metadata.insert("mode", Item::Value(TomlValue::from(LOCK_MODE_PINNED)));
    doc.insert("metadata", Item::Table(metadata));

    let mut project = Table::new();
    project.insert("name", Item::Value(TomlValue::from(snapshot.name.clone())));
    doc.insert("project", Item::Table(project));

    let mut python = Table::new();
    python.insert(
        "requirement",
        Item::Value(TomlValue::from(snapshot.python_requirement.clone())),
    );
    doc.insert("python", Item::Table(python));

    let resolved = collect_resolved_dependencies(lock);

    let mut deps = ArrayOfTables::new();
    for dep in &resolved {
        let mut table = Table::new();
        table.insert("name", Item::Value(TomlValue::from(dep.name.clone())));
        table.insert(
            "specifier",
            Item::Value(TomlValue::from(dep.specifier.clone())),
        );
        if !dep.artifact.filename.is_empty() {
            let mut artifact = Table::new();
            artifact.insert(
                "filename",
                Item::Value(TomlValue::from(dep.artifact.filename.clone())),
            );
            artifact.insert(
                "url",
                Item::Value(TomlValue::from(dep.artifact.url.clone())),
            );
            artifact.insert(
                "sha256",
                Item::Value(TomlValue::from(dep.artifact.sha256.clone())),
            );
            artifact.insert(
                "size",
                Item::Value(TomlValue::from(dep.artifact.size as i64)),
            );
            artifact.insert(
                "cached_path",
                Item::Value(TomlValue::from(dep.artifact.cached_path.clone())),
            );
            artifact.insert(
                "python_tag",
                Item::Value(TomlValue::from(dep.artifact.python_tag.clone())),
            );
            artifact.insert(
                "abi_tag",
                Item::Value(TomlValue::from(dep.artifact.abi_tag.clone())),
            );
            artifact.insert(
                "platform_tag",
                Item::Value(TomlValue::from(dep.artifact.platform_tag.clone())),
            );
            table.insert("artifact", Item::Table(artifact));
        }
        deps.push(table);
    }
    doc.insert("dependencies", Item::ArrayOfTables(deps));

    let mut graph_table = Table::new();

    let mut node_entries = ArrayOfTables::new();
    for dep in &resolved {
        if let Some(version) = specifier_version(&dep.specifier) {
            let mut node = Table::new();
            node.insert("name", Item::Value(TomlValue::from(dep.name.clone())));
            node.insert("version", Item::Value(TomlValue::from(version)));
            node.insert(
                "marker",
                Item::Value(TomlValue::from(dep.marker.clone().unwrap_or_default())),
            );
            if !dep.extras.is_empty() {
                let mut extras = Array::new();
                for extra in &dep.extras {
                    extras.push(TomlValue::from(extra.as_str()));
                }
                node.insert("extras", Item::Value(TomlValue::Array(extras)));
            }
            let mut parents = Array::new();
            parents.push(TomlValue::from("root"));
            node.insert("parents", Item::Value(TomlValue::Array(parents)));
            node_entries.push(node);
        }
    }
    if !node_entries.is_empty() {
        graph_table.insert("nodes", Item::ArrayOfTables(node_entries));
    }

    let mut target_map: HashMap<(String, String, String), String> = HashMap::new();
    let mut target_tables = ArrayOfTables::new();
    for dep in &resolved {
        let artifact = &dep.artifact;
        if artifact.filename.is_empty() {
            continue;
        }
        let key = (
            artifact.python_tag.clone(),
            artifact.abi_tag.clone(),
            artifact.platform_tag.clone(),
        );
        if target_map.contains_key(&key) {
            continue;
        }
        let id = format!(
            "{}-{}-{}",
            if key.0.is_empty() {
                "py"
            } else {
                key.0.as_str()
            },
            if key.1.is_empty() {
                "abi"
            } else {
                key.1.as_str()
            },
            if key.2.is_empty() {
                "plat"
            } else {
                key.2.as_str()
            }
        );
        target_map.insert(key.clone(), id.clone());
        let mut table = Table::new();
        table.insert("id", Item::Value(TomlValue::from(id)));
        table.insert("python_tag", Item::Value(TomlValue::from(key.0)));
        table.insert("abi_tag", Item::Value(TomlValue::from(key.1)));
        table.insert("platform_tag", Item::Value(TomlValue::from(key.2)));
        target_tables.push(table);
    }
    if !target_tables.is_empty() {
        graph_table.insert("targets", Item::ArrayOfTables(target_tables));
    }

    let mut artifact_tables = ArrayOfTables::new();
    for dep in &resolved {
        let artifact = &dep.artifact;
        if artifact.filename.is_empty() {
            continue;
        }
        let key = (
            artifact.python_tag.clone(),
            artifact.abi_tag.clone(),
            artifact.platform_tag.clone(),
        );
        let target_id = target_map
            .get(&key)
            .cloned()
            .unwrap_or_else(|| "py-abi-plat".to_string());
        let mut table = Table::new();
        table.insert("node", Item::Value(TomlValue::from(dep.name.clone())));
        table.insert("target", Item::Value(TomlValue::from(target_id)));
        table.insert(
            "filename",
            Item::Value(TomlValue::from(artifact.filename.clone())),
        );
        table.insert("url", Item::Value(TomlValue::from(artifact.url.clone())));
        table.insert(
            "sha256",
            Item::Value(TomlValue::from(artifact.sha256.clone())),
        );
        table.insert("size", Item::Value(TomlValue::from(artifact.size as i64)));
        table.insert(
            "cached_path",
            Item::Value(TomlValue::from(artifact.cached_path.clone())),
        );
        table.insert(
            "python_tag",
            Item::Value(TomlValue::from(artifact.python_tag.clone())),
        );
        table.insert(
            "abi_tag",
            Item::Value(TomlValue::from(artifact.abi_tag.clone())),
        );
        table.insert(
            "platform_tag",
            Item::Value(TomlValue::from(artifact.platform_tag.clone())),
        );
        artifact_tables.push(table);
    }
    if !artifact_tables.is_empty() {
        graph_table.insert("artifacts", Item::ArrayOfTables(artifact_tables));
    }

    doc.insert("graph", Item::Table(graph_table));

    Ok(doc.to_string())
}

fn collect_resolved_dependencies(lock: &LockSnapshot) -> Vec<ResolvedDependency> {
    let mut deps = Vec::new();
    let mut spec_lookup = HashMap::new();
    for spec in &lock.dependencies {
        spec_lookup.insert(dependency_name(spec), spec.clone());
    }
    for entry in &lock.resolved {
        let specifier = spec_lookup
            .get(&entry.name)
            .cloned()
            .unwrap_or_else(|| entry.name.clone());
        let artifact = entry
            .artifact
            .clone()
            .unwrap_or_else(LockedArtifact::default);
        let (extras, marker) = parse_spec_metadata(&specifier);
        deps.push(ResolvedDependency {
            name: entry.name.clone(),
            specifier,
            extras,
            marker,
            artifact,
        });
    }
    deps.sort_by(|a, b| a.name.cmp(&b.name).then(a.specifier.cmp(&b.specifier)));
    deps
}

fn specifier_version(spec: &str) -> Option<String> {
    let parts: Vec<&str> = spec.split("==").collect();
    if parts.len() == 2 {
        Some(parts[1].to_string())
    } else {
        None
    }
}

fn current_timestamp() -> Result<String> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|err| anyhow!("failed to format timestamp: {err}"))
}

struct ManifestSnapshot {
    #[allow(dead_code)]
    root: PathBuf,
    #[allow(dead_code)]
    manifest_path: PathBuf,
    lock_path: PathBuf,
    name: String,
    python_requirement: String,
    dependencies: Vec<String>,
}

struct LockSnapshot {
    version: i64,
    project_name: Option<String>,
    python_requirement: Option<String>,
    dependencies: Vec<String>,
    mode: Option<String>,
    resolved: Vec<LockedDependency>,
    #[allow(dead_code)]
    graph: Option<LockGraphSnapshot>,
}

#[derive(Clone)]
struct ResolvedDependency {
    name: String,
    specifier: String,
    extras: Vec<String>,
    marker: Option<String>,
    artifact: LockedArtifact,
}

#[derive(Clone, Default)]
struct LockedDependency {
    name: String,
    artifact: Option<LockedArtifact>,
}

#[derive(Clone, Default)]
struct LockedArtifact {
    filename: String,
    url: String,
    sha256: String,
    size: u64,
    cached_path: String,
    python_tag: String,
    abi_tag: String,
    platform_tag: String,
}

#[derive(Clone, Default)]
struct GraphNode {
    name: String,
    version: String,
    #[allow(dead_code)]
    marker: Option<String>,
    #[allow(dead_code)]
    parents: Vec<String>,
    extras: Vec<String>,
}

#[derive(Clone, Default)]
struct GraphTarget {
    #[allow(dead_code)]
    id: String,
    #[allow(dead_code)]
    python_tag: String,
    #[allow(dead_code)]
    abi_tag: String,
    #[allow(dead_code)]
    platform_tag: String,
}

#[derive(Clone, Default)]
struct GraphArtifactEntry {
    node: String,
    #[allow(dead_code)]
    target: String,
    artifact: LockedArtifact,
}

#[derive(Clone, Default)]
struct LockGraphSnapshot {
    nodes: Vec<GraphNode>,
    #[allow(dead_code)]
    targets: Vec<GraphTarget>,
    artifacts: Vec<GraphArtifactEntry>,
}

fn maybe_load_lock_snapshot(path: &Path) -> Result<Option<LockSnapshot>> {
    if path.exists() {
        Ok(Some(load_lock_snapshot(path)?))
    } else {
        Ok(None)
    }
}

fn load_lock_snapshot(path: &Path) -> Result<LockSnapshot> {
    let contents = fs::read_to_string(path)?;
    let doc: DocumentMut = contents.parse()?;
    Ok(parse_lock_snapshot(&doc))
}

fn parse_lock_snapshot(doc: &DocumentMut) -> LockSnapshot {
    let version = doc.get("version").and_then(Item::as_integer).unwrap_or(0);
    let project_name = doc
        .get("project")
        .and_then(Item::as_table)
        .and_then(|table| table.get("name"))
        .and_then(Item::as_str)
        .map(|s| s.to_string());
    let python_requirement = doc
        .get("python")
        .and_then(Item::as_table)
        .and_then(|table| table.get("requirement"))
        .and_then(Item::as_str)
        .map(|s| s.to_string());
    let mode = doc
        .get("metadata")
        .and_then(Item::as_table)
        .and_then(|table| table.get("mode"))
        .and_then(Item::as_str)
        .map(|s| s.to_string());

    if version >= 2 {
        if let Some(graph) = parse_graph_snapshot(doc) {
            let (dependencies, resolved) = normalized_from_graph(&graph);
            return LockSnapshot {
                version,
                project_name,
                python_requirement,
                dependencies,
                mode,
                resolved,
                graph: Some(graph),
            };
        }
    }

    let mut dependencies = Vec::new();
    let mut resolved = Vec::new();
    if let Some(tables) = doc.get("dependencies").and_then(Item::as_array_of_tables) {
        for table in tables.iter() {
            let specifier = table
                .get("specifier")
                .and_then(Item::as_str)
                .map(|s| s.to_string())
                .unwrap_or_default();
            if !specifier.is_empty() {
                dependencies.push(specifier.clone());
            }
            let name = table
                .get("name")
                .and_then(Item::as_str)
                .map(|s| s.to_string())
                .unwrap_or_else(|| dependency_name(&specifier));
            let artifact = table
                .get("artifact")
                .and_then(Item::as_table)
                .and_then(parse_artifact_table);
            resolved.push(LockedDependency { name, artifact });
        }
    } else if let Some(array) = doc.get("dependencies").and_then(Item::as_array) {
        dependencies = array
            .iter()
            .filter_map(|val| val.as_str().map(|s| s.to_string()))
            .collect();
    }

    LockSnapshot {
        version,
        project_name,
        python_requirement,
        dependencies,
        mode,
        resolved,
        graph: None,
    }
}

fn parse_artifact_table(table: &Table) -> Option<LockedArtifact> {
    let filename = table.get("filename").and_then(Item::as_str)?.to_string();
    let url = table.get("url").and_then(Item::as_str)?.to_string();
    let sha256 = table.get("sha256").and_then(Item::as_str)?.to_string();
    let size = table
        .get("size")
        .and_then(Item::as_integer)
        .map(|v| v as u64)
        .unwrap_or(0);
    let cached_path = table
        .get("cached_path")
        .and_then(Item::as_str)
        .unwrap_or_default()
        .to_string();
    let python_tag = table
        .get("python_tag")
        .and_then(Item::as_str)
        .unwrap_or_default()
        .to_string();
    let abi_tag = table
        .get("abi_tag")
        .and_then(Item::as_str)
        .unwrap_or_default()
        .to_string();
    let platform_tag = table
        .get("platform_tag")
        .and_then(Item::as_str)
        .unwrap_or_default()
        .to_string();

    Some(LockedArtifact {
        filename,
        url,
        sha256,
        size,
        cached_path,
        python_tag,
        abi_tag,
        platform_tag,
    })
}

fn parse_graph_snapshot(doc: &DocumentMut) -> Option<LockGraphSnapshot> {
    let graph = doc.get("graph")?.as_table()?;
    let node_tables = graph.get("nodes")?.as_array_of_tables()?;
    let mut nodes = Vec::new();
    for table in node_tables.iter() {
        let name = table.get("name").and_then(Item::as_str)?.to_string();
        let version = table
            .get("version")
            .and_then(Item::as_str)
            .unwrap_or_default()
            .to_string();
        let marker = table
            .get("marker")
            .and_then(Item::as_str)
            .map(|s| s.to_string());
        let extras = table
            .get("extras")
            .and_then(Item::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|val| val.as_str().map(|s| s.to_string()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(Vec::new);
        let parents = table
            .get("parents")
            .and_then(Item::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|val| val.as_str().map(|s| s.to_string()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(Vec::new);
        nodes.push(GraphNode {
            name,
            version,
            marker,
            parents,
            extras,
        });
    }
    if nodes.is_empty() {
        return None;
    }

    let mut targets = Vec::new();
    if let Some(target_tables) = graph.get("targets").and_then(Item::as_array_of_tables) {
        for table in target_tables.iter() {
            let target = GraphTarget {
                id: table
                    .get("id")
                    .and_then(Item::as_str)
                    .unwrap_or_default()
                    .to_string(),
                python_tag: table
                    .get("python_tag")
                    .and_then(Item::as_str)
                    .unwrap_or_default()
                    .to_string(),
                abi_tag: table
                    .get("abi_tag")
                    .and_then(Item::as_str)
                    .unwrap_or_default()
                    .to_string(),
                platform_tag: table
                    .get("platform_tag")
                    .and_then(Item::as_str)
                    .unwrap_or_default()
                    .to_string(),
            };
            targets.push(target);
        }
    }

    let mut artifacts = Vec::new();
    if let Some(artifact_tables) = graph.get("artifacts").and_then(Item::as_array_of_tables) {
        for table in artifact_tables.iter() {
            let node = table
                .get("node")
                .and_then(Item::as_str)
                .unwrap_or_default()
                .to_string();
            let target = table
                .get("target")
                .and_then(Item::as_str)
                .unwrap_or_default()
                .to_string();
            if let Some(artifact) = parse_artifact_table(table) {
                artifacts.push(GraphArtifactEntry {
                    node,
                    target,
                    artifact,
                });
            }
        }
    }

    Some(LockGraphSnapshot {
        nodes,
        targets,
        artifacts,
    })
}

fn normalized_from_graph(graph: &LockGraphSnapshot) -> (Vec<String>, Vec<LockedDependency>) {
    let mut nodes = graph.nodes.clone();
    nodes.sort_by(|a, b| a.name.cmp(&b.name).then(a.version.cmp(&b.version)));

    let mut dependencies = Vec::new();
    let mut resolved = Vec::new();
    for node in nodes {
        let marker = node.marker.as_deref().filter(|m| !m.is_empty());
        let spec = format_specifier(&node.name, &node.extras, &node.version, marker);
        dependencies.push(spec.clone());
        let artifact = graph
            .artifacts
            .iter()
            .find(|entry| entry.node == node.name)
            .map(|entry| entry.artifact.clone());
        resolved.push(LockedDependency {
            name: node.name,
            artifact,
        });
    }

    (dependencies, resolved)
}

fn detect_lock_drift(snapshot: &ManifestSnapshot, lock: &LockSnapshot) -> Vec<String> {
    analyze_lock_diff(snapshot, lock).to_messages()
}

fn current_project_root() -> Result<PathBuf> {
    env::current_dir().context("unable to determine project root")
}

fn scaffold_project(root: &Path, package: &str, python_req: &str) -> Result<Vec<String>> {
    let mut files = Vec::new();
    let pyproject_path = root.join("pyproject.toml");
    let package_dir = root.join(package);
    let tests_dir = root.join("tests");

    fs::create_dir_all(&package_dir)?;
    fs::create_dir_all(&tests_dir)?;

    let script_name = format!("{package}-cli");
    let pyproject = format!(
        "[project]\nname = \"{package}\"\nversion = \"0.1.0\"\ndescription = \"Generated by px project init\"\nrequires-python = \"{python_req}\"\ndependencies = []\n\n[project.scripts]\n{script_name} = \"{package}.cli:main\"\n\n[build-system]\nrequires = [\"setuptools>=70\", \"wheel\"]\nbuild-backend = \"setuptools.build_meta\"\n"
    );
    fs::write(&pyproject_path, pyproject)?;
    files.push(relative_path(root, &pyproject_path));

    let init_path = package_dir.join("__init__.py");
    fs::write(&init_path, "__all__ = [\"cli\"]\n")?;
    files.push(relative_path(root, &init_path));

    let cli_path = package_dir.join("cli.py");
    let cli_body = format!(
        r#"from __future__ import annotations


def greet(name: str | None = None) -> str:
    target = name or "World"
    return f"Hello, {{target}}!"


def main() -> None:
    import argparse

    parser = argparse.ArgumentParser(description="Print a greeting.")
    parser.add_argument("-n", "--name", default=None, help="Name to greet")
    args = parser.parse_args()
    print(greet(args.name))


if __name__ == "__main__":
    main()
"#
    );
    fs::write(&cli_path, cli_body)?;
    files.push(relative_path(root, &cli_path));

    let tests_path = tests_dir.join("test_cli.py");
    let tests_body = format!(
        r#"from {package}.cli import greet


def test_greet_default() -> None:
    assert greet() == "Hello, World!"


def test_greet_name() -> None:
    assert greet("Px") == "Hello, Px!"
"#
    );
    fs::write(&tests_path, tests_body)?;
    files.push(relative_path(root, &tests_path));

    ensure_gitignore(root, &mut files)?;

    Ok(files)
}

fn ensure_gitignore(root: &Path, files: &mut Vec<String>) -> Result<()> {
    let path = root.join(".gitignore");
    let entries = ["__pycache__/", "dist/", "build/", "*.egg-info/"];
    if !path.exists() {
        let mut contents = String::new();
        for entry in &entries {
            contents.push_str(entry);
            contents.push('\n');
        }
        fs::write(&path, contents)?;
        files.push(relative_path(root, &path));
        return Ok(());
    }

    let mut contents = fs::read_to_string(&path)?;
    let mut changed = false;
    for entry in &entries {
        if !contents.lines().any(|line| line.trim() == *entry) {
            if !contents.ends_with('\n') {
                contents.push('\n');
            }
            contents.push_str(entry);
            contents.push('\n');
            changed = true;
        }
    }
    if changed {
        fs::write(&path, contents)?;
        files.push(relative_path(root, &path));
    }
    Ok(())
}

fn validate_package_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    match chars.next() {
        Some(ch) if ch.is_ascii_alphabetic() || ch == '_' => {}
        _ => bail!("package name must start with a letter or underscore"),
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        bail!("package name may only contain letters, numbers, or underscores");
    }
    Ok(())
}

fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn ensure_pyproject_exists(path: &Path) -> Result<()> {
    if path.exists() {
        Ok(())
    } else {
        bail!(
            "pyproject.toml not found in {}",
            path.parent().unwrap_or(path).display()
        )
    }
}

fn read_dependencies(doc: &DocumentMut) -> Result<Vec<String>> {
    if let Some(project) = doc.get("project").and_then(Item::as_table) {
        if let Some(item) = project.get("dependencies") {
            if let Some(array) = item.as_array() {
                return Ok(array
                    .iter()
                    .filter_map(|val| val.as_str().map(|s| s.to_string()))
                    .collect());
            }
        }
    }
    Ok(Vec::new())
}

fn write_dependencies(doc: &mut DocumentMut, specs: &[String]) -> Result<()> {
    let table = project_table_mut(doc)?;
    let mut array = Array::new();
    for spec in specs {
        array.push_formatted(TomlValue::from(spec.clone()));
    }
    table.insert("dependencies", Item::Value(TomlValue::Array(array)));
    Ok(())
}

fn project_table(doc: &DocumentMut) -> Result<&Table> {
    doc.get("project")
        .and_then(Item::as_table)
        .ok_or_else(|| anyhow!("[project] must be a table"))
}

fn project_table_mut(doc: &mut DocumentMut) -> Result<&mut Table> {
    doc.entry("project")
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| anyhow!("[project] must be a table"))
}

fn upsert_dependency(deps: &mut Vec<String>, spec: &str) -> InsertOutcome {
    let name = dependency_name(spec);
    for existing in deps.iter_mut() {
        if dependency_name(existing) == name {
            if existing.trim() != spec.trim() {
                *existing = spec.to_string();
                return InsertOutcome::Updated(name);
            }
            return InsertOutcome::Unchanged;
        }
    }
    deps.push(spec.to_string());
    InsertOutcome::Added(name)
}

fn sort_and_dedupe(specs: &mut Vec<String>) {
    specs.sort_by(|a, b| dependency_name(a).cmp(&dependency_name(b)).then(a.cmp(b)));
    let mut seen = HashSet::new();
    specs.retain(|spec| seen.insert(dependency_name(spec)));
}

fn dependency_name(spec: &str) -> String {
    let trimmed = spec.trim().trim_matches(|c| c == '"' || c == '\'');
    let mut end = trimmed.len();
    for (idx, ch) in trimmed.char_indices() {
        if ch.is_ascii_whitespace() || matches!(ch, '<' | '>' | '=' | '!' | '~' | ';') {
            end = idx;
            break;
        }
    }
    let head = &trimmed[..end];
    let base = head.split('[').next().unwrap_or(head);
    base.to_lowercase()
}

enum InsertOutcome {
    Added(String),
    Updated(String),
    Unchanged,
}

fn outcome_from_output(
    command_name: &str,
    target: &str,
    output: RunOutput,
    prefix: &str,
) -> ExecutionOutcome {
    let mut details = json!({
        "stdout": output.stdout,
        "stderr": output.stderr,
        "code": output.code,
        "target": target,
    });

    if output.code == 0 {
        let message = if output.stdout.trim().is_empty() {
            format!("{prefix} {command_name}({target}) succeeded")
        } else {
            details["passthrough"] = Value::Bool(true);
            output.stdout.trim_end().to_string()
        };
        ExecutionOutcome::success(message, details)
    } else {
        let message = if output.stderr.trim().is_empty() {
            format!(
                "{prefix} {command_name}({target}) exited with {}",
                output.code
            )
        } else {
            details["passthrough"] = Value::Bool(true);
            output.stderr.trim_end().to_string()
        };
        ExecutionOutcome::failure(message, details)
    }
}

fn array_arg(command: &PxCommand, key: &str) -> Vec<String> {
    command
        .args
        .get(key)
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn missing_pytest(stderr: &str) -> bool {
    stderr.contains("No module named") && stderr.contains("pytest")
}

struct PythonContext {
    project_root: PathBuf,
    python: String,
    pythonpath: String,
}

impl PythonContext {
    fn new() -> Result<Self> {
        let project_root = env::current_dir().context("px must run inside a project")?;
        let python = px_python::detect_interpreter()?;
        let pythonpath = build_pythonpath(&project_root)?;
        Ok(Self {
            project_root,
            python,
            pythonpath,
        })
    }

    fn base_env(&self, command: &PxCommand) -> Result<Vec<(String, String)>> {
        let mut envs = Vec::new();
        envs.push(("PYTHONPATH".into(), self.pythonpath.clone()));
        envs.push(("PYTHONUNBUFFERED".into(), "1".into()));
        envs.push((
            "PX_PROJECT_ROOT".into(),
            self.project_root.display().to_string(),
        ));
        envs.push(("PX_COMMAND_JSON".into(), command.args.to_string()));
        Ok(envs)
    }
}

fn build_pythonpath(project_root: &Path) -> Result<String> {
    let mut paths = Vec::new();
    let src = project_root.join("src");
    if src.exists() {
        paths.push(src);
    }
    paths.push(project_root.to_path_buf());

    if let Some(site) = project_root.join(".px").join("site").canonicalize().ok() {
        paths.push(site);
    }

    if let Some(existing) = env::var_os("PYTHONPATH") {
        paths.extend(env::split_paths(&existing));
    }

    paths.retain(|p| p.exists());
    if paths.is_empty() {
        paths.push(project_root.to_path_buf());
    }

    let joined = env::join_paths(&paths).context("failed to build PYTHONPATH")?;
    joined
        .into_string()
        .map_err(|_| anyhow!("pythonpath contains non-UTF paths"))
}

struct CacheLocation {
    path: PathBuf,
    source: &'static str,
}

fn resolve_cache_store_path() -> Result<CacheLocation> {
    if let Some(override_path) = env::var_os("PX_CACHE_PATH") {
        let path = absolutize(PathBuf::from(override_path))?;
        return Ok(CacheLocation {
            path,
            source: "PX_CACHE_PATH",
        });
    }

    #[cfg(target_os = "windows")]
    {
        let (base, source) = resolve_windows_cache_base()?;
        return Ok(CacheLocation {
            path: base.join("px").join("store"),
            source,
        });
    }

    #[cfg(not(target_os = "windows"))]
    {
        let (base, source) = resolve_unix_cache_base()?;
        return Ok(CacheLocation {
            path: base.join("px").join("store"),
            source,
        });
    }
}

#[cfg(not(target_os = "windows"))]
fn resolve_unix_cache_base() -> Result<(PathBuf, &'static str)> {
    if let Some(xdg) = env::var_os("XDG_CACHE_HOME") {
        return Ok((PathBuf::from(xdg), "XDG_CACHE_HOME"));
    }
    let home = home_dir().ok_or_else(|| anyhow!("unable to determine home directory"))?;
    Ok((home.join(".cache"), "~/.cache"))
}

#[cfg(target_os = "windows")]
fn resolve_windows_cache_base() -> Result<(PathBuf, &'static str)> {
    if let Some(local) = env::var_os("LOCALAPPDATA") {
        return Ok((PathBuf::from(local), "LOCALAPPDATA"));
    }
    if let Some(user_profile) = env::var_os("USERPROFILE") {
        return Ok((
            PathBuf::from(user_profile).join("AppData").join("Local"),
            "USERPROFILE",
        ));
    }
    let home = home_dir().ok_or_else(|| anyhow!("unable to determine home directory"))?;
    Ok((home.join("AppData").join("Local"), "home/AppData/Local"))
}

fn absolutize(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(env::current_dir()?.join(path))
    }
}

pub fn to_json_response(command: &PxCommand, outcome: &ExecutionOutcome, _code: i32) -> Value {
    let status = match outcome.status {
        CommandStatus::Ok => "ok",
        CommandStatus::UserError => "user-error",
        CommandStatus::Failure => "failure",
    };
    let details = match &outcome.details {
        Value::Object(_) => outcome.details.clone(),
        Value::Null => json!({}),
        other => json!({ "value": other }),
    };
    json!({
        "status": status,
        "message": format_status_message(command, &outcome.message),
        "details": details,
    })
}

pub fn format_status_message(command: &PxCommand, message: &str) -> String {
    let prefix = format!("px {} {}", command.group, command.name);
    if message.is_empty() {
        prefix
    } else if message.starts_with(&prefix) {
        message.to_string()
    } else {
        format!("{prefix}: {message}")
    }
}
