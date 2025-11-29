#![deny(clippy::all, warnings)]

use std::{env, sync::Arc};

use clap::Parser;
use color_eyre::{eyre::eyre, Result};
use px_core::{CommandContext, GlobalOptions, SystemEffects};

mod cli;
mod dispatch;
mod output;
mod style;
mod traceback;

pub(crate) use crate::cli::*;

use dispatch::dispatch_command;
use output::{emit_output, OutputOptions, StatusRenderOptions};

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
    let status_opts = match &cli.command {
        CommandGroupCli::Status(args) => Some(StatusRenderOptions { brief: args.brief }),
        _ => None,
    };
    let code = emit_output(&output_opts, subcommand_json, status_opts, info, &outcome)?;

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
