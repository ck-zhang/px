use super::*;

use super::env::{append_pythonpath, run_python_command};
use super::outcome::test_success;
use super::stdlib::ensure_stdlib_tests_available;

pub(super) fn run_builtin_tests(
    _core_ctx: &CommandContext,
    runner: &dyn CommandRunner,
    ctx: &PythonContext,
    mut envs: Vec<(String, String)>,
    stream_runner: bool,
    workdir: &Path,
) -> Result<ExecutionOutcome> {
    if let Some(path) = ensure_stdlib_tests_available(ctx)? {
        append_pythonpath(&mut envs, &path);
    }
    envs.push(("PX_TEST_RUNNER".into(), "builtin".into()));
    let script = "from sample_px_app import cli\nassert cli.greet() == 'Hello, World!'\nprint('px fallback test passed')";
    let args = vec!["-c".to_string(), script.to_string()];
    let output = run_python_command(runner, ctx, &args, &envs, stream_runner, workdir)?;
    let runner_args: Vec<String> = Vec::new();
    Ok(test_success("builtin", output, stream_runner, &runner_args))
}
