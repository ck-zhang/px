//! Build and cache wheels from sdists.
//!
//! Mapping note (for reviewers):
//! - Old: `core/store/sdist.rs`
//! - New:
//!   - entrypoint + `BuildMethod`: `sdist/mod.rs`
//!   - build orchestration + cache match: `sdist/ensure.rs`
//!   - container builder glue: `sdist/builder.rs`
//!   - downloads + cross-device persistence: `sdist/download.rs`
//!   - native library scanning/copy: `sdist/native_libs.rs`
//!   - build options hashing: `sdist/hash.rs`
//!   - wheel discovery: `sdist/wheel.rs`

mod builder;
mod download;
mod ensure;
mod hash;
mod native_libs;
mod wheel;

#[cfg(test)]
mod tests;

use serde::{Deserialize, Serialize};

pub use ensure::ensure_sdist_build;
pub(crate) use hash::{compute_build_options_hash, wheel_build_options_hash};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum BuildMethod {
    PipWheel,
    PythonBuild,
    BuilderWheel,
}

impl Default for BuildMethod {
    fn default() -> Self {
        Self::PipWheel
    }
}

fn sanitize_builder_id(builder_id: &str) -> String {
    builder_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}
