#![deny(clippy::all, warnings)]

use std::{path::PathBuf, sync::Arc};

use atty::Stream;
use clap::{value_parser, ArgAction, Args, Parser, Subcommand, ValueEnum};
use color_eyre::{eyre::eyre, Result};
use px_core::{
    self, diag_commands, AutopinPreference, BuildRequest, CommandContext, CommandGroup,
    CommandInfo, CommandStatus, FmtRequest, GlobalOptions, LockBehavior, MigrateRequest,
    MigrationMode, ProjectAddRequest, ProjectInitRequest, ProjectRemoveRequest, ProjectSyncRequest,
    ProjectUpdateRequest, ProjectWhyRequest, PublishRequest, RunRequest, SystemEffects,
    TestRequest, ToolInstallRequest, ToolListRequest, ToolRemoveRequest, ToolRunRequest,
    ToolUpgradeRequest, WorkspacePolicy,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
    let code = emit_output(&cli, subcommand_json, info, &outcome)?;

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
    subcommand_json: bool,
    info: CommandInfo,
    outcome: &px_core::ExecutionOutcome,
) -> Result<i32> {
    let code = match outcome.status {
        CommandStatus::Ok => 0,
        CommandStatus::UserError => 1,
        CommandStatus::Failure => 2,
    };

    let style = Style::new(cli.no_color, atty::is(Stream::Stdout));

    if cli.json || subcommand_json {
        let payload = px_core::to_json_response(info, outcome, code);
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else if !cli.quiet {
        if let Some(note) = autosync_note_from_details(&outcome.details) {
            let line = format!("px {}: {}", info.name, note);
            println!("{}", style.info(&line));
        }
        let migrate_table = render_migrate_table(&style, info, &outcome.details);
        if let CommandStatus::Ok = outcome.status {
            if is_passthrough(&outcome.details) {
                println!("{}", outcome.message);
            } else {
                let message = px_core::format_status_message(info, &outcome.message);
                println!("{}", style.status(&outcome.status, &message));
                let mut hint_emitted = false;
                if let Some(trace) = traceback_from_details(&style, &outcome.details) {
                    println!("{}", trace.body);
                    if let Some(line) = trace.hint_line {
                        println!("{line}");
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
        } else {
            let header = format!("{}  {}", error_code(info), outcome.message);
            println!("{}", style.error_header(&header));
            println!();
            println!("Why:");
            for reason in collect_why_bullets(&outcome.details, &outcome.message) {
                println!("  • {reason}");
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
                    println!("{line}");
                }
            }
        }
        if let Some(table) = migrate_table {
            println!("{table}");
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
        CommandGroup::Init => diag_commands::INIT,
        CommandGroup::Add => diag_commands::ADD,
        CommandGroup::Remove => diag_commands::REMOVE,
        CommandGroup::Sync => diag_commands::SYNC,
        CommandGroup::Update => diag_commands::UPDATE,
        CommandGroup::Status => diag_commands::STATUS,
        CommandGroup::Run => diag_commands::RUN,
        CommandGroup::Test => diag_commands::TEST,
        CommandGroup::Fmt => diag_commands::FMT,
        CommandGroup::Build => diag_commands::BUILD,
        CommandGroup::Publish => diag_commands::PUBLISH,
        CommandGroup::Migrate => diag_commands::MIGRATE,
        CommandGroup::Why => diag_commands::WHY,
        CommandGroup::Tool => diag_commands::TOOL,
        CommandGroup::Python => diag_commands::PYTHON,
    }
}

fn collect_why_bullets(details: &Value, fallback: &str) -> Vec<String> {
    let mut bullets = Vec::new();
    if let Some(reason) = details.get("reason").and_then(Value::as_str) {
        push_unique(
            &mut bullets,
            reason_display(reason).unwrap_or(reason).to_string(),
        );
    }
    if let Some(status) = details.get("status").and_then(Value::as_str) {
        push_unique(&mut bullets, format!("Status: {status}"));
    }
    if let Some(issues) = details.get("issues").and_then(Value::as_array) {
        for entry in issues {
            match entry {
                Value::String(message) => push_unique(&mut bullets, message.to_string()),
                Value::Object(map) => {
                    let message = map
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if message.is_empty() {
                        continue;
                    }
                    if let Some(id) = map.get("id").and_then(Value::as_str) {
                        push_unique(&mut bullets, format!("{id}: {message}"));
                    } else {
                        push_unique(&mut bullets, message.to_string());
                    }
                }
                _ => {}
            }
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

fn reason_display(code: &str) -> Option<&'static str> {
    match code {
        "resolve_no_match" => Some("No compatible release satisfied the requested constraint."),
        "invalid_requirement" => Some("One of the requirements is invalid (PEP 508 parse failed)."),
        "pypi_unreachable" => Some("Unable to reach PyPI while resolving dependencies."),
        "resolve_failed" => Some("Dependency resolver failed."),
        _ => None,
    }
}

fn core_call(
    info: CommandInfo,
    outcome: anyhow::Result<px_core::ExecutionOutcome>,
) -> Result<(CommandInfo, px_core::ExecutionOutcome)> {
    let result = outcome.map_err(|err| eyre!("{err:?}"))?;
    Ok((info, result))
}

#[allow(clippy::too_many_lines)]
fn dispatch_command(
    ctx: &CommandContext,
    group: &CommandGroupCli,
) -> Result<(CommandInfo, px_core::ExecutionOutcome)> {
    match group {
        CommandGroupCli::Init(args) => {
            let info = CommandInfo::new(CommandGroup::Init, "init");
            let request = project_init_request_from_args(args);
            core_call(info, px_core::project_init(ctx, &request))
        }
        CommandGroupCli::Add(args) => {
            let info = CommandInfo::new(CommandGroup::Add, "add");
            let request = ProjectAddRequest {
                specs: args.specs.clone(),
            };
            core_call(info, px_core::project_add(ctx, &request))
        }
        CommandGroupCli::Remove(args) => {
            let info = CommandInfo::new(CommandGroup::Remove, "remove");
            let request = ProjectRemoveRequest {
                specs: args.specs.clone(),
            };
            core_call(info, px_core::project_remove(ctx, &request))
        }
        CommandGroupCli::Sync(args) => {
            let info = CommandInfo::new(CommandGroup::Sync, "sync");
            let request = project_sync_request_from_args(args);
            core_call(info, px_core::project_sync(ctx, &request))
        }
        CommandGroupCli::Update(args) => {
            let info = CommandInfo::new(CommandGroup::Update, "update");
            let request = ProjectUpdateRequest {
                specs: args.specs.clone(),
            };
            core_call(info, px_core::project_update(ctx, &request))
        }
        CommandGroupCli::Run(args) => {
            let info = CommandInfo::new(CommandGroup::Run, "run");
            let request = run_request_from_args(args);
            core_call(info, px_core::run_project(ctx, &request))
        }
        CommandGroupCli::Test(args) => {
            let info = CommandInfo::new(CommandGroup::Test, "test");
            let request = test_request_from_args(args);
            core_call(info, px_core::test_project(ctx, &request))
        }
        CommandGroupCli::Fmt(args) => {
            let info = CommandInfo::new(CommandGroup::Fmt, "fmt");
            let request = fmt_request_from_args(args);
            core_call(info, px_core::run_fmt(ctx, &request))
        }
        CommandGroupCli::Status => {
            let info = CommandInfo::new(CommandGroup::Status, "status");
            core_call(info, px_core::project_status(ctx))
        }
        CommandGroupCli::Build(args) => {
            let info = CommandInfo::new(CommandGroup::Build, "build");
            let request = build_request_from_args(args);
            core_call(info, px_core::build_project(ctx, &request))
        }
        CommandGroupCli::Publish(args) => {
            let info = CommandInfo::new(CommandGroup::Publish, "publish");
            let request = publish_request_from_args(args);
            core_call(info, px_core::publish_project(ctx, &request))
        }
        CommandGroupCli::Migrate(args) => {
            let info = CommandInfo::new(CommandGroup::Migrate, "migrate");
            let request = migrate_request_from_args(args);
            core_call(info, px_core::migrate(ctx, &request))
        }
        CommandGroupCli::Why(args) => {
            let info = CommandInfo::new(CommandGroup::Why, "why");
            let request = project_why_request_from_args(args);
            core_call(info, px_core::project_why(ctx, &request))
        }
        CommandGroupCli::Tool(cmd) => match cmd {
            ToolCommand::Install(args) => {
                let info = CommandInfo::new(CommandGroup::Tool, "install");
                let request = tool_install_request_from_args(args);
                core_call(info, px_core::tool_install(ctx, &request))
            }
            ToolCommand::Run(args) => {
                let info = CommandInfo::new(CommandGroup::Tool, "run");
                let request = tool_run_request_from_args(args);
                core_call(info, px_core::tool_run(ctx, &request))
            }
            ToolCommand::List => {
                let info = CommandInfo::new(CommandGroup::Tool, "list");
                core_call(info, px_core::tool_list(ctx, ToolListRequest))
            }
            ToolCommand::Remove(args) => {
                let info = CommandInfo::new(CommandGroup::Tool, "remove");
                let request = tool_remove_request_from_args(args);
                core_call(info, px_core::tool_remove(ctx, &request))
            }
            ToolCommand::Upgrade(args) => {
                let info = CommandInfo::new(CommandGroup::Tool, "upgrade");
                let request = tool_upgrade_request_from_args(args);
                core_call(info, px_core::tool_upgrade(ctx, &request))
            }
        },
        CommandGroupCli::Python(cmd) => match cmd {
            PythonCommand::List => {
                let info = CommandInfo::new(CommandGroup::Python, "list");
                core_call(info, px_core::python_list(ctx, &px_core::PythonListRequest))
            }
            PythonCommand::Info => {
                let info = CommandInfo::new(CommandGroup::Python, "info");
                core_call(info, px_core::python_info(ctx, &px_core::PythonInfoRequest))
            }
            PythonCommand::Install(args) => {
                let info = CommandInfo::new(CommandGroup::Python, "install");
                let request = python_install_request_from_args(args);
                core_call(info, px_core::python_install(ctx, &request))
            }
            PythonCommand::Use(args) => {
                let info = CommandInfo::new(CommandGroup::Python, "use");
                let request = python_use_request_from_args(args);
                core_call(info, px_core::python_use(ctx, &request))
            }
        },
    }
}

fn test_request_from_args(args: &TestArgs) -> TestRequest {
    TestRequest {
        pytest_args: args.args.clone(),
        frozen: args.frozen,
    }
}

fn run_request_from_args(args: &RunArgs) -> RunRequest {
    let (entry, forwarded_args) = normalize_run_invocation(args);
    RunRequest {
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

fn fmt_request_from_args(args: &FmtArgs) -> FmtRequest {
    FmtRequest {
        args: args.args.clone(),
        frozen: args.frozen,
    }
}

fn tool_install_request_from_args(args: &ToolInstallArgs) -> ToolInstallRequest {
    ToolInstallRequest {
        name: args.name.clone(),
        spec: args.spec.clone(),
        python: args.python.clone(),
        entry: args.module.clone(),
    }
}

fn tool_run_request_from_args(args: &ToolRunArgs) -> ToolRunRequest {
    ToolRunRequest {
        name: args.name.clone(),
        args: args.args.clone(),
        console: args.console.clone(),
    }
}

fn tool_remove_request_from_args(args: &ToolRemoveArgs) -> ToolRemoveRequest {
    ToolRemoveRequest {
        name: args.name.clone(),
    }
}

fn tool_upgrade_request_from_args(args: &ToolUpgradeArgs) -> ToolUpgradeRequest {
    ToolUpgradeRequest {
        name: args.name.clone(),
        python: args.python.clone(),
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
        mode: if args.write {
            MigrationMode::Apply
        } else {
            MigrationMode::Preview
        },
        workspace: if args.allow_dirty {
            WorkspacePolicy::AllowDirty
        } else {
            WorkspacePolicy::CleanOnly
        },
        lock_behavior: if args.lock_only {
            LockBehavior::LockOnly
        } else {
            LockBehavior::Full
        },
        autopin: if args.no_autopin {
            AutopinPreference::Disabled
        } else {
            AutopinPreference::Enabled
        },
    }
}

fn build_request_from_args(args: &BuildArgs) -> BuildRequest {
    let (include_sdist, include_wheel) = match args.format {
        BuildFormat::Sdist => (true, false),
        BuildFormat::Wheel => (false, true),
        BuildFormat::Both => (true, true),
    };
    BuildRequest {
        include_sdist,
        include_wheel,
        out: args.out.clone(),
        dry_run: args.common.dry_run,
    }
}

fn publish_request_from_args(args: &PublishArgs) -> PublishRequest {
    PublishRequest {
        registry: args.registry.clone(),
        token_env: args.token_env.clone(),
        dry_run: args.common.dry_run,
    }
}

fn project_sync_request_from_args(args: &SyncArgs) -> ProjectSyncRequest {
    ProjectSyncRequest {
        frozen: args.frozen,
    }
}

fn project_why_request_from_args(args: &WhyArgs) -> ProjectWhyRequest {
    ProjectWhyRequest {
        package: args.package.clone(),
        issue: args.issue.clone(),
    }
}

fn python_install_request_from_args(args: &PythonInstallArgs) -> px_core::PythonInstallRequest {
    px_core::PythonInstallRequest {
        version: args.version.clone(),
        path: args
            .path
            .as_ref()
            .map(|path| path.to_string_lossy().to_string()),
        set_default: args.default,
    }
}

fn python_use_request_from_args(args: &PythonUseArgs) -> px_core::PythonUseRequest {
    px_core::PythonUseRequest {
        version: args.version.clone(),
    }
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
        allow_hyphen_values = true,
        last = true
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
