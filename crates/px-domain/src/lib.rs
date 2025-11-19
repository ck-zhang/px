#![allow(dead_code)]
#![deny(clippy::all, warnings)]
#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate
)]

pub mod autopin;
pub mod init;
pub mod lockfile;
pub mod manifest;
pub mod onboard;
pub mod project_resolver;
pub mod resolver;
pub mod runtime;
pub mod snapshot;

pub use autopin::{
    plan_autopin, AutopinEntry, AutopinPending, AutopinPlan, AutopinScope, AutopinState,
};
pub use init::{infer_package_name, sanitize_package_candidate, ProjectInitializer};
pub use lockfile::{
    analyze_lock_diff, canonical_extras, collect_resolved_dependencies, detect_lock_drift,
    format_specifier, load_lockfile_optional, render_lockfile, verify_locked_artifacts,
    LockSnapshot, LockedArtifact, ResolvedDependency,
};
pub use manifest::{
    collect_pyproject_packages, collect_requirement_packages, read_requirements_file,
    ManifestAddReport, ManifestEditor, ManifestRemoveReport, OnboardPackagePlan,
};
pub use onboard::{
    prepare_pyproject_plan, resolve_onboard_path, BackupManager, BackupSummary, PyprojectPlan,
};
pub use project_resolver::{
    autopin_pin_key, autopin_spec_key, marker_applies, merge_resolved_dependencies,
    spec_requires_pin, InstallOverride, PinSpec, ResolvedSpecOutput,
};
pub use resolver::{
    normalize_dist_name, resolve, ResolveRequest as ResolverRequest, ResolvedSpecifier,
    ResolverEnv, ResolverTags,
};
pub use runtime::{run_command, RunOutput};
pub use snapshot::{
    current_project_root, discover_project_root, ensure_pyproject_exists,
    project_name_from_pyproject, ProjectSnapshot,
};
