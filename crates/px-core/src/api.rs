// Intended public API surface for `px-core`.
//
// This module exists to keep the crate root small and make it explicit which
// types/functions are part of the stable interface used by the CLI and other
// crates.

pub use crate::core::config::context::{CommandContext, CommandHandler, CommandInfo};
pub use crate::core::config::{
    CacheConfig, Config, GlobalOptions, NetworkConfig, PublishConfig, ResolverConfig, TestConfig,
};

pub use crate::core::distribution::{build_project, publish_project, BuildRequest, PublishRequest};
pub use crate::core::migration::{
    migrate, AutopinPreference, LockBehavior, MigrateRequest, MigrationMode, WorkspacePolicy,
};
pub use crate::core::project::{
    project_add, project_init, project_remove, project_status, project_sync, project_update,
    project_why, ProjectAddRequest, ProjectInitRequest, ProjectRemoveRequest, ProjectSyncRequest,
    ProjectUpdateRequest, ProjectWhyRequest,
};
pub use crate::core::python::python_cli::{
    python_info, python_install, python_list, python_use, PythonInfoRequest, PythonInstallRequest,
    PythonListRequest, PythonUseRequest,
};
pub use crate::core::runtime::effects::SystemEffects;
pub use crate::core::runtime::explain::{explain_entrypoint, explain_run};
pub use crate::core::runtime::fmt_runner::{run_fmt, FmtRequest};
pub use crate::core::runtime::process::RunOutput;
pub use crate::core::runtime::run::{run_project, test_project, RunRequest, TestRequest};
pub use crate::core::runtime::{
    format_status_message, is_missing_project_error, manifest_error_outcome,
    missing_project_outcome, run_target_completions, to_json_response, CommandGroup,
    RunTargetCompletions, RunTargetKind, RunTargetSuggestion, MISSING_PROJECT_HINT,
    MISSING_PROJECT_MESSAGE,
};
pub use crate::core::sandbox::{pack_app, pack_image, PackRequest, PackTarget};
pub use crate::core::status::{
    EnvHealth, EnvStatus, LockHealth, LockStatus, NextAction, NextActionKind, ProjectStatusPayload,
    RuntimeRole, RuntimeSource, RuntimeStatus, StatusContext, StatusContextKind, StatusPayload,
    WorkspaceMemberStatus, WorkspaceStatusPayload,
};
pub use crate::core::store::cas::{
    archive_dir_canonical, ensure_repo_snapshot, global_store, lookup_repo_snapshot_oid,
    materialize_repo_snapshot, pkg_build_lookup_key, repo_snapshot_lookup_key, source_lookup_key,
    ContentAddressableStore, DoctorSummary, GcSummary, LoadedObject, ObjectInfo, ObjectKind,
    ObjectPayload, OwnerId, OwnerType, RepoSnapshotHeader, RepoSnapshotSpec, StoreError,
    StoredObject,
};
pub use crate::core::tooling::diagnostics::commands as diag_commands;
pub use crate::core::tooling::outcome::{CommandStatus, ExecutionOutcome, InstallUserError};
pub use crate::core::tooling::progress;
pub use crate::core::tools::{
    tool_install, tool_list, tool_remove, tool_run, tool_upgrade, ToolInstallRequest,
    ToolListRequest, ToolRemoveRequest, ToolRunRequest, ToolUpgradeRequest,
};
pub use crate::core::workspace::{
    discover_workspace_scope, prepare_workspace_run_context, workspace_add, workspace_remove,
    workspace_status, workspace_sync, workspace_update, WorkspaceScope, WorkspaceSyncRequest,
};
