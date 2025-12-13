#![deny(clippy::all, warnings)]

use std::{
    io::{self, Read, Write},
    path::Path,
    process::{Command, Stdio},
    thread,
};

const PROXY_VARS: [&str; 8] = [
    "HTTP_PROXY",
    "http_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "ALL_PROXY",
    "all_proxy",
    "NO_PROXY",
    "no_proxy",
];

fn is_proxy_env(key: &str) -> bool {
    PROXY_VARS.contains(&key)
}

use anyhow::{Context, Result};

use crate::progress::ProgressSuspendGuard;

const DEFAULT_MAX_CAPTURE_BYTES: usize = 1024 * 1024;

fn max_capture_bytes() -> usize {
    std::env::var("PX_MAX_CAPTURE_BYTES")
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_MAX_CAPTURE_BYTES)
}

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
    let mut command = configured_command(program, args, envs, cwd);
    if inherit_stdin {
        command.stdin(Stdio::inherit());
    } else {
        command.stdin(Stdio::null());
    }
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    let mut child = command
        .spawn()
        .with_context(|| format!("failed to start {program}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("stdout missing for {program}"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("stderr missing for {program}"))?;
    let limit = max_capture_bytes();
    let stdout_handle = thread::spawn(move || read_to_string_limited(stdout, limit));
    let stderr_handle = thread::spawn(move || read_to_string_limited(stderr, limit));

    let status = child
        .wait()
        .with_context(|| format!("failed to wait for {program}"))?;
    let code = status.code().unwrap_or(-1);
    let (mut stdout, stdout_truncated) = stdout_handle
        .join()
        .map_err(|_| anyhow::anyhow!("stdout thread panicked"))??;
    let (mut stderr, stderr_truncated) = stderr_handle
        .join()
        .map_err(|_| anyhow::anyhow!("stderr thread panicked"))??;
    if stdout_truncated {
        stdout.push_str("\n[...truncated...]\n");
    }
    if stderr_truncated {
        stderr.push_str("\n[...truncated...]\n");
    }
    Ok(RunOutput {
        code,
        stdout,
        stderr,
    })
}

/// Execute a program while streaming stdout/stderr to the parent process.
///
/// # Errors
///
/// Returns an error when the program cannot be spawned or its output streams
/// cannot be read.
pub fn run_command_streaming(
    program: &str,
    args: &[String],
    envs: &[(String, String)],
    cwd: &Path,
) -> Result<RunOutput> {
    let _suspend = ProgressSuspendGuard::new();
    let mut command = configured_command(program, args, envs, cwd);
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    let mut child = command
        .spawn()
        .with_context(|| format!("failed to start {program}"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("stdout missing for {program}"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("stderr missing for {program}"))?;

    let limit = max_capture_bytes();
    let stdout_handle = thread::spawn(move || tee_to_string_limited(&mut stdout, io::stdout(), limit));
    let stderr_handle = thread::spawn(move || tee_to_string_limited(&mut stderr, io::stderr(), limit));

    let status = child
        .wait()
        .with_context(|| format!("failed to wait for {program}"))?;
    let code = status.code().unwrap_or(-1);
    let stdout = stdout_handle
        .join()
        .map_err(|_| anyhow::anyhow!("stdout thread panicked"))??;
    let stderr = stderr_handle
        .join()
        .map_err(|_| anyhow::anyhow!("stderr thread panicked"))??;

    Ok(RunOutput {
        code,
        stdout,
        stderr,
    })
}

fn configured_command(
    program: &str,
    args: &[String],
    envs: &[(String, String)],
    cwd: &Path,
) -> Command {
    let mut command = Command::new(program);
    command.args(args);
    for (key, value) in envs {
        if value.is_empty() && is_proxy_env(key) {
            command.env_remove(key);
            continue;
        }
        command.env(key, value);
    }
    command.current_dir(cwd);
    command
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
    let _suspend = ProgressSuspendGuard::new();
    let mut command = configured_command(program, args, envs, cwd);
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

fn read_to_string_limited(mut reader: impl Read, limit: usize) -> Result<(String, bool)> {
    let mut buffer = Vec::new();
    let mut truncated = false;
    let mut chunk = [0u8; 8192];
    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        append_limited(&mut buffer, &chunk[..read], limit, &mut truncated);
    }
    Ok((String::from_utf8_lossy(&buffer).to_string(), truncated))
}

fn tee_to_string_limited(
    reader: &mut dyn Read,
    mut writer: impl Write,
    limit: usize,
) -> Result<String> {
    let mut buffer = Vec::new();
    let mut truncated = false;
    let mut chunk = [0u8; 8192];
    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        writer.write_all(&chunk[..read])?;
        append_limited(&mut buffer, &chunk[..read], limit, &mut truncated);
    }
    writer.flush().ok();
    let mut text = String::from_utf8_lossy(&buffer).to_string();
    if truncated {
        text.push_str("\n[...truncated...]\n");
    }
    Ok(text)
}

fn append_limited(buffer: &mut Vec<u8>, chunk: &[u8], limit: usize, truncated: &mut bool) {
    if limit == 0 {
        return;
    }
    if buffer.len().saturating_add(chunk.len()) <= limit {
        buffer.extend_from_slice(chunk);
        return;
    }
    *truncated = true;
    let old_len = buffer.len();
    let excess = old_len.saturating_add(chunk.len()).saturating_sub(limit);
    if excess >= old_len {
        buffer.clear();
        let drop_from_chunk = excess.saturating_sub(old_len).min(chunk.len());
        buffer.extend_from_slice(&chunk[drop_from_chunk..]);
    } else {
        buffer.drain(0..excess);
        buffer.extend_from_slice(chunk);
    }
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

    #[cfg(unix)]
    #[test]
    fn run_command_truncates_large_output_unix() -> Result<()> {
        let bytes = DEFAULT_MAX_CAPTURE_BYTES + 1024;
        let output = run_command(
            "/bin/sh",
            &[
                "-c".to_string(),
                format!("head -c {bytes} /dev/zero | tr '\\\\0' a"),
            ],
            &[],
            Path::new("."),
        )?;
        assert!(
            output.stdout.contains("[...truncated...]"),
            "stdout should include truncation marker"
        );
        assert!(
            output.stdout.len() <= DEFAULT_MAX_CAPTURE_BYTES + 64,
            "stdout should be bounded"
        );
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

    #[cfg(unix)]
    #[test]
    fn run_command_streaming_captures_output_unix() -> Result<()> {
        let output = run_command_streaming(
            "/bin/sh",
            &["-c".to_string(), "printf out && printf err >&2".to_string()],
            &[],
            Path::new("."),
        )?;
        assert_eq!(output.stdout, "out");
        assert_eq!(output.stderr, "err");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn run_command_removes_proxy_vars_when_empty() -> Result<()> {
        let script = r#"if [ -z "${HTTP_PROXY+x}" ] && [ -z "${NO_PROXY+x}" ]; then echo missing; else echo present; fi"#;
        let output = run_command(
            "/bin/sh",
            &["-c".to_string(), script.to_string()],
            &[
                ("HTTP_PROXY".into(), String::new()),
                ("NO_PROXY".into(), String::new()),
            ],
            Path::new("."),
        )?;
        assert_eq!(output.stdout.trim(), "missing");
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
