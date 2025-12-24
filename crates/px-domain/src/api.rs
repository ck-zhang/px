pub use crate::lockfile::analysis::{
    analyze_lock_diff, collect_resolved_dependencies, detect_lock_drift, validate_lock_closure,
    verify_locked_artifacts,
};
pub use crate::lockfile::io::{
    load_lockfile, load_lockfile_optional, parse_lockfile, render_lockfile,
    render_lockfile_with_workspace,
};
pub use crate::lockfile::spec::{canonical_extras, format_specifier};
pub use crate::lockfile::types::{
    LockSnapshot, LockedArtifact, LockedDependency, ResolvedDependency, WorkspaceLock,
    WorkspaceMember, WorkspaceOwner,
};

pub use crate::project::init::{
    infer_package_name, sanitize_package_candidate, ProjectInitializer,
};
pub use crate::project::manifest::{
    canonicalize_package_name, canonicalize_spec, collect_pyproject_packages,
    collect_requirement_packages, collect_setup_cfg_packages, collect_setup_py_packages,
    manifest_fingerprint, px_options_from_doc, read_requirements_file, read_setup_cfg_requires,
    read_setup_py_requires, sandbox_config_from_doc, sandbox_config_from_manifest,
    DependencyGroupSource, ManifestAddReport, ManifestEditor, ManifestRemoveReport,
    OnboardPackagePlan, PxOptions, SandboxConfig,
};
pub use crate::project::onboard::{
    prepare_pyproject_plan, resolve_onboard_path, BackupManager, BackupSummary, PyprojectPlan,
};
pub use crate::project::snapshot::{
    current_project_root, discover_project_root, ensure_pyproject_exists, missing_project_guidance,
    project_name_from_pyproject, MissingProjectGuidance, ProjectSnapshot,
};
pub use crate::project::state::{ProjectStateKind, ProjectStateReport};

pub use crate::resolution::autopin::{
    plan_autopin, plan_autopin_document, AutopinEntry, AutopinPending, AutopinPlan, AutopinScope,
    AutopinState,
};
pub use crate::resolution::project_resolver::{
    autopin_pin_key, autopin_spec_key, marker_applies, merge_resolved_dependencies,
    spec_requires_pin, InstallOverride, PinSpec, ResolvedSpecOutput,
};
pub use crate::resolution::resolver::{
    normalize_dist_name, resolve, ResolveRequest as ResolverRequest, ResolvedDistSource,
    ResolvedSpecifier, ResolverEnv, ResolverTags,
};
pub use crate::workspace::{
    discover_workspace_root, manifest_has_workspace, read_workspace_config,
    workspace_config_from_doc, workspace_manifest_fingerprint, workspace_member_for_path,
    WorkspaceConfig,
};
