use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Result};
use px_domain::api::{LockSnapshot, LockedArtifact};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use toml_edit::{DocumentMut, Item};

use crate::core::runtime::builder::builder_identity_for_runtime;
use crate::core::runtime::artifacts::archive_source_dir_for_sdist;
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
    if wants_project_pkg_build(&snapshot.manifest_path) {
        let project_pkg = ensure_project_pkg_build(
            cache_root,
            snapshot,
            runtime,
            &runtime_abi,
            &builder_id,
            &default_build_options_hash,
        )?;
        if sys_seen.insert(project_pkg.pkg_build_oid.clone()) {
            sys_path_order.push(project_pkg.pkg_build_oid.clone());
        }
        packages.push(project_pkg);
    }
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
        let build_from_source = {
            let url = artifact.url.to_ascii_lowercase();
            let filename = artifact.filename.to_ascii_lowercase();
            !filename.ends_with(".whl")
                || url.ends_with(".tar.gz")
                || url.ends_with(".tgz")
                || url.ends_with(".tar.bz2")
                || url.ends_with(".tar")
                || url.ends_with(".zip")
        };
        let mut build_options_hash = if artifact.build_options_hash.is_empty() {
            default_build_options_hash.clone()
        } else {
            artifact.build_options_hash.clone()
        };
        let mut builder_dist: Option<PathBuf> = None;
        let wheel_path = if build_from_source {
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
                    source_subdir: None,
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
        if build_options_hash.contains("native-libs") {
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

fn wants_project_pkg_build(manifest_path: &Path) -> bool {
    fn contains_native_sources(project_root: &Path) -> bool {
        fn should_skip_entry(entry: &walkdir::DirEntry) -> bool {
            let name = entry.file_name().to_str().unwrap_or_default();
            matches!(
                name,
                ".git"
                    | ".px"
                    | "__pycache__"
                    | ".pytest_cache"
                    | ".mypy_cache"
                    | ".ruff_cache"
                    | "tests"
                    | "test"
                    | ".cache"
                    | ".venv"
                    | ".tox"
                    | "target"
                    | "dist"
                    | "build"
                    | "node_modules"
                    | ".idea"
                    | ".vscode"
            ) || name == "px.lock"
                || name == "px.workspace.lock"
        }

        for entry in walkdir::WalkDir::new(project_root)
            .max_depth(5)
            .into_iter()
            .filter_entry(|entry| !should_skip_entry(entry))
            .flatten()
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let Some(ext) = entry.path().extension().and_then(|value| value.to_str()) else {
                continue;
            };
            if matches!(
                ext,
                "c" | "cc" | "cpp" | "cxx" | "h" | "hpp" | "hh" | "hxx" | "pyx" | "pxd"
            ) {
                return true;
            }
        }
        false
    }

    let Ok(contents) = fs::read_to_string(manifest_path) else {
        return false;
    };
    let Ok(doc) = contents.parse::<DocumentMut>() else {
        return false;
    };
    let build_system = doc.get("build-system").and_then(Item::as_table);
    let build_backend = build_system
        .and_then(|table| table.get("build-backend"))
        .and_then(Item::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if build_backend == "mesonpy" || build_backend == "maturin" {
        return true;
    }
    let requires = build_system
        .and_then(|table| table.get("requires"))
        .and_then(Item::as_array)
        .map(|array| {
            array
                .iter()
                .filter_map(|value| value.as_str())
                .map(|value| value.to_ascii_lowercase())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let is_setuptools = build_backend.starts_with("setuptools")
        || (build_backend.is_empty() && requires.iter().any(|req| req.contains("setuptools")));
    if is_setuptools
        && requires.iter().any(|req| {
            req.contains("cython") || req.contains("scikit-build-core") || req.contains("setuptools-rust")
        })
    {
        return true;
    }
    if let Some(root) = manifest_path.parent() {
        if contains_native_sources(root) {
            return true;
        }
    }
    requires
        .iter()
        .any(|req| req.contains("meson-python") || req.contains("maturin"))
}

#[cfg(test)]
mod tests {
    use super::wants_project_pkg_build;

    use std::fs;

    use tempfile::tempdir;

    #[test]
    fn project_pkg_build_ignores_native_sources_in_tests_dirs() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path();
        fs::write(
            root.join("pyproject.toml"),
            r#"[build-system]
requires = ["setuptools>=42"]
"#,
        )
        .expect("write pyproject");
        fs::create_dir_all(root.join("tests")).expect("create tests");
        fs::write(root.join("tests/demo.pyx"), "print('demo')\n").expect("write pyx");

        assert!(
            !wants_project_pkg_build(&root.join("pyproject.toml")),
            "native sources under tests/ should not force a project wheel build"
        );

        fs::create_dir_all(root.join("src")).expect("create src");
        fs::write(root.join("src/native.c"), "/* native */\n").expect("write native");

        assert!(
            wants_project_pkg_build(&root.join("pyproject.toml")),
            "native sources under src/ should force a project wheel build"
        );
    }
}

fn ensure_project_pkg_build(
    cache_root: &Path,
    snapshot: &ManifestSnapshot,
    runtime: &RuntimeMetadata,
    runtime_abi: &str,
    builder_id: &str,
    default_build_options_hash: &str,
) -> Result<ProfilePackage> {
    fn wheel_metadata(dist_dir: &Path) -> Result<(String, String)> {
        let meta = fs::read_dir(dist_dir)?
            .flatten()
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with(".dist-info"))
            })
            .ok_or_else(|| anyhow!("wheel dist-info missing in {}", dist_dir.display()))?;
        let metadata = meta.join("METADATA");
        let contents = fs::read_to_string(&metadata)
            .map_err(|err| anyhow!(err).context(format!("reading {}", metadata.display())))?;
        let mut name = String::new();
        let mut version = String::new();
        for line in contents.lines() {
            if let Some(value) = line.strip_prefix("Name:") {
                name = value.trim().to_string();
            }
            if let Some(value) = line.strip_prefix("Version:") {
                version = value.trim().to_string();
            }
            if !name.is_empty() && !version.is_empty() {
                break;
            }
        }
        if name.is_empty() || version.is_empty() {
            return Err(anyhow!(
                "wheel metadata missing name/version in {}",
                metadata.display()
            ));
        }
        Ok((name, version))
    }

    fn setuptools_scm_root(snapshot: &ManifestSnapshot) -> Option<PathBuf> {
        let contents = fs::read_to_string(&snapshot.manifest_path).ok()?;
        let doc = contents.parse::<DocumentMut>().ok()?;
        let build_system = doc.get("build-system").and_then(Item::as_table);
        let build_backend = build_system
            .and_then(|table| table.get("build-backend"))
            .and_then(Item::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        let requires = build_system
            .and_then(|table| table.get("requires"))
            .and_then(Item::as_array)
            .map(|array| {
                array
                    .iter()
                    .filter_map(|value| value.as_str())
                    .map(|value| value.to_ascii_lowercase())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let is_setuptools = build_backend.starts_with("setuptools")
            || (build_backend.is_empty() && requires.iter().any(|req| req.contains("setuptools")));
        if !is_setuptools {
            return None;
        }
        let root = doc
            .get("tool")
            .and_then(Item::as_table)
            .and_then(|table| table.get("setuptools_scm"))
            .and_then(Item::as_table)
            .and_then(|table| table.get("root"))
            .and_then(Item::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty() && *value != ".");
        let root = root?;
        let candidate = snapshot.root.join(root);
        fs::canonicalize(candidate).ok()
    }

    let archive_prefix = format!("{}-0.0.0", snapshot.name);
    let snapshot_root_canon =
        fs::canonicalize(&snapshot.root).unwrap_or_else(|_| snapshot.root.clone());
    let (archive_root, source_subdir) = match setuptools_scm_root(snapshot) {
        Some(root) => {
            let root_canon = fs::canonicalize(&root).unwrap_or(root);
            match snapshot_root_canon.strip_prefix(&root_canon) {
                Ok(rel) => {
                    let rel = rel.to_string_lossy().replace('\\', "/");
                    let rel = rel.trim_matches('/').to_string();
                    let subdir = if rel.is_empty() { None } else { Some(rel) };
                    (root_canon, subdir)
                }
                Err(_) => (snapshot.root.clone(), None),
            }
        }
        None => (snapshot.root.clone(), None),
    };
    let archive = archive_source_dir_for_sdist(&archive_root, &archive_prefix)?;
    let sha256 = hex::encode(Sha256::digest(&archive));
    let version_id = format!("0.0.0+px{}", sha256.chars().take(16).collect::<String>());
    let filename = format!("{}-{}.tar.gz", snapshot.name, version_id);

    let archive_root = cache_root.join("project-sources").join(&snapshot.name);
    fs::create_dir_all(&archive_root)?;
    let archive_path = archive_root.join(format!("{sha256}.tar.gz"));
    if !archive_path.exists() {
        fs::write(&archive_path, &archive)?;
    }

    let built = ensure_sdist_build(
        cache_root,
        &SdistRequest {
            normalized_name: &snapshot.name,
            version: &version_id,
            filename: &filename,
            url: &archive_path.display().to_string(),
            sha256: Some(&sha256),
            source_subdir: source_subdir.as_deref(),
            python_path: &runtime.path,
            builder_id,
            builder_root: Some(cache_root.to_path_buf()),
        },
    )?;
    let wheel_filename = built.filename.clone();
    let wheel_sha256 = built.sha256.clone();
    let wheel_path = built.cached_path.clone();
    let dist_dir = built.dist_path.clone();
    let build_options_hash = if built.build_options_hash.is_empty() {
        default_build_options_hash.to_string()
    } else {
        built.build_options_hash.clone()
    };
    let (name, version) = wheel_metadata(&dist_dir)?;

    let store = global_store();
    let source_header = SourceHeader {
        name: name.clone(),
        version: version.clone(),
        filename: wheel_filename,
        index_url: archive_path.display().to_string(),
        sha256: wheel_sha256,
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
        runtime_abi: runtime_abi.to_string(),
        builder_id: builder_id.to_string(),
        build_options_hash,
    };
    let pkg_key = pkg_build_lookup_key(&pkg_header);
    let pkg_oid = match store.lookup_key(ObjectKind::PkgBuild, &pkg_key)? {
        Some(existing) => existing,
        None => {
            let (stage_guard, staging_root) = stage_pkg_build(&dist_dir)?;
            let archive = archive_dir_canonical(&staging_root)?;
            drop(stage_guard);
            let payload = ObjectPayload::PkgBuild {
                header: pkg_header,
                archive: Cow::Owned(archive),
            };
            let stored = store.store(&payload)?;
            store.record_key(ObjectKind::PkgBuild, &pkg_key, &stored.oid)?;
            stored.oid
        }
    };

    Ok(ProfilePackage {
        name,
        version,
        pkg_build_oid: pkg_oid,
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
