//! Runtime facade shared by `px run`, `px test`, and `px explain`.

mod cas_native;
mod context;
mod env_materialize;
mod errors;
mod execute;
mod plan;
mod sandbox;

#[cfg(test)]
mod tests;

pub(crate) const PX_VERSION: &str = env!("CARGO_PKG_VERSION");

pub(crate) type ManifestSnapshot = px_domain::api::ProjectSnapshot;

pub const MISSING_PROJECT_MESSAGE: &str = "No px project found.";
pub const MISSING_PROJECT_HINT: &str = "Run `px init` in your project directory first.";

pub use cas_native::ensure_env_matches_lock;
pub use context::CommandGroup;
pub use errors::{
    format_status_message, is_missing_project_error, manifest_error_outcome,
    missing_project_outcome, to_json_response,
};
pub use plan::{lock_is_fresh, marker_env_for_snapshot};

#[cfg(test)]
pub use env_materialize::materialize_project_site;

pub(crate) use cas_native::{ensure_project_environment_synced, validate_cas_environment};
pub(crate) use context::{
    attach_autosync_details, auto_sync_environment, build_pythonpath,
    ensure_environment_with_guard, ensure_version_file, issue_from_details, python_context,
    python_context_with_mode, EnvGuard, EnvironmentIssue, EnvironmentSyncReport, PythonContext,
};
pub(crate) use env_materialize::{
    detect_runtime_metadata, load_project_state, persist_project_state, prepare_project_runtime,
    refresh_project_site, select_python_from_site, site_packages_dir, write_project_metadata_stub,
    write_python_environment_markers, RuntimeMetadata, StoredEnvironment, StoredPython,
    StoredRuntime, SITE_CUSTOMIZE,
};
pub(crate) use execute::outcome_from_output;
pub(crate) use plan::{
    compute_lock_hash, compute_lock_hash_bytes, install_snapshot, manifest_snapshot,
    manifest_snapshot_at, persist_resolved_dependencies, relative_path_str,
    resolve_dependencies_with_effects, summarize_autopins, InstallOutcome, InstallState,
};
