#![deny(clippy::all, warnings)]

use std::{env, path::PathBuf, sync::Arc};

use clap::{value_parser, ArgAction, Args, Parser, Subcommand, ValueEnum};
use color_eyre::{eyre::eyre, Result};
use px_core::{CommandContext, GlobalOptions, SystemEffects};
use serde::{Deserialize, Serialize};

mod dispatch;
mod output;
mod style;
mod traceback;

use dispatch::dispatch_command;
use output::{emit_output, OutputOptions};

const PX_HELP_TEMPLATE: &str = "{before-help}\nUsage:\n    {usage}\n\nGlobal options:\n{options}\n";

const PX_BEFORE_HELP: &str = concat!(
    "px ",
    env!("CARGO_PKG_VERSION"),
    " – deterministic Python project manager\n\n",
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
    "  publish          Upload previously built artifacts (dry-run by default).\n",
    "  migrate          Create px metadata for an existing project.\n\n",
    "\x1b[1;36mAdvanced\x1b[0m\n",
    "  tool             Install and run px-managed global tools.\n",
    "  python           Manage px Python runtimes (list/install/use/info).\n",
);

fn main() -> Result<()> {
    color_eyre::install()?;

    let cli = PxCli::parse();
    init_tracing(cli.trace, cli.verbose);

    let subcommand_json = matches!(&cli.command, CommandGroupCli::Fmt(args) if args.json);
    if cli.json || subcommand_json {
        // Suppress spinners/progress when JSON output is requested.
        env::set_var("PX_PROGRESS", "0");
    }
    let global = GlobalOptions {
        quiet: cli.quiet,
        verbose: cli.verbose,
        trace: cli.trace,
        json: cli.json || subcommand_json,
        config: cli.config.as_ref().map(|p| p.to_string_lossy().to_string()),
    };

    let ctx = CommandContext::new(&global, Arc::new(SystemEffects::new()))
        .map_err(|err| eyre!("{err:?}"))?;
    let (info, outcome) = dispatch_command(&ctx, &cli.command)?;
    let output_opts = OutputOptions {
        quiet: cli.quiet,
        json: cli.json,
        no_color: cli.no_color,
    };
    let code = emit_output(&output_opts, subcommand_json, info, &outcome)?;

    if code == 0 {
        Ok(())
    } else {
        std::process::exit(code);
    }
}

fn init_tracing(trace: bool, verbose: u8) {
    let level = if trace {
        "trace"
    } else {
        match verbose {
            0 => "info",
            1 => "debug",
            _ => "trace",
        }
    };

    let filter = format!("px={level},px_cli={level},px_core={level},px_domain={level}");
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_level(true)
        .finish();

    let _ = tracing::subscriber::set_global_default(subscriber);
}

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
struct PxCli {
    #[arg(
        short,
        long,
        help = "Suppress human output (errors still print to stderr)",
        global = true
    )]
    quiet: bool,
    #[arg(short, long, action = ArgAction::Count, help = "Increase logging (-vv reaches trace)")]
    verbose: u8,
    #[arg(long, help = "Force trace logging regardless of -v/-q", global = true)]
    trace: bool,
    #[arg(
        long,
        help = "Emit {status,message,details} JSON envelopes",
        global = true
    )]
    json: bool,
    #[arg(long, help = "Disable colored human output", global = true)]
    no_color: bool,
    #[arg(long, value_parser = value_parser!(PathBuf), help = "Optional px config file path", global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: CommandGroupCli,
}

#[derive(Subcommand, Debug)]
enum CommandGroupCli {
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
        override_usage = "px run [ENTRY] [-- <ARG>...]"
    )]
    Run(RunArgs),
    #[command(
        about = "Run tests with the same auto-sync rules as px run.",
        override_usage = "px test [-- <PYTEST_ARG>...]"
    )]
    Test(TestArgs),
    #[command(
        about = "Run configured formatters inside the px environment.",
        override_usage = "px fmt [-- <ARG>...]"
    )]
    Fmt(FmtArgs),
    #[command(about = "Report whether pyproject, px.lock, and the env are in sync (read-only).")]
    Status,
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
enum PythonCommand {
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
struct PythonInstallArgs {
    #[arg(value_name = "VERSION", help = "Python version channel (e.g. 3.11)")]
    version: String,
    #[arg(long, value_parser = value_parser!(PathBuf), help = "Explicit interpreter path")]
    path: Option<PathBuf>,
    #[arg(long, help = "Mark this runtime as px's default")]
    default: bool,
}

#[derive(Args, Debug)]
struct PythonUseArgs {
    #[arg(value_name = "VERSION", help = "Python version channel (e.g. 3.11)")]
    version: String,
}

#[derive(Subcommand, Debug)]
enum ToolCommand {
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
struct MigrateArgs {
    #[arg(
        long = "python",
        value_name = "VERSION",
        help = "Python version channel to use for resolution (e.g. 3.11)"
    )]
    python: Option<String>,
    #[arg(
        long,
        action = ArgAction::SetTrue,
        help = "Preview the onboarding plan without touching files (default)",
        conflicts_with = "write"
    )]
    dry_run: bool,
    #[arg(
        long = "apply",
        alias = "write",
        action = ArgAction::SetTrue,
        help = "Apply the migration plan (writes px.lock and pyproject.toml)",
        conflicts_with = "dry_run"
    )]
    write: bool,
    #[arg(long, action = ArgAction::SetTrue, help = "Answer yes to upcoming prompts")]
    yes: bool,
    #[arg(
        long = "no-input",
        action = ArgAction::SetTrue,
        help = "Non-interactive mode; implied --yes"
    )]
    no_input: bool,
    #[arg(
        long,
        value_parser = value_parser!(PathBuf),
        help = "Explicit requirements source (defaults to requirements.txt)"
    )]
    source: Option<PathBuf>,
    #[arg(
        long,
        value_parser = value_parser!(PathBuf),
        help = "Explicit dev requirements source (defaults to requirements-dev.txt)"
    )]
    dev_source: Option<PathBuf>,
    #[arg(
        long,
        action = ArgAction::SetTrue,
        help = "Allow writes even when git status shows local changes"
    )]
    allow_dirty: bool,
    #[arg(
        long,
        action = ArgAction::SetTrue,
        help = "Only write px.lock; skip pyproject edits"
    )]
    lock_only: bool,
    #[arg(
        long,
        action = ArgAction::SetTrue,
        help = "Disable automatic pinning (will error if loose specs remain)"
    )]
    no_autopin: bool,
}

#[derive(Args, Debug, Clone, Default)]
struct CommonFlags {
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    force: bool,
}

#[derive(Args, Debug)]
struct InitArgs {
    #[command(flatten)]
    common: CommonFlags,
    #[arg(
        long,
        value_name = "NAME",
        help = "Package module name (defaults to sanitized directory name)"
    )]
    package: Option<String>,
    #[arg(long = "py")]
    py: Option<String>,
}

#[derive(Args, Debug)]
struct SpecArgs {
    #[command(flatten)]
    common: CommonFlags,
    #[arg(value_name = "SPEC")]
    specs: Vec<String>,
}

#[derive(Args, Debug)]
struct WhyArgs {
    #[arg(value_name = "PACKAGE", conflicts_with = "issue")]
    package: Option<String>,
    #[arg(long, value_name = "ID", conflicts_with = "package")]
    issue: Option<String>,
}

#[derive(Args, Debug)]
struct SyncArgs {
    #[command(flatten)]
    common: CommonFlags,
    #[arg(long)]
    frozen: bool,
}

#[derive(Args, Debug)]
struct RunArgs {
    #[command(flatten)]
    common: CommonFlags,
    #[arg(
        value_name = "ENTRY",
        help = "Module or script name (omit to use the inferred default)"
    )]
    entry: Option<String>,
    #[arg(long)]
    target: Option<String>,
    #[arg(
        long,
        help = "Fail if px.lock is missing or the environment is out of sync"
    )]
    frozen: bool,
    #[arg(
        value_name = "ARG",
        trailing_var_arg = true,
        allow_hyphen_values = true,
        help = "Arguments forwarded to the entry or executable"
    )]
    args: Vec<String>,
}

#[derive(Args, Debug)]
struct TestArgs {
    #[command(flatten)]
    common: CommonFlags,
    #[arg(
        long,
        help = "Fail if px.lock is missing or the environment is out of sync"
    )]
    frozen: bool,
    #[arg(last = true, value_name = "PYTEST_ARG")]
    args: Vec<String>,
}

#[derive(Args, Debug)]
struct FmtArgs {
    #[command(flatten)]
    common: CommonFlags,
    #[arg(
        long,
        help = "Fail if px.lock is missing or the environment is out of sync"
    )]
    frozen: bool,
    #[arg(long, help = "Emit {status,message,details} JSON envelopes")]
    json: bool,
    #[arg(last = true, value_name = "ARG")]
    args: Vec<String>,
}

#[derive(Args, Debug)]
struct ToolInstallArgs {
    #[arg(value_name = "NAME")]
    name: String,
    #[arg(
        value_name = "SPEC",
        help = "Optional requirement spec (default: NAME)"
    )]
    spec: Option<String>,
    #[arg(
        long,
        value_name = "VERSION",
        help = "Bind to a specific runtime version"
    )]
    python: Option<String>,
    #[arg(
        long,
        value_name = "MODULE",
        help = "Override module entry point (defaults to NAME)"
    )]
    module: Option<String>,
}

#[derive(Args, Debug)]
struct ToolRunArgs {
    #[arg(value_name = "NAME")]
    name: String,
    #[arg(
        long,
        value_name = "SCRIPT",
        help = "Invoke the given console script for this tool"
    )]
    console: Option<String>,
    #[arg(
        value_name = "ARG",
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    args: Vec<String>,
}

#[derive(Args, Debug)]
struct ToolRemoveArgs {
    #[arg(value_name = "NAME")]
    name: String,
}

#[derive(Args, Debug)]
struct ToolUpgradeArgs {
    #[arg(value_name = "NAME")]
    name: String,
    #[arg(
        long,
        value_name = "VERSION",
        help = "Switch the runtime version for this tool"
    )]
    python: Option<String>,
}

#[derive(Args, Debug)]
struct BuildArgs {
    #[command(flatten)]
    common: CommonFlags,
    #[arg(value_enum, default_value_t = BuildFormat::Both)]
    format: BuildFormat,
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(ValueEnum, Debug, Clone, Copy, Serialize, Deserialize)]
enum BuildFormat {
    Sdist,
    Wheel,
    Both,
}

#[derive(Args, Debug)]
struct PublishArgs {
    #[command(flatten)]
    common: CommonFlags,
    #[arg(long)]
    registry: Option<String>,
    #[arg(long = "token-env")]
    token_env: Option<String>,
}
