

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

pub fn run_command(
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
    command.stdin(Stdio::null());
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
