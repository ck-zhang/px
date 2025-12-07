#![deny(clippy::all, warnings)]

use std::{env, ffi::OsString, path::PathBuf, sync::Arc};

use clap::{CommandFactory, Parser};
use clap_complete::CompleteEnv;
use color_eyre::{eyre::eyre, Result};
use px_core::{CommandContext, GlobalOptions, SystemEffects};

mod cli;
mod completion;
mod dispatch;
mod output;
mod style;
mod traceback;

pub(crate) use crate::cli::*;

use dispatch::dispatch_command;
use output::{emit_output, OutputOptions, StatusRenderOptions};

fn main() -> Result<()> {
    color_eyre::install()?;
    CompleteEnv::with_factory(PxCli::command)
        .bin("px")
        .complete();

    if cfg!(windows) {
        eprintln!("px currently supports Linux and macOS only; Windows is not supported yet. Please use WSL or a Unix host.");
        std::process::exit(1);
    }

    let raw_args: Vec<_> = env::args_os().collect();
    let cli = PxCli::parse_from(normalize_run_args(raw_args));
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

fn normalize_run_args(args: Vec<OsString>) -> Vec<OsString> {
    let mut top_level = None;
    for (idx, arg) in args.iter().enumerate().skip(1) {
        let text = arg.to_string_lossy();
        if text.starts_with('-') {
            continue;
        }
        top_level = Some(idx);
        break;
    }
    let Some(run_pos) = top_level.filter(|idx| {
        args.get(*idx)
            .map(|arg| arg.to_string_lossy() == "run")
            .unwrap_or(false)
    }) else {
        return args;
    };
    if args.iter().skip(run_pos + 1).any(|arg| arg == "--") {
        return args;
    }

    let mut insert_pos = None;
    let mut idx = run_pos + 1;
    while idx < args.len() {
        let arg = &args[idx];
        if arg == "--" {
            return args;
        }
        if arg == "--target" {
            let value_idx = idx + 1;
            if value_idx < args.len() {
                insert_pos = Some(value_idx + 1);
            }
            break;
        }
        if arg == "--at" {
            idx += 2;
            continue;
        }
        let is_positional = {
            let text = arg.to_string_lossy();
            text == "-" || !text.starts_with('-')
        };
        if is_positional {
            insert_pos = Some(idx + 1);
            break;
        }
        idx += 1;
    }

    if let Some(pos) = insert_pos.filter(|pos| *pos < args.len()) {
        let mut normalized = args;
        normalized.insert(pos, OsString::from("--"));
        normalized
    } else {
        args
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_forwards_flags_after_target() {
        let cli = PxCli::try_parse_from(normalize_run_args(vec![
            OsString::from("px"),
            OsString::from("run"),
            OsString::from("tests/runtests.py"),
            OsString::from("--help"),
        ]))
        .expect("parse run args");

        match cli.command {
            CommandGroupCli::Run(run) => {
                assert_eq!(run.target_value.as_deref(), Some("tests/runtests.py"));
                assert_eq!(run.args, vec!["--help".to_string()]);
            }
            other => panic!("expected run command, got {other:?}"),
        }
    }

    #[test]
    fn run_does_not_add_delimiter_without_trailing_args() {
        let cli = PxCli::try_parse_from(normalize_run_args(vec![
            OsString::from("px"),
            OsString::from("run"),
            OsString::from("script.py"),
        ]))
        .expect("parse run args");

        match cli.command {
            CommandGroupCli::Run(run) => {
                assert_eq!(run.target_value.as_deref(), Some("script.py"));
                assert!(run.args.is_empty());
            }
            other => panic!("expected run command, got {other:?}"),
        }
    }

    #[test]
    fn run_handles_explicit_target_flag() {
        let cli = PxCli::try_parse_from(normalize_run_args(vec![
            OsString::from("px"),
            OsString::from("run"),
            OsString::from("--target"),
            OsString::from("./runtests.py"),
            OsString::from("--verbosity"),
            OsString::from("2"),
        ]))
        .expect("parse run args");

        match cli.command {
            CommandGroupCli::Run(run) => {
                assert_eq!(run.target.as_deref(), Some("./runtests.py"));
                let mut forwarded = Vec::new();
                if let Some(positional) = &run.target_value {
                    forwarded.push(positional.clone());
                }
                forwarded.extend(run.args.clone());
                assert_eq!(forwarded, vec!["--verbosity".to_string(), "2".to_string()]);
            }
            other => panic!("expected run command, got {other:?}"),
        }
    }
}
