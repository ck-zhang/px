// Workspace module split (refactor-only; no behavior changes):
// - Shared types -> types.rs
// - Scope discovery -> discovery.rs
// - Workspace snapshot loading -> snapshot.rs
// - State persistence + evaluation -> state.rs
// - Lock/env syncing -> sync.rs
// - Status payload assembly -> status.rs
// - Member operations (add/remove/update) -> member_ops.rs
// - Runtime/run context wiring -> run_context.rs

mod discovery;
mod member_ops;
mod run_context;
mod snapshot;
mod state;
mod status;
mod sync;
mod types;

#[cfg(test)]
mod tests;

pub use discovery::discover_workspace_scope;
pub use member_ops::{workspace_add, workspace_remove, workspace_update};
pub use run_context::prepare_workspace_run_context;
pub use status::workspace_status;
pub use sync::workspace_sync;
pub use types::{
    WorkspaceMember, WorkspaceRunContext, WorkspaceScope, WorkspaceSnapshot, WorkspaceStateKind,
    WorkspaceStateReport, WorkspaceSyncRequest,
};

pub(crate) use snapshot::{derive_workspace_python, load_workspace_snapshot};
pub(crate) use state::{
    evaluate_workspace_state, load_workspace_state, workspace_violation, StateViolation,
};
pub(crate) use status::workspace_status_payload;
