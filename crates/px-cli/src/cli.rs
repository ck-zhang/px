use std::path::PathBuf;

use clap::{value_parser, ArgAction, Args, Parser, Subcommand, ValueEnum};
use clap_complete::engine::ArgValueCompleter;
use serde::{Deserialize, Serialize};

use crate::completion::run_target_completer;

pub const PX_HELP_TEMPLATE: &str =
    "{before-help}\nUsage:\n    {usage}\n\nGlobal options:\n{options}\n";

pub const PX_BEFORE_HELP: &str = concat!(
    "px ",
    env!("CARGO_PKG_VERSION"),
    " – Unified Python Project Manager\n\n",
    "\x1b[1;36mCore workflow\x1b[0m\n",
    "  init             Start a px project; writes pyproject.toml, px.lock, and .px/.\n",
    "  add / remove     Declare or drop dependencies; px.lock and the env stay in sync.\n",
    "  sync             Resolve (if needed) and sync env from lock (use --frozen in CI).\n",
    "  update           Resolve newer pins, rewrite px.lock, then sync the env.\n",
    "  run              Execute scripts/tasks; auto-repair the env from px.lock unless --frozen or CI=1.\n",
    "  test             Run tests with the same auto-repair rules as `px run`.\n\n",
    "\x1b[1;36mEssentials\x1b[0m\n",
    "  status           Check whether pyproject, px.lock, and the env still agree.\n",
    "  explain          Inspect what px would execute (no execution).\n",
    "  why              Explain why a dependency is present.\n",
    "  fmt              Run formatters/linters/cleanup tools via px-managed tool environments.\n",
    "  build            Build sdists/wheels from project sources.\n",
    "  publish          Upload previously built artifacts (dry-run by default; use --upload to push).\n",
    "  pack image       Freeze the current env + sandbox config into an OCI image.\n",
    "  pack app         Build a portable .pxapp bundle runnable via `px run <file>.pxapp`.\n",
    "  migrate          Create px metadata for an existing project.\n\n",
    "\x1b[1;36mAdvanced\x1b[0m\n",
    "  tool             Install and run px-managed global tools.\n",
    "  python           Manage px Python runtimes (list/install/use/info).\n",
    "  completions      Print shell completion snippet (optional, one-time setup).\n",
);

#[derive(Parser, Debug)]
#[command(
    name = "px",
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
        about = "Resolve (if needed) and sync env from lock (run after clone or drift).",
        override_usage = "px sync [--frozen]"
    )]
    Sync(SyncArgs),
    #[command(
        about = "Resolve newer versions, rewrite px.lock, then sync the env.",
        override_usage = "px update [<SPEC> ...]"
    )]
    Update(SpecArgs),
    #[command(
        about = "Run scripts/tasks; auto-repair the env from px.lock unless --frozen or CI=1. Also supports https://... URL targets and gh:/git+ run-by-reference forms. Exit code matches the target process.",
        override_usage = "px run <TARGET> [ARG...]"
    )]
    Run(RunArgs),
    #[command(
        about = "Run tests with the same auto-repair rules as px run. Exit code matches the test runner (except: no tests collected is ok outside CI/--frozen).",
        override_usage = "px test [-- <TEST_ARG>...]"
    )]
    Test(TestArgs),
    #[command(
        about = "Run configured formatters/linters via px-managed tool environments.",
        override_usage = "px fmt [-- <ARG>...]"
    )]
    Fmt(FmtArgs),
    #[command(about = "Report whether pyproject, px.lock, and the env are in sync (read-only).")]
    Status(StatusArgs),
    #[command(about = "Explain what px would execute (read-only).", subcommand)]
    Explain(ExplainCommand),
    #[command(
        about = "Build sdists/wheels from project sources (prep for px publish).",
        override_usage = "px build [sdist|wheel|both] [--out DIR]"
    )]
    Build(BuildArgs),
    #[command(
        about = "Publish previously built artifacts; dry-run by default.",
        override_usage = "px publish [--dry-run] [--registry NAME] [--token-env VAR]"
    )]
    Publish(PublishArgs),
    #[command(
        about = "Package sandboxed apps as OCI images or portable bundles.",
        subcommand
    )]
    Pack(PackCommand),
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
    #[command(
        about = "Print shell completion setup snippet for px",
        override_usage = "px completions <bash|zsh|fish|powershell>"
    )]
    Completions(CompletionsArgs),
}

#[derive(Subcommand, Debug)]
pub enum ExplainCommand {
    #[command(
        about = "Explain the execution plan for px run (no execution, no repairs).",
        override_usage = "px explain run <TARGET> [ARG...]"
    )]
    Run(RunArgs),
    #[command(
        about = "Explain which distribution provides a console_script entrypoint.",
        override_usage = "px explain entrypoint <NAME>"
    )]
    Entrypoint(ExplainEntrypointArgs),
}

#[derive(Args, Debug)]
pub struct ExplainEntrypointArgs {
    #[arg(value_name = "NAME")]
    pub name: String,
}

#[derive(Subcommand, Debug)]
pub enum PythonCommand {
    #[command(about = "List registered px runtimes.")]
    List,
    #[command(about = "Show the runtime in use for this project (and default).")]
    Info,
    #[command(about = "Register a Python interpreter for px to use.")]
    Install(PythonInstallArgs),
    #[command(about = "Select a runtime for the current project/workspace and sync lock/env.")]
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

#[derive(Clone, Copy, ValueEnum, Debug)]
pub enum CompletionShell {
    Bash,
    Zsh,
    Fish,
    Powershell,
}

#[derive(Args, Debug)]
pub struct CompletionsArgs {
    #[arg(value_enum)]
    pub shell: CompletionShell,
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
    #[arg(long, help = "Preview changes without writing files or building envs")]
    pub dry_run: bool,
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
    #[arg(long = "py", value_name = "VERSION", help = "Python version requirement (e.g. 3.11)")]
    pub py: Option<String>,
    #[arg(
        long,
        help = "Bypass the dirty-worktree guard when scaffolding a project"
    )]
    pub force: bool,
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
    #[arg(
        long,
        help = "Fail if px.lock is missing or out of date (do not resolve dependencies)"
    )]
    pub frozen: bool,
}

#[derive(Args, Debug)]
pub struct RunArgs {
    #[arg(
        long,
        help = "Target to run (same as positional TARGET; supports https://... URL targets and gh:/git+ run-by-reference forms)",
        add = ArgValueCompleter::new(run_target_completer)
    )]
    pub target: Option<String>,
    #[arg(
        long,
        help = "Allow floating git refs in URL/gh:/git+ run targets (resolved to a commit at runtime)"
    )]
    pub allow_floating: bool,
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
        alias = "try",
        help = "Run without adopting the directory (no .px/ or px.lock writes)",
        conflicts_with = "at"
    )]
    pub ephemeral: bool,
    #[arg(
        long,
        help = "Execute inside the sandbox derived from [tool.px.sandbox]"
    )]
    pub sandbox: bool,
    #[arg(
        long,
        value_name = "GIT_REF",
        help = "Run using the manifest and lock at a past git ref without checking it out"
    )]
    pub at: Option<String>,
    #[arg(
        value_name = "TARGET",
        allow_hyphen_values = true,
        add = ArgValueCompleter::new(run_target_completer),
        help = "Target to run. URL forms: https://github.com/ORG/REPO/blob/<sha>/path/to/script.py (also raw.githubusercontent.com) or https://github.com/ORG/REPO/tree/<sha>/ (repo URL; may be followed by an entrypoint arg). Run-by-reference form: gh:ORG/REPO@<sha>:path/to/script.py or git+file:///abs/path/to/repo@<sha>:path/to/script.py"
    )]
    pub target_value: Option<String>,
    #[arg(
        value_name = "ARG",
        trailing_var_arg = true,
        allow_hyphen_values = true,
        num_args = 0..,
        help = "Arguments forwarded to the target"
    )]
    pub args: Vec<String>,
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
        alias = "try",
        help = "Run without adopting the directory (no .px/ or px.lock writes)",
        conflicts_with = "at"
    )]
    pub ephemeral: bool,
    #[arg(
        long,
        help = "Execute tests inside the sandbox derived from [tool.px.sandbox]"
    )]
    pub sandbox: bool,
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
        help = "Fail if required tools are missing or not ready (no auto-install/repair)"
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
    #[arg(
        long,
        value_name = "DIR",
        help = "Write artifacts to this directory (default: dist/)"
    )]
    pub out: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
pub enum PackCommand {
    #[command(
        about = "Build a sandbox-backed OCI image from the current env profile.",
        override_usage = "px pack image [--tag NAME] [--out PATH] [--push] [--allow-dirty]"
    )]
    Image(PackImageArgs),
    #[command(
        about = "Build a portable .pxapp bundle runnable via `px run <bundle>`.",
        override_usage = "px pack app [--out PATH] [--allow-dirty] [--entrypoint CMD] [--workdir DIR]"
    )]
    App(PackAppArgs),
}

#[derive(Args, Debug)]
pub struct PackImageArgs {
    #[arg(long, value_name = "TAG", help = "Explicit image tag to apply")]
    pub tag: Option<String>,
    #[arg(
        long,
        value_parser = value_parser!(PathBuf),
        value_name = "PATH",
        help = "Write OCI output to this directory or tarball"
    )]
    pub out: Option<PathBuf>,
    #[arg(long, help = "Push the built image to a registry")]
    pub push: bool,
    #[arg(long, help = "Allow packing with a dirty working tree")]
    pub allow_dirty: bool,
}

#[derive(Args, Debug)]
pub struct PackAppArgs {
    #[arg(
        long,
        value_parser = value_parser!(PathBuf),
        value_name = "PATH",
        help = "Write the .pxapp bundle to this path (default: dist/<name>-<version>.pxapp)"
    )]
    pub out: Option<PathBuf>,
    #[arg(long, help = "Allow packing with a dirty working tree")]
    pub allow_dirty: bool,
    #[arg(
        long,
        value_name = "CMD",
        help = "Override the bundle entrypoint (quote to include spaces, e.g. \"python -m app\")"
    )]
    pub entrypoint: Option<String>,
    #[arg(
        long,
        value_parser = value_parser!(PathBuf),
        value_name = "DIR",
        help = "Override the bundle working directory (default: /app)"
    )]
    pub workdir: Option<PathBuf>,
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
    #[arg(long, value_name = "NAME", help = "Registry name or upload URL (default: pypi)")]
    pub registry: Option<String>,
    #[arg(
        long = "token-env",
        value_name = "VAR",
        help = "Environment variable containing the registry token (default: PX_PUBLISH_TOKEN)"
    )]
    pub token_env: Option<String>,
}
