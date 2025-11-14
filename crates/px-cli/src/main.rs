use std::path::PathBuf;

use atty::Stream;
use clap::{value_parser, ArgAction, Args, Parser, Subcommand, ValueEnum};
use color_eyre::owo_colors::OwoColorize;
use color_eyre::{eyre::eyre, Result};
use px_core::{self, CommandGroup, CommandStatus, GlobalOptions, PxCommand};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

fn main() -> Result<()> {
    color_eyre::install()?;

    let cli = PxCli::parse();
    init_tracing(cli.trace, cli.verbose);

    let global = GlobalOptions {
        quiet: cli.quiet,
        verbose: cli.verbose,
        trace: cli.trace,
        json: cli.json,
        config: cli.config.as_ref().map(|p| p.to_string_lossy().to_string()),
    };

    let command = build_command(&cli.command);
    let outcome = px_core::execute(&global, &command).map_err(|err| eyre!("{err:?}"))?;
    let code = emit_output(&cli, &command, &outcome)?;

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

    let filter = format!("px={level},px_cli={level}");
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_level(true)
        .finish();

    let _ = tracing::subscriber::set_global_default(subscriber);
}

fn emit_output(
    cli: &PxCli,
    command: &PxCommand,
    outcome: &px_core::ExecutionOutcome,
) -> Result<i32> {
    let code = match outcome.status {
        CommandStatus::Ok => 0,
        CommandStatus::UserError => 1,
        CommandStatus::Failure => 2,
    };

    if cli.json {
        let payload = px_core::to_json_response(command, outcome, code);
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else if !cli.quiet {
        if is_passthrough(&outcome.details) {
            println!("{}", outcome.message);
        } else {
            let mut message = px_core::format_status_message(command, &outcome.message);
            if let Some(hint) = hint_from_details(&outcome.details) {
                message.push_str("\nHint: ");
                message.push_str(hint);
            }
            let use_color = atty::is(Stream::Stdout);
            let styled = colorize_message(outcome.status.clone(), &message, use_color);
            println!("{}", styled);
        }
    }

    Ok(code)
}

fn hint_from_details(details: &Value) -> Option<&str> {
    details
        .as_object()
        .and_then(|map| map.get("hint"))
        .and_then(Value::as_str)
}

fn colorize_message(status: CommandStatus, message: &str, enabled: bool) -> String {
    if !enabled {
        return message.to_string();
    }
    match status {
        CommandStatus::Ok => message.green().to_string(),
        CommandStatus::UserError => message.yellow().to_string(),
        CommandStatus::Failure => message.red().to_string(),
    }
}

fn is_passthrough(details: &Value) -> bool {
    details
        .as_object()
        .and_then(|map| map.get("passthrough"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn build_command(group: &CommandGroupCli) -> PxCommand {
    match group {
        CommandGroupCli::Project(cmd) => build_project(cmd),
        CommandGroupCli::Workflow(cmd) => build_workflow(cmd),
        CommandGroupCli::Quality(cmd) => build_quality(cmd),
        CommandGroupCli::Output(cmd) => build_output(cmd),
        CommandGroupCli::Infra(cmd) => build_infra(cmd),
        CommandGroupCli::Run(args) => build_run_command(args),
        CommandGroupCli::Test(args) => build_test_command(args),
        CommandGroupCli::Env(args) => build_env_command(args),
        CommandGroupCli::Cache(args) => build_cache_command(args),
        CommandGroupCli::Install(args) => build_install_command(args),
        CommandGroupCli::Tidy(args) => build_tidy_command(args),
        CommandGroupCli::Lock(cmd) => build_lock_command(cmd),
        CommandGroupCli::Workspace(cmd) => build_workspace_command(cmd),
        CommandGroupCli::Store(cmd) => build_store_command(cmd),
    }
}

fn build_project(cmd: &ProjectCommand) -> PxCommand {
    match cmd {
        ProjectCommand::Init(args) => PxCommand::new(
            CommandGroup::Project,
            "init",
            Vec::new(),
            json!({
                "package": args.package,
                "python": args.py,
            }),
            args.common.dry_run,
            args.common.force,
        ),
        ProjectCommand::Add(args) => to_spec_command(CommandGroup::Project, "add", args),
        ProjectCommand::Remove(args) => to_spec_command(CommandGroup::Project, "remove", args),
        ProjectCommand::Install(args) => build_install_command(args),
        ProjectCommand::Update(args) => to_spec_command(CommandGroup::Project, "update", args),
    }
}

fn build_workflow(cmd: &WorkflowCommand) -> PxCommand {
    match cmd {
        WorkflowCommand::Run(args) => build_run_command(args),
        WorkflowCommand::Test(args) => build_test_command(args),
    }
}

fn build_quality(cmd: &QualityCommand) -> PxCommand {
    match cmd {
        QualityCommand::Fmt(args) => to_tool_command(CommandGroup::Quality, "fmt", args),
        QualityCommand::Lint(args) => to_tool_command(CommandGroup::Quality, "lint", args),
        QualityCommand::Tidy(args) => build_tidy_command(args),
    }
}

fn build_output(cmd: &OutputCommand) -> PxCommand {
    match cmd {
        OutputCommand::Build(args) => PxCommand::new(
            CommandGroup::Output,
            "build",
            Vec::new(),
            json!({ "format": args.format, "out": args.out.clone() }),
            args.common.dry_run,
            args.common.force,
        ),
        OutputCommand::Publish(args) => PxCommand::new(
            CommandGroup::Output,
            "publish",
            Vec::new(),
            json!({
                "registry": args.registry.clone(),
                "token_env": args.token_env.clone(),
            }),
            args.common.dry_run,
            args.common.force,
        ),
    }
}

fn build_infra(cmd: &InfraCommand) -> PxCommand {
    match cmd {
        InfraCommand::Cache(args) => build_cache_command(args),
        InfraCommand::Env(args) => build_env_command(args),
    }
}

fn build_env_command(args: &EnvArgs) -> PxCommand {
    PxCommand::new(
        CommandGroup::Infra,
        "env",
        Vec::new(),
        json!({ "mode": args.mode.as_str() }),
        false,
        false,
    )
}

fn build_cache_command(args: &CacheArgs) -> PxCommand {
    match &args.command {
        CacheSubcommand::Path => PxCommand::new(
            CommandGroup::Infra,
            "cache",
            Vec::new(),
            json!({ "mode": "path" }),
            false,
            false,
        ),
        CacheSubcommand::Stats => PxCommand::new(
            CommandGroup::Infra,
            "cache",
            Vec::new(),
            json!({ "mode": "stats" }),
            false,
            false,
        ),
        CacheSubcommand::Prune(args) => PxCommand::new(
            CommandGroup::Infra,
            "cache",
            Vec::new(),
            json!({
                "mode": "prune",
                "all": args.all,
                "dry_run": args.dry_run,
            }),
            false,
            false,
        ),
    }
}

fn build_store_command(cmd: &StoreCommand) -> PxCommand {
    match cmd {
        StoreCommand::Prefetch(args) => PxCommand::new(
            CommandGroup::Store,
            "prefetch",
            Vec::new(),
            json!({ "workspace": args.workspace }),
            args.common.dry_run,
            args.common.force,
        ),
    }
}

fn build_install_command(args: &InstallArgs) -> PxCommand {
    PxCommand::new(
        CommandGroup::Project,
        "install",
        Vec::new(),
        json!({ "frozen": args.frozen }),
        args.common.dry_run,
        args.common.force,
    )
}

fn build_tidy_command(args: &TidyArgs) -> PxCommand {
    PxCommand::new(
        CommandGroup::Quality,
        "tidy",
        Vec::new(),
        json!({}),
        args.common.dry_run,
        args.common.force,
    )
}

fn build_lock_command(cmd: &LockCommand) -> PxCommand {
    match cmd {
        LockCommand::Diff => PxCommand::new(
            CommandGroup::Lock,
            "diff",
            Vec::new(),
            json!({}),
            false,
            false,
        ),
        LockCommand::Upgrade => PxCommand::new(
            CommandGroup::Lock,
            "upgrade",
            Vec::new(),
            json!({}),
            false,
            false,
        ),
    }
}

fn build_workspace_command(cmd: &WorkspaceCommand) -> PxCommand {
    match cmd {
        WorkspaceCommand::List => PxCommand::new(
            CommandGroup::Workspace,
            "list",
            Vec::new(),
            json!({}),
            false,
            false,
        ),
        WorkspaceCommand::Verify => PxCommand::new(
            CommandGroup::Workspace,
            "verify",
            Vec::new(),
            json!({}),
            false,
            false,
        ),
        WorkspaceCommand::Install(args) => PxCommand::new(
            CommandGroup::Workspace,
            "install",
            Vec::new(),
            json!({ "frozen": args.frozen }),
            false,
            false,
        ),
        WorkspaceCommand::Tidy => PxCommand::new(
            CommandGroup::Workspace,
            "tidy",
            Vec::new(),
            json!({}),
            false,
            false,
        ),
    }
}

fn build_run_command(args: &RunArgs) -> PxCommand {
    PxCommand::new(
        CommandGroup::Workflow,
        "run",
        Vec::new(),
        json!({
            "entry": args.entry.clone(),
            "target": args.target.clone(),
            "args": args.args.clone(),
        }),
        args.common.dry_run,
        args.common.force,
    )
}

fn build_test_command(args: &TestArgs) -> PxCommand {
    PxCommand::new(
        CommandGroup::Workflow,
        "test",
        Vec::new(),
        json!({ "pytest_args": args.args.clone() }),
        args.common.dry_run,
        args.common.force,
    )
}

fn to_spec_command(group: CommandGroup, name: &str, args: &SpecArgs) -> PxCommand {
    PxCommand::new(
        group,
        name,
        args.specs.clone(),
        json!({}),
        args.common.dry_run,
        args.common.force,
    )
}

fn to_tool_command(group: CommandGroup, name: &str, args: &ToolArgs) -> PxCommand {
    PxCommand::new(
        group,
        name,
        Vec::new(),
        json!({ "args": args.args.clone() }),
        args.common.dry_run,
        args.common.force,
    )
}

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "Opinionated Python toolchain (Phase A)",
    long_about = "Pinned installs, workspace utilities, and cache helpers for px.",
    after_help = "Examples:\n  px cache path\n  px --json lock diff\n  px store prefetch --workspace --json"
)]
struct PxCli {
    #[arg(
        short,
        long,
        help = "Suppress human output (errors still print to stderr)"
    )]
    quiet: bool,
    #[arg(short, long, action = ArgAction::Count, help = "Increase logging (-vv reaches trace)")]
    verbose: u8,
    #[arg(long, help = "Force trace logging regardless of -v/-q")]
    trace: bool,
    #[arg(long, help = "Emit {status,message,details} JSON envelopes")]
    json: bool,
    #[arg(long, value_parser = value_parser!(PathBuf), help = "Optional px config file path")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: CommandGroupCli,
}

#[derive(Subcommand, Debug)]
enum CommandGroupCli {
    #[command(subcommand)]
    Project(ProjectCommand),
    #[command(subcommand)]
    Workflow(WorkflowCommand),
    #[command(subcommand)]
    Quality(QualityCommand),
    #[command(subcommand)]
    Output(OutputCommand),
    #[command(subcommand)]
    Infra(InfraCommand),
    #[command(about = "Run a module or entry point inside px")]
    Run(RunArgs),
    #[command(about = "Run pytest with px-managed tooling")]
    Test(TestArgs),
    #[command(about = "Show interpreter paths and env metadata")]
    Env(EnvArgs),
    #[command(about = "Inspect or prune the shared px cache")]
    Cache(CacheArgs),
    #[command(about = "Install pinned dependencies for the current project")]
    Install(InstallArgs),
    #[command(about = "Clean lock drift and format metadata")]
    Tidy(TidyArgs),
    #[command(subcommand)]
    Lock(LockCommand),
    #[command(subcommand)]
    Workspace(WorkspaceCommand),
    #[command(subcommand)]
    Store(StoreCommand),
}

#[derive(Subcommand, Debug)]
enum ProjectCommand {
    Init(InitArgs),
    Add(SpecArgs),
    Remove(SpecArgs),
    Install(InstallArgs),
    Update(SpecArgs),
}

#[derive(Subcommand, Debug)]
enum WorkflowCommand {
    #[command(about = "Run a module or entry point inside px")]
    Run(RunArgs),
    #[command(about = "Run pytest with px-managed tooling")]
    Test(TestArgs),
}

#[derive(Subcommand, Debug)]
enum QualityCommand {
    Fmt(ToolArgs),
    Lint(ToolArgs),
    Tidy(TidyArgs),
}

#[derive(Subcommand, Debug)]
enum OutputCommand {
    Build(BuildArgs),
    Publish(PublishArgs),
}

#[derive(Subcommand, Debug)]
enum InfraCommand {
    Cache(CacheArgs),
    Env(EnvArgs),
}

#[derive(Subcommand, Debug)]
enum StoreCommand {
    Prefetch(StorePrefetchArgs),
}

#[derive(Args, Debug)]
struct StorePrefetchArgs {
    #[command(flatten)]
    common: CommonFlags,
    #[arg(long)]
    workspace: bool,
}

#[derive(Subcommand, Debug)]
enum LockCommand {
    Diff,
    Upgrade,
}

#[derive(Subcommand, Debug)]
enum WorkspaceCommand {
    List,
    Verify,
    Install(WorkspaceInstallArgs),
    Tidy,
}

#[derive(Args, Debug)]
struct WorkspaceInstallArgs {
    #[arg(long)]
    frozen: bool,
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
    #[arg(long)]
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
struct InstallArgs {
    #[command(flatten)]
    common: CommonFlags,
    #[arg(long)]
    frozen: bool,
}

#[derive(Args, Debug)]
struct RunArgs {
    #[command(flatten)]
    common: CommonFlags,
    #[arg(value_name = "ENTRY")]
    entry: Option<String>,
    #[arg(long)]
    target: Option<String>,
    #[arg(last = true, value_name = "ARG")]
    args: Vec<String>,
}

#[derive(Args, Debug)]
struct TestArgs {
    #[command(flatten)]
    common: CommonFlags,
    #[arg(last = true, value_name = "PYTEST_ARG")]
    args: Vec<String>,
}

#[derive(Args, Debug)]
struct ToolArgs {
    #[command(flatten)]
    common: CommonFlags,
    #[arg(last = true, value_name = "ARG")]
    args: Vec<String>,
}

#[derive(Args, Debug)]
struct TidyArgs {
    #[command(flatten)]
    common: CommonFlags,
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

#[derive(Args, Debug)]
struct CacheArgs {
    #[command(subcommand)]
    command: CacheSubcommand,
}

#[derive(Subcommand, Debug)]
enum CacheSubcommand {
    Path,
    Stats,
    Prune(PruneArgs),
}

#[derive(Args, Debug)]
struct PruneArgs {
    #[arg(long, help = "Confirm pruning the entire cache directory")]
    all: bool,
    #[arg(long, help = "Show what would be removed without deleting files")]
    dry_run: bool,
}

#[derive(Args, Debug)]
struct EnvArgs {
    #[arg(value_enum, default_value_t = EnvMode::Info)]
    mode: EnvMode,
}

#[derive(ValueEnum, Debug, Clone, Serialize, Deserialize)]
enum EnvMode {
    Info,
    Paths,
    Python,
}

impl EnvMode {
    fn as_str(&self) -> &'static str {
        match self {
            EnvMode::Info => "info",
            EnvMode::Paths => "paths",
            EnvMode::Python => "python",
        }
    }
}
