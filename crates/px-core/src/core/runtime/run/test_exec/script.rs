use super::*;

use super::env::run_python_command;
use super::outcome::{test_failure, test_success};

#[allow(clippy::too_many_arguments)]
pub(super) fn run_script_runner(
    _ctx: &CommandContext,
    runner: &dyn CommandRunner,
    py_ctx: &PythonContext,
    mut envs: EnvPairs,
    script: &Path,
    args: &[String],
    stream_runner: bool,
    workdir: &Path,
) -> Result<ExecutionOutcome> {
    let runner_label = script
        .strip_prefix(&py_ctx.project_root)
        .unwrap_or(script)
        .display()
        .to_string();
    envs.push(("PX_TEST_RUNNER".into(), runner_label.clone()));
    let mut cmd_args = vec![script.display().to_string()];
    cmd_args.extend_from_slice(args);
    let output = run_python_command(runner, py_ctx, &cmd_args, &envs, stream_runner, workdir)?;
    if output.code == 0 {
        Ok(test_success(&runner_label, output, stream_runner, args))
    } else {
        Ok(test_failure(&runner_label, output, stream_runner, args))
    }
}
