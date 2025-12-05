#![deny(clippy::all, warnings)]

use std::{env, path::PathBuf, sync::Arc};

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

    if cfg!(windows) {
        eprintln!("px currently supports Linux and macOS only; Windows is not supported yet. Please use WSL or a Unix host.");
        std::process::exit(1);
    }

    let cli = PxCli::parse();
    let trace = cli.trace || cli.debug;
    init_tracing(trace, cli.verbose);
    if cli.debug {
        env::set_var("PX_DEBUG", "1");
        if env::var_os("RUST_BACKTRACE").is_none() {
            env::set_var("RUST_BACKTRACE", "1");
        }
    }

    let subcommand_json = matches!(&cli.command, CommandGroupCli::Fmt(args) if args.json);
    if cli.json || subcommand_json {
        // Suppress spinners/progress when JSON output is requested.
        env::set_var("PX_PROGRESS", "0");
    }
    apply_env_overrides(&cli);
    let global = GlobalOptions {
        quiet: cli.quiet,
        verbose: cli.verbose,
        trace,
        debug: cli.debug,
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
        verbose: cli.verbose,
        debug: cli.debug,
        trace,
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

fn apply_env_overrides(cli: &PxCli) {
    if cli.offline {
        env::set_var("PX_ONLINE", "0");
    } else if cli.online {
        env::set_var("PX_ONLINE", "1");
    }
    if cli.no_resolver {
        env::set_var("PX_RESOLVER", "0");
    } else if cli.resolver {
        env::set_var("PX_RESOLVER", "1");
    }
    if cli.force_sdist {
        env::set_var("PX_FORCE_SDIST", "1");
    } else if cli.prefer_wheels {
        env::set_var("PX_FORCE_SDIST", "0");
    }

    // Keep the CAS store aligned with an explicit cache override. This avoids
    // surprises (and format mismatches) when a caller sets PX_CACHE_PATH but
    // forgets to pin PX_STORE_PATH as well.
    if env::var_os("PX_STORE_PATH").is_none() {
        if let Some(cache) = env::var_os("PX_CACHE_PATH") {
            let store = PathBuf::from(cache).join("store");
            env::set_var("PX_STORE_PATH", store);
        }
    }
}
