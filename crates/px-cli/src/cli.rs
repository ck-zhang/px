use std::path::PathBuf;

use clap::{value_parser, ArgAction, Args, Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};

pub const PX_HELP_TEMPLATE: &str =
    "{before-help}\nUsage:\n    {usage}\n\nGlobal options:\n{options}\n";

pub const PX_BEFORE_HELP: &str = concat!(
    "px ",
    env!("CARGO_PKG_VERSION"),
    " – Unified Python Project Manager\n\n",
    "\x1b[1;36mCore workflow\x1b[0m\n",
    "  init             Start a px project; writes pyproject.toml, px.lock, and .px/.\n",
    "  add / remove     Declare or drop dependencies; px.lock and the env stay in sync.\n",
    "  sync             Reconcile the environment with px.lock (use --frozen in CI).\n",
    "  update           Resolve newer pins, rewrite px.lock, then sync the env.\n",
    "  run              Execute scripts/tasks; auto-sync unless --frozen or CI=1.\n",
    "  test             Run tests with the same auto-sync rules as `px run`.\n\n",
    "\x1b[1;36mEssentials\x1b[0m\n",
    "  status           Check whether pyproject, px.lock, and the env still agree.\n",
    "  why              Explain why a dependency is present.\n",
    "  fmt              Run formatters/linters/cleanup tools inside the px environment.\n",
    "  build            Produce sdists/wheels from the px-managed environment.\n",
    "  publish          Upload previously built artifacts (dry-run by default; use --upload to push).\n",
    "  migrate          Create px metadata for an existing project.\n\n",
    "\x1b[1;36mAdvanced\x1b[0m\n",
    "  tool             Install and run px-managed global tools.\n",
    "  python           Manage px Python runtimes (list/install/use/info).\n",
);

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    propagate_version = false,
    disable_help_subcommand = true,
    before_help = PX_BEFORE_HELP,
    help_template = PX_HELP_TEMPLATE
)]
#[allow(clippy::struct_excessive_bools)]
pub struct PxCli {
    #[arg(
        short,
        long,
        help = "Suppress human output (errors still print to stderr)",
        global = true
    )]
    pub quiet: bool,
    #[arg(short, long, action = ArgAction::Count, help = "Increase logging (-vv reaches trace)")]
    pub verbose: u8,
    #[arg(long, help = "Force trace logging regardless of -v/-q", global = true)]
    pub trace: bool,
    #[arg(long, help = "Enable debug output and full tracebacks", global = true)]
    pub debug: bool,
    #[arg(
        long,
        help = "Emit {status,message,details} JSON envelopes",
        global = true
    )]
    pub json: bool,
    #[arg(long, help = "Disable colored human output", global = true)]
    pub no_color: bool,
    #[arg(
        long,
        help = "Force px to run offline for this invocation (sets PX_ONLINE=0)",
        conflicts_with = "online",
        global = true
    )]
    pub offline: bool,
    #[arg(
        long,
        help = "Force px to run online even if PX_ONLINE=0",
        conflicts_with = "offline",
        global = true
    )]
    pub online: bool,
    #[arg(
        long,
        help = "Skip the dependency resolver (use existing pins only; sets PX_RESOLVER=0)",
        conflicts_with = "resolver",
        global = true
    )]
    pub no_resolver: bool,
    #[arg(
        long,
        help = "Force the dependency resolver even if PX_RESOLVER=0",
        conflicts_with = "no_resolver",
        global = true
    )]
    pub resolver: bool,
    #[arg(
        long,
        help = "Build from sdists even when wheels exist (sets PX_FORCE_SDIST=1)",
        conflicts_with = "prefer_wheels",
        global = true
    )]
    pub force_sdist: bool,
    #[arg(
        long,
        help = "Prefer wheels when available (sets PX_FORCE_SDIST=0)",
        conflicts_with = "force_sdist",
        global = true
    )]
    pub prefer_wheels: bool,
    #[command(subcommand)]
    pub command: CommandGroupCli,
}

#[derive(Subcommand, Debug)]
pub enum CommandGroupCli {
    #[command(
        about = "Start a px project: writes pyproject, px.lock, and an empty env.",
        override_usage = "px init [--package NAME] [--py VERSION]"
    )]
    Init(InitArgs),
    #[command(
        about = "Declare direct dependencies and immediately sync px.lock + env.",
        override_usage = "px add <SPEC> [SPEC ...]"
    )]
    Add(SpecArgs),
    #[command(
        about = "Remove direct dependencies and re-sync px.lock + env.",
        override_usage = "px remove <NAME> [NAME ...]"
    )]
    Remove(SpecArgs),
    #[command(
        about = "Sync the project environment from px.lock (run after clone or drift).",
        override_usage = "px sync [--frozen]"
    )]
    Sync(SyncArgs),
    #[command(
        about = "Resolve newer versions, rewrite px.lock, then sync the env.",
        override_usage = "px update [<SPEC> ...]"
    )]
    Update(SpecArgs),
    #[command(
        about = "Run scripts/tasks with auto-sync unless --frozen or CI=1.",
        override_usage = "px run <TARGET> [ARG...]"
    )]
    Run(RunArgs),
    #[command(
        about = "Run tests with the same auto-sync rules as px run.",
        override_usage = "px test [-- <TEST_ARG>...]"
    )]
    Test(TestArgs),
    #[command(
        about = "Run configured formatters inside the px environment.",
        override_usage = "px fmt [-- <ARG>...]"
    )]
    Fmt(FmtArgs),
    #[command(about = "Report whether pyproject, px.lock, and the env are in sync (read-only).")]
    Status(StatusArgs),
    #[command(
        about = "Build sdists/wheels using the px env (prep for px publish).",
        override_usage = "px build [sdist|wheel|both] [--out DIR]"
    )]
    Build(BuildArgs),
    #[command(
        about = "Publish previously built artifacts; dry-run by default.",
        override_usage = "px publish [--dry-run] [--registry NAME] [--token-env VAR]"
    )]
    Publish(PublishArgs),
    #[command(about = "Create px metadata for an existing project.")]
    Migrate(MigrateArgs),
    #[command(
        about = "Explain why a dependency is present in the project.",
        override_usage = "px why <PACKAGE>"
    )]
    Why(WhyArgs),
    #[command(
        about = "Manage px-managed CLI tools.",
        override_usage = "px tool <install|run|list|remove|upgrade>",
        subcommand
    )]
    Tool(ToolCommand),
    #[command(
        about = "Manage px Python runtimes.",
        override_usage = "px python <list|install|use|info>",
        subcommand
    )]
    Python(PythonCommand),
}

#[derive(Subcommand, Debug)]
pub enum PythonCommand {
    #[command(about = "List registered px runtimes.")]
    List,
    #[command(about = "Show the runtime in use for this project (and default).")]
    Info,
    #[command(about = "Register a Python interpreter for px to use.")]
    Install(PythonInstallArgs),
    #[command(about = "Record the runtime version for the current project.")]
    Use(PythonUseArgs),
}

#[derive(Args, Debug)]
pub struct PythonInstallArgs {
    #[arg(value_name = "VERSION", help = "Python version channel (e.g. 3.11)")]
    pub version: String,
    #[arg(long, value_parser = value_parser!(PathBuf), help = "Explicit interpreter path")]
    pub path: Option<PathBuf>,
    #[arg(long, help = "Mark this runtime as px's default")]
    pub default: bool,
}

#[derive(Args, Debug)]
pub struct PythonUseArgs {
    #[arg(value_name = "VERSION", help = "Python version channel (e.g. 3.11)")]
    pub version: String,
}

#[derive(Subcommand, Debug)]
pub enum ToolCommand {
    #[command(
        about = "Install a px-managed CLI tool.",
        override_usage = "px tool install <NAME> [SPEC] [--python VERSION] [--module MODULE]"
    )]
    Install(ToolInstallArgs),
    #[command(
        about = "Run an installed tool (forwards arguments after --).",
        override_usage = "px tool run <NAME> [-- <ARG>...]"
    )]
    Run(ToolRunArgs),
    #[command(about = "List installed px-managed tools.")]
    List,
    #[command(about = "Remove an installed tool and its cached environment.")]
    Remove(ToolRemoveArgs),
    #[command(
        about = "Upgrade a tool's dependencies to the latest allowed versions.",
        override_usage = "px tool upgrade <NAME> [--python VERSION]"
    )]
    Upgrade(ToolUpgradeArgs),
}

#[derive(Args, Debug)]
#[allow(clippy::struct_excessive_bools)]
pub struct MigrateArgs {
    #[arg(
        long = "python",
        value_name = "VERSION",
        help = "Python version channel to use for resolution (e.g. 3.11)"
    )]
    pub python: Option<String>,
    #[arg(
        long,
        action = ArgAction::SetTrue,
        help = "Preview the onboarding plan without touching files (default)",
        conflicts_with = "write"
    )]
    pub dry_run: bool,
    #[arg(
        long = "apply",
        alias = "write",
        action = ArgAction::SetTrue,
        help = "Apply the migration plan (writes px.lock and pyproject.toml)",
        conflicts_with = "dry_run"
    )]
    pub write: bool,
    #[arg(long, action = ArgAction::SetTrue, help = "Answer yes to upcoming prompts")]
    pub yes: bool,
    #[arg(
        long = "no-input",
        action = ArgAction::SetTrue,
        help = "Non-interactive mode; implied --yes"
    )]
    pub no_input: bool,
    #[arg(
        long,
        value_parser = value_parser!(PathBuf),
        help = "Explicit requirements source (defaults to requirements.txt)"
    )]
    pub source: Option<PathBuf>,
    #[arg(
        long,
        value_parser = value_parser!(PathBuf),
        help = "Explicit dev requirements source (defaults to requirements-dev.txt)"
    )]
    pub dev_source: Option<PathBuf>,
    #[arg(
        long,
        action = ArgAction::SetTrue,
        help = "Allow writes even when git status shows local changes"
    )]
    pub allow_dirty: bool,
    #[arg(
        long,
        action = ArgAction::SetTrue,
        help = "Only write px.lock; skip pyproject edits"
    )]
    pub lock_only: bool,
    #[arg(
        long,
        action = ArgAction::SetTrue,
        help = "Disable automatic pinning (will error if loose specs remain)"
    )]
    pub no_autopin: bool,
}

#[derive(Args, Debug, Clone, Default)]
pub struct CommonFlags {
    #[arg(long)]
    pub dry_run: bool,
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug)]
pub struct InitArgs {
    #[command(flatten)]
    pub common: CommonFlags,
    #[arg(
        long,
        value_name = "NAME",
        help = "Package module name (defaults to sanitized directory name)"
    )]
    pub package: Option<String>,
    #[arg(long = "py")]
    pub py: Option<String>,
}

#[derive(Args, Debug)]
pub struct SpecArgs {
    #[command(flatten)]
    pub common: CommonFlags,
    #[arg(value_name = "SPEC")]
    pub specs: Vec<String>,
}

#[derive(Args, Debug)]
pub struct WhyArgs {
    #[arg(value_name = "PACKAGE", conflicts_with = "issue")]
    pub package: Option<String>,
    #[arg(long, value_name = "ID", conflicts_with = "package")]
    pub issue: Option<String>,
}

#[derive(Args, Debug)]
pub struct SyncArgs {
    #[command(flatten)]
    pub common: CommonFlags,
    #[arg(long)]
    pub frozen: bool,
}

#[derive(Args, Debug)]
pub struct RunArgs {
    #[arg(long)]
    pub target: Option<String>,
    #[arg(long, help = "Force interactive stdio (inherit stdin/stdout/stderr)")]
    pub interactive: bool,
    #[arg(
        long,
        help = "Force non-interactive stdio (capture stdout/stderr; stdin disabled)",
        conflicts_with = "interactive"
    )]
    pub non_interactive: bool,
    #[arg(
        long,
        help = "Fail if px.lock is missing or the environment is out of sync"
    )]
    pub frozen: bool,
    #[arg(
        long,
        value_name = "GIT_REF",
        help = "Run using the manifest and lock at a past git ref without checking it out"
    )]
    pub at: Option<String>,
    #[arg(
        value_name = "TARGET",
        trailing_var_arg = true,
        allow_hyphen_values = true,
        num_args = 0..,
        help = "Target to run followed by any arguments to pass through"
    )]
    pub target_args: Vec<String>,
}

#[derive(Args, Debug)]
pub struct TestArgs {
    #[arg(
        long,
        help = "Fail if px.lock is missing or the environment is out of sync"
    )]
    pub frozen: bool,
    #[arg(
        long,
        value_name = "GIT_REF",
        help = "Run tests using the manifest and lock at a past git ref"
    )]
    pub at: Option<String>,
    #[arg(last = true, value_name = "TEST_ARG")]
    pub args: Vec<String>,
}

#[derive(Args, Debug)]
pub struct FmtArgs {
    #[arg(
        long,
        help = "Fail if px.lock is missing or the environment is out of sync"
    )]
    pub frozen: bool,
    #[arg(long, help = "Emit {status,message,details} JSON envelopes")]
    pub json: bool,
    #[arg(last = true, value_name = "ARG")]
    pub args: Vec<String>,
}

#[derive(Args, Debug)]
pub struct StatusArgs {
    #[arg(long, help = "One-line status summary (good for hooks and prompts)")]
    pub brief: bool,
}

#[derive(Args, Debug)]
pub struct ToolInstallArgs {
    #[arg(value_name = "NAME")]
    pub name: String,
    #[arg(
        value_name = "SPEC",
        help = "Optional requirement spec (default: NAME)"
    )]
    pub spec: Option<String>,
    #[arg(
        long,
        value_name = "VERSION",
        help = "Bind to a specific runtime version"
    )]
    pub python: Option<String>,
    #[arg(
        long,
        value_name = "MODULE",
        help = "Override module entry point (defaults to NAME)"
    )]
    pub module: Option<String>,
}

#[derive(Args, Debug)]
pub struct ToolRunArgs {
    #[arg(value_name = "NAME")]
    pub name: String,
    #[arg(
        long,
        value_name = "SCRIPT",
        help = "Invoke the given console script for this tool"
    )]
    pub console: Option<String>,
    #[arg(
        value_name = "ARG",
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    pub args: Vec<String>,
}

#[derive(Args, Debug)]
pub struct ToolRemoveArgs {
    #[arg(value_name = "NAME")]
    pub name: String,
}

#[derive(Args, Debug)]
pub struct ToolUpgradeArgs {
    #[arg(value_name = "NAME")]
    pub name: String,
    #[arg(
        long,
        value_name = "VERSION",
        help = "Switch the runtime version for this tool"
    )]
    pub python: Option<String>,
}

#[derive(Args, Debug)]
pub struct BuildArgs {
    #[command(flatten)]
    pub common: CommonFlags,
    #[arg(value_enum, default_value_t = BuildFormat::Both)]
    pub format: BuildFormat,
    #[arg(long)]
    pub out: Option<PathBuf>,
}

#[derive(ValueEnum, Debug, Clone, Copy, Serialize, Deserialize)]
pub enum BuildFormat {
    Sdist,
    Wheel,
    Both,
}

#[derive(Args, Debug)]
pub struct PublishArgs {
    #[arg(
        long,
        help = "Preview publish actions without uploading (default)",
        default_value_t = true
    )]
    pub dry_run: bool,
    #[arg(
        long,
        help = "Upload artifacts to the registry instead of performing a dry-run",
        conflicts_with = "dry_run"
    )]
    pub upload: bool,
    #[arg(long)]
    pub registry: Option<String>,
    #[arg(long = "token-env")]
    pub token_env: Option<String>,
}
