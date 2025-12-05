pub mod init;
pub mod manifest;
pub mod onboard;
pub mod snapshot;
pub mod state;

pub use init::{infer_package_name, sanitize_package_candidate, ProjectInitializer};
pub use manifest::{
    collect_pyproject_packages, collect_requirement_packages, collect_setup_cfg_packages,
    read_requirements_file, read_setup_cfg_requires, DependencyGroupSource, ManifestAddReport,
    ManifestEditor, ManifestRemoveReport, OnboardPackagePlan, PxOptions,
};
pub use onboard::{
    prepare_pyproject_plan, resolve_onboard_path, BackupManager, BackupSummary, PyprojectPlan,
};
pub use snapshot::{
    current_project_root, discover_project_root, ensure_pyproject_exists, missing_project_guidance,
    project_name_from_pyproject, MissingProjectGuidance, ProjectSnapshot,
};
pub use state::{canonical_state, ProjectStateKind, ProjectStateReport};
