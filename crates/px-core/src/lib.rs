#![deny(clippy::all, warnings)]

mod core;

pub(crate) use px_domain::{discover_project_root, InstallOverride};

pub mod workspace {
    pub use crate::core::workspace::*;
}

pub(crate) use crate::core::config;
pub(crate) use crate::core::config::{context, state_guard};
pub(crate) use crate::core::python::{python_build, python_sys};
pub(crate) use crate::core::runtime::*;
pub(crate) use crate::core::runtime::{
    effects, fmt_plan, process, run_plan, runtime_manager, traceback,
};
pub(crate) use crate::core::store;
pub(crate) use crate::core::store::pypi;
pub(crate) use crate::core::tooling;
pub(crate) use crate::core::tooling::{diagnostics, outcome, progress};
pub(crate) use crate::core::{project, tools};

pub use crate::core::config::context::{CommandContext, CommandHandler, CommandInfo};
pub use crate::core::config::{
    CacheConfig, Config, GlobalOptions, NetworkConfig, PublishConfig, ResolverConfig, TestConfig,
};
pub use crate::core::runtime::effects::SystemEffects;
pub use crate::core::runtime::process::RunOutput;
pub use crate::core::runtime::CommandGroup;
pub use crate::core::tooling::diagnostics::commands as diag_commands;
pub use crate::core::tooling::outcome::{CommandStatus, ExecutionOutcome, InstallUserError};

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
pub use crate::core::runtime::fmt_runner::{run_fmt, FmtRequest};
pub use crate::core::runtime::run::{run_project, test_project, RunRequest, TestRequest};
pub use crate::core::tools::{
    tool_install, tool_list, tool_remove, tool_run, tool_upgrade, ToolInstallRequest,
    ToolListRequest, ToolRemoveRequest, ToolRunRequest, ToolUpgradeRequest,
};
pub use crate::core::workspace::{
    discover_workspace_scope, prepare_workspace_run_context, workspace_add, workspace_remove,
    workspace_status, workspace_sync, workspace_update, WorkspaceScope, WorkspaceSyncRequest,
};

pub(crate) use crate::core::runtime::PX_VERSION;
pub use crate::core::runtime::{
    format_status_message, is_missing_project_error, manifest_error_outcome,
    missing_project_outcome, to_json_response,
};

pub const MISSING_PROJECT_MESSAGE: &str = crate::core::runtime::MISSING_PROJECT_MESSAGE;
pub const MISSING_PROJECT_HINT: &str = crate::core::runtime::MISSING_PROJECT_HINT;
