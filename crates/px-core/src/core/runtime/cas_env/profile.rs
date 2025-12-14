use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Result};
use px_domain::api::{LockSnapshot, LockedArtifact};
use serde_json::Value;
use tempfile::TempDir;

use crate::core::runtime::builder::builder_identity_for_runtime;
use crate::core::runtime::facade::RuntimeMetadata;
use crate::store::cas::{
    archive_dir_canonical, global_store, pkg_build_lookup_key, source_lookup_key, LoadedObject,
    ObjectKind, ObjectPayload, OwnerId, OwnerType, PkgBuildHeader, ProfileHeader, ProfilePackage,
    SourceHeader, MATERIALIZED_PKG_BUILDS_DIR,
};
use crate::store::{
    cache_wheel, ensure_sdist_build, ensure_wheel_dist, wheel_path, ArtifactRequest, SdistRequest,
};
use crate::{CommandContext, ManifestSnapshot};

use super::copy_tree;
use super::materialize::{materialize_profile_env, materialize_runtime_archive};
use super::runtime::{default_build_options_hash, runtime_archive, runtime_header};
use super::scripts::materialize_wheel_scripts;

/// Result of preparing a CAS-backed profile and its environment materialization.
pub(crate) struct CasProfile {
    pub(crate) profile_oid: String,
    pub(crate) env_path: PathBuf,
    pub(crate) runtime_path: PathBuf,
}

/// Result of preparing a CAS-backed profile without materializing an environment directory.
pub(crate) struct CasProfileManifest {
    pub(crate) profile_oid: String,
    pub(crate) runtime_path: PathBuf,
    pub(crate) header: ProfileHeader,
}

/// Build CAS objects for the runtime and locked dependencies, then materialize the
/// profile environment on disk. Returns the profile metadata and env path.
pub(crate) fn ensure_profile_env(
    ctx: &CommandContext,
    snapshot: &ManifestSnapshot,
    lock: &LockSnapshot,
    runtime: &RuntimeMetadata,
    env_owner: &OwnerId,
) -> Result<CasProfile> {
    let manifest = ensure_profile_manifest(ctx, snapshot, lock, runtime, env_owner)?;
    let env_root = materialize_profile_env(
        snapshot,
        runtime,
        &manifest.header,
        &manifest.profile_oid,
        &manifest.runtime_path,
    )?;
    Ok(CasProfile {
        profile_oid: manifest.profile_oid,
        env_path: env_root,
        runtime_path: manifest.runtime_path,
    })
}

pub(crate) fn ensure_profile_manifest(
    ctx: &CommandContext,
    snapshot: &ManifestSnapshot,
    lock: &LockSnapshot,
    runtime: &RuntimeMetadata,
    env_owner: &OwnerId,
) -> Result<CasProfileManifest> {
    // Host-only escape hatch: when PX_RUNTIME_HOST_ONLY=1, skip archiving the
    // runtime into CAS and rely on the host interpreter path directly.
    let host_runtime_passthrough = env::var("PX_RUNTIME_HOST_ONLY")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let store = global_store();
    let cache_root = &ctx.cache().path;
    fs::create_dir_all(cache_root)?;

    let runtime_header = runtime_header(runtime)?;
    let runtime_payload = ObjectPayload::Runtime {
        header: runtime_header.clone(),
        archive: Cow::Owned(if host_runtime_passthrough {
            // Header-only runtime in host passthrough mode; bytes live outside CAS.
            Vec::new()
        } else {
            runtime_archive(runtime)?
        }),
    };
    let runtime_obj = store.store(&runtime_payload)?;
    let runtime_exe = if host_runtime_passthrough {
        PathBuf::from(&runtime.path)
    } else {
        let (runtime_header, runtime_archive) = match store.load(&runtime_obj.oid)? {
            LoadedObject::Runtime {
                header, archive, ..
            } => (header, archive),
            _ => {
                return Err(anyhow!(
                    "CAS object {} is not a runtime archive",
                    runtime_obj.oid
                ))
            }
        };
        materialize_runtime_archive(&runtime_obj.oid, &runtime_header, &runtime_archive)?
    };
    let runtime_owner = OwnerId {
        owner_type: OwnerType::Runtime,
        owner_id: format!(
            "runtime:{}:{}",
            runtime_header.version, runtime_header.platform
        ),
    };
    // Clean up pre-spec owner ids to avoid dangling references during rollout.
    let legacy_runtime_owner = OwnerId {
        owner_type: OwnerType::Runtime,
        owner_id: format!("{}:{}", runtime_header.version, runtime_header.platform),
    };
    let _ = store.remove_owner_refs(&legacy_runtime_owner)?;
    let _ = store.remove_owner_refs(&runtime_owner)?;
    let _ = store.add_ref(&runtime_owner, &runtime_obj.oid);

    let lookup_versions = dependency_versions(lock)?;
    let builder = builder_identity_for_runtime(runtime)?;
    let runtime_abi = builder.runtime_abi.clone();
    let builder_id = builder.builder_id.clone();
    let default_build_options_hash = default_build_options_hash(runtime);
    let mut env_vars = profile_env_vars(snapshot)?;
    let mut native_lib_paths = Vec::new();

    let mut packages = Vec::new();
    let mut sys_path_order = Vec::new();
    let mut sys_seen = HashSet::new();
    for dep in &lock.resolved {
        let Some(artifact) = &dep.artifact else {
            continue;
        };
        let version = lookup_versions
            .get(&dep.name.to_ascii_lowercase())
            .cloned()
            .unwrap_or_else(|| inferred_version_from_filename(&artifact.filename));
        let source_filename = if artifact.filename.ends_with(".whl")
            && (artifact.url.ends_with(".tar.gz") || artifact.url.ends_with(".zip"))
        {
            artifact
                .url
                .rsplit('/')
                .next()
                .unwrap_or(&artifact.filename)
                .to_string()
        } else {
            artifact.filename.clone()
        };
        let mut source_header = SourceHeader {
            name: dep.name.clone(),
            version: version.clone(),
            filename: source_filename,
            index_url: artifact.url.clone(),
            sha256: artifact.sha256.clone(),
        };
        let builder_needed = artifact.build_options_hash.contains("native-libs")
            || !artifact.platform_tag.eq_ignore_ascii_case("any")
            || !artifact.abi_tag.eq_ignore_ascii_case("none");
        let mut build_options_hash = if artifact.build_options_hash.is_empty() {
            default_build_options_hash.clone()
        } else {
            artifact.build_options_hash.clone()
        };
        let mut builder_dist: Option<PathBuf> = None;
        let wheel_path = if builder_needed {
            let sha = if artifact.filename.ends_with(".whl") || source_header.sha256.is_empty() {
                None
            } else {
                Some(source_header.sha256.as_str())
            };
            let built = ensure_sdist_build(
                cache_root,
                &SdistRequest {
                    normalized_name: &dep.name,
                    version: &version,
                    filename: &source_header.filename,
                    url: &source_header.index_url,
                    sha256: sha,
                    python_path: &runtime.path,
                    builder_id: &builder_id,
                    builder_root: Some(cache_root.to_path_buf()),
                },
            )?;
            build_options_hash = built.build_options_hash.clone();
            source_header.sha256 = built.source_sha256.clone();
            builder_dist = Some(built.dist_path.clone());
            built.cached_path
        } else if !artifact.cached_path.is_empty() && Path::new(&artifact.cached_path).exists() {
            PathBuf::from(&artifact.cached_path)
        } else {
            ensure_cached_wheel(cache_root, &source_header, artifact)?
        };
        let source_key = source_lookup_key(&source_header);
        let source_oid = match store.lookup_key(ObjectKind::Source, &source_key)? {
            Some(oid) => oid,
            None => {
                let bytes = fs::read(&wheel_path)?;
                let payload = ObjectPayload::Source {
                    header: source_header.clone(),
                    bytes: Cow::Owned(bytes),
                };
                let stored = store.store(&payload)?;
                store.record_key(ObjectKind::Source, &source_key, &stored.oid)?;
                stored.oid
            }
        };
        let pkg_header = PkgBuildHeader {
            source_oid,
            runtime_abi: runtime_abi.clone(),
            builder_id: builder_id.clone(),
            build_options_hash: build_options_hash.clone(),
        };
        let pkg_key = pkg_build_lookup_key(&pkg_header);
        let pkg_oid = match store.lookup_key(ObjectKind::PkgBuild, &pkg_key)? {
            Some(existing) => existing,
            None => {
                let dist = if let Some(path) = builder_dist.take() {
                    path
                } else {
                    ensure_wheel_dist(&wheel_path, &source_header.sha256)?
                };
                let (stage_guard, staging_root) = stage_pkg_build(&dist)?;
                let archive = archive_dir_canonical(&staging_root)?;
                drop(stage_guard);
                let payload = ObjectPayload::PkgBuild {
                    header: pkg_header.clone(),
                    archive: Cow::Owned(archive),
                };
                let stored = store.store(&payload)?;
                store.record_key(ObjectKind::PkgBuild, &pkg_key, &stored.oid)?;
                stored.oid
            }
        };
        for dir in [
            "lib",
            "lib64",
            "site-packages",
            "site-packages/lib",
            "site-packages/lib64",
            "site-packages/sys-libs",
            "sys-libs",
        ] {
            let path = store
                .root()
                .join(MATERIALIZED_PKG_BUILDS_DIR)
                .join(&pkg_oid)
                .join(dir);
            native_lib_paths.push(path);
        }
        let pkg = ProfilePackage {
            name: dep.name.clone(),
            version,
            pkg_build_oid: pkg_oid.clone(),
        };
        if sys_seen.insert(pkg_oid.clone()) {
            sys_path_order.push(pkg_oid.clone());
        }
        packages.push(pkg);
    }

    packages.sort_by(|a, b| a.name.cmp(&b.name));
    if !native_lib_paths.is_empty() {
        let mut entries = Vec::new();
        if let Some(existing) = env_vars
            .get("LD_LIBRARY_PATH")
            .and_then(|v| v.as_str().map(str::to_string))
        {
            entries.push(existing);
        }
        native_lib_paths.sort();
        native_lib_paths.dedup();
        for path in native_lib_paths {
            entries.push(path.display().to_string());
        }
        let sep = if cfg!(windows) { ';' } else { ':' };
        let joined = entries.join(&sep.to_string());
        env_vars.insert("LD_LIBRARY_PATH".to_string(), Value::String(joined));
    }
    let manifest = ProfileHeader {
        runtime_oid: runtime_obj.oid.clone(),
        packages,
        sys_path_order,
        env_vars,
    };
    let profile_payload = ObjectPayload::Profile {
        header: manifest.clone(),
    };
    let profile_obj = store.store(&profile_payload)?;
    for pkg in &manifest.packages {
        let owner = OwnerId {
            owner_type: OwnerType::Profile,
            owner_id: profile_obj.oid.clone(),
        };
        let _ = store.add_ref(&owner, &pkg.pkg_build_oid);
    }
    let owner = OwnerId {
        owner_type: OwnerType::Profile,
        owner_id: profile_obj.oid.clone(),
    };
    let _ = store.add_ref(&owner, &manifest.runtime_oid);
    let _ = store.add_ref(env_owner, &profile_obj.oid);

    Ok(CasProfileManifest {
        profile_oid: profile_obj.oid,
        runtime_path: runtime_exe,
        header: manifest,
    })
}

pub(super) fn profile_env_vars(snapshot: &ManifestSnapshot) -> Result<BTreeMap<String, Value>> {
    let mut vars = snapshot
        .px_options
        .env_vars
        .iter()
        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
        .collect::<BTreeMap<_, _>>();
    if let Some(raw) = env::var_os("PX_PROFILE_ENV_VARS") {
        let raw = raw.to_string_lossy();
        if !raw.is_empty() {
            let parsed: Value = serde_json::from_str(&raw)
                .map_err(|err| anyhow!("PX_PROFILE_ENV_VARS must be a JSON object: {err}"))?;
            let obj = parsed
                .as_object()
                .ok_or_else(|| anyhow!("PX_PROFILE_ENV_VARS must be a JSON object"))?;
            for (key, value) in obj {
                vars.insert(key.clone(), value.clone());
            }
        }
    }
    Ok(vars)
}

fn dependency_versions(lock: &LockSnapshot) -> Result<HashMap<String, String>> {
    let mut versions = HashMap::new();
    for spec in &lock.dependencies {
        let head = spec.split(';').next().unwrap_or(spec).trim();
        if let Some((name_part, ver_part)) = head.split_once("==") {
            let name =
                crate::core::runtime::artifacts::dependency_name(name_part).to_ascii_lowercase();
            let version = ver_part.trim().to_string();
            versions.entry(name).or_insert(version);
        }
    }
    if let Some(graph) = &lock.graph {
        for node in &graph.nodes {
            versions
                .entry(node.name.to_ascii_lowercase())
                .or_insert(node.version.clone());
        }
    }
    Ok(versions)
}

fn inferred_version_from_filename(filename: &str) -> String {
    let parts: Vec<&str> = filename.trim_end_matches(".whl").split('-').collect();
    if parts.len() >= 2 {
        parts[1].to_string()
    } else {
        "unknown".to_string()
    }
}

fn ensure_cached_wheel(
    cache_root: &Path,
    header: &SourceHeader,
    artifact: &LockedArtifact,
) -> Result<PathBuf> {
    if !artifact.cached_path.is_empty() {
        let cached = PathBuf::from(&artifact.cached_path);
        if cached.exists() {
            return Ok(cached);
        }
    }
    let dest = wheel_path(cache_root, &header.name, &header.version, &header.filename);
    if dest.exists() {
        return Ok(dest);
    }
    fs::create_dir_all(dest.parent().unwrap_or_else(|| Path::new(".")))?;
    let request = ArtifactRequest {
        name: &header.name,
        version: &header.version,
        filename: &header.filename,
        url: &header.index_url,
        sha256: &header.sha256,
    };
    let artifact = cache_wheel(cache_root, &request)?;
    Ok(artifact.wheel_path)
}

fn stage_pkg_build(dist_dir: &Path) -> Result<(TempDir, PathBuf)> {
    let staging = TempDir::new()?;
    let root = staging.path().join("pkg");
    fs::create_dir_all(&root)?;
    let site_dir = root.join("site-packages");
    let bin_dir = root.join("bin");
    fs::create_dir_all(&site_dir)?;
    fs::create_dir_all(&bin_dir)?;
    copy_tree(dist_dir, &site_dir)?;
    materialize_wheel_scripts(dist_dir, &bin_dir)?;
    Ok((staging, root))
}
