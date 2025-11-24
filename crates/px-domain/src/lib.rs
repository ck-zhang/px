#![deny(clippy::all, warnings)]
#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate
)]

pub mod lockfile;
pub mod project;
pub mod resolution;
pub mod workspace;

pub use lockfile::{
    analyze_lock_diff, canonical_extras, collect_resolved_dependencies, detect_lock_drift,
    format_specifier, load_lockfile_optional, render_lockfile, render_lockfile_with_workspace,
    verify_locked_artifacts, LockSnapshot, LockedArtifact, ResolvedDependency, WorkspaceLock,
    WorkspaceMember, WorkspaceOwner,
};
pub use project::{
    collect_pyproject_packages, collect_requirement_packages, current_project_root,
    discover_project_root, ensure_pyproject_exists, infer_package_name, prepare_pyproject_plan,
    project_name_from_pyproject, read_requirements_file, resolve_onboard_path,
    sanitize_package_candidate, BackupManager, BackupSummary, ManifestAddReport, ManifestEditor,
    ManifestRemoveReport, OnboardPackagePlan, ProjectInitializer, ProjectSnapshot,
    ProjectStateKind, ProjectStateReport, PyprojectPlan,
};
pub use resolution::{
    autopin_pin_key, autopin_spec_key, marker_applies, merge_resolved_dependencies,
    normalize_dist_name, plan_autopin, resolve, spec_requires_pin, AutopinEntry, AutopinPending,
    AutopinPlan, AutopinScope, AutopinState, InstallOverride, PinSpec, ResolvedSpecOutput,
    ResolvedSpecifier, ResolverEnv, ResolverRequest, ResolverTags,
};
pub use workspace::{
    discover_workspace_root, read_workspace_config, workspace_manifest_fingerprint,
    workspace_member_for_path, WorkspaceConfig,
};
