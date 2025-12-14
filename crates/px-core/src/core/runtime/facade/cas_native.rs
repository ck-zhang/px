use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::context::CommandContext;
use crate::core::runtime::cas_env::default_envs_root;
use crate::core::tooling::diagnostics;
use crate::outcome::InstallUserError;
use crate::python_sys::detect_marker_environment;
use crate::store::cas::{global_store, LoadedObject, ProfilePackage, MATERIALIZED_PKG_BUILDS_DIR};
use anyhow::Result;
use px_domain::api::{detect_lock_drift, load_lockfile_optional, verify_locked_artifacts};
use serde::Deserialize;
use serde_json::json;

use super::env_materialize::{
    ensure_uv_seed, has_uv_cli, module_available, project_site_env, uv_seed_required,
};
use super::{
    compute_lock_hash, detect_runtime_metadata, prepare_project_runtime, ManifestSnapshot,
};
use super::{load_project_state, StoredEnvironment};

pub(crate) fn ensure_project_environment_synced(
    ctx: &CommandContext,
    snapshot: &ManifestSnapshot,
) -> Result<()> {
    if !snapshot.manifest_path.exists() {
        return Err(InstallUserError::new(
            format!("pyproject.toml not found in {}", snapshot.root.display()),
            json!({
                "hint": "run `px migrate --apply` or pass ENTRY explicitly",
                "project_root": snapshot.root.display().to_string(),
                "manifest": snapshot.manifest_path.display().to_string(),
                "reason": "missing_manifest",
            }),
        )
        .into());
    }
    let lock_path = snapshot.lock_path.clone();
    let Some(lock) = load_lockfile_optional(&lock_path)? else {
        return Err(InstallUserError::new(
            "missing px.lock (run `px sync`)",
            json!({
                "lockfile": lock_path.display().to_string(),
                "hint": "run `px sync` to generate px.lock before running this command",
                "reason": "missing_lock",
            }),
        )
        .into());
    };

    let runtime = prepare_project_runtime(snapshot)?;
    let marker_env = detect_marker_environment(&runtime.record.path)?.to_marker_environment()?;

    let drift = detect_lock_drift(snapshot, &lock, Some(&marker_env));
    if !drift.is_empty() {
        return Err(InstallUserError::new(
            "px.lock is out of date",
            json!({
                "lockfile": lock_path.display().to_string(),
                "drift": drift,
                "hint": "run `px sync` to refresh px.lock",
                "reason": "lock_drift",
            }),
        )
        .into());
    }

    let missing = verify_locked_artifacts(&lock);
    if !missing.is_empty() {
        return Err(InstallUserError::new(
            "cached artifacts missing",
            json!({
                "lockfile": lock_path.display().to_string(),
                "missing": missing,
                "hint": "run `px sync` to rehydrate the environment",
                "reason": "missing_artifacts",
            }),
        )
        .into());
    }

    let lock_id = match lock.lock_id.clone() {
        Some(value) => value,
        None => compute_lock_hash(&lock_path)?,
    };
    ensure_env_matches_lock(ctx, snapshot, &lock_id)
}

#[derive(Deserialize)]
struct EnvManifest {
    profile_oid: String,
    runtime_oid: String,
    packages: Vec<ProfilePackage>,
    #[serde(default)]
    sys_path_order: Vec<String>,
}

fn expected_pxpth_entries(manifest: &EnvManifest, store_root: &Path) -> Vec<PathBuf> {
    let mut expected = Vec::new();
    let mut seen = HashSet::new();
    let pkg_path = |oid: &str| {
        let base = store_root.join(MATERIALIZED_PKG_BUILDS_DIR).join(oid);
        let site = base.join("site-packages");
        if site.exists() {
            site
        } else {
            base
        }
    };
    let ordered: Vec<String> = if manifest.sys_path_order.is_empty() {
        manifest
            .packages
            .iter()
            .map(|pkg| pkg.pkg_build_oid.clone())
            .collect()
    } else {
        manifest.sys_path_order.clone()
    };
    for oid in ordered {
        if seen.insert(oid.clone()) {
            expected.push(pkg_path(&oid));
        }
    }
    for pkg in &manifest.packages {
        if seen.insert(pkg.pkg_build_oid.clone()) {
            expected.push(pkg_path(&pkg.pkg_build_oid));
        }
    }
    expected
}

fn matches_versioned_entry(name: &str, base: &str) -> bool {
    let Some(mut suffix) = name.strip_prefix(base) else {
        return false;
    };
    if suffix.is_empty() {
        return true;
    }
    if let Some(rest) = suffix.strip_prefix('-') {
        suffix = rest;
    }
    if let Some(rest) = suffix
        .strip_suffix(".dist-info")
        .or_else(|| suffix.strip_suffix(".data"))
        .or_else(|| suffix.strip_suffix(".egg-info"))
    {
        suffix = rest;
    }
    suffix
        .chars()
        .all(|ch| ch.is_ascii_digit() || ch == '.' || ch == '-')
}

fn is_allowed_site_entry(name: &str) -> bool {
    matches!(
        name,
        "px.pth"
            | "sitecustomize.py"
            | "__pycache__"
            | "distutils-precedence.pth"
            | "pkg_resources"
    ) || matches_versioned_entry(name, "pip")
        || matches_versioned_entry(name, "setuptools")
        || matches_versioned_entry(name, "_distutils_hack")
        || matches_versioned_entry(name, "pipx")
        || matches_versioned_entry(name, "uv")
}

fn validate_env_site_packages(
    site_packages: &Path,
    manifest: &EnvManifest,
    store_root: &Path,
) -> Result<(), InstallUserError> {
    let entries = fs::read_dir(site_packages).map_err(|err| {
        InstallUserError::new(
            "unable to read environment site-packages",
            json!({
                "site": site_packages.display().to_string(),
                "error": err.to_string(),
                "reason": "missing_env",
                "code": diagnostics::cas::MISSING_OR_CORRUPT,
                "hint": "run `px sync` to refresh the environment",
            }),
        )
    })?;
    let mut unexpected = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !is_allowed_site_entry(name) {
            unexpected.push(name.to_string());
        }
    }
    if !unexpected.is_empty() {
        return Err(InstallUserError::new(
            "environment site-packages drifted from CAS profile",
            json!({
                "site": site_packages.display().to_string(),
                "unexpected": unexpected,
                "reason": "env_outdated",
                "code": diagnostics::cas::MISSING_OR_CORRUPT,
                "hint": "run `px sync` to refresh the environment",
            }),
        ));
    }

    let pth_path = site_packages.join("px.pth");
    let contents = fs::read_to_string(&pth_path).map_err(|err| {
        InstallUserError::new(
            "environment px.pth missing or unreadable",
            json!({
                "site": site_packages.display().to_string(),
                "pth": pth_path.display().to_string(),
                "error": err.to_string(),
                "reason": "missing_env",
                "code": diagnostics::cas::MISSING_OR_CORRUPT,
                "hint": "run `px sync` to refresh the environment",
            }),
        )
    })?;
    let actual_paths: HashSet<PathBuf> = contents
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(PathBuf::from(trimmed))
            }
        })
        .collect();
    let expected_paths = expected_pxpth_entries(manifest, store_root);
    let expected_set: HashSet<PathBuf> = expected_paths.iter().cloned().collect();
    if actual_paths != expected_set {
        return Err(InstallUserError::new(
            "environment px.pth drifted from CAS profile",
            json!({
                "site": site_packages.display().to_string(),
                "expected_px_pth": expected_paths
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>(),
                "px_pth": actual_paths
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>(),
                "reason": "env_outdated",
                "code": diagnostics::cas::MISSING_OR_CORRUPT,
                "hint": "run `px sync` to refresh the environment",
            }),
        ));
    }

    Ok(())
}

pub(crate) fn validate_cas_environment(env: &StoredEnvironment) -> Result<()> {
    if let Some(profile_oid) = env.profile_oid.as_deref() {
        let store = global_store();
        let profile = match store.load(profile_oid) {
            Ok(LoadedObject::Profile { header, .. }) => header,
            Ok(_) => {
                return Err(InstallUserError::new(
                    "environment CAS profile is corrupted",
                    json!({
                        "profile_oid": profile_oid,
                        "reason": "missing_env",
                        "code": diagnostics::cas::MISSING_OR_CORRUPT,
                        "hint": "run `px sync` to rebuild the environment",
                    }),
                )
                .into());
            }
            Err(err) => {
                return Err(InstallUserError::new(
                    "environment CAS profile missing",
                    json!({
                        "profile_oid": profile_oid,
                        "error": err.to_string(),
                        "reason": "missing_env",
                        "code": diagnostics::cas::MISSING_OR_CORRUPT,
                        "hint": "run `px sync` to rebuild the environment",
                    }),
                )
                .into());
            }
        };

        let env_root = env
            .env_path
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or(default_envs_root()?.join(profile_oid));
        let manifest_path = env_root.join("manifest.json");
        let manifest: EnvManifest = match fs::read_to_string(&manifest_path)
            .ok()
            .and_then(|contents| serde_json::from_str(&contents).ok())
        {
            Some(parsed) => parsed,
            None => {
                return Err(InstallUserError::new(
                    "environment manifest missing",
                    json!({
                        "profile_oid": profile_oid,
                        "manifest": manifest_path.display().to_string(),
                        "reason": "missing_env",
                        "code": diagnostics::cas::MISSING_OR_CORRUPT,
                        "hint": "run `px sync` to rebuild the environment",
                    }),
                )
                .into());
            }
        };

        if manifest.profile_oid != profile_oid {
            return Err(InstallUserError::new(
                "environment profile drifted",
                json!({
                    "expected": profile_oid,
                    "found": manifest.profile_oid,
                    "reason": "env_outdated",
                    "code": diagnostics::cas::MISSING_OR_CORRUPT,
                    "hint": "run `px sync` to refresh the environment",
                }),
            )
            .into());
        }
        if manifest.runtime_oid != profile.runtime_oid {
            return Err(InstallUserError::new(
                "environment runtime no longer matches profile",
                json!({
                    "expected": profile.runtime_oid,
                    "found": manifest.runtime_oid,
                    "reason": "env_outdated",
                    "code": diagnostics::cas::MISSING_OR_CORRUPT,
                    "hint": "run `px sync` to refresh the environment",
                }),
            )
            .into());
        }

        validate_env_site_packages(&PathBuf::from(&env.site_packages), &manifest, store.root())
            .map_err(anyhow::Error::from)?;

        let mut expected = profile.packages.clone();
        expected.sort_by(|a, b| {
            a.name
                .cmp(&b.name)
                .then(a.version.cmp(&b.version))
                .then(a.pkg_build_oid.cmp(&b.pkg_build_oid))
        });
        let mut materialized = manifest.packages.clone();
        materialized.sort_by(|a, b| {
            a.name
                .cmp(&b.name)
                .then(a.version.cmp(&b.version))
                .then(a.pkg_build_oid.cmp(&b.pkg_build_oid))
        });
        if expected != materialized {
            return Err(InstallUserError::new(
                "environment packages drifted from CAS profile",
                json!({
                    "reason": "env_outdated",
                    "code": diagnostics::cas::MISSING_OR_CORRUPT,
                    "hint": "run `px sync` to refresh the environment",
                }),
            )
            .into());
        }

        let expected_sys_path: Vec<String> = if profile.sys_path_order.is_empty() {
            expected
                .iter()
                .map(|pkg| pkg.pkg_build_oid.clone())
                .collect()
        } else {
            profile.sys_path_order.clone()
        };
        let materialized_sys_path: Vec<String> = if manifest.sys_path_order.is_empty() {
            materialized
                .iter()
                .map(|pkg| pkg.pkg_build_oid.clone())
                .collect()
        } else {
            manifest.sys_path_order.clone()
        };
        if expected_sys_path != materialized_sys_path {
            return Err(InstallUserError::new(
                "environment sys.path ordering drifted from CAS profile",
                json!({
                    "reason": "env_outdated",
                    "code": diagnostics::cas::MISSING_OR_CORRUPT,
                    "hint": "run `px sync` to refresh the environment",
                }),
            )
            .into());
        }

        if let Err(err) = store.load(&manifest.runtime_oid) {
            return Err(InstallUserError::new(
                "runtime CAS object missing or corrupt",
                json!({
                    "runtime_oid": manifest.runtime_oid,
                    "error": err.to_string(),
                    "reason": "missing_env",
                    "code": diagnostics::cas::MISSING_OR_CORRUPT,
                    "hint": "run `px sync` to rebuild the environment",
                }),
            )
            .into());
        }
        for pkg in &materialized {
            if let Err(err) = store.load(&pkg.pkg_build_oid) {
                return Err(InstallUserError::new(
                    "package CAS object missing or corrupt",
                    json!({
                        "package": pkg.name,
                        "pkg_build_oid": pkg.pkg_build_oid,
                        "error": err.to_string(),
                        "reason": "missing_env",
                        "code": diagnostics::cas::MISSING_OR_CORRUPT,
                        "hint": "run `px sync` to rebuild the environment",
                    }),
                )
                .into());
            }
        }
    }

    Ok(())
}

pub fn ensure_env_matches_lock(
    ctx: &CommandContext,
    snapshot: &ManifestSnapshot,
    lock_id: &str,
) -> Result<()> {
    let state = match load_project_state(ctx.fs(), &snapshot.root) {
        Ok(state) => state,
        Err(err) => {
            return Err(InstallUserError::new(
                "px state file is unreadable",
                json!({
                    "error": err.to_string(),
                    "state": snapshot.root.join(".px").join("state.json"),
                    "hint": "Repair or delete the corrupted .px/state.json file, then rerun the command.",
                    "reason": "invalid_state",
                }),
            )
            .into());
        }
    };
    let Some(env) = state.current_env else {
        return Err(InstallUserError::new(
            "project environment missing",
            json!({
                "hint": "run `px sync` to build the environment",
                "reason": "missing_env",
            }),
        )
        .into());
    };
    if env.lock_id != lock_id {
        return Err(InstallUserError::new(
            "environment is out of date",
            json!({
                "expected_lock_id": lock_id,
                "current_lock_id": env.lock_id,
                "hint": "run `px sync` to rebuild the environment",
                "reason": "env_outdated",
            }),
        )
        .into());
    }
    let site_dir = PathBuf::from(&env.site_packages);
    if !site_dir.exists() {
        return Err(InstallUserError::new(
            "environment files missing",
            json!({
                "site": env.site_packages,
                "hint": "run `px sync` to rebuild the environment",
                "reason": "missing_env",
            }),
        )
        .into());
    }

    let runtime = detect_runtime_metadata(ctx, snapshot)?;
    if runtime.version != env.python.version || runtime.platform != env.platform {
        return Err(InstallUserError::new(
            format!(
                "environment targets Python {} ({}) but {} ({}) is active",
                env.python.version, env.platform, runtime.version, runtime.platform
            ),
            json!({
                "expected_python": env.python.version,
                "current_python": runtime.version,
                "expected_platform": env.platform,
                "current_platform": runtime.platform,
                "hint": "run `px sync` to rebuild for the current runtime",
                "reason": "runtime_mismatch",
            }),
        )
        .into());
    }

    if env.profile_oid.is_none() {
        return Err(InstallUserError::new(
            "environment CAS profile missing",
            json!({
                "reason": "missing_env",
                "code": diagnostics::cas::MISSING_OR_CORRUPT,
                "hint": "run `px sync` to rebuild the environment",
            }),
        )
        .into());
    }

    validate_cas_environment(&env)?;
    ensure_packaging_seeds_present(ctx, snapshot, &env)?;

    Ok(())
}

fn env_root_from_site_packages(site_packages: &Path) -> Option<PathBuf> {
    site_packages
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .map(PathBuf::from)
}

fn ensure_packaging_seeds_present(
    ctx: &CommandContext,
    snapshot: &ManifestSnapshot,
    env: &StoredEnvironment,
) -> Result<()> {
    let env_root = env
        .env_path
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| env_root_from_site_packages(Path::new(&env.site_packages)));
    let Some(site_dir) = env_root else {
        return Ok(());
    };
    let env_python = PathBuf::from(&env.python.path);
    let envs = project_site_env(ctx, snapshot, &site_dir, &env_python)?;
    let setuptools_ok = module_available(ctx, snapshot, &env_python, &envs, "setuptools")?;
    let uv_needed = uv_seed_required(snapshot);
    let uv_ok = !uv_needed
        || has_uv_cli(&site_dir)
        || module_available(ctx, snapshot, &env_python, &envs, "uv")?;

    if setuptools_ok && uv_ok {
        return Ok(());
    }
    if !setuptools_ok {
        return Err(InstallUserError::new(
            "environment missing baseline packaging support",
            json!({
                "missing": ["setuptools"],
                "reason": "env_outdated",
                "code": diagnostics::cas::MISSING_OR_CORRUPT,
                "hint": "run `px sync` to refresh the environment",
            }),
        )
        .into());
    }

    ensure_uv_seed(ctx, snapshot, &site_dir, &env_python, &envs)
}
