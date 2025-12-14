use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use hex;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::core::runtime::builder::BUILDER_VERSION;

use super::BuildMethod;

pub(crate) fn compute_build_options_hash(python_path: &str, method: BuildMethod) -> Result<String> {
    #[derive(Serialize)]
    struct BuildOptionsFingerprint {
        python: String,
        method: BuildMethod,
        env: BTreeMap<String, String>,
    }

    let python = match method {
        BuildMethod::BuilderWheel => {
            let version = super::builder::python_version(python_path)
                .unwrap_or_else(|_| "unknown".to_string());
            format!("builder-v{BUILDER_VERSION}-py{version}")
        }
        _ => fs::canonicalize(python_path)
            .unwrap_or_else(|_| PathBuf::from(python_path))
            .display()
            .to_string(),
    };
    let fingerprint = BuildOptionsFingerprint {
        python,
        method,
        env: build_env_fingerprint(),
    };
    let bytes = serde_json::to_vec(&fingerprint)?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

/// Compute the build options hash for a wheel-style build/install.
pub(crate) fn wheel_build_options_hash(python_path: &str) -> Result<String> {
    compute_build_options_hash(python_path, BuildMethod::PipWheel)
}

fn build_env_fingerprint() -> BTreeMap<String, String> {
    const BUILD_ENV_VARS: &[&str] = &[
        "ARCHFLAGS",
        "CFLAGS",
        "CPPFLAGS",
        "CXXFLAGS",
        "LDFLAGS",
        "MACOSX_DEPLOYMENT_TARGET",
        "PKG_CONFIG_PATH",
        "PIP_CONFIG_FILE",
        "PIP_DISABLE_PIP_VERSION_CHECK",
        "PIP_EXTRA_INDEX_URL",
        "PIP_FIND_LINKS",
        "PIP_INDEX_URL",
        "PIP_NO_BUILD_ISOLATION",
        "PIP_NO_CACHE_DIR",
        "PIP_PREFER_BINARY",
        "PIP_PROGRESS_BAR",
        "PYTHONDONTWRITEBYTECODE",
        "PYTHONHASHSEED",
        "PYTHONUTF8",
        "PYTHONWARNINGS",
        "SETUPTOOLS_USE_DISTUTILS",
        "SOURCE_DATE_EPOCH",
        "CARGO_BUILD_TARGET",
        "CARGO_HOME",
        "CARGO_TARGET_DIR",
        "MATURIN_BUILD_ARGS",
        "MATURIN_CARGO_FLAGS",
        "MATURIN_CARGO_PROFILE",
        "MATURIN_FEATURES",
        "MATURIN_PEP517_ARGS",
        "MATURIN_PEP517_FEATURES",
        "PYO3_CONFIG_FILE",
        "RUSTFLAGS",
        "RUSTUP_TOOLCHAIN",
    ];
    let mut env = BTreeMap::new();
    for key in BUILD_ENV_VARS {
        if let Ok(value) = std::env::var(key) {
            env.insert((*key).to_string(), value);
        }
    }
    env
}
