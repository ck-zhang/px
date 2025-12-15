//! Runtime and execution engine (`px run`, `px test`, `px explain`).
//!
//! Most higher-level flows go through the `facade` re-exports.

mod artifacts;
mod builder;
mod cas_env;
mod effects;
mod execution_plan;
mod explain;
mod fmt_plan;
mod fmt_runner;
mod process;
mod run;
mod run_completion;
mod run_plan;
pub(crate) mod runtime_manager;
mod script;
mod traceback;

mod facade;

pub(crate) use artifacts::{
    build_http_client, dependency_name, fetch_release, resolve_pins, strip_wrapping_quotes,
};
pub(crate) use builder::BUILDER_VERSION;
#[cfg(test)]
pub(crate) use cas_env::default_envs_root;
pub(crate) use cas_env::{ensure_profile_env, workspace_env_owner_id};
pub use effects::SystemEffects;
pub(crate) use effects::{
    CacheStore, Effects, FileSystem, GitClient, PypiClient, PythonRuntime, SharedEffects,
};
pub use explain::{explain_entrypoint, explain_run};
pub use facade::*;
pub use fmt_runner::{run_fmt, FmtRequest};
pub use process::RunOutput;
pub(crate) use process::{
    run_command, run_command_passthrough, run_command_streaming, run_command_with_stdin,
};
pub use run::{run_project, test_project, RunRequest, TestRequest};
pub use run_completion::*;

#[cfg(test)]
mod run_plan_tests;
#[cfg(test)]
mod tests;
