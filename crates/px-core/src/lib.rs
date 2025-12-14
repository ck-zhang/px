#![deny(clippy::all, warnings)]

mod core;

pub(crate) use px_domain::api::{discover_project_root, InstallOverride};

pub mod api;

pub(crate) mod workspace {
    pub(crate) use crate::core::workspace::*;
}

pub(crate) use crate::core::config;
pub(crate) use crate::core::config::{context, state_guard};
pub(crate) use crate::core::python::{python_build, python_sys};
pub(crate) use crate::core::runtime::*;
pub(crate) use crate::core::runtime::{
    effects, fmt_plan, process, run_plan, runtime_manager, traceback,
};
pub(crate) use crate::core::store;
pub(crate) use crate::core::store::cas::{
    ensure_repo_snapshot, lookup_repo_snapshot_oid, materialize_repo_snapshot, LoadedObject,
    OwnerId, OwnerType, RepoSnapshotHeader, RepoSnapshotSpec, StoreError,
};
pub(crate) use crate::core::store::pypi;
pub(crate) use crate::core::tooling;
pub(crate) use crate::core::tooling::progress;
pub(crate) use crate::core::tooling::{diagnostics, outcome};
pub(crate) use crate::core::{project, tools};

pub(crate) use crate::core::config::context::CommandContext;
pub(crate) use crate::core::runtime::process::RunOutput;
pub(crate) use crate::core::status::{
    EnvHealth, EnvStatus, LockHealth, LockStatus, NextAction, NextActionKind, ProjectStatusPayload,
    RuntimeRole, RuntimeSource, RuntimeStatus, StatusContext, StatusContextKind, StatusPayload,
    WorkspaceMemberStatus, WorkspaceStatusPayload,
};
pub(crate) use crate::core::tooling::diagnostics::commands as diag_commands;
pub(crate) use crate::core::tooling::outcome::{CommandStatus, ExecutionOutcome, InstallUserError};

pub(crate) use crate::core::runtime::PX_VERSION;
