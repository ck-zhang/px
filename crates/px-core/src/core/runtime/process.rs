#![deny(clippy::all, warnings)]

use std::{
    path::Path,
    process::{Command, Stdio},
};

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct RunOutput {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Execute a program and capture stdout/stderr.
///
/// # Errors
///
/// Returns an error when the program cannot be spawned or the I/O streams cannot
/// be read entirely.
pub fn run_command(
    program: &str,
    args: &[String],
    envs: &[(String, String)],
    cwd: &Path,
) -> Result<RunOutput> {
    run_command_with_stdin(program, args, envs, cwd, false)
}

/// Execute a program and capture stdout/stderr, optionally inheriting stdin.
///
/// # Errors
///
/// Returns an error when the program cannot be spawned or the I/O streams cannot
/// be read entirely.
pub fn run_command_with_stdin(
    program: &str,
    args: &[String],
    envs: &[(String, String)],
    cwd: &Path,
    inherit_stdin: bool,
) -> Result<RunOutput> {
    let mut command = Command::new(program);
    command.args(args);
    for (key, value) in envs {
        command.env(key, value);
    }
    command.current_dir(cwd);
    if inherit_stdin {
        command.stdin(Stdio::inherit());
    } else {
        command.stdin(Stdio::null());
    }
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    let output = command
        .output()
        .with_context(|| format!("failed to start {program}"))?;
    let code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    Ok(RunOutput {
        code,
        stdout,
        stderr,
    })
}

/// Execute a program with inherited stdio for interactive tools.
///
/// # Errors
///
/// Returns an error when the program cannot be spawned or exits abnormally.
pub fn run_command_passthrough(
    program: &str,
    args: &[String],
    envs: &[(String, String)],
    cwd: &Path,
) -> Result<RunOutput> {
    let mut command = Command::new(program);
    command.args(args);
    for (key, value) in envs {
        command.env(key, value);
    }
    command.current_dir(cwd);
    command.stdin(Stdio::inherit());
    command.stdout(Stdio::inherit());
    command.stderr(Stdio::inherit());

    let status = command
        .status()
        .with_context(|| format!("failed to start {program}"))?;
    let code = status.code().unwrap_or(-1);
    Ok(RunOutput {
        code,
        stdout: String::new(),
        stderr: String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[cfg(unix)]
    #[test]
    fn run_command_captures_output_and_status_unix() -> Result<()> {
        let output = run_command(
            "/bin/sh",
            &[
                "-c".to_string(),
                "printf out && printf err >&2; exit 7".to_string(),
            ],
            &[],
            Path::new("."),
        )?;
        assert_eq!(output.code, 7);
        assert_eq!(output.stdout, "out");
        assert_eq!(output.stderr, "err");
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn run_command_captures_output_and_status_windows() -> Result<()> {
        let output = run_command(
            "cmd",
            &[
                "/C".to_string(),
                "@echo off & echo out & echo err 1>&2 & exit /B 7".to_string(),
            ],
            &[],
            Path::new("."),
        )?;
        assert_eq!(output.code, 7);
        assert_eq!(output.stdout.trim(), "out");
        assert_eq!(output.stderr.trim(), "err");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn run_command_passthrough_returns_status_unix() -> Result<()> {
        let output = run_command_passthrough(
            "/bin/sh",
            &["-c".to_string(), "exit 0".to_string()],
            &[],
            Path::new("."),
        )?;
        assert_eq!(output.code, 0);
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn run_command_passthrough_returns_status_windows() -> Result<()> {
        let output = run_command_passthrough(
            "cmd",
            &["/C".to_string(), "exit /B 0".to_string()],
            &[],
            Path::new("."),
        )?;
        assert_eq!(output.code, 0);
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
        Ok(())
    }
}
