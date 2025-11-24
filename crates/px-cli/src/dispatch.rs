use color_eyre::{eyre::eyre, Result};
use px_core::{
    self, is_missing_project_error, missing_project_outcome as core_missing_project_outcome,
    AutopinPreference, BuildRequest, CommandContext, CommandGroup, CommandInfo, FmtRequest,
    LockBehavior, MigrateRequest, MigrationMode, ProjectAddRequest, ProjectInitRequest,
    ProjectRemoveRequest, ProjectSyncRequest, ProjectUpdateRequest, ProjectWhyRequest,
    PublishRequest, RunRequest, TestRequest, ToolInstallRequest, ToolListRequest,
    ToolRemoveRequest, ToolRunRequest, ToolUpgradeRequest, WorkspacePolicy,
};

use crate::{
    BuildArgs, BuildFormat, CommandGroupCli, FmtArgs, InitArgs, MigrateArgs, PublishArgs, RunArgs,
    SyncArgs, TestArgs, ToolCommand, ToolInstallArgs, ToolRemoveArgs, ToolRunArgs, ToolUpgradeArgs,
    WhyArgs,
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
            core_call(info, px_core::project_init(ctx, &request))
        }
        CommandGroupCli::Add(args) => {
            let info = CommandInfo::new(CommandGroup::Add, "add");
            let request = ProjectAddRequest {
                specs: args.specs.clone(),
                dry_run: args.common.dry_run,
            };
            core_call(info, px_core::project_add(ctx, &request))
        }
        CommandGroupCli::Remove(args) => {
            let info = CommandInfo::new(CommandGroup::Remove, "remove");
            let request = ProjectRemoveRequest {
                specs: args.specs.clone(),
                dry_run: args.common.dry_run,
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
                dry_run: args.common.dry_run,
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

fn core_call(
    info: CommandInfo,
    outcome: anyhow::Result<px_core::ExecutionOutcome>,
) -> Result<(CommandInfo, px_core::ExecutionOutcome)> {
    match outcome {
        Ok(result) => Ok((info, result)),
        Err(err) => {
            if let Some(outcome) = missing_project_outcome(&err) {
                Ok((info, outcome))
            } else {
                Err(eyre!("{err:?}"))
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
        dry_run: args.common.dry_run,
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
    let mut forwarded = args.args.clone();
    match &args.entry {
        Some(value) if value.starts_with('-') => {
            forwarded.insert(0, value.clone());
            (None, forwarded)
        }
        _ => (args.entry.clone(), forwarded),
    }
}
