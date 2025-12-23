//! Runtime context assembly for run/test/explain.

mod env_sync;
mod python_context;
mod pythonpath;
mod types;
mod version_file;

pub use types::CommandGroup;

pub(crate) use env_sync::{
    attach_autosync_details, auto_sync_environment, ensure_environment_with_guard,
};
pub(crate) use python_context::python_context_with_mode;
pub(crate) use pythonpath::build_pythonpath;
pub(crate) use types::{
    issue_from_details, EnvGuard, EnvironmentIssue, EnvironmentSyncReport, PythonContext,
};
pub(crate) use version_file::ensure_version_file;

pub(super) use pythonpath::detect_local_site_packages;
pub(super) use version_file::{
    derive_vcs_version, hatch_drops_local_version, hatch_git_describe_command,
    hatch_prefers_simplified_semver, hatch_version_file, pdm_version_file,
    setuptools_scm_version_file, uses_hatch_vcs, VersionDeriveOptions,
};

#[cfg(test)]
pub(super) use version_file::pep440_from_describe;
