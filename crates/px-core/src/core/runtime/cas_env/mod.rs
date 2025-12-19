//! CAS-backed environment materialization helpers.

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
#[cfg(not(windows))]
pub(crate) use scripts::write_python_shim;
