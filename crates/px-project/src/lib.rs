#![allow(dead_code)]

pub mod autopin;
pub mod init;
pub mod manifest;
pub mod onboard;
pub mod resolver;
pub mod snapshot;

pub use autopin::{
    plan_autopin, AutopinEntry, AutopinPending, AutopinPlan, AutopinScope, AutopinState,
};
pub use init::{infer_package_name, sanitize_package_candidate, ProjectInitializer};
pub use manifest::{
    collect_pyproject_packages, collect_requirement_packages, read_requirements_file,
    ManifestAddReport, ManifestEditor, ManifestRemoveReport, OnboardPackagePlan,
};
pub use onboard::{
    prepare_pyproject_plan, resolve_onboard_path, BackupManager, BackupSummary, PyprojectPlan,
};
pub use resolver::{current_marker_environment, InstallOverride, PinSpec, ResolvedSpecOutput};
pub use snapshot::{
    current_project_root, ensure_pyproject_exists, project_name_from_pyproject, ProjectSnapshot,
};
