use std::path::PathBuf;

use atty::Stream;
use clap::{value_parser, ArgAction, Args, Parser, Subcommand, ValueEnum};
use color_eyre::{eyre::eyre, Result};
use px_core::{self, CommandGroup, CommandStatus, GlobalOptions, PxCommand};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

mod style;

use style::Style;

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

    let style = Style::new(cli.no_color, atty::is(Stream::Stdout));

    if cli.json {
        let payload = px_core::to_json_response(command, outcome, code);
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else if !cli.quiet {
        if is_passthrough(&outcome.details) {
            println!("{}", outcome.message);
        } else {
            let message = px_core::format_status_message(command, &outcome.message);
            println!("{}", style.status(&outcome.status, &message));
            if let Some(hint) = hint_from_details(&outcome.details) {
                let hint_line = format!("Hint: {}", hint);
                println!("{}", style.info(&hint_line));
            }
            if let Some(table) = render_migrate_table(&style, command, &outcome.details) {
                println!("{}", table);
            }
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

fn is_passthrough(details: &Value) -> bool {
    details
        .as_object()
        .and_then(|map| map.get("passthrough"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn render_migrate_table(style: &Style, command: &PxCommand, details: &Value) -> Option<String> {
    if command.group != CommandGroup::Migrate {
        return None;
    }
    let packages = details.get("packages")?.as_array()?;
    if packages.is_empty() {
        return None;
    }

    let mut rows = Vec::new();
    for pkg in packages {
        let obj = pkg.as_object()?;
        rows.push(PackageRow {
            name: obj.get("name")?.as_str()?.to_string(),
            source: obj.get("source")?.as_str()?.to_string(),
            requested: obj.get("requested")?.as_str()?.to_string(),
            scope: obj.get("scope")?.as_str()?.to_string(),
        });
    }

    Some(format_package_table(style, &rows))
}

struct PackageRow {
    name: String,
    source: String,
    requested: String,
    scope: String,
}

fn format_package_table(style: &Style, rows: &[PackageRow]) -> String {
    let headers = ["Package", "Source", "Requested", "Scope"];
    let mut widths = [
        headers[0].len(),
        headers[1].len(),
        headers[2].len(),
        headers[3].len(),
    ];

    for row in rows {
        widths[0] = widths[0].max(row.name.len());
        widths[1] = widths[1].max(row.source.len());
        widths[2] = widths[2].max(row.requested.len());
        widths[3] = widths[3].max(row.scope.len());
    }

    let header_line = format!(
        "{:<width0$}  {:<width1$}  {:<width2$}  {:<width3$}",
        headers[0],
        headers[1],
        headers[2],
        headers[3],
        width0 = widths[0],
        width1 = widths[1],
        width2 = widths[2],
        width3 = widths[3],
    );

    let mut lines = Vec::new();
    lines.push(style.table_header(&header_line));
    lines.push(format!(
        "{:-<width0$}  {:-<width1$}  {:-<width2$}  {:-<width3$}",
        "",
        "",
        "",
        "",
        width0 = widths[0],
        width1 = widths[1],
        width2 = widths[2],
        width3 = widths[3],
    ));

    for row in rows {
        lines.push(format!(
            "{:<width0$}  {:<width1$}  {:<width2$}  {:<width3$}",
            row.name,
            row.source,
            row.requested,
            row.scope,
            width0 = widths[0],
            width1 = widths[1],
            width2 = widths[2],
            width3 = widths[3],
        ));
    }

    lines.join("\n")
}

fn build_command(group: &CommandGroupCli) -> PxCommand {
    match group {
        CommandGroupCli::Project(cmd) => build_project(cmd),
        CommandGroupCli::Workflow(cmd) => build_workflow(cmd),
        CommandGroupCli::Quality(cmd) => build_quality(cmd),
        CommandGroupCli::Output(cmd) => build_output(cmd),
        CommandGroupCli::Build(args) => build_output_build_command(args),
        CommandGroupCli::Publish(args) => build_output_publish_command(args),
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
        CommandGroupCli::Migrate(args) => build_migrate_command(args),
        CommandGroupCli::Onboard(args) => build_onboard_alias_command(args),
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
        OutputCommand::Build(args) => build_output_build_command(args),
        OutputCommand::Publish(args) => build_output_publish_command(args),
    }
}

fn build_output_build_command(args: &BuildArgs) -> PxCommand {
    PxCommand::new(
        CommandGroup::Output,
        "build",
        Vec::new(),
        json!({ "format": args.format, "out": args.out.clone() }),
        args.common.dry_run,
        args.common.force,
    )
}

fn build_output_publish_command(args: &PublishArgs) -> PxCommand {
    PxCommand::new(
        CommandGroup::Output,
        "publish",
        Vec::new(),
        json!({
            "registry": args.registry.clone(),
            "token_env": args.token_env.clone(),
        }),
        args.common.dry_run,
        args.common.force,
    )
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

fn build_migrate_command(args: &MigrateArgs) -> PxCommand {
    let dry_run = if args.write { false } else { true };
    let non_interactive = args.yes || args.no_input;
    PxCommand::new(
        CommandGroup::Migrate,
        "migrate",
        Vec::new(),
        json!({
            "source": args.source.as_ref().map(|p| p.to_string_lossy().to_string()),
            "dev_source": args
                .dev_source
                .as_ref()
                .map(|p| p.to_string_lossy().to_string()),
            "write": args.write,
            "dry_run_flag": args.dry_run,
            "yes": args.yes,
            "no_input": args.no_input,
            "non_interactive": non_interactive,
            "allow_dirty": args.allow_dirty,
            "lock_only": args.lock_only,
            "no_autopin": args.no_autopin,
        }),
        dry_run,
        false,
    )
}

fn build_onboard_alias_command(args: &MigrateArgs) -> PxCommand {
    eprintln!("`px onboard` is deprecated; use `px migrate`.");
    build_migrate_command(args)
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
    let (entry, forwarded_args) = normalize_run_invocation(args);
    PxCommand::new(
        CommandGroup::Workflow,
        "run",
        Vec::new(),
        json!({
            "entry": entry,
            "target": args.target.clone(),
            "args": forwarded_args,
        }),
        args.common.dry_run,
        args.common.force,
    )
}

fn normalize_run_invocation(args: &RunArgs) -> (Option<String>, Vec<String>) {
    let mut forwarded = args.args.clone();
    match &args.entry {
        Some(value) if value.starts_with('-') => {
            forwarded.insert(0, value.clone());
            (None, forwarded)
        }
        _ => (args.entry.clone(), forwarded),
    }
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
    #[arg(long, help = "Disable colored human output")]
    no_color: bool,
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
    #[command(
        name = "build",
        about = "Build sdists and wheels into the project build/ folder.",
        override_usage = "px build [sdist|wheel|both] [--out DIR]",
        after_help = "Examples:\n  PX_SKIP_TESTS=1 px build\n  px build wheel --out dist\n"
    )]
    Build(BuildArgs),
    #[command(
        name = "publish",
        about = "Publish build artifacts (dry-run by default).",
        override_usage = "px publish [--dry-run] [--registry NAME] [--token-env VAR]",
        after_help = "Examples:\n  px publish --dry-run\n  PX_ONLINE=1 PX_PUBLISH_TOKEN=<token> px publish\n"
    )]
    Publish(PublishArgs),
    #[command(subcommand)]
    Infra(InfraCommand),
    #[command(
        about = "Run the inferred entry or a named module inside px.",
        override_usage = "px run [ENTRY] [-- <ARG>...]",
        after_help = "Examples:\n  px run\n  px run sample_px_app.cli -- -n Demo\n"
    )]
    Run(RunArgs),
    #[command(
        about = "Run pytest (or px's fallback) with cached dependencies.",
        override_usage = "px test [-- <PYTEST_ARG>...]",
        after_help = "Examples:\n  px test\n  PX_TEST_FALLBACK_STD=1 px test -- -k smoke\n"
    )]
    Test(TestArgs),
    #[command(
        about = "Show interpreter info, pythonpath entries, or the shim itself.",
        override_usage = "px env [python|info|paths]",
        after_help = "Examples:\n  px env python\n  px env info\n  px env paths\n"
    )]
    Env(EnvArgs),
    #[command(
        about = "Inspect the px cache path, stats, or prune contents.",
        after_help = "Examples:\n  px cache path\n  px cache stats\n  px cache prune --all --dry-run\n"
    )]
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
    #[command(about = "Migrate existing projects into px")]
    Migrate(MigrateArgs),
    #[command(name = "onboard", hide = true)]
    Onboard(MigrateArgs),
}

#[derive(Subcommand, Debug)]
enum ProjectCommand {
    #[command(
        about = "Scaffold pyproject, src/, and tests using the current folder.",
        override_usage = "px project init [--package NAME] [--py VERSION]",
        after_help = "Examples:\n  px project init\n  px project init --package demo_pkg --py 3.11\n"
    )]
    Init(InitArgs),
    #[command(
        about = "Add or update pinned dependencies in pyproject.toml.",
        override_usage = "px project add <SPEC> [SPEC ...]",
        after_help = "Examples:\n  px project add requests==2.32.3\n  px project add pandas==2.2.3\n"
    )]
    Add(SpecArgs),
    #[command(
        about = "Remove dependencies by name across prod and dev scopes.",
        override_usage = "px project remove <NAME> [NAME ...]",
        after_help = "Example:\n  px project remove requests\n"
    )]
    Remove(SpecArgs),
    Install(InstallArgs),
    #[command(
        about = "Update named dependencies to the newest allowed versions.",
        override_usage = "px project update <SPEC> [SPEC ...]",
        after_help = "Example:\n  px project update requests\n"
    )]
    Update(SpecArgs),
}

#[derive(Subcommand, Debug)]
enum WorkflowCommand {
    #[command(
        about = "Run the inferred entry or a named module inside px.",
        override_usage = "px workflow run [ENTRY] [-- <ARG>...]",
        after_help = "Examples:\n  px workflow run\n  px workflow run sample_px_app.cli -- -n Demo\n"
    )]
    Run(RunArgs),
    #[command(
        about = "Run pytest (or px's fallback) with cached dependencies.",
        override_usage = "px workflow test [-- <PYTEST_ARG>...]",
        after_help = "Examples:\n  px workflow test\n  PX_TEST_FALLBACK_STD=1 px workflow test -- -k smoke\n"
    )]
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
    #[command(
        about = "Build sdists and wheels into the project build/ folder.",
        override_usage = "px output build [sdist|wheel|both] [--out DIR]",
        after_help = "Examples:\n  PX_SKIP_TESTS=1 px output build\n  px output build wheel --out dist\n"
    )]
    Build(BuildArgs),
    #[command(
        about = "Publish build artifacts (dry-run by default).",
        override_usage = "px output publish [--dry-run] [--registry NAME] [--token-env VAR]",
        after_help = "Examples:\n  px output publish --dry-run\n  PX_ONLINE=1 PX_PUBLISH_TOKEN=<token> px output publish\n"
    )]
    Publish(PublishArgs),
}

#[derive(Subcommand, Debug)]
enum InfraCommand {
    Cache(CacheArgs),
    Env(EnvArgs),
}

#[derive(Subcommand, Debug)]
enum StoreCommand {
    #[command(
        about = "Hydrate lock artifacts into the cache (workspace optional).",
        after_help = "Examples:\n  px store prefetch --dry-run\n  PX_ONLINE=1 px store prefetch --workspace\n"
    )]
    Prefetch(StorePrefetchArgs),
}

#[derive(Args, Debug)]
struct MigrateArgs {
    #[arg(
        long,
        action = ArgAction::SetTrue,
        help = "Preview the onboarding plan without touching files (default)",
        conflicts_with = "write"
    )]
    dry_run: bool,
    #[arg(
        long,
        action = ArgAction::SetTrue,
        help = "Request write mode once it becomes available",
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

#[derive(Args, Debug)]
struct StorePrefetchArgs {
    #[command(flatten)]
    common: CommonFlags,
    #[arg(long)]
    workspace: bool,
}

#[derive(Subcommand, Debug)]
enum LockCommand {
    #[command(
        about = "Compare px.lock to pyproject dependencies without mutating files.",
        after_help = "Examples:\n  px lock diff\n  px --json lock diff\n"
    )]
    Diff,
    #[command(
        about = "Upgrade px.lock to the latest schema or dependency pins.",
        after_help = "Example:\n  px lock upgrade\n"
    )]
    Upgrade,
}

#[derive(Subcommand, Debug)]
enum WorkspaceCommand {
    #[command(
        about = "List workspace members from pyproject.toml.",
        after_help = "Example:\n  px workspace list\n"
    )]
    List,
    #[command(
        about = "Verify each workspace member for lock drift or missing files.",
        after_help = "Examples:\n  px workspace verify\n  px workspace verify --json\n"
    )]
    Verify,
    #[command(
        about = "Install dependencies for every workspace member.",
        after_help = "Examples:\n  px workspace install\n  px workspace install --frozen\n"
    )]
    Install(WorkspaceInstallArgs),
    #[command(
        about = "Clean drift and metadata across workspace members.",
        after_help = "Example:\n  px workspace tidy\n"
    )]
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
    #[arg(
        value_name = "ENTRY",
        help = "Module or script name (omit to use the inferred default)"
    )]
    entry: Option<String>,
    #[arg(long)]
    target: Option<String>,
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
    #[command(
        about = "Print the resolved px cache directory.",
        after_help = "Example:\n  px cache path\n"
    )]
    Path,
    #[command(
        about = "Report cache entry counts and total bytes.",
        after_help = "Example:\n  px cache stats\n"
    )]
    Stats,
    #[command(
        about = "Prune cache files (pair with --dry-run to preview).",
        after_help = "Examples:\n  px cache prune --all --dry-run\n  px cache prune --all\n"
    )]
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
    #[arg(
        value_enum,
        default_value_t = EnvMode::Info,
        help = "Output mode: info, paths, or python (defaults to info)"
    )]
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
