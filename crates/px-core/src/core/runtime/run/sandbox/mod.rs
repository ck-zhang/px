//! Sandbox runner glue for `px run` / `px test`.

mod prepare;
mod runner;

pub(crate) use prepare::SandboxRunContext;
pub(crate) use runner::{sandbox_runner_for_context, SandboxCommandRunner};

pub(super) use prepare::{
    attach_sandbox_details, prepare_commit_sandbox, prepare_project_sandbox,
    prepare_workspace_sandbox, sandbox_workspace_env_inconsistent,
};
#[cfg(all(test, unix))]
pub(super) use runner::map_program_for_container;
