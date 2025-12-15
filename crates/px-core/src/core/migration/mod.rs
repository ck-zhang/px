//! Adoption/migration flows for existing projects and workspaces (`px migrate`).

mod apply;
mod plan;
mod runtime;

pub use apply::{
    migrate, AutopinPreference, LockBehavior, MigrateRequest, MigrationMode, WorkspacePolicy,
};
