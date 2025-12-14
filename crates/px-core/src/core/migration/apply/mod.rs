// Mapping note: `apply.rs` was split for reviewability:
// - `types.rs`: request/config enums + request struct
// - `migrate.rs`: migrate entrypoint + main flow
// - `foreign_tools.rs`: foreign tool detection helpers
// - `locked_versions.rs`: uv/poetry lock pin reuse helpers

mod foreign_tools;
mod locked_versions;
mod migrate;
mod types;

pub use migrate::migrate;
pub use types::{AutopinPreference, LockBehavior, MigrateRequest, MigrationMode, WorkspacePolicy};
