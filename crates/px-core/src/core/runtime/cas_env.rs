use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap, HashSet},
    env, fs,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{anyhow, bail, Context, Result};
use flate2::read::GzDecoder;
use hex;
use px_domain::lockfile::types::LockedArtifact;
use px_domain::LockSnapshot;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tar::Archive;
use tempfile::TempDir;

use crate::core::runtime::builder::builder_identity_for_runtime;
use crate::core::runtime::facade::{site_packages_dir, RuntimeMetadata, SITE_CUSTOMIZE};
use crate::store::cas::{
    archive_dir_canonical, archive_selected, global_store, make_read_only_recursive,
    pkg_build_lookup_key, source_lookup_key, LoadedObject, ObjectKind, ObjectPayload, OwnerId,
    OwnerType, PkgBuildHeader, ProfileHeader, ProfilePackage, RuntimeHeader, SourceHeader,
    MATERIALIZED_PKG_BUILDS_DIR, MATERIALIZED_RUNTIMES_DIR,
};
use crate::store::{
    cache_wheel, ensure_sdist_build, ensure_wheel_dist, wheel_path, ArtifactRequest, SdistRequest,
};
use crate::{CommandContext, ManifestSnapshot};
use tracing::debug;

#[cfg(unix)]
fn make_writable_recursive(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = fs::metadata(path) {
        let mode = if meta.is_dir() { 0o755 } else { 0o644 };
        let _ = fs::set_permissions(path, PermissionsExt::from_mode(mode));
        if meta.is_dir() {
            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries.flatten() {
                    make_writable_recursive(&entry.path());
                }
            }
        }
    }
}

#[cfg(not(unix))]
fn make_writable_recursive(_path: &Path) {}

/// Result of preparing a CAS-backed profile and its environment materialization.
pub(crate) struct CasProfile {
    pub(crate) profile_oid: String,
    pub(crate) env_path: PathBuf,
    pub(crate) runtime_path: PathBuf,
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
        let mut source_header = SourceHeader {
            name: dep.name.clone(),
            version: version.clone(),
            filename: artifact.filename.clone(),
            index_url: artifact.url.clone(),
            sha256: artifact.sha256.clone(),
        };
        let builder_needed = artifact.build_options_hash.contains("native-libs");
        let mut build_options_hash = if artifact.build_options_hash.is_empty() {
            default_build_options_hash.clone()
        } else {
            artifact.build_options_hash.clone()
        };
        let mut builder_dist: Option<PathBuf> = None;
        let wheel_path = if builder_needed {
            let built = ensure_sdist_build(
                cache_root,
                &SdistRequest {
                    normalized_name: &dep.name,
                    version: &version,
                    filename: &artifact.filename,
                    url: &artifact.url,
                    sha256: None,
                    python_path: &runtime.path,
                    builder_id: &builder_id,
                    builder_root: Some(cache_root.to_path_buf()),
                },
            )?;
            build_options_hash = built.build_options_hash.clone();
            source_header.filename = built.filename.clone();
            source_header.index_url = built.url.clone();
            source_header.sha256 = built.sha256.clone();
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

    let env_root =
        materialize_profile_env(snapshot, runtime, &manifest, &profile_obj.oid, &runtime_exe)?;
    Ok(CasProfile {
        profile_oid: profile_obj.oid,
        env_path: env_root,
        runtime_path: runtime_exe,
    })
}

fn profile_env_vars(snapshot: &ManifestSnapshot) -> Result<BTreeMap<String, Value>> {
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

fn runtime_header(runtime: &RuntimeMetadata) -> Result<RuntimeHeader> {
    let tags = crate::python_sys::detect_interpreter_tags(&runtime.path)?;
    let abi = tags
        .abi
        .first()
        .cloned()
        .unwrap_or_else(|| "none".to_string());
    let platform = tags
        .platform
        .first()
        .cloned()
        .unwrap_or_else(|| "any".to_string());
    let exe_path = {
        let python_path = PathBuf::from(&runtime.path);
        python_path
            .parent()
            .and_then(|bin| bin.parent())
            .and_then(|root| python_path.strip_prefix(root).ok())
            .map(|rel| rel.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|| {
                let name = python_path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "python".to_string());
                format!("bin/{name}")
            })
    };
    Ok(RuntimeHeader {
        version: runtime.version.clone(),
        abi,
        platform,
        build_config_hash: runtime_config_hash(&tags),
        exe_path,
    })
}

fn runtime_archive(runtime: &RuntimeMetadata) -> Result<Vec<u8>> {
    let python_path = PathBuf::from(&runtime.path);
    let Some(bin_dir) = python_path.parent() else {
        return Err(anyhow!(
            "unable to resolve runtime root for {}",
            runtime.path
        ));
    };
    let root_dir = bin_dir
        .parent()
        .ok_or_else(|| anyhow!("unable to resolve runtime root for {}", runtime.path))?;
    let mut include_paths = python_runtime_paths(runtime)?;
    let probed = !include_paths.is_empty();
    include_paths.push(python_path.clone());
    if !probed {
        let version_tag = runtime
            .version
            .split('.')
            .take(2)
            .collect::<Vec<_>>()
            .join(".");
        let version_dir = format!("python{version_tag}");
        include_paths.extend([
            root_dir.join("lib").join(&version_dir),
            root_dir.join("lib64").join(&version_dir),
            root_dir.join("include"),
            root_dir.join("include").join(&version_dir),
            root_dir.join("Include").join(&version_dir),
            root_dir.join("Lib").join(&version_dir),
            root_dir.join("pyvenv.cfg"),
        ]);
    }
    include_paths.retain(|path| path.exists());
    include_paths.sort();
    include_paths.dedup();
    if include_paths.is_empty() {
        bail!("no runtime paths found to archive for {}", runtime.path);
    }
    archive_selected(root_dir, &include_paths)
}

fn runtime_config_hash(tags: &crate::python_sys::InterpreterTags) -> String {
    let payload = json!({
        "python": tags.python,
        "abi": tags.abi,
        "platform": tags.platform,
    });
    let bytes = serde_json::to_vec(&payload).unwrap_or_default();
    hex::encode(Sha256::digest(bytes))
}

fn default_build_options_hash(runtime: &RuntimeMetadata) -> String {
    let payload = json!({
        "runtime": runtime.version,
        "platform": runtime.platform,
        "kind": "default",
    });
    hex::encode(Sha256::digest(
        serde_json::to_vec(&payload).unwrap_or_default(),
    ))
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

fn python_runtime_paths(runtime: &RuntimeMetadata) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    let script = r#"
import json, sys, sysconfig
paths = {
    "executable": sys.executable,
    "stdlib": sysconfig.get_path("stdlib"),
    "platstdlib": sysconfig.get_path("platstdlib"),
    "include": sysconfig.get_config_var("INCLUDEPY"),
    "scripts": sysconfig.get_path("scripts"),
}
print(json.dumps(paths))
"#;
    match Command::new(&runtime.path).arg("-c").arg(script).output() {
        Ok(output) if output.status.success() => {
            if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&output.stdout) {
                let entries = [
                    value.get("executable"),
                    value.get("stdlib"),
                    value.get("platstdlib"),
                    value.get("include"),
                    value.get("scripts"),
                ];
                for entry in entries.into_iter().flatten() {
                    if let Some(s) = entry.as_str() {
                        if !s.is_empty() {
                            paths.push(PathBuf::from(s));
                        }
                    }
                }
            }
        }
        Ok(_) => {}
        Err(err) => {
            debug!(
                %err,
                python = %runtime.path,
                "failed to probe runtime paths; falling back to interpreter only"
            );
        }
    }
    Ok(paths)
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

pub(crate) fn copy_tree(src: &Path, dest: &Path) -> Result<()> {
    for entry in walkdir::WalkDir::new(src) {
        let entry = entry?;
        let path = entry.path();
        if path == src {
            continue;
        }
        let rel = path.strip_prefix(src).unwrap_or(path);
        let target = dest.join(rel);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(path, &target)?;
        } else if entry.file_type().is_symlink() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            let link_target = fs::read_link(path)?;
            let _ = fs::remove_file(&target);
            #[cfg(unix)]
            {
                use std::os::unix::fs::symlink;
                symlink(&link_target, &target)?;
            }
            #[cfg(not(unix))]
            {
                if link_target.is_file() {
                    let _ = fs::copy(&link_target, &target)?;
                }
            }
        }
    }
    Ok(())
}

fn materialize_profile_env(
    _snapshot: &ManifestSnapshot,
    runtime: &RuntimeMetadata,
    manifest: &ProfileHeader,
    profile_oid: &str,
    runtime_exe: &Path,
) -> Result<PathBuf> {
    let envs_root = default_envs_root()?;
    fs::create_dir_all(&envs_root)?;
    let env_root = envs_root.join(profile_oid);

    let store = global_store();
    let _lock = store.acquire_lock(profile_oid)?;

    let temp_root = env_root.with_extension("partial");
    if temp_root.exists() {
        let _ = fs::remove_dir_all(&temp_root);
    }
    fs::create_dir_all(&temp_root)?;
    let site_packages = site_packages_dir(&temp_root, &runtime.version);
    fs::create_dir_all(&site_packages)?;
    let bin_dir = temp_root.join("bin");
    fs::create_dir_all(&bin_dir)?;

    let mut site_entries: HashMap<String, PathBuf> = HashMap::new();
    for pkg in &manifest.packages {
        let loaded = store.load(&pkg.pkg_build_oid)?;
        let LoadedObject::PkgBuild { archive, .. } = loaded else {
            return Err(anyhow!(
                "CAS object {} is not a pkg-build archive",
                pkg.pkg_build_oid
            ));
        };
        let materialized = materialize_pkg_archive(&pkg.pkg_build_oid, &archive)?;
        let pkg_site = materialized.join("site-packages");
        if pkg_site.exists() {
            site_entries.insert(pkg.pkg_build_oid.clone(), pkg_site);
        }
        let pkg_bin = materialized.join("bin");
        if pkg_bin.exists() {
            for entry in fs::read_dir(&pkg_bin)? {
                let entry = entry?;
                if entry.file_type()?.is_file() {
                    let src = entry.path();
                    let dest = bin_dir.join(entry.file_name());
                    let env_python = env_root.join("bin").join("python");
                    link_bin_entry(&src, &dest, Some(&env_python))?;
                }
            }
        }
    }

    let sys_path_order = if manifest.sys_path_order.is_empty() {
        manifest
            .packages
            .iter()
            .map(|pkg| pkg.pkg_build_oid.clone())
            .collect()
    } else {
        manifest.sys_path_order.clone()
    };
    let mut seen = HashSet::new();
    let resolved_sys_path: Vec<String> = sys_path_order
        .into_iter()
        .filter(|oid| seen.insert(oid.clone()))
        .collect();
    let mut pth_body = String::new();
    for oid in &resolved_sys_path {
        if let Some(entry) = site_entries.get(oid) {
            pth_body.push_str(&entry.display().to_string());
            pth_body.push('\n');
        }
    }
    for (oid, entry) in site_entries {
        if seen.insert(oid) {
            pth_body.push_str(&entry.display().to_string());
            pth_body.push('\n');
        }
    }
    fs::write(site_packages.join("px.pth"), pth_body)?;
    write_sitecustomize(&temp_root, Some(&site_packages))?;

    let manifest_path = temp_root.join("manifest.json");
    fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&json!({
            "profile_oid": profile_oid,
            "runtime_oid": manifest.runtime_oid,
            "packages": manifest.packages,
            "sys_path_order": resolved_sys_path,
            "env_vars": manifest.env_vars,
        }))?,
    )?;
    let backup_root = env_root.with_extension("backup");
    if backup_root.exists() {
        let _ = fs::remove_dir_all(&backup_root);
    }
    if env_root.exists() {
        fs::rename(&env_root, &backup_root).with_context(|| {
            format!(
                "failed to move existing environment {} to backup",
                env_root.display()
            )
        })?;
    }
    if let Err(err) = fs::rename(&temp_root, &env_root) {
        let _ = fs::remove_dir_all(&temp_root);
        if backup_root.exists() {
            let _ = fs::rename(&backup_root, &env_root);
        }
        return Err(err).with_context(|| {
            format!(
                "failed to finalize environment materialization at {}",
                env_root.display()
            )
        });
    }
    let _ = fs::remove_dir_all(&backup_root);

    let final_site = site_packages_dir(&env_root, &runtime.version);
    write_python_shim(
        &env_root.join("bin"),
        runtime_exe,
        &final_site,
        &manifest.env_vars,
    )?;
    install_python_links(&env_root.join("bin"), runtime_exe)?;
    Ok(env_root)
}

fn link_bin_entry(src: &Path, dest: &Path, env_python: Option<&Path>) -> Result<()> {
    if let Some(python) = env_python {
        if should_rewrite_python_entrypoint(src)? {
            rewrite_python_entrypoint(src, dest, python)?;
            return Ok(());
        }
    }

    if dest.exists() {
        let _ = fs::remove_file(dest);
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        if let Err(sym_err) = symlink(src, dest) {
            fs::hard_link(src, dest).with_context(|| {
                format!(
                    "failed to link CAS bin entry {} -> {} (symlink error: {})",
                    dest.display(),
                    src.display(),
                    sym_err
                )
            })?;
        }
    }

    #[cfg(not(unix))]
    {
        if let Err(hard_err) = fs::hard_link(src, dest) {
            #[cfg(windows)]
            {
                use std::os::windows::fs::symlink_file;
                symlink_file(src, dest).with_context(|| {
                    format!(
                        "failed to link CAS bin entry {} -> {} (hard link error: {})",
                        dest.display(),
                        src.display(),
                        hard_err
                    )
                })?;
            }
            #[cfg(not(windows))]
            {
                return Err(hard_err).with_context(|| {
                    format!(
                        "failed to link CAS bin entry {} -> {}",
                        dest.display(),
                        src.display()
                    )
                });
            }
        }
    }
    Ok(())
}

fn materialize_pkg_archive(oid: &str, archive: &[u8]) -> Result<PathBuf> {
    let store = global_store();
    let root = store.root().join(MATERIALIZED_PKG_BUILDS_DIR).join(oid);
    if root.exists() {
        return Ok(root);
    }
    let _lock = store.acquire_lock(oid)?;
    if root.exists() {
        return Ok(root);
    }
    if let Some(parent) = root.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = root.with_extension("partial");
    if tmp.exists() {
        let _ = fs::remove_dir_all(&tmp);
    }
    fs::create_dir_all(&tmp)?;
    let decoder = GzDecoder::new(archive);
    let mut tar = Archive::new(decoder);
    tar.unpack(&tmp)?;
    fs::rename(&tmp, &root)?;
    make_read_only_recursive(&root)?;
    Ok(root)
}

fn materialize_runtime_archive(
    oid: &str,
    header: &RuntimeHeader,
    archive: &[u8],
) -> Result<PathBuf> {
    let exe_rel = Path::new(&header.exe_path);
    if exe_rel.is_absolute() {
        return Err(anyhow!(
            "runtime executable path must be relative (got {})",
            header.exe_path
        ));
    }
    let store = global_store();
    let root = store.root().join(MATERIALIZED_RUNTIMES_DIR).join(oid);
    let _lock = store.acquire_lock(oid)?;
    let exe_path = root.join(exe_rel);
    if root.exists() && exe_path.exists() {
        let _ = store.write_runtime_manifest(oid, header);
        return Ok(exe_path);
    }
    if root.exists() {
        let _ = fs::remove_dir_all(&root);
    }
    if let Some(parent) = root.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = root.with_extension("partial");
    if tmp.exists() {
        let _ = fs::remove_dir_all(&tmp);
    }
    fs::create_dir_all(&tmp)?;
    let decoder = GzDecoder::new(archive);
    let mut tar = Archive::new(decoder);
    tar.unpack(&tmp)?;
    if let Err(err) = fs::rename(&tmp, &root) {
        make_writable_recursive(&root);
        let _ = fs::remove_dir_all(&root);
        fs::rename(&tmp, &root).map_err(|retry| anyhow!(retry).context(err))?;
    }
    let exe_path = root.join(exe_rel);
    if !exe_path.exists() {
        return Err(anyhow!(
            "runtime executable missing after materialization: {}",
            exe_path.display()
        ));
    }
    store.write_runtime_manifest(oid, header)?;
    make_read_only_recursive(&root)?;
    Ok(exe_path)
}

pub(crate) fn default_envs_root() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("PX_ENVS_PATH") {
        return Ok(PathBuf::from(path));
    }
    let home = dirs_next::home_dir().ok_or_else(|| anyhow!("HOME not set"))?;
    Ok(home.join(".px").join("envs"))
}

fn project_root_fingerprint(root: &Path) -> Result<String> {
    let canonical = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    Ok(hex::encode(Sha256::digest(
        canonical.display().to_string().as_bytes(),
    )))
}

pub(crate) fn project_env_owner_id(
    project_root: &Path,
    lock_id: &str,
    runtime_version: &str,
) -> Result<String> {
    Ok(format!(
        "project-env:{}:{}:{}",
        project_root_fingerprint(project_root)?,
        lock_id,
        runtime_version
    ))
}

pub(crate) fn workspace_env_owner_id(
    workspace_root: &Path,
    lock_id: &str,
    runtime_version: &str,
) -> Result<String> {
    Ok(format!(
        "workspace-env:{}:{}:{}",
        project_root_fingerprint(workspace_root)?,
        lock_id,
        runtime_version
    ))
}

fn write_sitecustomize(env_root: &Path, site_packages: Option<&Path>) -> Result<()> {
    let path = env_root.join("sitecustomize.py");
    fs::write(&path, SITE_CUSTOMIZE.as_bytes())?;
    if let Some(extra) = site_packages {
        fs::create_dir_all(extra)?;
        fs::write(extra.join("sitecustomize.py"), SITE_CUSTOMIZE.as_bytes())?;
    }
    Ok(())
}

pub(crate) fn write_python_shim(
    bin_dir: &Path,
    runtime: &Path,
    site: &Path,
    env_vars: &BTreeMap<String, Value>,
) -> Result<()> {
    fs::create_dir_all(bin_dir)?;
    let shim = bin_dir.join("python");
    let mut script = String::new();
    script.push_str("#!/usr/bin/env bash\n");
    if let Some(runtime_root) = runtime.parent().and_then(|bin| bin.parent()) {
        script.push_str(&format!(
            "export PYTHONHOME=\"{}\"\n",
            runtime_root.display()
        ));
    }
    let path_sep = if cfg!(windows) { ";" } else { ":" };
    let mut pythonpath = site.display().to_string();
    if let Some(runtime_root) = runtime.parent().and_then(|bin| bin.parent()) {
        if let Some(version_dir) = site.parent().and_then(|p| p.file_name()) {
            let runtime_site = runtime_root
                .join("lib")
                .join(version_dir)
                .join("site-packages");
            pythonpath.push_str(path_sep);
            pythonpath.push_str(&runtime_site.display().to_string());
        }
    }
    script.push_str("if [ -n \"$PYTHONPATH\" ]; then\n");
    script.push_str(&format!(
        "  export PYTHONPATH=\"$PYTHONPATH{path_sep}{pythonpath}\"\n"
    ));
    script.push_str("else\n");
    script.push_str(&format!("  export PYTHONPATH=\"{pythonpath}\"\n"));
    script.push_str("fi\n");
    script.push_str(&format!("export PX_PYTHON=\"{}\"\n", runtime.display()));
    script.push_str("export PYTHONUNBUFFERED=1\n");
    script.push_str("export PYTHONDONTWRITEBYTECODE=1\n");
    // Profile env_vars override the parent environment for the launched runtime.
    for (key, value) in env_vars {
        let rendered = env_var_value(value);
        script.push_str(&format!("export {key}={}\n", shell_escape(&rendered)));
    }
    script.push_str(&format!("exec \"{}\" \"$@\"\n", runtime.display()));
    fs::write(&shim, script)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&shim, fs::Permissions::from_mode(0o755))?;
    }
    for alias in ["python3", "python3.11", "python3.12"] {
        let dest = bin_dir.join(alias);
        let _ = fs::remove_file(&dest);
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let _ = symlink(Path::new("python"), &dest)
                .or_else(|_| fs::hard_link(&shim, &dest))
                .or_else(|_| fs::copy(&shim, &dest).map(|_| ()));
        }
        #[cfg(not(unix))]
        {
            let _ = fs::hard_link(&shim, &dest).or_else(|_| fs::copy(&shim, &dest).map(|_| ()));
        }
    }
    Ok(())
}

fn env_var_value(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn shell_escape(value: &str) -> String {
    let mut escaped = String::from("'");
    for ch in value.chars() {
        if ch == '\'' {
            escaped.push_str("'\\''");
        } else {
            escaped.push(ch);
        }
    }
    escaped.push('\'');
    escaped
}

fn install_python_links(bin_dir: &Path, runtime: &Path) -> Result<()> {
    let python_path = PathBuf::from(runtime);
    for name in ["python", "python3"] {
        let dest = bin_dir.join(name);
        if dest.exists() {
            continue;
        }
        let _ = fs::remove_file(&dest);
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink(&python_path, &dest).or_else(|_| fs::copy(&python_path, &dest).map(|_| ()))?;
        }
        #[cfg(not(unix))]
        {
            fs::copy(&python_path, &dest).map(|_| ())?;
        }
    }
    Ok(())
}

fn materialize_wheel_scripts(artifact_path: &Path, bin_dir: &Path) -> Result<()> {
    fs::create_dir_all(bin_dir)?;
    if artifact_path.extension().is_some_and(|ext| ext == "dist") && artifact_path.is_dir() {
        let entry_points = fs::read_dir(artifact_path)?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .find(|path| path.extension().is_some_and(|ext| ext == "dist-info"))
            .and_then(|dist_info| {
                let ep = dist_info.join("entry_points.txt");
                ep.exists().then_some(ep)
            });
        if let Some(ep_path) = entry_points {
            if let Ok(contents) = fs::read_to_string(&ep_path) {
                let mut section = String::new();
                for line in contents.lines() {
                    let trimmed = line.trim();
                    if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
                        continue;
                    }
                    if trimmed.starts_with('[') && trimmed.ends_with(']') {
                        section = trimmed
                            .trim_start_matches('[')
                            .trim_end_matches(']')
                            .to_string();
                        continue;
                    }
                    if section != "console_scripts" && section != "gui_scripts" {
                        continue;
                    }
                    if let Some((name, target)) = trimmed.split_once('=') {
                        let entry_name = name.trim();
                        let raw_target = target.trim();
                        let target_value = raw_target
                            .split_whitespace()
                            .next()
                            .unwrap_or(raw_target)
                            .trim();
                        if let Some((module, callable)) = target_value.split_once(':') {
                            let _ = write_entrypoint_script(
                                bin_dir,
                                entry_name,
                                module.trim(),
                                callable.trim(),
                            );
                        }
                    }
                }
            }
        }

        let script_dirs: Vec<PathBuf> = fs::read_dir(artifact_path)?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .map(|name| name.ends_with(".data"))
                    .unwrap_or(false)
            })
            .map(|data_dir| data_dir.join("scripts"))
            .filter(|path| path.exists())
            .collect();
        for dir in script_dirs {
            for entry in fs::read_dir(&dir)? {
                let entry = entry?;
                if entry.file_type()?.is_file() {
                    let dest = bin_dir.join(entry.file_name());
                    fs::copy(entry.path(), &dest)?;
                    let _ = set_exec_permissions(&dest);
                }
            }
        }
        return Ok(());
    }

    Ok(())
}

fn write_entrypoint_script(
    bin_dir: &Path,
    name: &str,
    module: &str,
    callable: &str,
) -> Result<PathBuf> {
    fs::create_dir_all(bin_dir)?;
    let python_shebang = "/usr/bin/env python3".to_string();
    let parts: Vec<String> = callable
        .split('.')
        .filter(|part| !part.is_empty())
        .map(ToString::to_string)
        .collect();
    let parts_repr = format!("{parts:?}");
    let contents = format!(
        "#!{python_shebang}\nimport importlib\nimport sys\n\ndef _load():\n    module = importlib.import_module({module:?})\n    target = module\n    for attr in {parts_repr}:\n        target = getattr(target, attr)\n    return target\n\nif __name__ == '__main__':\n    sys.exit(_load()())\n"
    );
    let script_path = bin_dir.join(name);
    fs::write(&script_path, contents)?;
    let _ = set_exec_permissions(&script_path);
    Ok(script_path)
}

fn set_exec_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

fn should_rewrite_python_entrypoint(path: &Path) -> Result<bool> {
    let mut file = fs::File::open(path)?;
    let mut buf = [0u8; 256];
    let read = file.read(&mut buf)?;
    let prefix = std::str::from_utf8(&buf[..read]).unwrap_or_default();
    let Some(first_line) = prefix.lines().next() else {
        return Ok(false);
    };
    if !first_line.starts_with("#!") {
        return Ok(false);
    }
    Ok(first_line.to_ascii_lowercase().contains("python"))
}

fn rewrite_python_entrypoint(src: &Path, dest: &Path, python: &Path) -> Result<()> {
    if dest.exists() {
        let _ = fs::remove_file(dest);
    }
    let contents = fs::read_to_string(src)?;
    let mut parts = contents.splitn(2, '\n');
    let _ = parts.next();
    let rest = parts.next().unwrap_or("");
    let mut rewritten = format!("#!{}\n", python.display());
    rewritten.push_str(rest);
    fs::write(dest, rewritten.as_bytes())?;
    set_exec_permissions(dest)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use px_domain::project::manifest::DependencyGroupSource;
    use px_domain::PxOptions;
    use std::collections::BTreeMap;
    use tempfile::tempdir;

    #[test]
    fn runtime_archive_captures_full_tree() -> Result<()> {
        let temp = tempdir()?;
        let root = temp.path().join("runtime");
        let bin = root.join("bin");
        let lib = root.join("lib/python3.11");
        let include = root.join("include");
        fs::create_dir_all(&bin)?;
        fs::create_dir_all(&lib)?;
        fs::create_dir_all(&include)?;
        fs::write(bin.join("python"), b"#!python")?;
        fs::write(lib.join("stdlib.py"), b"# stdlib")?;
        fs::write(include.join("Python.h"), b"// header")?;

        let runtime = RuntimeMetadata {
            path: bin.join("python").display().to_string(),
            version: "3.11.0".to_string(),
            platform: "linux".to_string(),
        };
        let archive = runtime_archive(&runtime)?;
        let decoder = GzDecoder::new(&archive[..]);
        let mut tar = Archive::new(decoder);
        let mut seen = Vec::new();
        for entry in tar.entries()? {
            let entry = entry?;
            seen.push(entry.path()?.into_owned());
        }
        assert!(
            seen.contains(&PathBuf::from("bin/python")),
            "interpreter should be captured"
        );
        assert!(
            seen.contains(&PathBuf::from("lib/python3.11/stdlib.py")),
            "stdlib should be captured"
        );
        assert!(
            seen.contains(&PathBuf::from("include/Python.h")),
            "headers should be captured"
        );
        Ok(())
    }

    #[test]
    fn python_shim_carries_runtime_site_and_home() -> Result<()> {
        let temp = tempdir()?;
        let runtime_root = temp.path().join("runtime");
        let bin_dir = runtime_root.join("bin");
        fs::create_dir_all(&bin_dir)?;
        let runtime = bin_dir.join("python");
        fs::write(&runtime, b"#!/bin/false")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&runtime, fs::Permissions::from_mode(0o755))?;
        }

        let env_root = temp.path().join("env");
        let env_bin = env_root.join("bin");
        let site = env_root.join("lib/python3.11/site-packages");
        fs::create_dir_all(&site)?;

        write_python_shim(&env_bin, &runtime, &site, &BTreeMap::new())?;
        let shim = fs::read_to_string(env_bin.join("python"))?;
        assert!(
            shim.contains(&runtime_root.display().to_string()),
            "PYTHONHOME should include runtime root"
        );
        let runtime_site = runtime_root.join("lib/python3.11/site-packages");
        assert!(
            shim.contains(&runtime_site.display().to_string()),
            "PYTHONPATH should include runtime site-packages"
        );
        assert!(
            shim.contains(&site.display().to_string()),
            "PYTHONPATH should include env site-packages"
        );
        Ok(())
    }

    #[test]
    fn env_bin_entries_link_to_store_materialization() -> Result<()> {
        let store = global_store();
        let temp = tempdir()?;

        // Minimal runtime executable.
        let runtime_root = temp.path().join("runtime");
        let runtime_bin = runtime_root.join("bin");
        fs::create_dir_all(&runtime_bin)?;
        let runtime_exe = runtime_bin.join("python");
        fs::write(&runtime_exe, b"#!/bin/false")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&runtime_exe, fs::Permissions::from_mode(0o755))?;
        }

        // CAS pkg-build with a bin script.
        let pkg_root = temp.path().join("pkg");
        let pkg_bin = pkg_root.join("bin");
        let pkg_site = pkg_root.join("site-packages");
        fs::create_dir_all(&pkg_bin)?;
        fs::create_dir_all(&pkg_site)?;
        let script = pkg_bin.join("demo");
        fs::write(&script, b"#!/bin/echo demo")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755))?;
        }

        let pkg_archive = archive_dir_canonical(&pkg_root)?;
        let pkg_obj = store.store(&ObjectPayload::PkgBuild {
            header: PkgBuildHeader {
                source_oid: "src".into(),
                runtime_abi: "abi".into(),
                builder_id: "builder".into(),
                build_options_hash: "opts".into(),
            },
            archive: Cow::Owned(pkg_archive),
        })?;

        // Runtime object to back the profile.
        let runtime_header = RuntimeHeader {
            version: "3.11.0".to_string(),
            abi: "cp311".to_string(),
            platform: "linux".to_string(),
            build_config_hash: "abc".to_string(),
            exe_path: "bin/python".to_string(),
        };
        let runtime_obj = store.store(&ObjectPayload::Runtime {
            header: runtime_header.clone(),
            archive: Cow::Owned(archive_dir_canonical(&runtime_root)?),
        })?;

        let profile_header = ProfileHeader {
            runtime_oid: runtime_obj.oid.clone(),
            packages: vec![ProfilePackage {
                name: "demo".to_string(),
                version: "1.0.0".to_string(),
                pkg_build_oid: pkg_obj.oid.clone(),
            }],
            sys_path_order: Vec::new(),
            env_vars: BTreeMap::new(),
        };
        let profile_obj = store.store(&ObjectPayload::Profile {
            header: profile_header.clone(),
        })?;

        let snapshot = ManifestSnapshot {
            root: temp.path().to_path_buf(),
            manifest_path: temp.path().join("pyproject.toml"),
            lock_path: temp.path().join("px.lock"),
            name: "demo".to_string(),
            python_requirement: ">=3.11".to_string(),
            dependencies: Vec::new(),
            dependency_groups: Vec::new(),
            declared_dependency_groups: Vec::new(),
            dependency_group_source: DependencyGroupSource::None,
            group_dependencies: Vec::new(),
            requirements: Vec::new(),
            python_override: None,
            px_options: PxOptions::default(),
            manifest_fingerprint: "fp".to_string(),
        };

        let env_root = materialize_profile_env(
            &snapshot,
            &RuntimeMetadata {
                path: runtime_exe.display().to_string(),
                version: "3.11.0".to_string(),
                platform: "linux".to_string(),
            },
            &profile_header,
            &profile_obj.oid,
            &runtime_exe,
        )?;

        let env_bin = env_root.join("bin").join("demo");
        let store_bin = store
            .root()
            .join(MATERIALIZED_PKG_BUILDS_DIR)
            .join(&pkg_obj.oid)
            .join("bin")
            .join("demo");

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let meta = fs::symlink_metadata(&env_bin)?;
            if meta.file_type().is_symlink() {
                let target = fs::read_link(&env_bin)?;
                assert_eq!(target, store_bin, "bin entry should be a symlink into CAS");
            } else {
                let src_meta = fs::metadata(&store_bin)?;
                let dest_meta = fs::metadata(&env_bin)?;
                assert_eq!(
                    src_meta.ino(),
                    dest_meta.ino(),
                    "bin entry should be hard-linked to CAS materialization"
                );
                assert_eq!(
                    src_meta.dev(),
                    dest_meta.dev(),
                    "bin entry should share the same device"
                );
            }
        }
        #[cfg(not(unix))]
        {
            assert_eq!(
                fs::metadata(&env_bin)?.len(),
                fs::metadata(&store_bin)?.len(),
                "bin entry should point to CAS materialization"
            );
        }
        Ok(())
    }

    #[test]
    fn python_bin_entries_are_rewritten_to_env_python() -> Result<()> {
        let store = global_store();
        let temp = tempdir()?;

        // Minimal runtime executable.
        let runtime_root = temp.path().join("runtime");
        let runtime_bin = runtime_root.join("bin");
        fs::create_dir_all(&runtime_bin)?;
        let runtime_exe = runtime_bin.join("python");
        fs::write(&runtime_exe, b"#!/bin/false")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&runtime_exe, fs::Permissions::from_mode(0o755))?;
        }

        // CAS pkg-build with a python shebang.
        let pkg_root = temp.path().join("pkg");
        let pkg_bin = pkg_root.join("bin");
        let pkg_site = pkg_root.join("site-packages");
        fs::create_dir_all(&pkg_bin)?;
        fs::create_dir_all(&pkg_site)?;
        let script = pkg_bin.join("demo");
        fs::write(&script, b"#!/usr/bin/env python3\nprint('hi from demo')\n")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755))?;
        }

        let pkg_archive = archive_dir_canonical(&pkg_root)?;
        let pkg_obj = store.store(&ObjectPayload::PkgBuild {
            header: PkgBuildHeader {
                source_oid: "src".into(),
                runtime_abi: "abi".into(),
                builder_id: "builder".into(),
                build_options_hash: "opts".into(),
            },
            archive: Cow::Owned(pkg_archive),
        })?;

        let runtime_header = RuntimeHeader {
            version: "3.11.0".to_string(),
            abi: "cp311".to_string(),
            platform: "linux".to_string(),
            build_config_hash: "abc".to_string(),
            exe_path: "bin/python".to_string(),
        };
        let runtime_obj = store.store(&ObjectPayload::Runtime {
            header: runtime_header.clone(),
            archive: Cow::Owned(archive_dir_canonical(&runtime_root)?),
        })?;

        let profile_header = ProfileHeader {
            runtime_oid: runtime_obj.oid.clone(),
            packages: vec![ProfilePackage {
                name: "demo".to_string(),
                version: "1.0.0".to_string(),
                pkg_build_oid: pkg_obj.oid.clone(),
            }],
            sys_path_order: Vec::new(),
            env_vars: BTreeMap::new(),
        };
        let profile_obj = store.store(&ObjectPayload::Profile {
            header: profile_header.clone(),
        })?;

        let snapshot = ManifestSnapshot {
            root: temp.path().to_path_buf(),
            manifest_path: temp.path().join("pyproject.toml"),
            lock_path: temp.path().join("px.lock"),
            name: "demo".to_string(),
            python_requirement: ">=3.11".to_string(),
            dependencies: Vec::new(),
            dependency_groups: Vec::new(),
            declared_dependency_groups: Vec::new(),
            dependency_group_source: DependencyGroupSource::None,
            group_dependencies: Vec::new(),
            requirements: Vec::new(),
            python_override: None,
            px_options: PxOptions::default(),
            manifest_fingerprint: "fp".to_string(),
        };

        let env_root = materialize_profile_env(
            &snapshot,
            &RuntimeMetadata {
                path: runtime_exe.display().to_string(),
                version: "3.11.0".to_string(),
                platform: "linux".to_string(),
            },
            &profile_header,
            &profile_obj.oid,
            &runtime_exe,
        )?;

        let env_script = env_root.join("bin").join("demo");
        let contents = fs::read_to_string(&env_script)?;
        let expected_shebang = format!("#!{}\n", env_root.join("bin").join("python").display());
        assert!(
            contents.starts_with(&expected_shebang),
            "shebang should point at env python"
        );
        assert!(
            contents.contains("hi from demo"),
            "script body should be preserved"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert!(
                !fs::symlink_metadata(&env_script)?.file_type().is_symlink(),
                "python bin shims should be copied, not linked"
            );
            let mode = fs::metadata(&env_script)?.mode();
            assert!(
                mode & 0o111 != 0,
                "rewritten script should remain executable"
            );
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn python_shim_applies_profile_env_vars() -> Result<()> {
        let temp = tempdir()?;
        let runtime_root = temp.path().join("runtime");
        let bin_dir = runtime_root.join("bin");
        fs::create_dir_all(&bin_dir)?;
        let runtime = bin_dir.join("python");
        fs::write(
            &runtime,
            "#!/usr/bin/env bash\nprintf \"%s\" \"$FOO_FROM_PROFILE\"\n",
        )?;
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o755))?;

        let env_root = temp.path().join("env");
        let env_bin = env_root.join("bin");
        let site = env_root.join("lib/python3.11/site-packages");
        fs::create_dir_all(&site)?;

        let mut env_vars = BTreeMap::new();
        env_vars.insert(
            "FOO_FROM_PROFILE".to_string(),
            Value::String("from_profile".to_string()),
        );
        write_python_shim(&env_bin, &runtime, &site, &env_vars)?;
        let shim = env_bin.join("python");
        let output = Command::new(&shim)
            .env("FOO_FROM_PROFILE", "ignored")
            .output()?;
        assert!(
            output.status.success(),
            "shim should run successfully: {:?}",
            output
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(
            stdout, "from_profile",
            "profile env vars should override parent values"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn python_shim_preserves_existing_pythonpath() -> Result<()> {
        let temp = tempdir()?;
        let runtime_root = temp.path().join("runtime");
        let bin_dir = runtime_root.join("bin");
        fs::create_dir_all(&bin_dir)?;
        let runtime = bin_dir.join("python");
        fs::write(
            &runtime,
            "#!/usr/bin/env bash\nprintf \"%s\" \"$PYTHONPATH\"\n",
        )?;
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o755))?;

        let env_root = temp.path().join("env");
        let env_bin = env_root.join("bin");
        let site = env_root.join("lib/python3.11/site-packages");
        fs::create_dir_all(&site)?;

        write_python_shim(&env_bin, &runtime, &site, &BTreeMap::new())?;
        let shim = env_bin.join("python");
        let existing = "/tmp/custom";
        let runtime_site = runtime_root.join("lib/python3.11/site-packages");
        let expected = format!("{existing}:{}:{}", site.display(), runtime_site.display());

        let output = Command::new(&shim).env("PYTHONPATH", existing).output()?;
        assert!(
            output.status.success(),
            "shim should run successfully: {:?}",
            output
        );
        let value = String::from_utf8_lossy(&output.stdout);
        assert_eq!(value, expected);
        Ok(())
    }

    #[test]
    fn profile_env_vars_merge_snapshot_and_env_override() -> Result<()> {
        let snapshot = px_domain::ProjectSnapshot {
            root: PathBuf::from("/tmp/demo"),
            manifest_path: PathBuf::from("/tmp/demo/pyproject.toml"),
            lock_path: PathBuf::from("/tmp/demo/px.lock"),
            name: "demo".to_string(),
            python_requirement: ">=3.11".to_string(),
            dependencies: vec![],
            dependency_groups: vec![],
            declared_dependency_groups: vec![],
            dependency_group_source: px_domain::project::manifest::DependencyGroupSource::None,
            group_dependencies: vec![],
            requirements: vec![],
            python_override: None,
            px_options: px_domain::PxOptions {
                manage_command: None,
                plugin_imports: vec![],
                env_vars: BTreeMap::from([("FROM_SNAPSHOT".to_string(), "snap".to_string())]),
            },
            manifest_fingerprint: "fp".to_string(),
        };
        let prev_env = env::var("PX_PROFILE_ENV_VARS").ok();
        env::set_var(
            "PX_PROFILE_ENV_VARS",
            r#"{"FROM_ENV":"env","FROM_SNAPSHOT":"override"}"#,
        );
        let merged = profile_env_vars(&snapshot)?;
        if let Some(val) = prev_env {
            env::set_var("PX_PROFILE_ENV_VARS", val);
        } else {
            env::remove_var("PX_PROFILE_ENV_VARS");
        }
        assert_eq!(
            merged.get("FROM_SNAPSHOT"),
            Some(&Value::String("override".to_string()))
        );
        assert_eq!(
            merged.get("FROM_ENV"),
            Some(&Value::String("env".to_string()))
        );
        Ok(())
    }
}
