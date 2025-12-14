// Mapping note: the former `cas_env.rs` mega-module was split into focused files:
// - `owners.rs`: env root + owner id helpers
// - `fs_tree.rs`: filesystem tree copy + permission helpers
// - `scripts.rs`: python shim + entrypoint script helpers
// - `runtime.rs`: runtime header/archive helpers
// - `materialize.rs`: CAS materialization (runtime/pkg-build/profile env)
// - `profile.rs`: CAS profile assembly + dependency staging
// - `tests.rs`: unit tests previously inline in `cas_env.rs`

mod fs_tree;
mod materialize;
mod owners;
mod profile;
mod runtime;
mod scripts;

#[cfg(test)]
mod tests;

pub(crate) use fs_tree::copy_tree;
pub(crate) use materialize::materialize_pkg_archive;
pub(crate) use owners::{default_envs_root, project_env_owner_id, workspace_env_owner_id};
pub(crate) use profile::{ensure_profile_env, ensure_profile_manifest};
pub(crate) use scripts::write_python_shim;
