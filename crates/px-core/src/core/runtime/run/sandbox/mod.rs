// Mapping note: `sandbox.rs` was split for navigability:
// - `runner.rs`: container runner + argv/env path rewriting
// - `prepare.rs`: sandbox image/store preparation + outcome detail helpers

mod prepare;
mod runner;

pub(crate) use prepare::SandboxRunContext;
pub(crate) use runner::{sandbox_runner_for_context, SandboxCommandRunner};

pub(super) use prepare::{
    attach_sandbox_details, prepare_commit_sandbox, prepare_project_sandbox,
    prepare_workspace_sandbox, sandbox_workspace_env_inconsistent,
};
#[cfg(test)]
pub(super) use runner::map_program_for_container;
