use std::path::Path;

use anyhow::Result;

use crate::CommandContext;

pub(crate) trait CommandRunner {
    fn run_command(
        &self,
        program: &str,
        args: &[String],
        envs: &[(String, String)],
        cwd: &Path,
    ) -> Result<crate::RunOutput>;

    fn run_command_streaming(
        &self,
        program: &str,
        args: &[String],
        envs: &[(String, String)],
        cwd: &Path,
    ) -> Result<crate::RunOutput>;

    fn run_command_with_stdin(
        &self,
        program: &str,
        args: &[String],
        envs: &[(String, String)],
        cwd: &Path,
        inherit_stdin: bool,
    ) -> Result<crate::RunOutput>;

    fn run_command_passthrough(
        &self,
        program: &str,
        args: &[String],
        envs: &[(String, String)],
        cwd: &Path,
    ) -> Result<crate::RunOutput>;
}

#[derive(Clone, Copy)]
pub(crate) struct HostCommandRunner<'a> {
    ctx: &'a CommandContext<'a>,
}

impl<'a> HostCommandRunner<'a> {
    pub(crate) fn new(ctx: &'a CommandContext<'a>) -> Self {
        Self { ctx }
    }
}

impl CommandRunner for HostCommandRunner<'_> {
    fn run_command(
        &self,
        program: &str,
        args: &[String],
        envs: &[(String, String)],
        cwd: &Path,
    ) -> Result<crate::RunOutput> {
        self.ctx
            .python_runtime()
            .run_command(program, args, envs, cwd)
    }

    fn run_command_streaming(
        &self,
        program: &str,
        args: &[String],
        envs: &[(String, String)],
        cwd: &Path,
    ) -> Result<crate::RunOutput> {
        self.ctx
            .python_runtime()
            .run_command_streaming(program, args, envs, cwd)
    }

    fn run_command_with_stdin(
        &self,
        program: &str,
        args: &[String],
        envs: &[(String, String)],
        cwd: &Path,
        inherit_stdin: bool,
    ) -> Result<crate::RunOutput> {
        self.ctx
            .python_runtime()
            .run_command_with_stdin(program, args, envs, cwd, inherit_stdin)
    }

    fn run_command_passthrough(
        &self,
        program: &str,
        args: &[String],
        envs: &[(String, String)],
        cwd: &Path,
    ) -> Result<crate::RunOutput> {
        self.ctx
            .python_runtime()
            .run_command_passthrough(program, args, envs, cwd)
    }
}
