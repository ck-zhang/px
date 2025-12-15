//! OCI layer tar creation helpers for `px pack`.
//!
//! Mapping note (for reviewers):
//! - Old: `core/sandbox/pack/layers.rs`
//! - New:
//!   - env layer + path rewriting: `layers/env.rs`
//!   - app layer (source tree): `layers/app.rs`
//!   - system-deps rootfs layer: `layers/system_deps.rs`
//!   - base OS export layer: `layers/base_os.rs`
//!   - deterministic tar writing helpers: `layers/tar.rs`

mod app;
mod base_os;
mod env;
mod system_deps;
mod tar;

pub(crate) use base_os::write_base_os_layer;
pub(crate) use env::write_env_layer_tar;
pub(crate) use system_deps::write_system_deps_layer;

use std::path::Path;

use anyhow::Result;

use super::LayerTar;
use crate::InstallUserError;

pub(super) fn write_app_layer_tar(
    source_root: &Path,
    blobs: &Path,
) -> Result<LayerTar, InstallUserError> {
    app::write_app_layer_tar(source_root, blobs)
}
