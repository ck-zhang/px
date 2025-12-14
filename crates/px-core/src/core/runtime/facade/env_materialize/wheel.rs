// Wheel build + caching helpers for project materialization.
use super::*;

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
struct WheelCacheMeta {
    wheel: String,
    sha256: String,
    name: Option<String>,
    version: Option<String>,
}

pub(in super::super) fn project_wheel_cache_dir(
    cache_root: &Path,
    snapshot: &ManifestSnapshot,
    runtime: &RuntimeMetadata,
    python: &Path,
    keep_proxies: bool,
    build_hash: &str,
) -> PathBuf {
    let cache_key = format!(
        "{}:{}:{}:{}:{}:{}",
        snapshot.manifest_fingerprint,
        runtime.version,
        runtime.platform,
        python.display(),
        keep_proxies,
        build_hash
    );
    let key_hash = hex::encode(Sha256::digest(cache_key.as_bytes()));
    cache_root
        .join("project-wheels")
        .join(normalize_project_name(&snapshot.name))
        .join(key_hash)
}

pub(in super::super) fn cached_project_wheel(dir: &Path) -> Result<Option<PathBuf>> {
    let meta_path = dir.join("wheel.json");
    if meta_path.exists() {
        let contents = fs::read_to_string(&meta_path)?;
        let meta: WheelCacheMeta = serde_json::from_str(&contents).unwrap_or_default();
        if !meta.wheel.is_empty() && !meta.sha256.is_empty() {
            let wheel_path = dir.join(&meta.wheel);
            if wheel_path.exists()
                && compute_file_sha256(&wheel_path).ok().as_deref() == Some(meta.sha256.as_str())
            {
                ensure_nonempty_wheel(&wheel_path)?;
                return Ok(Some(wheel_path));
            }
        }
    }

    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(None);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("whl"))
        {
            continue;
        }
        ensure_nonempty_wheel(&path)?;
        let sha = compute_file_sha256(&path)?;
        if let Ok(dist_dir) = ensure_wheel_dist(&path, &sha) {
            if let Ok((name, version)) = wheel_metadata(&dist_dir) {
                let _ = persist_wheel_metadata(dir, &path, &sha, &name, &version);
            }
        }
        return Ok(Some(path));
    }
    Ok(None)
}

fn reuse_cached_project_wheel(
    cache_root: &Path,
    snapshot: &ManifestSnapshot,
    target_dir: &Path,
) -> Result<Option<PathBuf>> {
    let project_dir = cache_root
        .join("project-wheels")
        .join(normalize_project_name(&snapshot.name));
    let mut candidates = match fs::read_dir(project_dir) {
        Ok(entries) => entries
            .flatten()
            .map(|entry| entry.path())
            .collect::<Vec<_>>(),
        Err(_) => Vec::new(),
    };
    candidates.sort();

    for candidate in candidates {
        if candidate == target_dir || !candidate.is_dir() {
            continue;
        }
        if let Some(existing) = cached_project_wheel(&candidate)? {
            let Some(filename) = existing.file_name() else {
                continue;
            };
            fs::create_dir_all(target_dir)?;
            let dest = target_dir.join(filename);
            fs::copy(&existing, &dest)?;
            let sha256 = compute_file_sha256(&dest)?;
            let dist_dir = ensure_wheel_dist(&dest, &sha256)?;
            let (name, version) = wheel_metadata(&dist_dir)?;
            persist_wheel_metadata(target_dir, &dest, &sha256, &name, &version)?;
            return Ok(Some(dest));
        }
    }

    Ok(None)
}

pub(in super::super) fn ensure_project_wheel_scripts(
    cache_root: &Path,
    snapshot: &ManifestSnapshot,
    env_root: &Path,
    runtime: &RuntimeMetadata,
    env_owner: &OwnerId,
    profile_oid: Option<&str>,
) -> Result<bool> {
    if !uses_maturin_backend(&snapshot.manifest_path)? {
        return Ok(false);
    }

    let python = env_root.join("bin").join("python");
    let python_path = if python.exists() {
        python
    } else {
        PathBuf::from(&runtime.path)
    };
    let keep_proxies = env::var("PX_KEEP_PROXIES")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let builder = builder_identity_for_runtime(runtime)?;
    let build_hash = project_build_hash(
        runtime,
        snapshot,
        &python_path,
        keep_proxies,
        &builder.builder_id,
    )?;
    let cache_dir = project_wheel_cache_dir(
        cache_root,
        snapshot,
        runtime,
        &python_path,
        keep_proxies,
        &build_hash,
    );
    fs::create_dir_all(&cache_dir)?;

    let store = global_store();
    let lock_id = format!(
        "project-wheel:{}:{}",
        snapshot.name,
        cache_dir
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("cache")
    );
    let _lock = store.acquire_lock(&lock_id)?;

    let wheel_path = match cached_project_wheel(&cache_dir)? {
        Some(path) => path,
        None => match reuse_cached_project_wheel(cache_root, snapshot, &cache_dir)? {
            Some(path) => path,
            None => build_project_wheel(
                &python_path,
                &snapshot.root,
                cache_root,
                &cache_dir,
                keep_proxies,
                &builder.builder_id,
            )?,
        },
    };

    let bin_dir = env_root.join("bin");
    let shebang_python = bin_dir.join("python");
    let python_for_scripts = shebang_python.exists().then_some(shebang_python.as_path());
    let sha256 = compute_file_sha256(&wheel_path)?;
    let dist_dir = ensure_wheel_dist(&wheel_path, &sha256)?;
    let (pkg_name, pkg_version) = wheel_metadata(&dist_dir)?;
    let pkg_oid =
        store_project_wheel_in_cas(&wheel_path, &sha256, &dist_dir, &python_path, &build_hash)?;

    materialize_wheel_scripts(&dist_dir, &bin_dir, python_for_scripts)
        .with_context(|| format!("installing project scripts from {}", wheel_path.display()))?;
    persist_wheel_metadata(&cache_dir, &wheel_path, &sha256, &pkg_name, &pkg_version)?;
    let _ = store.add_ref(env_owner, &pkg_oid);
    if let Some(profile) = profile_oid {
        let profile_owner = OwnerId {
            owner_type: OwnerType::Profile,
            owner_id: profile.to_string(),
        };
        let _ = store.add_ref(&profile_owner, &pkg_oid);
    }
    Ok(true)
}

fn strip_proxy_env(cmd: &mut Command) {
    for key in [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        cmd.env_remove(key);
    }
}

pub(in super::super) fn compute_file_sha256(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn ensure_nonempty_wheel(path: &Path) -> Result<()> {
    let file = File::open(path)?;
    let archive = zip::ZipArchive::new(file)?;
    if archive.is_empty() {
        bail!(
            "project wheel build produced an empty archive at {}",
            path.display()
        );
    }
    Ok(())
}

fn build_project_wheel(
    python: &Path,
    project_root: &Path,
    cache_root: &Path,
    out_dir: &Path,
    keep_proxies: bool,
    builder_id: &str,
) -> Result<PathBuf> {
    fs::create_dir_all(out_dir)?;
    let staging = out_dir.join("build");
    if staging.exists() {
        let _ = fs::remove_dir_all(&staging);
    }
    fs::create_dir_all(&staging)?;

    let mut build_cmd = Command::new(python);
    build_cmd
        .arg("-m")
        .arg("build")
        .arg("--sdist")
        .arg("--outdir")
        .arg(&staging)
        .arg(project_root);
    build_cmd.current_dir(project_root);
    if !keep_proxies {
        strip_proxy_env(&mut build_cmd);
    }
    let build_output = build_cmd.output().with_context(|| {
        format!(
            "running python -m build --sdist in {}",
            project_root.display()
        )
    })?;
    if !build_output.status.success() {
        let build_stderr = String::from_utf8_lossy(&build_output.stderr);
        bail!("failed to build project sdist: {build_stderr}");
    }
    let sdist = find_sdist_in_dir(&staging)?;
    let sdist_sha = compute_file_sha256(&sdist)?;
    let filename = sdist
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let sdist_url = format!("file://{}", sdist.display());
    let (normalized_name, version) = parse_sdist_name_version(&filename);
    let built = ensure_sdist_build(
        cache_root,
        &SdistRequest {
            normalized_name: &normalized_name,
            version: &version,
            filename: &filename,
            url: &sdist_url,
            sha256: Some(&sdist_sha),
            python_path: &python.display().to_string(),
            builder_id,
            builder_root: Some(cache_root.to_path_buf()),
        },
    )?;
    let final_path = out_dir.join(&built.filename);
    if let Some(parent) = final_path.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::rename(&built.cached_path, &final_path) {
        Ok(_) => {}
        Err(_) => {
            fs::copy(&built.cached_path, &final_path)?;
        }
    }
    let _ = fs::remove_dir_all(&staging);
    Ok(final_path)
}

fn find_sdist_in_dir(dir: &Path) -> Result<PathBuf> {
    let sdist = fs::read_dir(dir)?
        .flatten()
        .map(|entry| entry.path())
        .find(|path| {
            path.extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| {
                    ext.eq_ignore_ascii_case("gz") || ext.eq_ignore_ascii_case("zip")
                })
        })
        .ok_or_else(|| anyhow!("project sdist not found in {}", dir.display()))?;
    Ok(sdist)
}

fn parse_sdist_name_version(filename: &str) -> (String, String) {
    let trimmed = filename
        .trim_end_matches(".tar.gz")
        .trim_end_matches(".zip")
        .to_string();
    let mut parts: Vec<&str> = trimmed.rsplitn(2, '-').collect();
    if parts.len() == 2 {
        let version = parts.remove(0).to_string();
        let name = canonicalize_package_name(parts.remove(0));
        (name, version)
    } else {
        (canonicalize_package_name(&trimmed), "0.0.0".to_string())
    }
}

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
        .with_context(|| format!("reading wheel metadata at {}", metadata.display()))?;
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
        bail!(
            "wheel metadata missing name/version in {}",
            metadata.display()
        );
    }
    Ok((name, version))
}

pub(in super::super) fn persist_wheel_metadata(
    cache_dir: &Path,
    wheel_path: &Path,
    sha256: &str,
    name: &str,
    version: &str,
) -> Result<()> {
    let meta = WheelCacheMeta {
        wheel: wheel_path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default(),
        sha256: sha256.to_string(),
        name: Some(name.to_string()),
        version: Some(version.to_string()),
    };
    let body = serde_json::to_string_pretty(&meta)?;
    fs::write(cache_dir.join("wheel.json"), body)?;
    Ok(())
}

pub(in super::super) fn project_build_hash(
    runtime: &RuntimeMetadata,
    snapshot: &ManifestSnapshot,
    python: &Path,
    keep_proxies: bool,
    builder_id: &str,
) -> Result<String> {
    let build_env_hash = wheel_build_options_hash(&python.display().to_string())?;
    let payload = json!({
        "runtime": runtime.version,
        "platform": runtime.platform,
        "manifest": snapshot.manifest_fingerprint,
        "python": python.display().to_string(),
        "keep_proxies": keep_proxies,
        "builder_id": builder_id,
        "build_env": build_env_hash,
    });
    Ok(hex::encode(Sha256::digest(
        serde_json::to_vec(&payload).unwrap_or_default(),
    )))
}

fn store_project_wheel_in_cas(
    wheel_path: &Path,
    sha256: &str,
    dist_dir: &Path,
    python: &Path,
    build_options_hash: &str,
) -> Result<String> {
    let (name, version) = wheel_metadata(dist_dir)?;
    let bytes = fs::read(wheel_path)?;
    let header = SourceHeader {
        name: name.clone(),
        version: version.clone(),
        filename: wheel_path
            .file_name()
            .map(|v| v.to_string_lossy().to_string())
            .unwrap_or_else(|| "wheel.whl".into()),
        index_url: format!("file://{}", wheel_path.display()),
        sha256: sha256.to_string(),
    };
    let store = global_store();
    let source_key = source_lookup_key(&header);
    let source_oid = match store.lookup_key(ObjectKind::Source, &source_key)? {
        Some(existing) => existing,
        None => {
            let payload = ObjectPayload::Source {
                header: header.clone(),
                bytes: Cow::Owned(bytes),
            };
            let stored = store.store(&payload)?;
            store.record_key(ObjectKind::Source, &source_key, &stored.oid)?;
            stored.oid
        }
    };

    let builder = builder_identity_for_python(&python.display().to_string())?;
    let pkg_header = PkgBuildHeader {
        source_oid,
        runtime_abi: builder.runtime_abi,
        builder_id: builder.builder_id,
        build_options_hash: build_options_hash.to_string(),
    };
    let pkg_key = pkg_build_lookup_key(&pkg_header);
    let pkg_oid = match store.lookup_key(ObjectKind::PkgBuild, &pkg_key)? {
        Some(existing) => existing,
        None => {
            let archive = archive_dir_canonical(dist_dir)?;
            let payload = ObjectPayload::PkgBuild {
                header: pkg_header,
                archive: Cow::Owned(archive),
            };
            let stored = store.store(&payload)?;
            store.record_key(ObjectKind::PkgBuild, &pkg_key, &stored.oid)?;
            stored.oid
        }
    };
    Ok(pkg_oid)
}
