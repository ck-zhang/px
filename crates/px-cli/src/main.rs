use std::{path::PathBuf, sync::Arc};

use atty::Stream;
use clap::{value_parser, ArgAction, Args, Parser, Subcommand, ValueEnum};
use color_eyre::{eyre::eyre, Result};
use px_core::{
    self, CachePathRequest, CachePruneRequest, CacheStatsRequest, CommandContext, CommandGroup,
    CommandInfo, CommandStatus, EnvMode as CoreEnvMode, EnvRequest, GlobalOptions, LockDiffRequest,
    LockUpgradeRequest, MigrateRequest, OutputBuildRequest, OutputPublishRequest,
    ProjectAddRequest, ProjectInitRequest, ProjectInstallRequest, ProjectRemoveRequest,
    ProjectUpdateRequest, QualityTidyRequest, StorePrefetchRequest, SystemEffects,
    ToolCommandRequest, WorkflowRunRequest, WorkflowTestRequest, WorkspaceInstallRequest,
    WorkspaceListRequest, WorkspaceTidyRequest, WorkspaceVerifyRequest,
};
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

    let ctx = CommandContext::new(&global, Arc::new(SystemEffects::new()))
        .map_err(|err| eyre!("{err:?}"))?;
    let (info, outcome) = dispatch_command(&ctx, &cli.command)?;
    let code = emit_output(&cli, info, &outcome)?;

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

fn emit_output(cli: &PxCli, info: CommandInfo, outcome: &px_core::ExecutionOutcome) -> Result<i32> {
    let code = match outcome.status {
        CommandStatus::Ok => 0,
        CommandStatus::UserError => 1,
        CommandStatus::Failure => 2,
    };

    let style = Style::new(cli.no_color, atty::is(Stream::Stdout));

    if cli.json {
        let payload = px_core::to_json_response(info, outcome, code);
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else if !cli.quiet {
        if is_passthrough(&outcome.details) {
            println!("{}", outcome.message);
        } else {
            let message = px_core::format_status_message(info, &outcome.message);
            println!("{}", style.status(&outcome.status, &message));
            if let Some(hint) = hint_from_details(&outcome.details) {
                let hint_label = match outcome.status {
                    CommandStatus::Ok => "Tip",
                    CommandStatus::UserError => "Hint",
                    CommandStatus::Failure => "Note",
                };
                let hint_line = format!("{hint_label}: {hint}");
                println!("{}", style.info(&hint_line));
            }
            if let Some(table) = render_migrate_table(&style, info, &outcome.details) {
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

fn render_migrate_table(style: &Style, info: CommandInfo, details: &Value) -> Option<String> {
    if info.group != CommandGroup::Migrate {
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

fn core_call(
    info: CommandInfo,
    outcome: anyhow::Result<px_core::ExecutionOutcome>,
) -> Result<(CommandInfo, px_core::ExecutionOutcome)> {
    let result = outcome.map_err(|err| eyre!("{err:?}"))?;
    Ok((info, result))
}

fn upcoming_command(
    info: CommandInfo,
    summary: &str,
    hint: &str,
) -> Result<(CommandInfo, px_core::ExecutionOutcome)> {
    Ok((
        info,
        px_core::ExecutionOutcome::user_error(summary.to_string(), json!({ "hint": hint })),
    ))
}

fn handle_env_command(
    ctx: &CommandContext,
    args: &EnvArgs,
) -> Result<(CommandInfo, px_core::ExecutionOutcome)> {
    let info = CommandInfo::new(CommandGroup::Env, args.mode.label());
    let request = env_request_from_args(args);
    core_call(info, px_core::env(ctx, request))
}

fn handle_cache_command(
    ctx: &CommandContext,
    subcommand: &CacheSubcommand,
) -> Result<(CommandInfo, px_core::ExecutionOutcome)> {
    match subcommand {
        CacheSubcommand::Path => {
            let info = CommandInfo::new(CommandGroup::Cache, "path");
            core_call(info, px_core::cache_path(ctx, CachePathRequest))
        }
        CacheSubcommand::Stats => {
            let info = CommandInfo::new(CommandGroup::Cache, "stats");
            core_call(info, px_core::cache_stats(ctx, CacheStatsRequest))
        }
        CacheSubcommand::Prune(prune_args) => {
            let info = CommandInfo::new(CommandGroup::Cache, "prune");
            let request = CachePruneRequest {
                all: prune_args.all,
                dry_run: prune_args.dry_run,
            };
            core_call(info, px_core::cache_prune(ctx, request))
        }
        CacheSubcommand::Prefetch(prefetch_args) => {
            let info = CommandInfo::new(CommandGroup::Cache, "prefetch");
            let request = store_prefetch_request_from_args(prefetch_args);
            core_call(info, px_core::store_prefetch(ctx, request))
        }
    }
}

fn dispatch_command(
    ctx: &CommandContext,
    group: &CommandGroupCli,
) -> Result<(CommandInfo, px_core::ExecutionOutcome)> {
    match group {
        CommandGroupCli::Init(args) => {
            let info = CommandInfo::new(CommandGroup::Init, "init");
            let request = project_init_request_from_args(args);
            core_call(info, px_core::project_init(ctx, request))
        }
        CommandGroupCli::Add(args) => {
            let info = CommandInfo::new(CommandGroup::Add, "add");
            let request = ProjectAddRequest {
                specs: args.specs.clone(),
            };
            core_call(info, px_core::project_add(ctx, request))
        }
        CommandGroupCli::Remove(args) => {
            let info = CommandInfo::new(CommandGroup::Remove, "remove");
            let request = ProjectRemoveRequest {
                specs: args.specs.clone(),
            };
            core_call(info, px_core::project_remove(ctx, request))
        }
        CommandGroupCli::Install(args) => {
            let info = CommandInfo::new(CommandGroup::Install, "install");
            let request = project_install_request_from_args(args);
            core_call(info, px_core::project_install(ctx, request))
        }
        CommandGroupCli::Update(args) => {
            let info = CommandInfo::new(CommandGroup::Update, "update");
            let request = ProjectUpdateRequest {
                specs: args.specs.clone(),
            };
            core_call(info, px_core::project_update(ctx, request))
        }
        CommandGroupCli::Run(args) => {
            let info = CommandInfo::new(CommandGroup::Run, "run");
            let request = workflow_run_request_from_args(args);
            core_call(info, px_core::workflow_run(ctx, request))
        }
        CommandGroupCli::Test(args) => {
            let info = CommandInfo::new(CommandGroup::Test, "test");
            let request = workflow_test_request_from_args(args);
            core_call(info, px_core::workflow_test(ctx, request))
        }
        CommandGroupCli::Fmt(args) => {
            let info = CommandInfo::new(CommandGroup::Fmt, "fmt");
            let request = tool_command_request_from_args(args);
            core_call(info, px_core::quality_fmt(ctx, request))
        }
        CommandGroupCli::Lint(args) => {
            let info = CommandInfo::new(CommandGroup::Lint, "lint");
            let request = tool_command_request_from_args(args);
            core_call(info, px_core::quality_lint(ctx, request))
        }
        CommandGroupCli::Tidy(args) => {
            let info = CommandInfo::new(CommandGroup::Tidy, "tidy");
            let request = quality_tidy_request_from_args(args);
            core_call(info, px_core::quality_tidy(ctx, request))
        }
        CommandGroupCli::Build(args) => {
            let info = CommandInfo::new(CommandGroup::Build, "build");
            let request = output_build_request_from_args(args);
            core_call(info, px_core::output_build(ctx, request))
        }
        CommandGroupCli::Publish(args) => {
            let info = CommandInfo::new(CommandGroup::Publish, "publish");
            let request = output_publish_request_from_args(args);
            core_call(info, px_core::output_publish(ctx, request))
        }
        CommandGroupCli::Migrate(args) => {
            let info = CommandInfo::new(CommandGroup::Migrate, "migrate");
            let request = migrate_request_from_args(args);
            core_call(info, px_core::migrate(ctx, request))
        }
        CommandGroupCli::Status => upcoming_command(
            CommandInfo::new(CommandGroup::Status, "status"),
            "status reporting is not available yet",
            "Use `px env info` and `px lock diff` until this lands.",
        ),
        CommandGroupCli::Env(args) => handle_env_command(ctx, args),
        CommandGroupCli::Cache(args) => handle_cache_command(ctx, &args.command),
        CommandGroupCli::Lock(cmd) => match cmd {
            LockCommand::Diff => {
                let info = CommandInfo::new(CommandGroup::Lock, "diff");
                core_call(info, px_core::lock_diff(ctx, LockDiffRequest))
            }
            LockCommand::Upgrade => {
                let info = CommandInfo::new(CommandGroup::Lock, "upgrade");
                core_call(info, px_core::lock_upgrade(ctx, LockUpgradeRequest))
            }
        },
        CommandGroupCli::Workspace(cmd) => match cmd {
            WorkspaceCommand::List => {
                let info = CommandInfo::new(CommandGroup::Workspace, "list");
                core_call(info, px_core::workspace_list(ctx, WorkspaceListRequest))
            }
            WorkspaceCommand::Verify => {
                let info = CommandInfo::new(CommandGroup::Workspace, "verify");
                core_call(info, px_core::workspace_verify(ctx, WorkspaceVerifyRequest))
            }
            WorkspaceCommand::Install(args) => {
                let info = CommandInfo::new(CommandGroup::Workspace, "install");
                let request = workspace_install_request_from_args(args);
                core_call(info, px_core::workspace_install(ctx, request))
            }
            WorkspaceCommand::Tidy => {
                let info = CommandInfo::new(CommandGroup::Workspace, "tidy");
                core_call(info, px_core::workspace_tidy(ctx, WorkspaceTidyRequest))
            }
        },
        CommandGroupCli::Explain(_args) => upcoming_command(
            CommandInfo::new(CommandGroup::Explain, "explain"),
            "issue explanations are not available yet",
            "Capture the issue id and check docs/design.md for roadmap updates.",
        ),
        CommandGroupCli::Why(_args) => upcoming_command(
            CommandInfo::new(CommandGroup::Why, "why"),
            "dependency provenance is not available yet",
            "Inspect px.lock manually until the `px why` flow lands.",
        ),
        CommandGroupCli::Project(cmd) => match cmd {
            ProjectCommand::Init(args) => {
                let info = CommandInfo::new(CommandGroup::Init, "init");
                let request = project_init_request_from_args(args);
                core_call(info, px_core::project_init(ctx, request))
            }
            ProjectCommand::Add(args) => {
                let info = CommandInfo::new(CommandGroup::Add, "add");
                let request = ProjectAddRequest {
                    specs: args.specs.clone(),
                };
                core_call(info, px_core::project_add(ctx, request))
            }
            ProjectCommand::Remove(args) => {
                let info = CommandInfo::new(CommandGroup::Remove, "remove");
                let request = ProjectRemoveRequest {
                    specs: args.specs.clone(),
                };
                core_call(info, px_core::project_remove(ctx, request))
            }
            ProjectCommand::Install(args) => {
                let info = CommandInfo::new(CommandGroup::Install, "install");
                let request = project_install_request_from_args(args);
                core_call(info, px_core::project_install(ctx, request))
            }
            ProjectCommand::Update(args) => {
                let info = CommandInfo::new(CommandGroup::Update, "update");
                let request = ProjectUpdateRequest {
                    specs: args.specs.clone(),
                };
                core_call(info, px_core::project_update(ctx, request))
            }
        },
        CommandGroupCli::Workflow(cmd) => match cmd {
            WorkflowCommand::Run(args) => {
                let info = CommandInfo::new(CommandGroup::Run, "run");
                let request = workflow_run_request_from_args(args);
                core_call(info, px_core::workflow_run(ctx, request))
            }
            WorkflowCommand::Test(args) => {
                let info = CommandInfo::new(CommandGroup::Test, "test");
                let request = workflow_test_request_from_args(args);
                core_call(info, px_core::workflow_test(ctx, request))
            }
        },
        CommandGroupCli::Quality(cmd) => match cmd {
            QualityCommand::Fmt(args) => {
                let info = CommandInfo::new(CommandGroup::Fmt, "fmt");
                let request = tool_command_request_from_args(args);
                core_call(info, px_core::quality_fmt(ctx, request))
            }
            QualityCommand::Lint(args) => {
                let info = CommandInfo::new(CommandGroup::Lint, "lint");
                let request = tool_command_request_from_args(args);
                core_call(info, px_core::quality_lint(ctx, request))
            }
            QualityCommand::Tidy(args) => {
                let info = CommandInfo::new(CommandGroup::Tidy, "tidy");
                let request = quality_tidy_request_from_args(args);
                core_call(info, px_core::quality_tidy(ctx, request))
            }
        },
        CommandGroupCli::Output(cmd) => match cmd {
            OutputCommand::Build(args) => {
                let info = CommandInfo::new(CommandGroup::Build, "build");
                let request = output_build_request_from_args(args);
                core_call(info, px_core::output_build(ctx, request))
            }
            OutputCommand::Publish(args) => {
                let info = CommandInfo::new(CommandGroup::Publish, "publish");
                let request = output_publish_request_from_args(args);
                core_call(info, px_core::output_publish(ctx, request))
            }
        },
        CommandGroupCli::Infra(InfraCommand::Env(args)) => handle_env_command(ctx, args),
        CommandGroupCli::Infra(InfraCommand::Cache(args)) => {
            handle_cache_command(ctx, &args.command)
        }
        CommandGroupCli::Store(StoreCommand::Prefetch(args)) => {
            let info = CommandInfo::new(CommandGroup::Cache, "prefetch");
            let request = store_prefetch_request_from_args(args);
            core_call(info, px_core::store_prefetch(ctx, request))
        }
        CommandGroupCli::Onboard(args) => {
            eprintln!("`px onboard` is deprecated; use `px migrate`.");
            let info = CommandInfo::new(CommandGroup::Migrate, "migrate");
            let request = migrate_request_from_args(args);
            core_call(info, px_core::migrate(ctx, request))
        }
    }
}

fn workflow_test_request_from_args(args: &TestArgs) -> WorkflowTestRequest {
    WorkflowTestRequest {
        pytest_args: args.args.clone(),
    }
}

fn workflow_run_request_from_args(args: &RunArgs) -> WorkflowRunRequest {
    let (entry, forwarded_args) = normalize_run_invocation(args);
    WorkflowRunRequest {
        entry,
        target: args.target.clone(),
        args: forwarded_args,
    }
}

fn project_init_request_from_args(args: &InitArgs) -> ProjectInitRequest {
    ProjectInitRequest {
        package: args.package.clone(),
        python: args.py.clone(),
        dry_run: args.common.dry_run,
        force: args.common.force,
    }
}

fn tool_command_request_from_args(args: &ToolArgs) -> ToolCommandRequest {
    ToolCommandRequest {
        args: args.args.clone(),
    }
}

fn migrate_request_from_args(args: &MigrateArgs) -> MigrateRequest {
    MigrateRequest {
        source: args
            .source
            .as_ref()
            .map(|p| p.to_string_lossy().to_string()),
        dev_source: args
            .dev_source
            .as_ref()
            .map(|p| p.to_string_lossy().to_string()),
        write: args.write,
        allow_dirty: args.allow_dirty,
        lock_only: args.lock_only,
        no_autopin: args.no_autopin,
    }
}

fn output_build_request_from_args(args: &BuildArgs) -> OutputBuildRequest {
    let (include_sdist, include_wheel) = match args.format {
        BuildFormat::Sdist => (true, false),
        BuildFormat::Wheel => (false, true),
        BuildFormat::Both => (true, true),
    };
    OutputBuildRequest {
        include_sdist,
        include_wheel,
        out: args.out.clone(),
        dry_run: args.common.dry_run,
    }
}

fn output_publish_request_from_args(args: &PublishArgs) -> OutputPublishRequest {
    OutputPublishRequest {
        registry: args.registry.clone(),
        token_env: args.token_env.clone(),
        dry_run: args.common.dry_run,
    }
}

fn project_install_request_from_args(args: &InstallArgs) -> ProjectInstallRequest {
    ProjectInstallRequest {
        frozen: args.frozen,
    }
}

fn workspace_install_request_from_args(args: &WorkspaceInstallArgs) -> WorkspaceInstallRequest {
    WorkspaceInstallRequest {
        frozen: args.frozen,
    }
}

fn env_request_from_args(args: &EnvArgs) -> EnvRequest {
    let mode = match args.mode {
        EnvMode::Info => CoreEnvMode::Info,
        EnvMode::Paths => CoreEnvMode::Paths,
        EnvMode::Python => CoreEnvMode::Python,
    };
    EnvRequest { mode }
}

fn store_prefetch_request_from_args(args: &StorePrefetchArgs) -> StorePrefetchRequest {
    StorePrefetchRequest {
        workspace: args.workspace,
        dry_run: args.common.dry_run,
    }
}

fn quality_tidy_request_from_args(_args: &TidyArgs) -> QualityTidyRequest {
    QualityTidyRequest
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

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "Opinionated Python toolchain (Phase A)",
    long_about = "Pinned installs, workspace utilities, and cache helpers for px.",
    after_help = "Examples:\n  px add requests==2.32.3\n  px env info --json\n  px cache prefetch --workspace --dry-run"
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
    #[command(
        about = "Create a new px project and environment.",
        override_usage = "px init [--package NAME] [--py VERSION]",
        after_help = "Examples:\n  px init\n  px init --package demo_pkg --py 3.11\n"
    )]
    Init(InitArgs),
    #[command(
        about = "Add dependencies and update lock/env.",
        override_usage = "px add <SPEC> [SPEC ...]",
        after_help = "Examples:\n  px add requests==2.32.3\n  px add pandas==2.2.3\n"
    )]
    Add(SpecArgs),
    #[command(
        about = "Remove dependencies and update lock/env.",
        override_usage = "px remove <NAME> [NAME ...]",
        after_help = "Example:\n  px remove requests\n"
    )]
    Remove(SpecArgs),
    #[command(
        about = "Sync environment from px.lock.",
        override_usage = "px install [--frozen]",
        after_help = "Examples:\n  px install\n  px install --frozen\n"
    )]
    Install(InstallArgs),
    #[command(
        about = "Update dependencies and lockfile.",
        override_usage = "px update [<SPEC> ...]",
        after_help = "Examples:\n  px update\n  px update requests\n"
    )]
    Update(SpecArgs),
    #[command(
        about = "Run a named task or script.",
        override_usage = "px run [ENTRY] [-- <ARG>...]",
        after_help = "Examples:\n  px run\n  px run sample_px_app.cli -- -n Demo\n"
    )]
    Run(RunArgs),
    #[command(
        about = "Run tests in the px environment.",
        override_usage = "px test [-- <PYTEST_ARG>...]",
        after_help = "Examples:\n  px test\n  px test -- -k smoke\n"
    )]
    Test(TestArgs),
    #[command(about = "Format code with configured formatters.")]
    Fmt(ToolArgs),
    #[command(about = "Lint code with configured linters.")]
    Lint(ToolArgs),
    #[command(about = "Clean up project artifacts and metadata.")]
    Tidy(TidyArgs),
    #[command(
        about = "Build distributable artifacts.",
        override_usage = "px build [sdist|wheel|both] [--out DIR]",
        after_help = "Examples:\n  PX_SKIP_TESTS=1 px build\n  px build wheel --out dist\n"
    )]
    Build(BuildArgs),
    #[command(
        about = "Publish build artifacts (dry-run by default).",
        override_usage = "px publish [--dry-run] [--registry NAME] [--token-env VAR]",
        after_help = "Examples:\n  px publish --dry-run\n  PX_ONLINE=1 PX_PUBLISH_TOKEN=<token> px publish\n"
    )]
    Publish(PublishArgs),
    #[command(about = "Convert an existing project to deterministic px.")]
    Migrate(MigrateArgs),
    #[command(about = "Show project/env/lock status at a glance.")]
    Status,
    #[command(
        about = "Inspect and manage the runtime environment.",
        override_usage = "px env [python|info|paths]",
        after_help = "Examples:\n  px env python\n  px env info\n  px env paths\n"
    )]
    Env(EnvArgs),
    #[command(
        about = "Inspect and manage caches.",
        after_help = "Examples:\n  px cache path\n  px cache stats\n  px cache prefetch --workspace --dry-run\n"
    )]
    Cache(CacheArgs),
    #[command(about = "Inspect and manage the lockfile.", subcommand)]
    Lock(LockCommand),
    #[command(about = "Work with multi-project workspaces.", subcommand)]
    Workspace(WorkspaceCommand),
    #[command(about = "Show detailed explanation for a recorded issue/resolution.")]
    Explain(ExplainArgs),
    #[command(about = "Explain why a given package is present.")]
    Why(WhyArgs),
    #[command(subcommand, hide = true)]
    Project(ProjectCommand),
    #[command(subcommand, hide = true)]
    Workflow(WorkflowCommand),
    #[command(subcommand, hide = true)]
    Quality(QualityCommand),
    #[command(subcommand, hide = true)]
    Output(OutputCommand),
    #[command(subcommand, hide = true)]
    Infra(InfraCommand),
    #[command(subcommand, hide = true)]
    Store(StoreCommand),
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
        about = "Legacy alias for `px cache prefetch` (workspace optional).",
        after_help = "Examples:\n  px cache prefetch --dry-run\n  PX_ONLINE=1 px cache prefetch --workspace\n"
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
struct ExplainArgs {
    #[arg(value_name = "ISSUE-ID")]
    issue: String,
}

#[derive(Args, Debug)]
struct WhyArgs {
    #[arg(value_name = "PACKAGE")]
    package: String,
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
    #[command(
        about = "Prefetch and cache artifacts for offline use.",
        after_help = "Examples:\n  px cache prefetch --dry-run\n  PX_ONLINE=1 px cache prefetch --workspace\n"
    )]
    Prefetch(StorePrefetchArgs),
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
    fn label(&self) -> &'static str {
        match self {
            EnvMode::Info => "info",
            EnvMode::Paths => "paths",
            EnvMode::Python => "python",
        }
    }
}
