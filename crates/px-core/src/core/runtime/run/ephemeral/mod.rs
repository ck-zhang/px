//! Implementation for `px run --ephemeral` / `px test --ephemeral`.
//!
//! Ephemeral runs build a cache-rooted snapshot and execute from the user's
//! directory without writing `.px/` or `px.lock` into the working tree.

use super::*;

mod execute;
mod input;
mod python_context;
mod snapshot;

const EPHEMERAL_PROJECT_NAME: &str = "px-ephemeral";
const DEFAULT_EPHEMERAL_REQUIRES_PYTHON: &str = ">=3.8";

#[derive(Clone, Debug)]
enum EphemeralInput {
    InlineScript {
        requires_python: String,
        deps: Vec<String>,
    },
    Pyproject {
        requires_python: String,
        deps: Vec<String>,
    },
    Requirements {
        deps: Vec<String>,
    },
    Empty,
}

pub(super) fn run_ephemeral_outcome(
    ctx: &CommandContext,
    request: &RunRequest,
    target: &str,
    interactive: bool,
    strict: bool,
) -> Result<ExecutionOutcome> {
    execute::run_ephemeral_outcome(ctx, request, target, interactive, strict)
}

pub(super) fn test_ephemeral_outcome(
    ctx: &CommandContext,
    request: &TestRequest,
    strict: bool,
) -> Result<ExecutionOutcome> {
    execute::test_ephemeral_outcome(ctx, request, strict)
}
