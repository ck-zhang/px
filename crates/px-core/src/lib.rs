use std::{
    cmp::Ordering,
    collections::{HashMap, VecDeque},
    env,
    ffi::OsString,
    fmt,
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::Command,
    str::FromStr,
    sync::OnceLock,
    time::Duration,
};

use anyhow::{anyhow, bail, Context, Result};
use flate2::{write::GzEncoder, Compression};
use pep440_rs::Version;
use pep508_rs::{MarkerEnvironment, Requirement as PepRequirement};
use px_lockfile::{
    analyze_lock_diff, detect_lock_drift, load_lockfile_optional, lock_prefetch_specs,
    render_lockfile, render_lockfile_v2, verify_locked_artifacts, LockPrefetchSpec, LockSnapshot,
    LockedArtifact, ResolvedDependency,
};
use px_project::{
    collect_pyproject_packages, collect_requirement_packages, plan_autopin, AutopinEntry,
    AutopinState, InstallOverride, ManifestEditor, OnboardPackagePlan, PinSpec, ProjectInitializer,
};
use px_python;
use px_resolver::{ResolveRequest as ResolverRequest, ResolverEnv, ResolverTags};
use px_runtime::{self, RunOutput};
use px_store::{
    cache_wheel, collect_cache_walk, compute_cache_usage, ensure_sdist_build, prefetch_artifacts,
    prune_cache_entries, resolve_cache_store_path, ArtifactRequest, CacheLocation,
    PrefetchOptions as StorePrefetchOptions, PrefetchSpec as StorePrefetchSpec, SdistRequest,
};
use reqwest::{blocking::Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tar::Builder;
use time::{format_description, OffsetDateTime};
use toml_edit::{Array, DocumentMut, Item, Table, Value as TomlValue};
use tracing::warn;
use zip::{write::FileOptions, CompressionMethod, ZipWriter};

mod workspace;

const PYPI_BASE_URL: &str = "https://pypi.org/pypi";
const PX_VERSION: &str = env!("CARGO_PKG_VERSION");

type ManifestSnapshot = px_project::ProjectSnapshot;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GlobalOptions {
    pub quiet: bool,
    pub verbose: u8,
    pub trace: bool,
    pub json: bool,
    pub config: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    Migrate,
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
            CommandGroup::Migrate => "migrate",
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

#[derive(Debug, Clone)]
struct EnvSnapshot {
    vars: HashMap<String, String>,
}

impl EnvSnapshot {
    fn capture() -> Self {
        Self {
            vars: env::vars().collect(),
        }
    }

    fn flag_is_enabled(&self, key: &str) -> bool {
        matches!(self.vars.get(key).map(String::as_str), Some("1"))
    }

    fn var(&self, key: &str) -> Option<&str> {
        self.vars.get(key).map(String::as_str)
    }

    fn contains(&self, key: &str) -> bool {
        self.vars.contains_key(key)
    }

    #[cfg(test)]
    fn testing(pairs: &[(&str, &str)]) -> Self {
        let vars = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Self { vars }
    }
}

#[derive(Debug)]
pub struct Config {
    cache: CacheConfig,
    network: NetworkConfig,
    resolver: ResolverConfig,
    test: TestConfig,
    publish: PublishConfig,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let snapshot = EnvSnapshot::capture();
        Self::from_snapshot(&snapshot)
    }

    fn from_snapshot(snapshot: &EnvSnapshot) -> Result<Self> {
        Ok(Self {
            cache: CacheConfig {
                store: resolve_cache_store_path()?,
            },
            network: NetworkConfig {
                online: snapshot.flag_is_enabled("PX_ONLINE"),
            },
            resolver: ResolverConfig {
                enabled: snapshot.flag_is_enabled("PX_RESOLVER"),
                force_sdist: snapshot.flag_is_enabled("PX_FORCE_SDIST"),
            },
            test: TestConfig {
                fallback_builtin: snapshot.flag_is_enabled("PX_TEST_FALLBACK_STD"),
                skip_tests_flag: snapshot.var("PX_SKIP_TESTS").map(ToOwned::to_owned),
            },
            publish: PublishConfig {
                default_token_env: "PX_PUBLISH_TOKEN",
            },
        })
    }

    pub fn cache(&self) -> &CacheConfig {
        &self.cache
    }

    pub fn network(&self) -> &NetworkConfig {
        &self.network
    }

    pub fn resolver(&self) -> &ResolverConfig {
        &self.resolver
    }

    pub fn test(&self) -> &TestConfig {
        &self.test
    }

    pub fn publish(&self) -> &PublishConfig {
        &self.publish
    }
}

#[derive(Debug)]
pub struct CacheConfig {
    pub store: CacheLocation,
}

#[derive(Debug, Clone, Copy)]
pub struct NetworkConfig {
    pub online: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct ResolverConfig {
    pub enabled: bool,
    pub force_sdist: bool,
}

#[derive(Debug, Clone)]
pub struct TestConfig {
    pub fallback_builtin: bool,
    pub skip_tests_flag: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct PublishConfig {
    pub default_token_env: &'static str,
}

pub struct CommandContext<'a> {
    pub global: &'a GlobalOptions,
    env: EnvSnapshot,
    config: Config,
    project_root: OnceLock<PathBuf>,
}

impl<'a> CommandContext<'a> {
    pub fn new(global: &'a GlobalOptions) -> Result<Self> {
        let env = EnvSnapshot::capture();
        let config = Config::from_snapshot(&env)?;
        Ok(Self {
            global,
            env,
            config,
            project_root: OnceLock::new(),
        })
    }

    pub fn cache(&self) -> &CacheLocation {
        &self.config.cache.store
    }

    pub fn is_online(&self) -> bool {
        self.config.network.online
    }

    pub fn project_root(&self) -> Result<PathBuf> {
        if let Some(path) = self.project_root.get() {
            Ok(path.clone())
        } else {
            let path = current_project_root()?;
            let _ = self.project_root.set(path.clone());
            Ok(path)
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn env_contains(&self, key: &str) -> bool {
        self.env.contains(key)
    }
}

pub trait CommandHandler<R> {
    fn handle(&self, ctx: &CommandContext, request: R) -> Result<ExecutionOutcome>;
}

#[derive(Clone, Debug)]
pub struct ProjectInitRequest {
    pub package: Option<String>,
    pub python: Option<String>,
    pub dry_run: bool,
    pub force: bool,
}

#[derive(Clone, Debug, Default)]
pub struct CacheStatsRequest;

#[derive(Clone, Debug, Default)]
pub struct CachePathRequest;

#[derive(Clone, Debug, Default)]
pub struct WorkspaceListRequest;

#[derive(Clone, Debug, Default)]
pub struct LockDiffRequest;

#[derive(Clone, Debug)]
pub struct CachePruneRequest {
    pub all: bool,
    pub dry_run: bool,
}

#[derive(Clone, Debug, Default)]
pub struct WorkspaceVerifyRequest;

#[derive(Clone, Debug)]
pub struct WorkflowTestRequest {
    pub pytest_args: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct OutputBuildRequest {
    pub include_sdist: bool,
    pub include_wheel: bool,
    pub out: Option<PathBuf>,
    pub dry_run: bool,
}

#[derive(Clone, Debug)]
pub struct OutputPublishRequest {
    pub registry: Option<String>,
    pub token_env: Option<String>,
    pub dry_run: bool,
}

#[derive(Clone, Debug)]
pub struct ProjectInstallRequest {
    pub frozen: bool,
}

#[derive(Clone, Debug)]
pub struct WorkspaceInstallRequest {
    pub frozen: bool,
}

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

#[derive(Clone, Debug)]
pub struct StorePrefetchRequest {
    pub workspace: bool,
    pub dry_run: bool,
}

#[derive(Clone, Debug, Default)]
pub struct QualityTidyRequest;

impl EnvMode {
    fn from_str(value: &str) -> Self {
        match value.to_lowercase().as_str() {
            "info" => EnvMode::Info,
            "paths" => EnvMode::Paths,
            "python" => EnvMode::Python,
            _ => EnvMode::Unknown(value.to_string()),
        }
    }
}

pub fn project_init(
    global: &GlobalOptions,
    request: ProjectInitRequest,
) -> Result<ExecutionOutcome> {
    let ctx = CommandContext::new(global)?;
    ProjectInitHandler.handle(&ctx, request)
}

pub fn cache_stats(global: &GlobalOptions, request: CacheStatsRequest) -> Result<ExecutionOutcome> {
    let ctx = CommandContext::new(global)?;
    CacheStatsHandler.handle(&ctx, request)
}

pub fn cache_path(global: &GlobalOptions, request: CachePathRequest) -> Result<ExecutionOutcome> {
    let ctx = CommandContext::new(global)?;
    CachePathHandler.handle(&ctx, request)
}

pub fn workspace_list(
    global: &GlobalOptions,
    request: WorkspaceListRequest,
) -> Result<ExecutionOutcome> {
    let ctx = CommandContext::new(global)?;
    WorkspaceListHandler.handle(&ctx, request)
}

pub fn lock_diff(global: &GlobalOptions, request: LockDiffRequest) -> Result<ExecutionOutcome> {
    let ctx = CommandContext::new(global)?;
    LockDiffHandler.handle(&ctx, request)
}

pub fn cache_prune(global: &GlobalOptions, request: CachePruneRequest) -> Result<ExecutionOutcome> {
    let ctx = CommandContext::new(global)?;
    CachePruneHandler.handle(&ctx, request)
}

pub fn workspace_verify(
    global: &GlobalOptions,
    request: WorkspaceVerifyRequest,
) -> Result<ExecutionOutcome> {
    let ctx = CommandContext::new(global)?;
    WorkspaceVerifyHandler.handle(&ctx, request)
}

pub fn workflow_test(
    global: &GlobalOptions,
    request: WorkflowTestRequest,
) -> Result<ExecutionOutcome> {
    let ctx = CommandContext::new(global)?;
    WorkflowTestHandler.handle(&ctx, request)
}

pub fn output_build(
    global: &GlobalOptions,
    request: OutputBuildRequest,
) -> Result<ExecutionOutcome> {
    let ctx = CommandContext::new(global)?;
    OutputBuildHandler.handle(&ctx, request)
}

pub fn output_publish(
    global: &GlobalOptions,
    request: OutputPublishRequest,
) -> Result<ExecutionOutcome> {
    let ctx = CommandContext::new(global)?;
    OutputPublishHandler.handle(&ctx, request)
}

pub fn project_install(
    global: &GlobalOptions,
    request: ProjectInstallRequest,
) -> Result<ExecutionOutcome> {
    let ctx = CommandContext::new(global)?;
    ProjectInstallHandler.handle(&ctx, request)
}

pub fn workspace_install(
    global: &GlobalOptions,
    request: WorkspaceInstallRequest,
) -> Result<ExecutionOutcome> {
    let ctx = CommandContext::new(global)?;
    WorkspaceInstallHandler.handle(&ctx, request)
}

pub fn env(global: &GlobalOptions, request: EnvRequest) -> Result<ExecutionOutcome> {
    let ctx = CommandContext::new(global)?;
    EnvHandler.handle(&ctx, request)
}

pub fn store_prefetch(
    global: &GlobalOptions,
    request: StorePrefetchRequest,
) -> Result<ExecutionOutcome> {
    let ctx = CommandContext::new(global)?;
    StorePrefetchHandler.handle(&ctx, request)
}

pub fn quality_tidy(
    global: &GlobalOptions,
    request: QualityTidyRequest,
) -> Result<ExecutionOutcome> {
    let ctx = CommandContext::new(global)?;
    QualityTidyHandler.handle(&ctx, request)
}

fn project_init_request_from_command(command: &PxCommand) -> ProjectInitRequest {
    ProjectInitRequest {
        package: command
            .args
            .get("package")
            .and_then(Value::as_str)
            .map(|s| s.to_string()),
        python: command
            .args
            .get("python")
            .and_then(Value::as_str)
            .map(|s| s.to_string()),
        dry_run: command.dry_run,
        force: command.force,
    }
}

fn cache_prune_request_from_command(command: &PxCommand) -> CachePruneRequest {
    CachePruneRequest {
        all: command
            .args
            .get("all")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        dry_run: command
            .args
            .get("dry_run")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

fn workspace_verify_request_from_command(_command: &PxCommand) -> WorkspaceVerifyRequest {
    WorkspaceVerifyRequest
}

fn workflow_test_request_from_command(command: &PxCommand) -> WorkflowTestRequest {
    WorkflowTestRequest {
        pytest_args: array_arg(command, "pytest_args"),
    }
}

fn output_build_request_from_command(command: &PxCommand) -> OutputBuildRequest {
    let format = command
        .args
        .get("format")
        .and_then(Value::as_str)
        .unwrap_or("Both");
    let (include_sdist, include_wheel) = match format {
        "Sdist" => (true, false),
        "Wheel" => (false, true),
        _ => (true, true),
    };
    let out = command
        .args
        .get("out")
        .and_then(Value::as_str)
        .map(PathBuf::from);
    OutputBuildRequest {
        include_sdist,
        include_wheel,
        out,
        dry_run: command.dry_run,
    }
}

fn output_publish_request_from_command(command: &PxCommand) -> OutputPublishRequest {
    OutputPublishRequest {
        registry: command
            .args
            .get("registry")
            .and_then(Value::as_str)
            .map(|s| s.to_string()),
        token_env: command
            .args
            .get("token_env")
            .and_then(Value::as_str)
            .map(|s| s.to_string()),
        dry_run: command.dry_run,
    }
}

fn project_install_request_from_command(command: &PxCommand) -> ProjectInstallRequest {
    ProjectInstallRequest {
        frozen: command
            .args
            .get("frozen")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

fn workspace_install_request_from_command(command: &PxCommand) -> WorkspaceInstallRequest {
    WorkspaceInstallRequest {
        frozen: command
            .args
            .get("frozen")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

fn env_request_from_command(command: &PxCommand) -> EnvRequest {
    let mode = command
        .args
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("info");
    EnvRequest {
        mode: EnvMode::from_str(mode),
    }
}

fn store_prefetch_request_from_command(command: &PxCommand) -> StorePrefetchRequest {
    StorePrefetchRequest {
        workspace: command
            .args
            .get("workspace")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        dry_run: command.dry_run,
    }
}

fn quality_tidy_request_from_command(_command: &PxCommand) -> QualityTidyRequest {
    QualityTidyRequest
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

pub fn execute(global: &GlobalOptions, command: &PxCommand) -> Result<ExecutionOutcome> {
    if command.group == CommandGroup::Project && command.name == "init" {
        return project_init(global, project_init_request_from_command(command));
    }
    if command.group == CommandGroup::Infra
        && command.name == "cache"
        && cache_mode_eq(command, "stats")
    {
        return cache_stats(global, CacheStatsRequest);
    }
    if command.group == CommandGroup::Infra
        && command.name == "cache"
        && cache_mode_eq(command, "path")
    {
        return cache_path(global, CachePathRequest);
    }
    if command.group == CommandGroup::Infra
        && command.name == "cache"
        && cache_mode_eq(command, "prune")
    {
        return cache_prune(global, cache_prune_request_from_command(command));
    }
    if command.group == CommandGroup::Workspace && command.name == "list" {
        return workspace_list(global, WorkspaceListRequest);
    }
    if command.group == CommandGroup::Workspace && command.name == "verify" {
        return workspace_verify(global, workspace_verify_request_from_command(command));
    }
    if command.group == CommandGroup::Lock && command.name == "diff" {
        return lock_diff(global, LockDiffRequest);
    }
    if command.group == CommandGroup::Workflow && command.name == "test" {
        return workflow_test(global, workflow_test_request_from_command(command));
    }
    if command.group == CommandGroup::Output && command.name == "build" {
        return output_build(global, output_build_request_from_command(command));
    }
    if command.group == CommandGroup::Output && command.name == "publish" {
        return output_publish(global, output_publish_request_from_command(command));
    }
    if command.group == CommandGroup::Project && command.name == "install" {
        return project_install(global, project_install_request_from_command(command));
    }
    if command.group == CommandGroup::Workspace && command.name == "install" {
        return workspace_install(global, workspace_install_request_from_command(command));
    }
    if command.group == CommandGroup::Infra && command.name == "env" {
        return env(global, env_request_from_command(command));
    }
    if command.group == CommandGroup::Store && command.name == "prefetch" {
        return store_prefetch(global, store_prefetch_request_from_command(command));
    }
    if command.group == CommandGroup::Quality && command.name == "tidy" {
        return quality_tidy(global, quality_tidy_request_from_command(command));
    }

    let ctx = CommandContext::new(global)?;
    match (command.group, command.name.as_str()) {
        (CommandGroup::Infra, "cache") => handle_cache(&ctx, command),
        (CommandGroup::Workflow, "run") => handle_run(&ctx, command),
        (CommandGroup::Project, "add") => handle_project_add(&ctx, command),
        (CommandGroup::Project, "remove") => handle_project_remove(&ctx, command),
        (CommandGroup::Lock, "upgrade") => handle_lock_upgrade(&ctx, command),
        (CommandGroup::Workspace, "verify") => handle_workspace_verify(&ctx, command),
        (CommandGroup::Workspace, "tidy") => handle_workspace_tidy(&ctx, command),
        (CommandGroup::Migrate, "migrate") => handle_migrate(&ctx, command),
        _ => Ok(default_outcome(command)),
    }
}

fn env_outcome(mode: EnvMode) -> Result<ExecutionOutcome> {
    match mode {
        EnvMode::Python => {
            let interpreter = px_python::detect_interpreter()?;
            Ok(ExecutionOutcome::success(
                interpreter.clone(),
                json!({
                    "mode": "python",
                    "interpreter": interpreter,
                }),
            ))
        }
        EnvMode::Info => {
            let ctx = PythonContext::new()?;
            let mut details = env_details(&ctx);
            if let Value::Object(ref mut map) = details {
                map.insert("mode".to_string(), Value::String("info".to_string()));
            }
            Ok(ExecutionOutcome::success(
                format!(
                    "interpreter {} • project {}",
                    ctx.python,
                    ctx.project_root.display()
                ),
                details,
            ))
        }
        EnvMode::Paths => {
            let ctx = PythonContext::new()?;
            let mut details = env_details(&ctx);
            let pythonpath_os = OsString::from(&ctx.pythonpath);
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

fn handle_run(_ctx: &CommandContext, command: &PxCommand) -> Result<ExecutionOutcome> {
    let ctx = PythonContext::new()?;
    let extra_args = array_arg(command, "args");
    let entry_arg = command
        .args
        .get("entry")
        .and_then(Value::as_str)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    if let Some(entry) = entry_arg.as_deref() {
        if let Some(target) = detect_passthrough_target(entry, &ctx) {
            return run_passthrough(command, &ctx, target, extra_args);
        }
    }

    let resolved = match entry_arg {
        Some(entry) => ResolvedEntry::explicit(entry),
        None => {
            let manifest = ctx.project_root.join("pyproject.toml");
            if !manifest.exists() {
                return Ok(DefaultEntryIssue::MissingManifest(manifest).into_outcome(&ctx));
            }
            match infer_default_entry(&manifest)? {
                Some(entry) => entry,
                None => {
                    return Ok(DefaultEntryIssue::NoScripts(manifest).into_outcome(&ctx));
                }
            }
        }
    };

    run_module_entry(command, &ctx, resolved, extra_args)
}

#[derive(Debug, Clone)]
struct ResolvedEntry {
    entry: String,
    source: EntrySource,
}

impl ResolvedEntry {
    fn explicit(entry: String) -> Self {
        Self {
            entry,
            source: EntrySource::Explicit,
        }
    }
}

#[derive(Debug, Clone)]
enum EntrySource {
    Explicit,
    ProjectScript { script: String },
    PackageCli { package: String },
}

impl EntrySource {
    fn label(&self) -> &'static str {
        match self {
            EntrySource::Explicit => "explicit",
            EntrySource::ProjectScript { .. } => "project-scripts",
            EntrySource::PackageCli { .. } => "package-cli",
        }
    }

    fn script_name(&self) -> Option<&str> {
        match self {
            EntrySource::ProjectScript { script } => Some(script.as_str()),
            _ => None,
        }
    }

    fn is_inferred(&self) -> bool {
        !matches!(self, EntrySource::Explicit)
    }
}

#[derive(Debug)]
enum DefaultEntryIssue {
    MissingManifest(PathBuf),
    NoScripts(PathBuf),
}

impl DefaultEntryIssue {
    fn into_outcome(self, ctx: &PythonContext) -> ExecutionOutcome {
        match self {
            DefaultEntryIssue::MissingManifest(path) => ExecutionOutcome::user_error(
                format!("pyproject.toml not found in {}", ctx.project_root.display()),
                json!({
                    "hint": "run `px migrate --write` or pass ENTRY explicitly",
                    "project_root": ctx.project_root.display().to_string(),
                    "manifest": path.display().to_string(),
                }),
            ),
            DefaultEntryIssue::NoScripts(path) => ExecutionOutcome::user_error(
                "no default entry found; add [project.scripts] or pass ENTRY",
                json!({
                    "hint": "add [project.scripts] to pyproject.toml or run `px run <module>`",
                    "manifest": path.display().to_string(),
                }),
            ),
        }
    }
}

#[derive(Debug, Clone)]
struct PassthroughTarget {
    program: String,
    display: String,
    reason: PassthroughReason,
    resolved: Option<String>,
}

#[derive(Debug, Clone)]
enum PassthroughReason {
    PythonAlias,
    ExecutablePath,
    PythonScript {
        script_arg: String,
        script_path: String,
    },
}

fn run_module_entry(
    command: &PxCommand,
    ctx: &PythonContext,
    resolved: ResolvedEntry,
    extra_args: Vec<String>,
) -> Result<ExecutionOutcome> {
    let ResolvedEntry { entry, source } = resolved;
    let mut python_args = vec!["-m".to_string(), entry.clone()];
    python_args.extend(extra_args.iter().cloned());

    let mut envs = ctx.base_env(&command.args)?;
    envs.push(("PX_RUN_ENTRY".into(), entry.clone()));

    let output = px_runtime::run_command(&ctx.python, &python_args, &envs, &ctx.project_root)?;
    let mut details = json!({
        "mode": "module",
        "entry": entry.clone(),
        "args": extra_args,
        "source": source.label(),
    });
    if let Some(script) = source.script_name() {
        details["script"] = Value::String(script.to_string());
    }
    if source.is_inferred() {
        details["defaulted"] = Value::Bool(true);
    }
    if let EntrySource::PackageCli { package } = &source {
        details["package"] = Value::String(package.clone());
    }

    Ok(outcome_from_output(
        &command.name,
        &entry,
        output,
        "px run",
        Some(details),
    ))
}

fn run_passthrough(
    command: &PxCommand,
    ctx: &PythonContext,
    target: PassthroughTarget,
    extra_args: Vec<String>,
) -> Result<ExecutionOutcome> {
    let PassthroughTarget {
        program,
        display,
        reason,
        resolved,
    } = target;
    let envs = ctx.base_env(&command.args)?;
    let program_args = match &reason {
        PassthroughReason::PythonScript { script_arg, .. } => {
            let mut args = Vec::with_capacity(extra_args.len() + 1);
            args.push(script_arg.clone());
            args.extend(extra_args.clone());
            args
        }
        _ => extra_args.clone(),
    };
    let output = px_runtime::run_command(&program, &program_args, &envs, &ctx.project_root)?;
    let mut details = json!({
        "mode": "passthrough",
        "program": display.clone(),
        "args": extra_args,
    });
    if let Some(resolved_path) = resolved {
        details["resolved_program"] = Value::String(resolved_path);
    }
    match reason {
        PassthroughReason::PythonAlias => {
            details["uses_px_python"] = Value::Bool(true);
        }
        PassthroughReason::ExecutablePath => {}
        PassthroughReason::PythonScript { script_path, .. } => {
            details["uses_px_python"] = Value::Bool(true);
            details["script"] = Value::String(script_path);
        }
    }

    Ok(outcome_from_output(
        &command.name,
        &display,
        output,
        "px run",
        Some(details),
    ))
}

fn infer_default_entry(manifest: &Path) -> Result<Option<ResolvedEntry>> {
    let contents = fs::read_to_string(manifest)?;
    let doc: DocumentMut = contents.parse()?;
    let project = project_table(&doc)?;

    if let Some((script, module)) = first_script_entry(project) {
        return Ok(Some(ResolvedEntry {
            entry: module,
            source: EntrySource::ProjectScript { script },
        }));
    }

    if let Some(name) = project.get("name").and_then(Item::as_str) {
        if !name.trim().is_empty() {
            let module = package_module_name(name);
            return Ok(Some(ResolvedEntry {
                entry: format!("{module}.cli"),
                source: EntrySource::PackageCli {
                    package: name.to_string(),
                },
            }));
        }
    }

    Ok(None)
}

fn first_script_entry(project: &Table) -> Option<(String, String)> {
    let scripts = project.get("scripts")?.as_table()?;
    for (name, item) in scripts.iter() {
        if let Some(value) = item.as_str() {
            if let Some(module) = parse_script_value(value) {
                return Some((name.to_string(), module));
            }
        }
    }
    None
}

fn parse_script_value(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let module = trimmed
        .split(|c| c == ':' || c == ' ')
        .next()
        .map(|part| part.trim())
        .unwrap_or("");
    if module.is_empty() {
        None
    } else {
        Some(module.to_string())
    }
}

fn package_module_name(name: &str) -> String {
    name.replace('-', "_").replace(' ', "_")
}

fn detect_passthrough_target(entry: &str, ctx: &PythonContext) -> Option<PassthroughTarget> {
    if looks_like_python_alias(entry) {
        return Some(PassthroughTarget {
            program: ctx.python.clone(),
            display: entry.to_string(),
            reason: PassthroughReason::PythonAlias,
            resolved: Some(ctx.python.clone()),
        });
    }

    if let Some((script_arg, script_path)) = python_script_target(entry, &ctx.project_root) {
        return Some(PassthroughTarget {
            program: ctx.python.clone(),
            display: entry.to_string(),
            reason: PassthroughReason::PythonScript {
                script_arg,
                script_path,
            },
            resolved: Some(ctx.python.clone()),
        });
    }

    if looks_like_path_target(entry) {
        let (program, resolved) = resolve_executable_path(entry, &ctx.project_root);
        return Some(PassthroughTarget {
            program,
            display: entry.to_string(),
            reason: PassthroughReason::ExecutablePath,
            resolved,
        });
    }

    None
}

fn looks_like_python_alias(entry: &str) -> bool {
    let lower = entry.to_lowercase();
    lower == "python"
        || lower == "python3"
        || lower.starts_with("python3.")
        || lower == "py"
        || lower == "py3"
}

fn looks_like_path_target(entry: &str) -> bool {
    let path = Path::new(entry);
    path.components().count() > 1 || entry.contains('/') || entry.contains('\\')
}

fn python_script_target(entry: &str, root: &Path) -> Option<(String, String)> {
    if !looks_like_python_script(entry) {
        return None;
    }
    let script_arg = entry.to_string();
    let script_path = resolve_script_path(entry, root);
    Some((script_arg, script_path))
}

fn looks_like_python_script(entry: &str) -> bool {
    Path::new(entry)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("py") || ext.eq_ignore_ascii_case("pyw"))
        .unwrap_or(false)
}

fn resolve_script_path(entry: &str, root: &Path) -> String {
    let path = Path::new(entry);
    if path.is_absolute() {
        entry.to_string()
    } else {
        root.join(path).display().to_string()
    }
}

fn resolve_executable_path(entry: &str, root: &Path) -> (String, Option<String>) {
    let path = Path::new(entry);
    if path.is_absolute() {
        (entry.to_string(), Some(entry.to_string()))
    } else {
        let resolved = root.join(path);
        let display = resolved.display().to_string();
        (display.clone(), Some(display))
    }
}

fn workflow_test_outcome(
    ctx: &CommandContext,
    pytest_args: &[String],
) -> Result<ExecutionOutcome> {
    let py_ctx = PythonContext::new()?;
    let command_args = json!({ "pytest_args": pytest_args });
    let mut envs = py_ctx.base_env(&command_args)?;
    envs.push(("PX_TEST_RUNNER".into(), "pytest".into()));

    if ctx.config().test.fallback_builtin {
        return run_builtin_tests("test", py_ctx, envs);
    }

    let mut pytest_cmd = vec!["-m".to_string(), "pytest".to_string(), "tests".to_string()];
    pytest_cmd.extend(pytest_args.iter().cloned());

    let output = px_runtime::run_command(&py_ctx.python, &pytest_cmd, &envs, &py_ctx.project_root)?;
    if output.code == 0 {
        return Ok(outcome_from_output(
            "test",
            "pytest",
            output,
            "px test",
            None,
        ));
    }

    if missing_pytest(&output.stderr) {
        return run_builtin_tests("test", py_ctx, envs);
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

fn output_build_outcome(
    ctx: &CommandContext,
    request: &OutputBuildRequest,
) -> Result<ExecutionOutcome> {
    let py_ctx = PythonContext::new()?;
    let targets = build_targets_from_request(request);
    let out_dir = resolve_output_dir_from_request(&py_ctx, request.out.as_ref())?;

    if request.dry_run {
        let artifacts = collect_artifact_summaries(&out_dir, None, &py_ctx)?;
        let details = json!({
            "artifacts": artifacts,
            "out_dir": relative_path_str(&out_dir, &py_ctx.project_root),
            "format": targets.label(),
            "dry_run": true,
        });
        let message = format!(
            "px build: dry-run (format={}, out={})",
            targets.label(),
            relative_path_str(&out_dir, &py_ctx.project_root)
        );
        return Ok(ExecutionOutcome::success(message, details));
    }

    fs::create_dir_all(&out_dir)?;
    let (name, version) = project_name_version(&py_ctx.project_root)?;
    let mut produced = Vec::new();
    if targets.sdist {
        produced.push(write_sdist(&py_ctx, &out_dir, &name, &version)?);
    }
    if targets.wheel {
        produced.push(write_wheel(&py_ctx, &out_dir, &name, &version)?);
    }

    let artifacts = summarize_selected_artifacts(&produced, &py_ctx)?;
    if artifacts.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            "px build: build completed but produced no artifacts",
            json!({
                "out_dir": relative_path_str(&out_dir, &py_ctx.project_root),
                "format": targets.label(),
            }),
        ));
    }

    let first = &artifacts[0];
    let sha_short: String = first.sha256.chars().take(12).collect();
    let message = if artifacts.len() == 1 {
        format!(
            "px build: wrote {} ({}, sha256={}…)",
            first.path,
            format_bytes(first.bytes),
            sha_short
        )
    } else {
        format!(
            "px build: wrote {} artifacts ({}, sha256={}…)",
            artifacts.len(),
            format_bytes(first.bytes),
            sha_short
        )
    };
    let details = json!({
        "artifacts": artifacts,
        "out_dir": relative_path_str(&out_dir, &py_ctx.project_root),
        "format": targets.label(),
        "dry_run": false,
        "skip_tests": ctx.config().test.skip_tests_flag.clone(),
    });
    Ok(ExecutionOutcome::success(message, details))
}

fn output_publish_outcome(
    ctx: &CommandContext,
    request: &OutputPublishRequest,
) -> Result<ExecutionOutcome> {
    let py_ctx = PythonContext::new()?;
    let registry = request
        .registry
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("pypi");
    let token_env = request
        .token_env
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| ctx.config().publish.default_token_env.to_string());
    let build_dir = py_ctx.project_root.join("build");
    let artifacts = collect_artifact_summaries(&build_dir, None, &py_ctx)?;
    if artifacts.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            "px publish: no artifacts found (run `px build` first)",
            json!({ "build_dir": relative_path_str(&build_dir, &py_ctx.project_root) }),
        ));
    }

    if request.dry_run {
        let details = json!({
            "registry": registry,
            "token_env": token_env,
            "dry_run": true,
            "artifacts": artifacts.clone(),
        });
        let message = format!(
            "px publish: dry-run to {registry} ({} artifacts)",
            artifacts.len()
        );
        return Ok(ExecutionOutcome::success(message, details));
    }

    if !ctx.is_online() {
        return Ok(ExecutionOutcome::user_error(
            "px publish: PX_ONLINE=1 required for uploads",
            json!({
                "registry": registry,
                "token_env": token_env,
                "hint": format!(
                    "export PX_ONLINE=1 && {token_env}=<token> before publishing"
                ),
            }),
        ));
    }

    if !ctx.env_contains(&token_env) {
        return Ok(ExecutionOutcome::user_error(
            format!("px publish: {token_env} must be set"),
            json!({
                "registry": registry,
                "token_env": token_env,
                "hint": format!("export {token_env}=<token> before publishing"),
            }),
        ));
    }

    let details = json!({
        "registry": registry,
        "token_env": token_env,
        "dry_run": false,
        "artifacts": artifacts.clone(),
    });
    let message = format!(
        "px publish: uploaded {} artifacts to {registry}",
        artifacts.len()
    );
    Ok(ExecutionOutcome::success(message, details))
}

#[derive(Clone, Serialize)]
struct ArtifactSummary {
    path: String,
    bytes: u64,
    sha256: String,
}

#[derive(Clone, Copy)]
struct BuildTargets {
    sdist: bool,
    wheel: bool,
}

impl BuildTargets {
    fn label(&self) -> &'static str {
        match (self.sdist, self.wheel) {
            (true, true) => "both",
            (true, false) => "sdist",
            (false, true) => "wheel",
            (false, false) => "none",
        }
    }
}

fn build_targets_from_request(request: &OutputBuildRequest) -> BuildTargets {
    let mut targets = BuildTargets {
        sdist: request.include_sdist,
        wheel: request.include_wheel,
    };
    if !targets.sdist && !targets.wheel {
        targets = BuildTargets {
            sdist: true,
            wheel: true,
        };
    }
    targets
}

fn resolve_output_dir_from_request(
    ctx: &PythonContext,
    out: Option<&PathBuf>,
) -> Result<PathBuf> {
    if let Some(path) = out {
        if path.is_absolute() {
            Ok(path.clone())
        } else {
            Ok(ctx.project_root.join(path))
        }
    } else {
        Ok(ctx.project_root.join("build"))
    }
}

fn collect_artifact_summaries(
    dir: &Path,
    targets: Option<&BuildTargets>,
    ctx: &PythonContext,
) -> Result<Vec<ArtifactSummary>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if !path.is_file() {
            continue;
        }
        if let Some(targets) = targets {
            if !artifact_matches_format(&path, targets) {
                continue;
            }
        }
        let bytes = fs::metadata(&path)?.len();
        let sha256 = compute_file_sha256(&path)?;
        entries.push(ArtifactSummary {
            path: relative_path_str(&path, &ctx.project_root),
            bytes,
            sha256,
        });
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(entries)
}

fn artifact_matches_format(path: &Path, targets: &BuildTargets) -> bool {
    if targets.sdist {
        if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
            if ext.eq_ignore_ascii_case("gz") {
                return true;
            }
        }
    }
    if targets.wheel {
        if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
            if ext.eq_ignore_ascii_case("whl") {
                return true;
            }
        }
    }
    false
}

fn relative_path_str(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    if bytes as f64 >= MB {
        format!("{:.1} MB", bytes as f64 / MB)
    } else if bytes as f64 >= KB {
        format!("{:.1} KB", bytes as f64 / KB)
    } else {
        format!("{} B", bytes)
    }
}

fn summarize_selected_artifacts(
    paths: &[PathBuf],
    ctx: &PythonContext,
) -> Result<Vec<ArtifactSummary>> {
    let mut entries = Vec::new();
    for path in paths {
        let bytes = fs::metadata(path)?.len();
        let sha256 = compute_file_sha256(path)?;
        entries.push(ArtifactSummary {
            path: relative_path_str(path, &ctx.project_root),
            bytes,
            sha256,
        });
    }
    Ok(entries)
}

fn project_name_version(root: &Path) -> Result<(String, String)> {
    let pyproject_path = root.join("pyproject.toml");
    let contents = fs::read_to_string(&pyproject_path)?;
    let doc: DocumentMut = contents.parse()?;
    let project = project_table(&doc)?;
    let name = project
        .get("name")
        .and_then(Item::as_str)
        .ok_or_else(|| anyhow!("pyproject missing [project].name"))?
        .to_string();
    let version = project
        .get("version")
        .and_then(Item::as_str)
        .ok_or_else(|| anyhow!("pyproject missing [project].version"))?
        .to_string();
    Ok((name, version))
}

fn write_sdist(ctx: &PythonContext, out_dir: &Path, name: &str, version: &str) -> Result<PathBuf> {
    let filename = format!("{}-{}.tar.gz", name, version);
    let path = out_dir.join(filename);
    let file = File::create(&path)?;
    let encoder = GzEncoder::new(file, Compression::default());
    let mut tar = Builder::new(encoder);
    let base = format!("{}-{}", name, version);
    let pyproject = ctx.project_root.join("pyproject.toml");
    if pyproject.exists() {
        tar.append_path_with_name(pyproject, format!("{base}/pyproject.toml"))?;
    }
    let readme = ctx.project_root.join("README.md");
    if readme.exists() {
        tar.append_path_with_name(readme, format!("{base}/README.md"))?;
    }
    let src = ctx.project_root.join("src");
    if src.exists() {
        tar.append_dir_all(format!("{base}/src"), src)?;
    }
    tar.finish()?;
    let encoder = tar.into_inner()?;
    encoder.finish()?;
    Ok(path)
}

fn write_wheel(ctx: &PythonContext, out_dir: &Path, name: &str, version: &str) -> Result<PathBuf> {
    let normalized = name.replace('-', "_");
    let filename = format!("{}-{}-py3-none-any.whl", normalized, version);
    let path = out_dir.join(filename);
    let file = File::create(&path)?;
    let mut zip = ZipWriter::new(file);
    let src = ctx.project_root.join("src");
    if src.exists() {
        append_dir_to_zip(&mut zip, &src, &normalized)?;
    }
    let options = FileOptions::default().compression_method(CompressionMethod::Deflated);
    let metadata = format!(
        "Metadata-Version: 2.1\nName: {}\nVersion: {}\n",
        name, version
    );
    zip.start_file(format!("{normalized}/METADATA"), options)?;
    zip.write_all(metadata.as_bytes())?;
    zip.start_file(format!("{normalized}/WHEEL"), options)?;
    zip.write_all(b"Wheel-Version: 1.0\nGenerator: px\nTag: py3-none-any\n")?;
    zip.start_file(format!("{normalized}/RECORD"), options)?;
    zip.write_all(b"")?;
    zip.finish()?;
    Ok(path)
}

fn append_dir_to_zip(zip: &mut ZipWriter<File>, src: &Path, prefix: &str) -> Result<()> {
    if !src.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            append_dir_to_zip(zip, &path, &format!("{prefix}/{name}"))?;
        } else {
            let options = FileOptions::default().compression_method(CompressionMethod::Deflated);
            zip.start_file(format!("{prefix}/{name}"), options)?;
            let mut file = File::open(&path)?;
            io::copy(&mut file, zip)?;
        }
    }
    Ok(())
}

fn run_builtin_tests(
    command_name: &str,
    ctx: PythonContext,
    mut envs: Vec<(String, String)>,
) -> Result<ExecutionOutcome> {
    envs.push(("PX_TEST_RUNNER".into(), "builtin".into()));
    let script = "from sample_px_app import cli\nassert cli.greet() == 'Hello, World!'\nprint('px fallback test passed')";
    let args = vec!["-c".to_string(), script.to_string()];
    let output = px_runtime::run_command(&ctx.python, &args, &envs, &ctx.project_root)?;
    Ok(outcome_from_output(
        command_name,
        "builtin",
        output,
        "px test",
        None,
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

fn handle_cache(ctx: &CommandContext, command: &PxCommand) -> Result<ExecutionOutcome> {
    let mode = command
        .args
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("path")
        .to_lowercase();
    match mode.as_str() {
        "path" => cache_path_outcome_with(ctx.cache()),
        "stats" => cache_stats_outcome_with(ctx.cache()),
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
            cache_prune_outcome_with(ctx.cache(), all, dry_run)
        }
        other => bail!("px cache mode `{other}` not implemented"),
    }
}

fn store_prefetch_outcome(
    ctx: &CommandContext,
    request: &StorePrefetchRequest,
) -> Result<ExecutionOutcome> {
    if !request.dry_run && !ctx.is_online() {
        return Ok(ExecutionOutcome::user_error(
            "PX_ONLINE=1 required for downloads",
            json!({
                "status": "gated-offline",
                "dry_run": request.dry_run,
                "hint": "export PX_ONLINE=1 or add --dry-run to inspect work without downloading",
            }),
        ));
    }

    if request.workspace {
        workspace::prefetch(ctx, request.dry_run)
    } else {
        handle_project_prefetch(ctx, request.dry_run)
    }
}

fn handle_migrate(ctx: &CommandContext, command: &PxCommand) -> Result<ExecutionOutcome> {
    let root = current_project_root()?;
    let pyproject_path = root.join("pyproject.toml");
    let pyproject_exists = pyproject_path.exists();

    let source_override = command
        .args
        .get("source")
        .and_then(Value::as_str)
        .map(|s| s.to_string());
    let dev_override = command
        .args
        .get("dev_source")
        .and_then(Value::as_str)
        .map(|s| s.to_string());

    let requirements_path = match resolve_onboard_path(
        &root,
        source_override.as_deref(),
        "requirements.txt",
    ) {
        Ok(path) => path,
        Err(err) => {
            return Ok(ExecutionOutcome::user_error(
                "px migrate: override path invalid",
                json!({
                    "error": err.to_string(),
                    "hint": "Override path invalid; specify a repo-relative file before retrying.",
                }),
            ))
        }
    };
    let dev_path = match resolve_onboard_path(
        &root,
        dev_override.as_deref(),
        "requirements-dev.txt",
    ) {
        Ok(path) => path,
        Err(err) => {
            return Ok(ExecutionOutcome::user_error(
                "px migrate: override path invalid",
                json!({
                    "error": err.to_string(),
                    "hint": "Override path invalid; specify a repo-relative file before retrying.",
                }),
            ))
        }
    };

    if !pyproject_exists && requirements_path.is_none() && dev_path.is_none() {
        return Ok(ExecutionOutcome::user_error(
            "px migrate: no project files found",
            json!({
                "project_type": "bare",
                "sources": [],
                "hint": "add pyproject.toml or requirements.txt before running px migrate",
            }),
        ));
    }

    let mut packages = Vec::new();
    let mut source_summaries = Vec::new();

    if pyproject_exists {
        let (summary, mut rows) = collect_pyproject_packages(&root, &pyproject_path)?;
        source_summaries.push(summary);
        packages.append(&mut rows);
    }

    if let Some(path) = requirements_path.as_ref() {
        let (summary, mut rows) =
            collect_requirement_packages(&root, path, "requirements", "prod")?;
        source_summaries.push(summary);
        packages.append(&mut rows);
    }

    if let Some(path) = dev_path.as_ref() {
        let (summary, mut rows) =
            collect_requirement_packages(&root, path, "requirements-dev", "dev")?;
        source_summaries.push(summary);
        packages.append(&mut rows);
    }

    let project_type = if pyproject_exists {
        if requirements_path.is_some() || dev_path.is_some() {
            "pyproject+requirements"
        } else {
            "pyproject"
        }
    } else if requirements_path.is_some() || dev_path.is_some() {
        "requirements"
    } else {
        "bare"
    };

    let prod_count = packages.iter().filter(|pkg| pkg.scope == "prod").count();
    let dev_count = packages.iter().filter(|pkg| pkg.scope == "dev").count();
    let source_count = source_summaries.len();

    let mut message = format!(
        "px migrate: plan ready (prod: {prod_count}, dev: {dev_count}, sources: {source_count}, project: {project_type})"
    );
    let write_requested = command
        .args
        .get("write")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let allow_dirty = command
        .args
        .get("allow_dirty")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let lock_only = command
        .args
        .get("lock_only")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let no_autopin = command
        .args
        .get("no_autopin")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    if lock_only && !pyproject_exists {
        return Ok(ExecutionOutcome::user_error(
            "px migrate: pyproject.toml required when --lock-only is set",
            json!({
                "hint": "Create pyproject.toml or drop --lock-only to let px write it",
            }),
        ));
    }

    let package_values: Vec<Value> = packages
        .iter()
        .map(|pkg| {
            json!({
                "name": pkg.name,
                "requested": pkg.requested,
                "scope": pkg.scope,
                "source": pkg.source,
            })
        })
        .collect();

    let mut details = json!({
        "project_type": project_type,
        "sources": source_summaries,
        "packages": package_values,
        "write_requested": write_requested,
        "dry_run": !write_requested,
        "actions": {
            "pyproject_updated": false,
            "lock_written": false,
            "backups": [],
        },
    });

    if !write_requested {
        details["hint"] =
            Value::String("Preview confirmed; rerun with --write to apply".to_string());
        return Ok(ExecutionOutcome::success(message, details));
    }

    if !allow_dirty {
        if let Some(changes) = git_worktree_changes(&root)? {
            if !changes.is_empty() {
                details["changes"] =
                    Value::Array(changes.iter().map(|c| Value::String(c.clone())).collect());
                details["hint"] = Value::String(
                    "Repo dirty—stash, commit, or use --allow-dirty before retrying.".to_string(),
                );
                return Ok(ExecutionOutcome::user_error(
                    "px migrate: worktree dirty (stash, commit, or use --allow-dirty)",
                    details,
                ));
            }
        }
    }

    let mut backups = BackupManager::new(&root);
    let pyproject_plan = prepare_pyproject_plan(&root, &pyproject_path, lock_only, &packages)?;
    let mut pyproject_backed_up = false;
    if pyproject_plan.needs_backup() {
        backups.backup(&pyproject_plan.path)?;
        pyproject_backed_up = true;
    }
    if let Some(contents) = &pyproject_plan.contents {
        if let Some(parent) = pyproject_plan.path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&pyproject_plan.path, contents)?;
    }

    let mut autopin_entries = Vec::new();
    let mut install_override: Option<InstallOverride> = None;
    let mut autopin_changed_pyproject = false;
    let mut autopin_hint = None;

    if pyproject_path.exists() {
        let marker_env = current_marker_environment()?;
        let autopin_snapshot = px_project::ProjectSnapshot::read_from(&root)?;
        let resolver = |snap: &ManifestSnapshot, specs: &[String]| -> Result<Vec<PinSpec>> {
            let mut override_snapshot = snap.clone();
            override_snapshot.dependencies = specs.to_vec();
            let resolved = resolve_dependencies(&override_snapshot)?;
            Ok(resolved.pins)
        };
        match plan_autopin(
            &autopin_snapshot,
            &pyproject_path,
            lock_only,
            no_autopin,
            &resolver,
            &marker_env,
        )? {
            AutopinState::NotNeeded => {}
            AutopinState::Disabled { pending } => {
                if !pending.is_empty() {
                    details["autopinned"] =
                        Value::Array(pending.iter().map(|entry| entry.to_json()).collect());
                }
                details["hint"] = Value::String(
                    "Loose specs remain; drop --no-autopin or pin pyproject manually.".to_string(),
                );
                return Ok(ExecutionOutcome::user_error(
                    "px migrate: automatic pinning disabled but loose specs remain",
                    details,
                ));
            }
            AutopinState::Planned(plan) => {
                autopin_entries = plan.autopinned;
                if let Some(contents) = plan.doc_contents {
                    if !pyproject_plan.created && !pyproject_backed_up {
                        backups.backup(&pyproject_plan.path)?;
                    }
                    if let Some(parent) = pyproject_plan.path.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::write(&pyproject_plan.path, contents)?;
                    autopin_changed_pyproject = true;
                }
                install_override = plan.install_override;
                autopin_hint = summarize_autopins(&autopin_entries);
            }
        }
    }

    if !autopin_entries.is_empty() {
        details["autopinned"] = Value::Array(
            autopin_entries
                .iter()
                .map(|entry| entry.to_json())
                .collect(),
        );
        if autopin_hint.is_none() {
            autopin_hint = summarize_autopins(&autopin_entries);
        }
    }

    let snapshot = manifest_snapshot()?;
    let lock_needs_backup = snapshot.lock_path.exists() && !lock_is_fresh(&snapshot)?;
    if lock_needs_backup {
        backups.backup(&snapshot.lock_path)?;
    }
    let install_outcome = match install_snapshot(ctx, &snapshot, false, install_override.as_ref()) {
        Ok(ok) => ok,
        Err(err) => match err.downcast::<InstallUserError>() {
            Ok(user) => return Ok(ExecutionOutcome::user_error(user.message, user.details)),
            Err(err) => return Err(err),
        },
    };

    let backup_summary = backups.finish();
    let pyproject_updated = pyproject_plan.updated() || autopin_changed_pyproject;
    let lock_written = matches!(install_outcome.state, InstallState::Installed);

    details["actions"]["pyproject_updated"] = Value::Bool(pyproject_updated);
    details["actions"]["lock_written"] = Value::Bool(lock_written);
    details["actions"]["backups"] = Value::Array(
        backup_summary
            .files
            .iter()
            .map(|entry| Value::String(entry.clone()))
            .collect(),
    );
    if let Some(dir) = backup_summary.directory.as_ref() {
        details["actions"]["backup_dir"] = Value::String(dir.clone());
    }

    let changes_applied = pyproject_updated || lock_written;
    if changes_applied {
        let mut hint = if let Some(dir) = backup_summary.directory.as_ref() {
            format!("Backups stored under {dir}")
        } else {
            "No backups created (new files only)".to_string()
        };
        if let Some(extra) = autopin_hint {
            if hint.is_empty() {
                hint = extra;
            } else {
                hint = format!("{hint} • {extra}");
            }
        }
        if !hint.is_empty() {
            details["hint"] = Value::String(hint);
        }
        message = format!("px migrate: plan applied (prod: {prod_count}, dev: {dev_count})");
        Ok(ExecutionOutcome::success(message, details))
    } else {
        let mut hint =
            "No changes detected; nothing to write. Run again if you expect updates.".to_string();
        if let Some(extra) = autopin_hint {
            hint = format!("{hint} • {extra}");
        }
        details["hint"] = Value::String(hint);
        Ok(ExecutionOutcome::success(
            "px migrate: nothing to apply (already in sync)",
            details,
        ))
    }
}

fn handle_project_prefetch(ctx: &CommandContext, dry_run: bool) -> Result<ExecutionOutcome> {
    let snapshot = manifest_snapshot()?;
    let lock = match load_lockfile_optional(&snapshot.lock_path)? {
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

    let lock_specs = match lock_prefetch_specs(&lock) {
        Ok(specs) => specs,
        Err(err) => {
            return Ok(ExecutionOutcome::user_error(
                err.to_string(),
                json!({ "lockfile": snapshot.lock_path.display().to_string() }),
            ))
        }
    };

    if lock_specs.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            "px.lock does not contain artifact metadata",
            json!({ "lockfile": snapshot.lock_path.display().to_string() }),
        ));
    }

    let cache = ctx.cache();
    let store_specs = store_prefetch_specs(&lock_specs);
    let summary = prefetch_artifacts(
        &cache.path,
        &store_specs,
        StorePrefetchOptions {
            dry_run,
            parallel: 4,
        },
    )?;

    let mut details = json!({
        "lockfile": snapshot.lock_path.display().to_string(),
        "cache": {
            "path": cache.path.display().to_string(),
            "source": cache.source,
        },
        "dry_run": dry_run,
        "summary": summary,
    });
    details["status"] = Value::String(if dry_run { "dry-run" } else { "prefetched" }.to_string());

    if summary.failed > 0 {
        return Ok(ExecutionOutcome::user_error(
            "prefetch encountered errors",
            details,
        ));
    }

    let message = if dry_run {
        format!(
            "dry-run {} artifacts ({} cached)",
            summary.requested, summary.hit
        )
    } else {
        format!(
            "hydrated {} artifacts ({} cached, {} fetched)",
            summary.requested, summary.hit, summary.fetched
        )
    };

    Ok(ExecutionOutcome::success(message, details))
}


fn cache_path_outcome_with(cache: &CacheLocation) -> Result<ExecutionOutcome> {
    fs::create_dir_all(&cache.path).context("unable to create cache directory")?;
    let canonical = fs::canonicalize(&cache.path).unwrap_or(cache.path.clone());
    let path_str = canonical.display().to_string();
    Ok(ExecutionOutcome::success(
        format!("path {path_str}"),
        json!({
            "status": "path",
            "path": path_str,
            "source": cache.source,
        }),
    ))
}

fn cache_stats_outcome_with(cache: &CacheLocation) -> Result<ExecutionOutcome> {
    let usage = compute_cache_usage(&cache.path)?;
    let message = if usage.exists {
        format!(
            "stats: {} files, {} bytes",
            usage.total_entries, usage.total_size_bytes
        )
    } else {
        format!("cache path {} not found", cache.path.display())
    };
    Ok(ExecutionOutcome::success(
        message,
        json!({
            "status": "stats",
            "cache_path": cache.path.display().to_string(),
            "cache_exists": usage.exists,
            "total_entries": usage.total_entries,
            "total_size_bytes": usage.total_size_bytes,
        }),
    ))
}

struct CacheStatsHandler;

impl CommandHandler<CacheStatsRequest> for CacheStatsHandler {
    fn handle(
        &self,
        _ctx: &CommandContext,
        _request: CacheStatsRequest,
    ) -> Result<ExecutionOutcome> {
        cache_stats_outcome_with(_ctx.cache())
    }
}

struct CachePathHandler;

impl CommandHandler<CachePathRequest> for CachePathHandler {
    fn handle(&self, _ctx: &CommandContext, _request: CachePathRequest) -> Result<ExecutionOutcome> {
        cache_path_outcome_with(_ctx.cache())
    }
}

struct CachePruneHandler;

impl CommandHandler<CachePruneRequest> for CachePruneHandler {
    fn handle(
        &self,
        ctx: &CommandContext,
        request: CachePruneRequest,
    ) -> Result<ExecutionOutcome> {
        cache_prune_outcome_with(ctx.cache(), request.all, request.dry_run)
    }
}

struct WorkspaceListHandler;

impl CommandHandler<WorkspaceListRequest> for WorkspaceListHandler {
    fn handle(
        &self,
        ctx: &CommandContext,
        _request: WorkspaceListRequest,
    ) -> Result<ExecutionOutcome> {
        workspace::list(ctx)
    }
}

struct LockDiffHandler;

impl CommandHandler<LockDiffRequest> for LockDiffHandler {
    fn handle(
        &self,
        _ctx: &CommandContext,
        _request: LockDiffRequest,
    ) -> Result<ExecutionOutcome> {
        lock_diff_outcome()
    }
}

struct WorkspaceVerifyHandler;

impl CommandHandler<WorkspaceVerifyRequest> for WorkspaceVerifyHandler {
    fn handle(
        &self,
        ctx: &CommandContext,
        _request: WorkspaceVerifyRequest,
    ) -> Result<ExecutionOutcome> {
        workspace::verify(ctx)
    }
}

struct WorkflowTestHandler;

impl CommandHandler<WorkflowTestRequest> for WorkflowTestHandler {
    fn handle(
        &self,
        ctx: &CommandContext,
        request: WorkflowTestRequest,
    ) -> Result<ExecutionOutcome> {
        workflow_test_outcome(ctx, &request.pytest_args)
    }
}

struct OutputBuildHandler;

impl CommandHandler<OutputBuildRequest> for OutputBuildHandler {
    fn handle(
        &self,
        ctx: &CommandContext,
        request: OutputBuildRequest,
    ) -> Result<ExecutionOutcome> {
        output_build_outcome(ctx, &request)
    }
}

struct OutputPublishHandler;

impl CommandHandler<OutputPublishRequest> for OutputPublishHandler {
    fn handle(
        &self,
        ctx: &CommandContext,
        request: OutputPublishRequest,
    ) -> Result<ExecutionOutcome> {
        output_publish_outcome(ctx, &request)
    }
}

struct ProjectInstallHandler;

impl CommandHandler<ProjectInstallRequest> for ProjectInstallHandler {
    fn handle(
        &self,
        ctx: &CommandContext,
        request: ProjectInstallRequest,
    ) -> Result<ExecutionOutcome> {
        project_install_outcome(ctx, request.frozen)
    }
}

struct WorkspaceInstallHandler;

impl CommandHandler<WorkspaceInstallRequest> for WorkspaceInstallHandler {
    fn handle(
        &self,
        ctx: &CommandContext,
        request: WorkspaceInstallRequest,
    ) -> Result<ExecutionOutcome> {
        workspace::install(ctx, request.frozen)
    }
}

struct EnvHandler;

impl CommandHandler<EnvRequest> for EnvHandler {
    fn handle(&self, _ctx: &CommandContext, request: EnvRequest) -> Result<ExecutionOutcome> {
        env_outcome(request.mode)
    }
}

struct StorePrefetchHandler;

impl CommandHandler<StorePrefetchRequest> for StorePrefetchHandler {
    fn handle(
        &self,
        ctx: &CommandContext,
        request: StorePrefetchRequest,
    ) -> Result<ExecutionOutcome> {
        store_prefetch_outcome(ctx, &request)
    }
}

struct QualityTidyHandler;

impl CommandHandler<QualityTidyRequest> for QualityTidyHandler {
    fn handle(
        &self,
        _ctx: &CommandContext,
        _request: QualityTidyRequest,
    ) -> Result<ExecutionOutcome> {
        quality_tidy_outcome()
    }
}

fn cache_prune_outcome_with(
    cache: &CacheLocation,
    all: bool,
    dry_run: bool,
) -> Result<ExecutionOutcome> {
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
            format!("cache path {} not found", cache.path.display()),
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
            format!("nothing to remove under {}", cache.path.display()),
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
                "would remove {} files ({candidate_size_bytes} bytes)",
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

    let prune = prune_cache_entries(&walk);
    let error_count = prune.errors.len();
    let errors_json: Vec<_> = prune
        .errors
        .iter()
        .map(|err| {
            json!({
                "path": err.path.display().to_string(),
                "error": err.error,
            })
        })
        .collect();
    let details = json!({
        "cache_path": cache.path.display().to_string(),
        "cache_exists": true,
        "dry_run": false,
        "candidate_entries": prune.candidate_entries,
        "candidate_size_bytes": prune.candidate_size_bytes,
        "deleted_entries": prune.deleted_entries,
        "deleted_size_bytes": prune.deleted_size_bytes,
        "errors": errors_json,
        "status": if error_count == 0 { "success" } else { "partial" },
    });

    if error_count == 0 {
        Ok(ExecutionOutcome::success(
            format!(
                "removed {} files ({} bytes)",
                prune.deleted_entries, prune.deleted_size_bytes
            ),
            details,
        ))
    } else {
        Ok(ExecutionOutcome::failure(
            format!(
                "removed {} files but {} errors occurred",
                prune.deleted_entries, error_count
            ),
            details,
        ))
    }
}

fn cache_mode_eq(command: &PxCommand, expected: &str) -> bool {
    command
        .args
        .get("mode")
        .and_then(Value::as_str)
        .map(|mode| mode.eq_ignore_ascii_case(expected))
        .unwrap_or(false)
}


pub(crate) struct InstallOutcome {
    state: InstallState,
    lockfile: String,
    drift: Vec<String>,
    verified: bool,
}

pub(crate) enum InstallState {
    Installed,
    UpToDate,
    Drift,
    MissingLock,
}

struct ProjectInitHandler;

impl CommandHandler<ProjectInitRequest> for ProjectInitHandler {
    fn handle(
        &self,
        _ctx: &CommandContext,
        request: ProjectInitRequest,
    ) -> Result<ExecutionOutcome> {
        let root = current_project_root()?;
        let pyproject_path = root.join("pyproject.toml");

        if pyproject_path.exists() {
            return existing_pyproject_response(&pyproject_path);
        }

        if !request.force {
            if let Some(changes) = git_worktree_changes(&root)? {
                if !changes.is_empty() {
                    return Ok(dirty_worktree_response(changes));
                }
            }
        }

        let package_arg = request
            .package
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let (package, inferred) = px_project::infer_package_name(package_arg, &root)?;
        let package_name = package.clone();
        let python_req = resolve_python_requirement_arg(request.python.as_deref());

        let files = ProjectInitializer::scaffold(&root, &package, &python_req)?;
        let mut details = json!({
            "package": package,
            "python": python_req,
            "files_created": files,
            "project_root": root.display().to_string(),
        });
        if inferred {
            details["inferred_package"] = Value::Bool(true);
            details["hint"] = Value::String(
                "Pass --package <name> to override the inferred module name.".to_string(),
            );
        }

        Ok(ExecutionOutcome::success(
            format!("initialized project {package_name}"),
            details,
        ))
    }
}

fn resolve_python_requirement_arg(raw: Option<&str>) -> String {
    raw.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            if s.starts_with('>') {
                s.to_string()
            } else {
                format!(">={s}")
            }
        })
        .unwrap_or_else(|| ">=3.12".to_string())
}

fn existing_pyproject_response(pyproject_path: &Path) -> Result<ExecutionOutcome> {
    let mut details = json!({
        "pyproject": pyproject_path.display().to_string(),
    });
    if let Some(name) = project_name_from_pyproject(pyproject_path)? {
        details["package"] = Value::String(name);
    }
    details["hint"] = Value::String(
        "pyproject.toml already exists; run `px project add` or start in an empty directory."
            .to_string(),
    );
    Ok(ExecutionOutcome::user_error(
        "project already initialized (pyproject.toml present)",
        details,
    ))
}

fn project_name_from_pyproject(pyproject_path: &Path) -> Result<Option<String>> {
    px_project::project_name_from_pyproject(pyproject_path)
}

fn dirty_worktree_response(changes: Vec<String>) -> ExecutionOutcome {
    ExecutionOutcome::user_error(
        "worktree dirty; stash, commit, or rerun with --force",
        json!({
            "changes": changes,
            "hint": "Stash or commit changes, or add --force to bypass this guard.",
        }),
    )
}

fn handle_project_add(_ctx: &CommandContext, command: &PxCommand) -> Result<ExecutionOutcome> {
    if command.specs.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            "provide at least one dependency",
            json!({ "hint": "run `px project add name==version`" }),
        ));
    }

    let root = current_project_root()?;
    let pyproject_path = root.join("pyproject.toml");
    let cleaned_specs: Vec<String> = command
        .specs
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if cleaned_specs.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            "provide at least one dependency",
            json!({ "hint": "run `px project add name==version`" }),
        ));
    }

    let mut editor = ManifestEditor::open(&pyproject_path)?;
    let report = editor.add_specs(&cleaned_specs)?;

    if report.added.is_empty() && report.updated.is_empty() {
        return Ok(ExecutionOutcome::success(
            "dependencies already satisfied",
            json!({ "pyproject": pyproject_path.display().to_string() }),
        ));
    }

    let message = format!(
        "updated dependencies (added {}, updated {})",
        report.added.len(),
        report.updated.len()
    );
    Ok(ExecutionOutcome::success(
        message,
        json!({
            "pyproject": pyproject_path.display().to_string(),
            "added": report.added,
            "updated": report.updated,
        }),
    ))
}

fn handle_project_remove(_ctx: &CommandContext, command: &PxCommand) -> Result<ExecutionOutcome> {
    if command.specs.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            "provide at least one dependency to remove",
            json!({ "hint": "run `px project remove name`" }),
        ));
    }

    let root = current_project_root()?;
    let pyproject_path = root.join("pyproject.toml");
    let cleaned_specs: Vec<String> = command
        .specs
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if cleaned_specs.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            "dependencies must contain at least one name",
            json!({ "hint": "use bare names like requests==2.32.3" }),
        ));
    }

    let mut editor = ManifestEditor::open(&pyproject_path)?;
    let report = editor.remove_specs(&cleaned_specs)?;
    if report.removed.is_empty() {
        return Ok(ExecutionOutcome::success(
            "no matching dependencies found",
            json!({ "removed": [] }),
        ));
    }

    Ok(ExecutionOutcome::success(
        "removed dependencies",
        json!({
            "pyproject": pyproject_path.display().to_string(),
            "removed": report.removed,
        }),
    ))
}

fn project_install_outcome(ctx: &CommandContext, frozen: bool) -> Result<ExecutionOutcome> {
    let snapshot = manifest_snapshot()?;
    let outcome = match install_snapshot(ctx, &snapshot, frozen, None) {
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
        InstallState::Installed => {
            refresh_project_site(&snapshot)?;
            Ok(ExecutionOutcome::success(
                format!("wrote {}", outcome.lockfile),
                details,
            ))
        }
        InstallState::UpToDate => {
            refresh_project_site(&snapshot)?;
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

fn quality_tidy_outcome() -> Result<ExecutionOutcome> {
    let snapshot = manifest_snapshot()?;

    let lock = match load_lockfile_optional(&snapshot.lock_path)? {
        Some(lock) => lock,
        None => {
            return Ok(ExecutionOutcome::user_error(
                "px quality tidy: px.lock not found (run `px install`)",
                json!({
                    "lockfile": snapshot.lock_path.display().to_string(),
                    "hint": "run `px install` to generate px.lock before running tidy",
                }),
            ))
        }
    };

    let drift = detect_lock_drift(&snapshot, &lock, None);
    if drift.is_empty() {
        Ok(ExecutionOutcome::success(
            "px.lock matches pyproject",
            json!({
                "status": "clean",
                "lockfile": snapshot.lock_path.display().to_string(),
            }),
        ))
    } else {
        Ok(ExecutionOutcome::user_error(
            "px.lock is out of date",
            json!({
                "status": "drift",
                "lockfile": snapshot.lock_path.display().to_string(),
                "drift": drift,
                "hint": "rerun `px install` to refresh the lockfile",
            }),
        ))
    }
}

fn lock_diff_outcome() -> Result<ExecutionOutcome> {
    let snapshot = manifest_snapshot()?;
    match load_lockfile_optional(&snapshot.lock_path)? {
        Some(lock) => {
            let report = analyze_lock_diff(&snapshot, &lock, None);
            let mut details = report.to_json(&snapshot);
            if report.is_clean() {
                Ok(ExecutionOutcome::success(report.summary(), details))
            } else {
                details["hint"] = Value::String(
                    "run `px install` (or `px lock upgrade`) to regenerate the lock".to_string(),
                );
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
                    "missing px.lock at {} (run `px install` first)",
                    snapshot.lock_path.display()
                ),
                details,
            ))
        }
    }
}

fn handle_lock_upgrade(_ctx: &CommandContext, _command: &PxCommand) -> Result<ExecutionOutcome> {
    let snapshot = manifest_snapshot()?;
    let lock_path = snapshot.lock_path.clone();
    let lock = match load_lockfile_optional(&lock_path)? {
        Some(lock) => lock,
        None => {
            return Ok(ExecutionOutcome::user_error(
                "missing px.lock (run `px install` first)",
                json!({
                    "status": "missing_lock",
                    "lockfile": lock_path.display().to_string(),
                    "hint": "run `px install` to create a lock before upgrading",
                }),
            ))
        }
    };

    if lock.version >= 2 {
        return Ok(ExecutionOutcome::success(
            "lock already at version 2",
            json!({
                "lockfile": lock_path.display().to_string(),
                "version": lock.version,
                "status": "unchanged",
            }),
        ));
    }

    let upgraded = render_lockfile_v2(&snapshot, &lock, PX_VERSION)?;
    fs::write(&lock_path, upgraded)?;

    Ok(ExecutionOutcome::success(
        "upgraded lock to version 2",
        json!({
            "lockfile": lock_path.display().to_string(),
            "version": 2,
            "status": "upgraded",
        }),
    ))
}


fn handle_workspace_verify(ctx: &CommandContext, _command: &PxCommand) -> Result<ExecutionOutcome> {
    workspace::verify(ctx)
}




fn handle_workspace_tidy(ctx: &CommandContext, _command: &PxCommand) -> Result<ExecutionOutcome> {
    workspace::tidy(ctx)
}

fn lock_is_fresh(snapshot: &ManifestSnapshot) -> Result<bool> {
    match load_lockfile_optional(&snapshot.lock_path)? {
        Some(lock) => Ok(detect_lock_drift(snapshot, &lock, None).is_empty()),
        None => Ok(false),
    }
}

pub(crate) fn manifest_snapshot() -> Result<ManifestSnapshot> {
    Ok(px_project::ProjectSnapshot::read_current()?)
}

pub(crate) fn manifest_snapshot_at(root: &Path) -> Result<ManifestSnapshot> {
    Ok(px_project::ProjectSnapshot::read_from(root)?)
}

pub(crate) fn install_snapshot(
    ctx: &CommandContext,
    snapshot: &ManifestSnapshot,
    frozen: bool,
    override_pins: Option<&InstallOverride>,
) -> Result<InstallOutcome> {
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
        let mut dependencies = if let Some(override_data) = override_pins {
            override_data.dependencies.clone()
        } else {
            snapshot.dependencies.clone()
        };
        let mut resolved_override = None;
        if override_pins.is_none()
            && dependencies_require_resolution(&dependencies)
            && ctx.config().resolver.enabled
        {
            let resolved = resolve_dependencies(snapshot)?;
            let marker_env = current_marker_environment()?;
            dependencies = merge_resolved_dependencies(&dependencies, &resolved.specs, &marker_env);
            resolved_override = Some(resolved.pins);
            persist_resolved_dependencies(snapshot, &dependencies)?;
        }
        let pins = if let Some(override_data) = override_pins {
            pins_with_override(&dependencies, override_data)?
        } else {
            match resolved_override {
                Some(pins) => pins,
                None => ensure_exact_pins(&dependencies)?,
            }
        };
        let resolved = resolve_pins(&pins, ctx.cache(), ctx.config().resolver.force_sdist)?;
        let contents = render_lockfile(snapshot, &resolved, PX_VERSION)?;
        fs::write(&snapshot.lock_path, contents)?;
        Ok(InstallOutcome {
            state: InstallState::Installed,
            lockfile,
            drift: Vec::new(),
            verified: false,
        })
    }
}

fn refresh_project_site(snapshot: &ManifestSnapshot) -> Result<()> {
    let lock = load_lockfile_optional(&snapshot.lock_path)?.ok_or_else(|| {
        anyhow!(
            "px install: lockfile missing at {}",
            snapshot.lock_path.display()
        )
    })?;
    materialize_project_site(snapshot, &lock)
}

fn materialize_project_site(snapshot: &ManifestSnapshot, lock: &LockSnapshot) -> Result<()> {
    let site_dir = snapshot.root.join(".px").join("site");
    fs::create_dir_all(&site_dir)?;
    let pth_path = site_dir.join("px.pth");

    let mut entries = Vec::new();
    for dep in &lock.resolved {
        let Some(artifact) = &dep.artifact else {
            continue;
        };
        if artifact.cached_path.is_empty() {
            continue;
        }
        let path = PathBuf::from(&artifact.cached_path);
        if !path.exists() {
            continue;
        }
        let canonical = path.canonicalize().unwrap_or(path);
        entries.push(canonical);
    }

    entries.sort();
    entries.dedup();

    let mut contents = entries
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    if !contents.is_empty() {
        contents.push('\n');
    }
    fs::write(&pth_path, contents)?;
    Ok(())
}

fn ensure_project_site_bootstrap(project_root: &Path) {
    let pth_path = project_root.join(".px").join("site").join("px.pth");
    if pth_path.exists() {
        return;
    }
    let lock_path = project_root.join("px.lock");
    if !lock_path.exists() {
        return;
    }
    let snapshot = ManifestSnapshot {
        root: project_root.to_path_buf(),
        manifest_path: project_root.join("pyproject.toml"),
        lock_path,
        name: String::new(),
        python_requirement: String::new(),
        dependencies: Vec::new(),
    };
    match load_lockfile_optional(&snapshot.lock_path) {
        Ok(Some(lock)) => {
            if let Err(err) = materialize_project_site(&snapshot, &lock) {
                warn!("failed to refresh .px/site from px.lock: {err:?}");
            }
        }
        Ok(None) => {}
        Err(err) => {
            warn!(
                "failed to load px.lock at {}: {err:?}",
                snapshot.lock_path.display()
            );
        }
    }
}

fn verify_lock(snapshot: &ManifestSnapshot) -> Result<InstallOutcome> {
    let lockfile = snapshot.lock_path.display().to_string();
    match load_lockfile_optional(&snapshot.lock_path)? {
        Some(lock) => {
            let report = analyze_lock_diff(snapshot, &lock, None);
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

pub(crate) fn store_prefetch_specs<'a>(entries: &'a [LockPrefetchSpec]) -> Vec<StorePrefetchSpec<'a>> {
    entries
        .iter()
        .map(|entry| StorePrefetchSpec {
            name: entry.name.as_str(),
            version: entry.version.as_str(),
            filename: entry.filename.as_str(),
            url: entry.url.as_str(),
            sha256: entry.sha256.as_str(),
        })
        .collect()
}

fn ensure_exact_pins(specs: &[String]) -> Result<Vec<PinSpec>> {
    let marker_env = current_marker_environment()?;
    let mut pins = Vec::new();
    for spec in specs {
        if !marker_applies(spec, &marker_env) {
            continue;
        }
        pins.push(parse_exact_pin(spec)?);
    }
    Ok(pins)
}

fn parse_exact_pin(spec: &str) -> Result<PinSpec> {
    let trimmed_raw = spec.trim();
    let trimmed = strip_wrapping_quotes(trimmed_raw);
    if trimmed.is_empty() {
        return Err(InstallUserError::new(
            "dependency specifier cannot be empty",
            json!({ "specifier": spec }),
        )
        .into());
    }
    let (base, marker) = if let Some((head, tail)) = trimmed.split_once(';') {
        (head.trim(), Some(tail.trim().to_string()))
    } else {
        (trimmed, None)
    };
    if base.contains('[') {
        return Err(InstallUserError::new(
            "extras are not supported in pinned installs",
            json!({ "specifier": base }),
        )
        .into());
    }

    let Some((name_part, version_part)) = base.split_once("==") else {
        return Err(InstallUserError::new(
            format!("px install requires `name==version`; `{base}` is not pinned"),
            json!({ "specifier": base }),
        )
        .into());
    };

    let version_str = version_part.trim();
    if version_str.is_empty() {
        return Err(InstallUserError::new(
            "version after `==` cannot be empty",
            json!({ "specifier": base }),
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
            json!({ "specifier": base }),
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
        marker,
    })
}

fn resolve_pins(
    pins: &[PinSpec],
    cache: &CacheLocation,
    force_sdist: bool,
) -> Result<Vec<ResolvedDependency>> {
    if pins.is_empty() {
        return Ok(Vec::new());
    }

    let client = build_http_client()?;
    let python = px_python::detect_interpreter()?;
    let tags = detect_interpreter_tags_with(&python)?;
    let mut resolved = Vec::new();
    for pin in pins {
        let release = fetch_release(&client, &pin.normalized, &pin.version, &pin.specifier)?;
        let artifact = if force_sdist {
            build_wheel_via_sdist(cache, &release, pin, &python)?
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
                Err(err) => match build_wheel_via_sdist(cache, &release, pin, &python) {
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

fn current_marker_environment() -> Result<MarkerEnvironment> {
    let python = px_python::detect_interpreter()?;
    let resolver_env = detect_marker_environment_with(&python)?;
    resolver_env.to_marker_environment()
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

fn current_project_root() -> Result<PathBuf> {
    Ok(px_project::current_project_root()?)
}

#[allow(dead_code)]
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

#[allow(dead_code)]
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

#[allow(dead_code)]
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

pub(crate) fn ensure_pyproject_exists(path: &Path) -> Result<()> {
    if path.exists() {
        Ok(())
    } else {
        bail!(
            "pyproject.toml not found in {}",
            path.parent().unwrap_or(path).display()
        )
    }
}

fn resolve_onboard_path(
    root: &Path,
    override_value: Option<&str>,
    default_name: &str,
) -> Result<Option<PathBuf>> {
    if let Some(raw) = override_value {
        let candidate = normalize_onboard_path(root, PathBuf::from(raw));
        if !candidate.exists() {
            bail!("path not found: {}", candidate.display());
        }
        return Ok(Some(candidate));
    }
    let candidate = root.join(default_name);
    if candidate.exists() {
        Ok(Some(candidate))
    } else {
        Ok(None)
    }
}

fn summarize_autopins(entries: &[AutopinEntry]) -> Option<String> {
    if entries.is_empty() {
        return None;
    }
    let mut labels = Vec::new();
    for entry in entries.iter().take(3) {
        labels.push(entry.short_label());
    }
    let mut summary = format!(
        "Pinned {} package{} automatically",
        entries.len(),
        if entries.len() == 1 { "" } else { "s" }
    );
    if !labels.is_empty() {
        summary.push_str(" (");
        summary.push_str(&labels.join(", "));
        if entries.len() > 3 {
            summary.push_str(&format!(", +{} more", entries.len() - 3));
        }
        summary.push(')');
    }
    Some(summary)
}

fn marker_applies(spec: &str, marker_env: &MarkerEnvironment) -> bool {
    let cleaned = strip_wrapping_quotes(spec.trim());
    match PepRequirement::from_str(cleaned) {
        Ok(req) => req.evaluate_markers(marker_env, &[]),
        Err(_) => true,
    }
}

fn spec_requires_pin(spec: &str) -> bool {
    let head = spec.split(';').next().unwrap_or(spec).trim();
    !head.contains("==")
}

fn merge_resolved_dependencies(
    original: &[String],
    resolved: &[String],
    marker_env: &MarkerEnvironment,
) -> Vec<String> {
    let mut merged = Vec::with_capacity(original.len());
    let mut resolved_iter = resolved.iter();
    for spec in original {
        if spec_requires_pin(spec) && marker_applies(spec, marker_env) {
            if let Some(pinned) = resolved_iter.next() {
                merged.push(pinned.clone());
            } else {
                merged.push(spec.clone());
            }
        } else {
            merged.push(spec.clone());
        }
    }
    debug_assert!(resolved_iter.next().is_none(), "resolver/pin mismatch");
    merged
}

fn autopin_spec_key(spec: &str) -> String {
    match PepRequirement::from_str(spec.trim()) {
        Ok(req) => {
            let name = req.name.to_string().to_ascii_lowercase();
            let mut extras = req
                .extras
                .iter()
                .map(|extra| extra.to_string().to_ascii_lowercase())
                .collect::<Vec<_>>();
            extras.sort();
            let extras_part = extras.join(",");
            let marker_part = req
                .marker
                .as_ref()
                .map(|m| canonicalize_marker(&m.to_string()))
                .unwrap_or_default();
            format!("{name}|{extras_part}|{marker_part}")
        }
        Err(_) => {
            let name = dependency_name(spec);
            format!("{name}||")
        }
    }
}

fn autopin_pin_key(pin: &PinSpec) -> String {
    let mut extras = pin
        .extras
        .iter()
        .map(|extra| extra.to_ascii_lowercase())
        .collect::<Vec<_>>();
    extras.sort();
    let extras_part = extras.join(",");
    let marker_part = pin
        .marker
        .as_deref()
        .map(canonicalize_marker)
        .unwrap_or_default();
    format!("{}|{extras_part}|{marker_part}", pin.normalized)
}

fn canonicalize_marker(raw: &str) -> String {
    raw.split_whitespace()
        .collect::<String>()
        .to_ascii_lowercase()
}

fn pins_with_override(
    dependencies: &[String],
    override_pins: &InstallOverride,
) -> Result<Vec<PinSpec>> {
    let marker_env = current_marker_environment()?;
    let mut lookup: HashMap<String, VecDeque<PinSpec>> = HashMap::new();
    for pin in &override_pins.pins {
        lookup
            .entry(autopin_pin_key(pin))
            .or_insert_with(VecDeque::new)
            .push_back(pin.clone());
    }
    let mut pins = Vec::new();
    for spec in dependencies {
        if !marker_applies(spec, &marker_env) {
            continue;
        }
        let key = autopin_spec_key(spec);
        if let Some(queue) = lookup.get_mut(&key) {
            if let Some(pin) = queue.pop_front() {
                pins.push(pin);
                continue;
            }
        }
        pins.push(parse_exact_pin(spec)?);
    }
    Ok(pins)
}

fn normalize_onboard_path(root: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}

#[derive(Clone)]
struct PyprojectPlan {
    path: PathBuf,
    contents: Option<String>,
    created: bool,
}

impl PyprojectPlan {
    fn needs_backup(&self) -> bool {
        self.contents.is_some() && !self.created
    }

    fn updated(&self) -> bool {
        self.contents.is_some()
    }
}

struct BackupSummary {
    files: Vec<String>,
    directory: Option<String>,
}

struct BackupManager {
    root: PathBuf,
    dir: Option<PathBuf>,
    entries: Vec<String>,
}

impl BackupManager {
    fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            dir: None,
            entries: Vec::new(),
        }
    }

    fn backup(&mut self, path: &Path) -> Result<()> {
        if !path.exists() {
            return Ok(());
        }
        let dir = self.ensure_dir()?;
        let rel = relative_path(&self.root, path);
        let dest = dir.join(&rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(path, &dest)?;
        self.entries.push(relative_path(&self.root, &dest));
        Ok(())
    }

    fn ensure_dir(&mut self) -> Result<PathBuf> {
        if self.dir.is_none() {
            let fmt = format_description::parse("[year][month][day]T[hour][minute][second]")?;
            let stamp = OffsetDateTime::now_utc().format(&fmt)?;
            let dir = self.root.join(".px").join("onboard-backups").join(stamp);
            fs::create_dir_all(&dir)?;
            self.dir = Some(dir);
        }
        Ok(self.dir.clone().unwrap())
    }

    fn finish(self) -> BackupSummary {
        let dir_rel = self.dir.map(|dir| relative_path(&self.root, &dir));
        BackupSummary {
            files: self.entries,
            directory: dir_rel,
        }
    }
}

fn prepare_pyproject_plan(
    root: &Path,
    pyproject_path: &Path,
    lock_only: bool,
    packages: &[OnboardPackagePlan],
) -> Result<PyprojectPlan> {
    if lock_only {
        ensure_pyproject_exists(pyproject_path)?;
        return Ok(PyprojectPlan {
            path: pyproject_path.to_path_buf(),
            contents: None,
            created: false,
        });
    }

    let mut created = false;
    let mut doc: DocumentMut = if pyproject_path.exists() {
        fs::read_to_string(pyproject_path)?.parse()?
    } else {
        created = true;
        create_minimal_pyproject_doc(root)?
    };

    let mut prod_specs = Vec::new();
    let mut dev_specs = Vec::new();
    for pkg in packages {
        if pkg.source.ends_with("pyproject.toml") {
            continue;
        }
        if pkg.scope == "dev" {
            dev_specs.push(pkg.requested.clone());
        } else {
            prod_specs.push(pkg.requested.clone());
        }
    }

    let mut changed = false;
    changed |= merge_dependency_specs(&mut doc, &prod_specs);
    changed |= merge_dev_dependency_specs(&mut doc, &dev_specs);

    if changed || created {
        Ok(PyprojectPlan {
            path: pyproject_path.to_path_buf(),
            contents: Some(doc.to_string()),
            created,
        })
    } else {
        Ok(PyprojectPlan {
            path: pyproject_path.to_path_buf(),
            contents: None,
            created: false,
        })
    }
}

fn create_minimal_pyproject_doc(root: &Path) -> Result<DocumentMut> {
    let name = default_package_name(root);
    let template = format!("[project]\nname = \"{name}\"\nversion = \"0.1.0\"\n",)
        + "description = \"Onboarded by px\"\n"
        + "requires-python = \">=3.12\"\n"
        + "dependencies = []\n\n[build-system]\n"
        + "requires = [\"setuptools>=70\", \"wheel\"]\n"
        + "build-backend = \"setuptools.build_meta\"\n";
    let doc: DocumentMut = template.parse()?;
    Ok(doc)
}

fn default_package_name(root: &Path) -> String {
    let raw = root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("px_onboard");
    sanitize_package_name(raw)
}

fn sanitize_package_name(raw: &str) -> String {
    let mut name = raw
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>()
        .replace('-', "_")
        .to_lowercase();
    if name.is_empty() || !name.chars().next().unwrap().is_ascii_alphabetic() {
        name = format!("px_{name}");
    }
    name
}

fn merge_dependency_specs(doc: &mut DocumentMut, specs: &[String]) -> bool {
    if specs.is_empty() {
        return false;
    }
    let array = ensure_dependencies_array_mut(doc);
    let mut changed = false;
    for spec in specs {
        if !array.iter().any(|val| val.as_str() == Some(spec.as_str())) {
            array.push(spec.as_str());
            changed = true;
        }
    }
    changed
}

fn merge_dev_dependency_specs(doc: &mut DocumentMut, specs: &[String]) -> bool {
    if specs.is_empty() {
        return false;
    }
    let array = ensure_optional_dependency_array_mut(doc, "px-dev");
    let mut changed = false;
    for spec in specs {
        if !array.iter().any(|val| val.as_str() == Some(spec.as_str())) {
            array.push(spec.as_str());
            changed = true;
        }
    }
    changed
}

fn ensure_dependencies_array_mut(doc: &mut DocumentMut) -> &mut Array {
    if !doc["project"].is_table() {
        doc["project"] = Item::Table(Table::default());
    }
    if !doc["project"]["dependencies"].is_array() {
        doc["project"]["dependencies"] = Item::Value(TomlValue::Array(Array::new()));
    }
    doc["project"]["dependencies"].as_array_mut().unwrap()
}

fn ensure_optional_dependency_array_mut<'a>(
    doc: &'a mut DocumentMut,
    group: &str,
) -> &'a mut Array {
    if !doc["project"].is_table() {
        doc["project"] = Item::Table(Table::default());
    }
    let project_table = doc["project"].as_table_mut().unwrap();
    if !project_table.contains_key("optional-dependencies")
        || !project_table["optional-dependencies"].is_table()
    {
        project_table["optional-dependencies"] = Item::Table(Table::default());
    }
    let table = project_table["optional-dependencies"]
        .as_table_mut()
        .unwrap();
    if !table.contains_key(group) || !table[group].is_array() {
        table[group] = Item::Value(TomlValue::Array(Array::new()));
    }
    table[group].as_array_mut().unwrap()
}

fn git_worktree_changes(root: &Path) -> Result<Option<Vec<String>>> {
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(root)
        .output();
    match output {
        Ok(out) if out.status.success() => {
            let lines = String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(|line| line.to_string())
                .collect::<Vec<_>>();
            Ok(Some(lines))
        }
        Ok(_) => Ok(None),
        Err(_) => Ok(None),
    }
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

fn dependency_name(spec: &str) -> String {
    let trimmed = strip_wrapping_quotes(spec.trim());
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

fn strip_wrapping_quotes(input: &str) -> &str {
    if input.len() >= 2 {
        let bytes = input.as_bytes();
        let first = bytes[0];
        let last = bytes[input.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &input[1..input.len() - 1];
        }
    }
    input
}

fn outcome_from_output(
    command_name: &str,
    target: &str,
    output: RunOutput,
    prefix: &str,
    extra: Option<Value>,
) -> ExecutionOutcome {
    let mut details = json!({
        "stdout": output.stdout,
        "stderr": output.stderr,
        "code": output.code,
        "target": target,
    });

    if let Some(extra_value) = extra {
        if let Value::Object(map) = extra_value {
            if let Some(details_map) = details.as_object_mut() {
                for (key, value) in map {
                    details_map.insert(key, value);
                }
            }
        } else {
            details["extra"] = extra_value;
        }
    }

    if output.code == 0 {
        let stdout = output.stdout.trim_end();
        if !stdout.is_empty() {
            details["passthrough"] = Value::Bool(true);
            return ExecutionOutcome::success(stdout.to_string(), details);
        }
        let stderr = output.stderr.trim_end();
        if !stderr.is_empty() {
            details["passthrough"] = Value::Bool(true);
            return ExecutionOutcome::success(stderr.to_string(), details);
        }
        let message = format!("{prefix} {command_name}({target}) succeeded");
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
        ensure_project_site_bootstrap(&project_root);
        let python = px_python::detect_interpreter()?;
        let pythonpath = build_pythonpath(&project_root)?;
        Ok(Self {
            project_root,
            python,
            pythonpath,
        })
    }

    fn base_env(&self, command_args: &Value) -> Result<Vec<(String, String)>> {
        let mut envs = Vec::new();
        envs.push(("PYTHONPATH".into(), self.pythonpath.clone()));
        envs.push(("PYTHONUNBUFFERED".into(), "1".into()));
        envs.push((
            "PX_PROJECT_ROOT".into(),
            self.project_root.display().to_string(),
        ));
        envs.push(("PX_COMMAND_JSON".into(), command_args.to_string()));
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

    if let Some(site_dir) = project_root.join(".px").join("site").canonicalize().ok() {
        paths.push(site_dir.clone());
        let pth = site_dir.join("px.pth");
        if pth.exists() {
            if let Ok(contents) = fs::read_to_string(&pth) {
                for line in contents.lines() {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let path = PathBuf::from(trimmed);
                    if path.exists() {
                        paths.push(path);
                    }
                }
            }
        }
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
    let group_name = command.group.to_string();
    let prefix = if matches!(command.group, CommandGroup::Output)
        && matches!(command.name.as_str(), "build" | "publish")
    {
        format!("px {}", command.name)
    } else if group_name == command.name {
        format!("px {}", command.name)
    } else {
        format!("px {} {}", group_name, command.name)
    };
    if message.is_empty() {
        prefix
    } else if message.starts_with(&prefix) {
        message.to_string()
    } else {
        format!("{prefix}: {message}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use px_lockfile::LockedDependency;
    use std::path::PathBuf;
    use tempfile::tempdir;

    #[test]
    fn config_respects_env_flags() {
        let snapshot = EnvSnapshot::testing(&[
            ("PX_ONLINE", "1"),
            ("PX_RESOLVER", "1"),
            ("PX_FORCE_SDIST", "1"),
            ("PX_TEST_FALLBACK_STD", "1"),
            ("PX_SKIP_TESTS", "1"),
        ]);
        let config = Config::from_snapshot(&snapshot).expect("config");
        assert!(config.network.online);
        assert!(config.resolver.enabled);
        assert!(config.resolver.force_sdist);
        assert!(config.test.fallback_builtin);
        assert_eq!(config.test.skip_tests_flag.as_deref(), Some("1"));
    }

    #[test]
    fn marker_applies_respects_python_version() {
        let env = current_marker_environment().expect("marker env");
        assert!(
            !marker_applies("tomli>=1.1.0; python_version < '3.11'", &env),
            "non-matching marker should be skipped"
        );
    }

    #[test]
    fn python_script_target_detects_relative_paths() {
        let root = PathBuf::from("/tmp/project");
        let (arg, path) =
            python_script_target("src/app.py", &root).expect("relative script detected");
        assert_eq!(arg, "src/app.py");
        assert_eq!(PathBuf::from(path), root.join("src/app.py"));
    }

    #[test]
    fn python_script_target_detects_absolute_paths() {
        let absolute = PathBuf::from("/opt/demo/main.py");
        let entry = absolute.to_string_lossy().to_string();
        let root = PathBuf::from("/tmp/project");
        let (arg, path) = python_script_target(&entry, &root).expect("absolute script detected");
        assert_eq!(arg, entry);
        assert_eq!(PathBuf::from(path), absolute);
    }

    #[test]
    fn python_script_target_ignores_non_python_files() {
        let root = PathBuf::from("/tmp/project");
        assert!(python_script_target("bin/tool", &root).is_none());
    }

    #[test]
    fn materialize_project_site_writes_cached_paths() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path();
        let cache_dir = root.join("cache");
        fs::create_dir_all(&cache_dir).expect("cache dir");
        let wheel = cache_dir.join("demo-1.0.0.whl");
        fs::write(&wheel, b"demo").expect("wheel stub");

        let snapshot = ManifestSnapshot {
            root: root.to_path_buf(),
            manifest_path: root.join("pyproject.toml"),
            lock_path: root.join("px.lock"),
            name: "demo".into(),
            python_requirement: ">=3.11".into(),
            dependencies: Vec::new(),
        };
        let lock = LockSnapshot {
            version: 1,
            project_name: Some("demo".into()),
            python_requirement: Some(">=3.11".into()),
            dependencies: Vec::new(),
            mode: Some("p0-pinned".into()),
            resolved: vec![LockedDependency {
                name: "demo".into(),
                artifact: Some(LockedArtifact {
                    filename: "demo.whl".into(),
                    url: "https://example.invalid/demo.whl".into(),
                    sha256: "abc123".into(),
                    size: 4,
                    cached_path: wheel.display().to_string(),
                    python_tag: "py3".into(),
                    abi_tag: "none".into(),
                    platform_tag: "any".into(),
                }),
            }],
            graph: None,
        };

        materialize_project_site(&snapshot, &lock).expect("materialize site");

        let pxpth = snapshot.root.join(".px").join("site").join("px.pth");
        assert!(
            pxpth.exists(),
            ".px/site/px.pth should be created alongside install"
        );
        let contents = fs::read_to_string(pxpth).expect("read px.pth");
        assert!(
            contents.contains(wheel.to_str().unwrap()),
            "px.pth should reference cached wheel path"
        );
    }

    #[test]
    fn materialize_project_site_skips_missing_artifacts() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path();
        let snapshot = ManifestSnapshot {
            root: root.to_path_buf(),
            manifest_path: root.join("pyproject.toml"),
            lock_path: root.join("px.lock"),
            name: "demo".into(),
            python_requirement: ">=3.11".into(),
            dependencies: Vec::new(),
        };
        let lock = LockSnapshot {
            version: 1,
            project_name: Some("demo".into()),
            python_requirement: Some(">=3.11".into()),
            dependencies: Vec::new(),
            mode: Some("p0-pinned".into()),
            resolved: vec![LockedDependency {
                name: "missing".into(),
                artifact: Some(LockedArtifact {
                    filename: "missing.whl".into(),
                    url: "https://example.invalid/missing.whl".into(),
                    sha256: "deadbeef".into(),
                    size: 0,
                    cached_path: root.join("nope").display().to_string(),
                    python_tag: "py3".into(),
                    abi_tag: "none".into(),
                    platform_tag: "any".into(),
                }),
            }],
            graph: None,
        };

        materialize_project_site(&snapshot, &lock).expect("materialize site with gap");
        let pxpth = snapshot.root.join(".px").join("site").join("px.pth");
        assert!(pxpth.exists(), "px.pth should still be created");
        let contents = fs::read_to_string(pxpth).expect("read px.pth");
        assert!(
            contents.trim().is_empty(),
            "missing artifacts should not be written to px.pth"
        );
    }
}
