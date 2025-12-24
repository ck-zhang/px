use std::io::{self, BufRead, Write};

use atty::Stream;
use color_eyre::Result;
use px_core::api as px_core;
use px_core::{
    explain_entrypoint, explain_run, is_missing_project_error,
    missing_project_outcome as core_missing_project_outcome, AutopinPreference, BuildRequest,
    CommandContext, CommandGroup, CommandInfo, FmtRequest, LockBehavior, MigrateRequest,
    MigrationMode, ProjectAddRequest, ProjectInitRequest, ProjectRemoveRequest, ProjectSyncRequest,
    ProjectUpdateRequest, ProjectWhyRequest, PublishRequest, RunRequest, TestRequest,
    ToolInstallRequest, ToolListRequest, ToolRemoveRequest, ToolRunRequest, ToolUpgradeRequest,
    WorkspacePolicy,
};

use crate::{
    BuildArgs, BuildFormat, CommandGroupCli, CompletionShell, CompletionsArgs, ExplainCommand,
    ExplainEntrypointArgs, FmtArgs, InitArgs, MigrateArgs, PackAppArgs, PackCommand, PackImageArgs,
    PublishArgs, RunArgs, SyncArgs, TestArgs, ToolCommand, ToolInstallArgs, ToolRemoveArgs,
    ToolRunArgs, ToolUpgradeArgs, WhyArgs,
};
use crate::{PythonCommand, PythonInstallArgs, PythonUseArgs};

pub fn dispatch_command(
    ctx: &CommandContext,
    group: &CommandGroupCli,
) -> Result<(CommandInfo, px_core::ExecutionOutcome)> {
    match group {
        CommandGroupCli::Init(args) => {
            let info = CommandInfo::new(CommandGroup::Init, "init");
            let request = project_init_request_from_args(args);
            let outcome = dispatch_init(ctx, info, &request)?;
            Ok((info, outcome))
        }
        CommandGroupCli::Add(args) => {
            let info = CommandInfo::new(CommandGroup::Add, "add");
            let request = ProjectAddRequest {
                specs: args.specs.clone(),
                pin: args.pin,
                dry_run: args.common.dry_run,
            };
            core_call(info, || px_core::project_add(ctx, &request))
        }
        CommandGroupCli::Remove(args) => {
            let info = CommandInfo::new(CommandGroup::Remove, "remove");
            let request = ProjectRemoveRequest {
                specs: args.names.clone(),
                dry_run: args.common.dry_run,
            };
            core_call(info, || px_core::project_remove(ctx, &request))
        }
        CommandGroupCli::Sync(args) => {
            let info = CommandInfo::new(CommandGroup::Sync, "sync");
            let request = project_sync_request_from_args(args);
            core_call(info, || px_core::project_sync(ctx, &request))
        }
        CommandGroupCli::Update(args) => {
            let info = CommandInfo::new(CommandGroup::Update, "update");
            let request = ProjectUpdateRequest {
                specs: args.specs.clone(),
                dry_run: args.common.dry_run,
            };
            core_call(info, || px_core::project_update(ctx, &request))
        }
        CommandGroupCli::Run(args) => {
            let info = CommandInfo::new(CommandGroup::Run, "run");
            let request = run_request_from_args(args);
            core_call(info, || px_core::run_project(ctx, &request))
        }
        CommandGroupCli::Test(args) => {
            let info = CommandInfo::new(CommandGroup::Test, "test");
            let request = test_request_from_args(args);
            core_call(info, || px_core::test_project(ctx, &request))
        }
        CommandGroupCli::Fmt(args) => {
            let info = CommandInfo::new(CommandGroup::Fmt, "fmt");
            let request = fmt_request_from_args(args);
            core_call(info, || px_core::run_fmt(ctx, &request))
        }
        CommandGroupCli::Status(_args) => {
            let info = CommandInfo::new(CommandGroup::Status, "status");
            core_call(info, || px_core::project_status(ctx))
        }
        CommandGroupCli::Explain(cmd) => match cmd {
            ExplainCommand::Run(args) => {
                let info = CommandInfo::new(CommandGroup::Explain, "run");
                let request = run_request_from_args(args);
                core_call(info, || explain_run(ctx, &request))
            }
            ExplainCommand::Entrypoint(ExplainEntrypointArgs { name }) => {
                let info = CommandInfo::new(CommandGroup::Explain, "entrypoint");
                core_call(info, || explain_entrypoint(ctx, name.as_str()))
            }
        },
        CommandGroupCli::Build(args) => {
            let info = CommandInfo::new(CommandGroup::Build, "build");
            let request = build_request_from_args(args);
            core_call(info, || px_core::build_project(ctx, &request))
        }
        CommandGroupCli::Publish(args) => {
            let info = CommandInfo::new(CommandGroup::Publish, "publish");
            let request = publish_request_from_args(args);
            core_call(info, || px_core::publish_project(ctx, &request))
        }
        CommandGroupCli::Pack(cmd) => match cmd {
            PackCommand::Image(args) => {
                let info = CommandInfo::new(CommandGroup::Pack, "pack image");
                let request = pack_image_request_from_args(args);
                core_call(info, || px_core::pack_image(ctx, &request))
            }
            PackCommand::App(args) => {
                let info = CommandInfo::new(CommandGroup::Pack, "pack app");
                let request = pack_app_request_from_args(args);
                core_call(info, || px_core::pack_app(ctx, &request))
            }
        },
        CommandGroupCli::Migrate(args) => {
            let info = CommandInfo::new(CommandGroup::Migrate, "migrate");
            let request = migrate_request_from_args(args);
            core_call(info, || px_core::migrate(ctx, &request))
        }
        CommandGroupCli::Why(args) => {
            let info = CommandInfo::new(CommandGroup::Why, "why");
            let request = project_why_request_from_args(args);
            core_call(info, || px_core::project_why(ctx, &request))
        }
        CommandGroupCli::Tool(cmd) => match cmd {
            ToolCommand::Install(args) => {
                let info = CommandInfo::new(CommandGroup::Tool, "install");
                let request = tool_install_request_from_args(args);
                core_call(info, || px_core::tool_install(ctx, &request))
            }
            ToolCommand::Run(args) => {
                let info = CommandInfo::new(CommandGroup::Tool, "run");
                let request = tool_run_request_from_args(args);
                core_call(info, || px_core::tool_run(ctx, &request))
            }
            ToolCommand::List => {
                let info = CommandInfo::new(CommandGroup::Tool, "list");
                core_call(info, || px_core::tool_list(ctx, ToolListRequest))
            }
            ToolCommand::Remove(args) => {
                let info = CommandInfo::new(CommandGroup::Tool, "remove");
                let request = tool_remove_request_from_args(args);
                core_call(info, || px_core::tool_remove(ctx, &request))
            }
            ToolCommand::Upgrade(args) => {
                let info = CommandInfo::new(CommandGroup::Tool, "upgrade");
                let request = tool_upgrade_request_from_args(args);
                core_call(info, || px_core::tool_upgrade(ctx, &request))
            }
        },
        CommandGroupCli::Python(cmd) => match cmd {
            PythonCommand::List => {
                let info = CommandInfo::new(CommandGroup::Python, "list");
                core_call(info, || {
                    px_core::python_list(ctx, &px_core::PythonListRequest)
                })
            }
            PythonCommand::Info => {
                let info = CommandInfo::new(CommandGroup::Python, "info");
                core_call(info, || {
                    px_core::python_info(ctx, &px_core::PythonInfoRequest)
                })
            }
            PythonCommand::Install(args) => {
                let info = CommandInfo::new(CommandGroup::Python, "install");
                let request = python_install_request_from_args(args);
                core_call(info, || px_core::python_install(ctx, &request))
            }
            PythonCommand::Use(args) => {
                let info = CommandInfo::new(CommandGroup::Python, "use");
                let request = python_use_request_from_args(args);
                core_call(info, || px_core::python_use(ctx, &request))
            }
        },
        CommandGroupCli::Completions(args) => {
            let info = CommandInfo::new(CommandGroup::Completions, "completions");
            Ok((info, completions_outcome(args)))
        }
    }
}

fn dispatch_init(
    ctx: &CommandContext,
    info: CommandInfo,
    request: &ProjectInitRequest,
) -> Result<px_core::ExecutionOutcome> {
    let attempt = core_call_no_spinner(info, || px_core::project_init(ctx, request))?;
    let Some(version) = init_install_prompt_version(&attempt) else {
        return Ok(attempt);
    };

    if request.dry_run || !init_can_prompt_for_runtime(ctx) {
        return Ok(attempt);
    }

    eprint!(
        "No px-managed Python runtime found. Install Python {version} now? (Y/n) "
    );
    io::stderr().flush().ok();
    let mut answer = String::new();
    let _ = io::stdin().lock().read_line(&mut answer);
    let answer = answer.trim();
    let accepted = answer.is_empty()
        || matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes");
    if !accepted {
        return Ok(px_core::ExecutionOutcome::user_error(
            "px init requires a px-managed Python runtime",
            serde_json::json!({
                "reason": "missing_init_runtime",
                "install_version": version,
                "recommendation": {
                    "command": format!("px python install {version}"),
                    "hint": "Re-run `px init` after the runtime is installed."
                }
            }),
        ));
    }

    let install = core_call_no_spinner(info, || {
        px_core::python_install(
            ctx,
            &px_core::PythonInstallRequest {
                version: version.clone(),
                path: None,
                set_default: false,
            },
        )
    })?;
    if install.status != px_core::CommandStatus::Ok {
        return Ok(install);
    }

    core_call_no_spinner(info, || px_core::project_init(ctx, request))
}

fn init_can_prompt_for_runtime(ctx: &CommandContext) -> bool {
    if ctx.global.json {
        return false;
    }
    if ctx.env_flag_enabled("CI") {
        return false;
    }
    atty::is(Stream::Stdin) && atty::is(Stream::Stdout)
}

fn init_install_prompt_version(outcome: &px_core::ExecutionOutcome) -> Option<String> {
    if outcome.status != px_core::CommandStatus::UserError {
        return None;
    }
    let details = outcome.details.as_object()?;
    if details.get("reason").and_then(serde_json::Value::as_str) != Some("missing_init_runtime") {
        return None;
    }
    details
        .get("install_version")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
}

fn core_call<F>(info: CommandInfo, action: F) -> Result<(CommandInfo, px_core::ExecutionOutcome)>
where
    F: FnOnce() -> anyhow::Result<px_core::ExecutionOutcome>,
{
    let group = info.group.to_string();
    let name = info.name;
    let label = if name.starts_with(&group) {
        name.to_string()
    } else {
        format!("{group} {name}")
    };
    let _spinner = px_core::progress::ProgressReporter::spinner(format!("Running {label}"));
    let outcome = action();
    match outcome {
        Ok(result) => Ok((info, result)),
        Err(err) => {
            if let Some(outcome) = missing_project_outcome(&err) {
                Ok((info, outcome))
            } else if let Some(outcome) = px_core::manifest_error_outcome(&err) {
                Ok((info, outcome))
            } else if let Some(user) = err.downcast_ref::<px_core::InstallUserError>() {
                Ok((
                    info,
                    px_core::ExecutionOutcome::user_error(
                        user.message().to_string(),
                        user.details().clone(),
                    ),
                ))
            } else {
                let issues: Vec<String> =
                    err.chain().map(std::string::ToString::to_string).collect();
                Ok((
                    info,
                    px_core::ExecutionOutcome::failure(
                        err.to_string(),
                        serde_json::json!({
                            "reason": "internal_error",
                            "error": err.to_string(),
                            "issues": issues,
                            "hint": "Re-run with `--debug` for more detail, or open an issue if this persists.",
                        }),
                    ),
                ))
            }
        }
    }
}

fn core_call_no_spinner<F>(_info: CommandInfo, action: F) -> Result<px_core::ExecutionOutcome>
where
    F: FnOnce() -> anyhow::Result<px_core::ExecutionOutcome>,
{
    let outcome = action();
    match outcome {
        Ok(result) => Ok(result),
        Err(err) => {
            if let Some(outcome) = missing_project_outcome(&err) {
                Ok(outcome)
            } else if let Some(outcome) = px_core::manifest_error_outcome(&err) {
                Ok(outcome)
            } else if let Some(user) = err.downcast_ref::<px_core::InstallUserError>() {
                Ok(px_core::ExecutionOutcome::user_error(
                    user.message().to_string(),
                    user.details().clone(),
                ))
            } else {
                let issues: Vec<String> =
                    err.chain().map(std::string::ToString::to_string).collect();
                Ok(px_core::ExecutionOutcome::failure(
                    err.to_string(),
                    serde_json::json!({
                        "reason": "internal_error",
                        "error": err.to_string(),
                        "issues": issues,
                        "hint": "Re-run with `--debug` for more detail, or open an issue if this persists.",
                    }),
                ))
            }
        }
    }
}

fn missing_project_outcome(err: &anyhow::Error) -> Option<px_core::ExecutionOutcome> {
    if is_missing_project_error(err) {
        Some(core_missing_project_outcome())
    } else {
        None
    }
}

fn test_request_from_args(args: &TestArgs) -> TestRequest {
    TestRequest {
        args: args.args.clone(),
        frozen: args.frozen,
        ephemeral: args.ephemeral,
        sandbox: args.sandbox,
        at: args.at.clone(),
    }
}

fn run_request_from_args(args: &RunArgs) -> RunRequest {
    let (entry, forwarded_args) = normalize_run_invocation(args);
    RunRequest {
        entry,
        target: args.target.clone(),
        args: forwarded_args,
        frozen: args.frozen,
        ephemeral: args.ephemeral,
        allow_floating: args.allow_floating,
        interactive: if args.interactive {
            Some(true)
        } else if args.non_interactive {
            Some(false)
        } else {
            None
        },
        sandbox: args.sandbox,
        at: args.at.clone(),
    }
}

fn project_init_request_from_args(args: &InitArgs) -> ProjectInitRequest {
    ProjectInitRequest {
        package: args.package.clone(),
        python: args.py.clone(),
        dry_run: args.common.dry_run,
        force: args.force,
    }
}

fn completions_outcome(args: &CompletionsArgs) -> px_core::ExecutionOutcome {
    let snippet = match args.shell {
        CompletionShell::Bash => "source <(COMPLETE=bash px)".to_string(),
        CompletionShell::Zsh => "source <(COMPLETE=zsh px)".to_string(),
        CompletionShell::Fish => "source (COMPLETE=fish px | psub)".to_string(),
        CompletionShell::Powershell => {
            "$env:COMPLETE='powershell'; px | Out-String | Invoke-Expression; Remove-Item Env:\\COMPLETE".to_string()
        }
    };
    px_core::ExecutionOutcome::success(
        snippet.clone(),
        serde_json::json!({
            "passthrough": true,
            "snippet": snippet,
            "shell": format!("{:?}", args.shell).to_lowercase(),
        }),
    )
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
        python: args.python.clone(),
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
        dry_run: if args.upload { false } else { args.dry_run },
    }
}

fn pack_image_request_from_args(args: &PackImageArgs) -> px_core::PackRequest {
    px_core::PackRequest {
        target: px_core::PackTarget::Image,
        tag: args.tag.clone(),
        out: args.out.clone(),
        push: args.push,
        allow_dirty: args.allow_dirty,
        entrypoint: None,
        workdir: None,
    }
}

fn pack_app_request_from_args(args: &PackAppArgs) -> px_core::PackRequest {
    let entrypoint = args
        .entrypoint
        .as_ref()
        .map(|raw| raw.split_whitespace().map(|s| s.to_string()).collect());
    px_core::PackRequest {
        target: px_core::PackTarget::App,
        tag: None,
        out: args.out.clone(),
        push: false,
        allow_dirty: args.allow_dirty,
        entrypoint,
        workdir: args.workdir.clone(),
    }
}

fn project_sync_request_from_args(args: &SyncArgs) -> ProjectSyncRequest {
    ProjectSyncRequest {
        frozen: args.frozen,
        dry_run: args.common.dry_run,
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
    if args.target.is_some() {
        let mut forwarded = Vec::new();
        if let Some(positional) = &args.target_value {
            forwarded.push(positional.clone());
        }
        forwarded.extend(args.args.clone());
        return (None, forwarded);
    }
    match args.target_value.as_ref() {
        Some(first) if first.starts_with('-') => {
            let mut forwarded = Vec::with_capacity(args.args.len() + 1);
            forwarded.push(first.clone());
            forwarded.extend(args.args.clone());
            (None, forwarded)
        }
        Some(first) => (Some(first.clone()), args.args.clone()),
        None => (None, args.args.clone()),
    }
}
