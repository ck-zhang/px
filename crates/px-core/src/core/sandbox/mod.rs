//! Sandbox integration and `px pack` implementation.

mod app_bundle;
mod pack;
mod pxapp;
mod runner;

mod errors;
mod image;
mod paths;
mod resolve;
mod store;
mod system_deps;
mod time;
mod types;

#[cfg(test)]
mod tests;

pub use pack::{pack_app, pack_image, PackRequest, PackTarget};
pub(crate) use pxapp::run_pxapp_bundle;
pub(crate) use runner::{
    detect_container_backend, ensure_image_layout, run_container, ContainerBackend,
    ContainerRunArgs, Mount, RunMode, SandboxImageLayout,
};

pub(crate) use errors::sandbox_error;
pub(crate) use image::ensure_sandbox_image;
pub(crate) use paths::{
    default_store_root, discover_site_packages, env_root_from_site_packages, sandbox_image_tag,
};
pub(crate) use resolve::resolve_sandbox_definition;
pub(crate) use store::SandboxStore;
pub(crate) use system_deps::{
    base_apt_opts, ensure_system_deps_rootfs, internal_apt_mirror_env_overrides,
    internal_apt_mirror_setup_snippet, internal_keep_proxies, internal_proxy_env_overrides,
    pin_system_deps, should_disable_apt_proxy, system_deps_mode, SystemDepsMode, SYSTEM_DEPS_IMAGE,
};
pub(crate) use time::sandbox_timestamp_string;
pub(crate) use types::{
    SandboxArtifacts, SandboxDefinition, SandboxImageManifest, PXAPP_VERSION, SBX_VERSION,
};
