use std::{path::PathBuf, sync::Arc};

use atty::Stream;
use clap::{value_parser, ArgAction, Args, Parser, Subcommand, ValueEnum};
use color_eyre::{eyre::eyre, Result};
use px_core::{
    self, CachePathRequest, CachePruneRequest, CacheStatsRequest, CommandContext, CommandGroup,
    CommandInfo, CommandStatus, EnvMode as CoreEnvMode, EnvRequest, GlobalOptions, MigrateRequest,
    OutputBuildRequest, OutputPublishRequest, ProjectAddRequest, ProjectInitRequest,
    ProjectInstallRequest, ProjectRemoveRequest, ProjectUpdateRequest, QualityTidyRequest,
    StorePrefetchRequest, SystemEffects, ToolCommandRequest, WorkflowRunRequest,
    WorkflowTestRequest,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

mod style;
mod traceback;

use style::Style;

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
    "  fmt | lint       Run configured formatters/linters inside the px environment.\n",
    "  build            Produce sdists/wheels from the px-managed environment.\n",
    "  publish          Upload previously built artifacts (dry-run by default).\n",
    "  migrate          Create px metadata for an existing project.\n\n",
    "\x1b[1;36mAdvanced\x1b[0m\n",
    "  debug env        Show interpreter info or pythonpath.\n",
    "  debug cache      Inspect cached artifacts (path, stats, prune, prefetch).\n",
    "  debug tidy       Clean cached metadata and stray files.\n",
    "  debug why        Advanced dependency provenance (coming soon).\n",
);

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
        if let Some(note) = autosync_note_from_details(&outcome.details) {
            let line = format!("px {}: {}", info.name, note);
            println!("{}", style.info(&line));
        }
        let migrate_table = render_migrate_table(&style, info, &outcome.details);
        match outcome.status {
            CommandStatus::Ok => {
                if is_passthrough(&outcome.details) {
                    println!("{}", outcome.message);
                } else {
                    let message = px_core::format_status_message(info, &outcome.message);
                    println!("{}", style.status(&outcome.status, &message));
                    let mut hint_emitted = false;
                    if let Some(trace) = traceback_from_details(&style, &outcome.details) {
                        println!("{}", trace.body);
                        if let Some(line) = trace.hint_line {
                            println!("{}", line);
                            hint_emitted = true;
                        }
                    }
                    if !hint_emitted {
                        if let Some(hint) = hint_from_details(&outcome.details) {
                            let hint_line = format!("Tip: {hint}");
                            println!("{}", style.info(&hint_line));
                        }
                    }
                }
            }
            _ => {
                let header = format!("{}  {}", error_code(info), outcome.message);
                println!("{}", style.error_header(&header));
                println!();
                println!("Why:");
                for reason in collect_why_bullets(&outcome.details, &outcome.message) {
                    println!("  • {}", reason);
                }
                let fixes = collect_fix_bullets(&outcome.details);
                if !fixes.is_empty() {
                    println!();
                    println!("Fix:");
                    for fix in fixes {
                        println!("{}", style.fix_bullet(&format!("  • {fix}")));
                    }
                }
                if let Some(trace) = traceback_from_details(&style, &outcome.details) {
                    println!();
                    println!("{}", trace.body);
                    if let Some(line) = trace.hint_line {
                        println!("{}", line);
                    }
                }
            }
        }
        if let Some(table) = migrate_table {
            println!("{}", table);
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

fn traceback_from_details(style: &Style, details: &Value) -> Option<traceback::TracebackDisplay> {
    let map = details.as_object()?;
    let traceback_value = map.get("traceback")?;
    traceback::format_traceback(style, traceback_value)
}

fn autosync_note_from_details(details: &Value) -> Option<&str> {
    details
        .as_object()
        .and_then(|map| map.get("autosync"))
        .and_then(Value::as_object)
        .and_then(|map| map.get("note"))
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

fn error_code(info: CommandInfo) -> &'static str {
    match info.group {
        CommandGroup::Init => "PX101",
        CommandGroup::Add => "PX110",
        CommandGroup::Remove => "PX111",
        CommandGroup::Sync => "PX120",
        CommandGroup::Update => "PX130",
        CommandGroup::Status => "PX140",
        CommandGroup::Run => "PX201",
        CommandGroup::Test => "PX202",
        CommandGroup::Fmt => "PX301",
        CommandGroup::Lint => "PX302",
        CommandGroup::Tidy => "PX303",
        CommandGroup::Build => "PX401",
        CommandGroup::Publish => "PX402",
        CommandGroup::Migrate => "PX501",
        CommandGroup::Env => "PX601",
        CommandGroup::Cache => "PX602",
        CommandGroup::Lock => "PX610",
        CommandGroup::Workspace => "PX620",
        CommandGroup::Explain => "PX701",
        CommandGroup::Why => "PX702",
    }
}

fn collect_why_bullets(details: &Value, fallback: &str) -> Vec<String> {
    let mut bullets = Vec::new();
    if let Some(reason) = details.get("reason").and_then(Value::as_str) {
        push_unique(&mut bullets, reason);
    }
    if let Some(status) = details.get("status").and_then(Value::as_str) {
        push_unique(&mut bullets, format!("Status: {status}"));
    }
    if let Some(issues) = details.get("issues").and_then(Value::as_array) {
        if !issues.is_empty() {
            push_unique(
                &mut bullets,
                format!("Detected {} issue(s) in the environment", issues.len()),
            );
        }
    }
    if let Some(drift) = details.get("drift").and_then(Value::as_array) {
        if !drift.is_empty() {
            push_unique(
                &mut bullets,
                format!("Manifest drift detected ({} entries)", drift.len()),
            );
        }
    }
    if bullets.is_empty() {
        bullets.push(fallback.to_string());
    }
    bullets
}

fn collect_fix_bullets(details: &Value) -> Vec<String> {
    let mut fixes = Vec::new();
    if let Some(hint) = hint_from_details(details) {
        push_unique(&mut fixes, hint.to_string());
    }
    if let Some(rec) = details
        .as_object()
        .and_then(|map| map.get("recommendation"))
        .and_then(Value::as_object)
    {
        if let Some(command) = rec.get("command").and_then(Value::as_str) {
            push_unique(&mut fixes, format!("Run `{command}`"));
        }
        if let Some(hint) = rec.get("hint").and_then(Value::as_str) {
            push_unique(&mut fixes, hint.to_string());
        }
    }
    if fixes.is_empty() {
        fixes.push("Re-run with --help for usage or inspect the output above.".to_string());
    }
    fixes
}

fn push_unique(vec: &mut Vec<String>, text: impl Into<String>) {
    let entry = text.into();
    if entry.trim().is_empty() {
        return;
    }
    if !vec.iter().any(|existing| existing == &entry) {
        vec.push(entry);
    }
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
        CommandGroupCli::Sync(args) => {
            let info = CommandInfo::new(CommandGroup::Sync, "sync");
            let request = project_sync_request_from_args(args);
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
        CommandGroupCli::Status => {
            let info = CommandInfo::new(CommandGroup::Status, "status");
            core_call(info, px_core::project_status(ctx))
        }
        CommandGroupCli::Debug(cmd) => match cmd {
            DebugCommand::Env(args) => handle_env_command(ctx, args),
            DebugCommand::Cache(args) => handle_cache_command(ctx, &args.command),
            DebugCommand::Tidy(args) => {
                let info = CommandInfo::new(CommandGroup::Tidy, "tidy");
                let request = quality_tidy_request_from_args(args);
                core_call(info, px_core::quality_tidy(ctx, request))
            }
            DebugCommand::Why(_args) => upcoming_command(
                CommandInfo::new(CommandGroup::Why, "why"),
                "dependency provenance is not available yet",
                "Inspect px.lock manually until the `px debug why` flow lands.",
            ),
            DebugCommand::Explain(_args) => upcoming_command(
                CommandInfo::new(CommandGroup::Explain, "explain"),
                "issue explanations are not available yet",
                "Capture the issue id and check docs/design.md for roadmap updates.",
            ),
        },
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
            ProjectCommand::Sync(args) => {
                let info = CommandInfo::new(CommandGroup::Sync, "sync");
                let request = project_sync_request_from_args(args);
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
        frozen: args.frozen,
    }
}

fn workflow_run_request_from_args(args: &RunArgs) -> WorkflowRunRequest {
    let (entry, forwarded_args) = normalize_run_invocation(args);
    WorkflowRunRequest {
        entry,
        target: args.target.clone(),
        args: forwarded_args,
        frozen: args.frozen,
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
        frozen: args.frozen,
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

fn project_sync_request_from_args(args: &SyncArgs) -> ProjectInstallRequest {
    ProjectInstallRequest {
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
    disable_help_subcommand = true,
    before_help = PX_BEFORE_HELP,
    help_template = PX_HELP_TEMPLATE
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
    Fmt(ToolArgs),
    #[command(
        about = "Run configured linters inside the px environment.",
        override_usage = "px lint [-- <ARG>...]"
    )]
    Lint(ToolArgs),
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
        about = "Advanced utilities (env, cache, fmt, lint, tidy, why).",
        subcommand
    )]
    Debug(DebugCommand),
    #[command(subcommand, hide = true)]
    Project(ProjectCommand),
    #[command(subcommand, hide = true)]
    Workflow(WorkflowCommand),
    #[command(subcommand, hide = true)]
    Quality(QualityCommand),
    #[command(subcommand, hide = true)]
    Output(OutputCommand),
    #[command(name = "onboard", hide = true)]
    Onboard(MigrateArgs),
}

#[derive(Subcommand, Debug)]
enum ProjectCommand {
    #[command(
        about = "Scaffold pyproject, src/, and tests using the current folder.",
        override_usage = "px project init [--package NAME] [--py VERSION]"
    )]
    Init(InitArgs),
    #[command(
        about = "Add or update pinned dependencies in pyproject.toml.",
        override_usage = "px project add <SPEC> [SPEC ...]"
    )]
    Add(SpecArgs),
    #[command(
        about = "Remove dependencies by name across prod and dev scopes.",
        override_usage = "px project remove <NAME> [NAME ...]"
    )]
    Remove(SpecArgs),
    Sync(SyncArgs),
    #[command(
        about = "Update named dependencies to the newest allowed versions.",
        override_usage = "px project update <SPEC> [SPEC ...]"
    )]
    Update(SpecArgs),
}

#[derive(Subcommand, Debug)]
enum WorkflowCommand {
    #[command(
        about = "Run the inferred entry or a named module inside px.",
        override_usage = "px workflow run [ENTRY] [-- <ARG>...]"
    )]
    Run(RunArgs),
    #[command(
        about = "Run pytest (or px's fallback) with cached dependencies.",
        override_usage = "px workflow test [-- <PYTEST_ARG>...]"
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
        about = "Build sdists and wheels into the project dist/ folder.",
        override_usage = "px output build [sdist|wheel|both] [--out DIR]"
    )]
    Build(BuildArgs),
    #[command(
        about = "Publish dist/ artifacts (dry-run by default).",
        override_usage = "px output publish [--dry-run] [--registry NAME] [--token-env VAR]"
    )]
    Publish(PublishArgs),
}

#[derive(Subcommand, Debug)]
enum DebugCommand {
    #[command(
        about = "Show interpreter info or pythonpath.",
        override_usage = "px debug env [python|info|paths]"
    )]
    Env(EnvArgs),
    #[command(
        about = "Inspect or manage cached artifacts (path, stats, prune, prefetch).",
        override_usage = "px debug cache <SUBCOMMAND>"
    )]
    Cache(CacheArgs),
    #[command(about = "Clean cached metadata and stray files.")]
    Tidy(TidyArgs),
    #[command(about = "Explain why a dependency is present (advanced).")]
    Why(WhyArgs),
    #[command(name = "explain", hide = true)]
    Explain(ExplainArgs),
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
struct ToolArgs {
    #[command(flatten)]
    common: CommonFlags,
    #[arg(
        long,
        help = "Fail if px.lock is missing or the environment is out of sync"
    )]
    frozen: bool,
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
    #[command(about = "Print the resolved px cache directory.")]
    Path,
    #[command(about = "Report cache entry counts and total bytes.")]
    Stats,
    #[command(about = "Prune cache files (pair with --dry-run to preview).")]
    Prune(PruneArgs),
    #[command(about = "Prefetch and cache artifacts for offline use.")]
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
