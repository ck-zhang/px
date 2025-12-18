// Refresh/seed project site (pip/setuptools/uv) and refresh CAS materialization.
use super::*;

pub(crate) fn refresh_project_site(
    snapshot: &ManifestSnapshot,
    ctx: &CommandContext,
) -> Result<()> {
    fn lock_dependency_versions(lock: &px_domain::api::LockSnapshot) -> HashMap<String, String> {
        let mut versions = HashMap::new();
        for spec in &lock.dependencies {
            let head = spec.split(';').next().unwrap_or(spec).trim();
            if let Some((name_part, ver_part)) = head.split_once("==") {
                let name = dependency_name(name_part).to_ascii_lowercase();
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
        versions
    }

    let previous_env = load_project_state(ctx.fs(), &snapshot.root)
        .ok()
        .and_then(|state| state.current_env);
    let _ = prepare_project_runtime(snapshot)?;
    let mut lock = load_lockfile_optional(&snapshot.lock_path)?.ok_or_else(|| {
        anyhow!(
            "px sync: lockfile missing at {}",
            snapshot.lock_path.display()
        )
    })?;
    let cache_versions = lock_dependency_versions(&lock);
    let mut cached_path_updates: HashMap<String, String> = HashMap::new();
    for dep in lock.resolved.iter_mut() {
        let Some(artifact) = dep.artifact.as_mut() else {
            continue;
        };
        if artifact
            .build_options_hash
            .to_ascii_lowercase()
            .contains("native-libs")
        {
            continue;
        }
        if artifact.filename.is_empty() || artifact.sha256.is_empty() {
            continue;
        }
        let existing_valid = if artifact.cached_path.is_empty() {
            false
        } else {
            let existing = Path::new(&artifact.cached_path);
            crate::store::validate_existing(existing, &artifact.sha256)
                .ok()
                .flatten()
                .is_some()
        };
        if existing_valid {
            continue;
        }
        let Some(version) = cache_versions.get(&dependency_name(&dep.name).to_ascii_lowercase())
        else {
            continue;
        };
        let dest =
            crate::store::wheel_path(&ctx.cache().path, &dep.name, version, &artifact.filename);
        let dest_str = dest.display().to_string();
        if artifact.cached_path != dest_str {
            artifact.cached_path = dest_str.clone();
            cached_path_updates.insert(dep.name.to_ascii_lowercase(), dest_str);
        }
    }
    let runtime = detect_runtime_metadata(ctx, snapshot)?;
    let lock_id = match lock.lock_id.clone() {
        Some(value) => value,
        None => compute_lock_hash(&snapshot.lock_path)?,
    };
    let env_owner = OwnerId {
        owner_type: OwnerType::ProjectEnv,
        owner_id: project_env_owner_id(&snapshot.root, &lock_id, &runtime.version)?,
    };
    let cas_profile = ensure_profile_env(ctx, snapshot, &lock, &runtime, &env_owner)?;

    if !cached_path_updates.is_empty() {
        let contents = fs::read_to_string(&snapshot.lock_path)?;
        let mut doc: DocumentMut = contents.parse()?;
        if let Some(tables) = doc
            .get_mut("dependencies")
            .and_then(Item::as_array_of_tables_mut)
        {
            for table in tables.iter_mut() {
                let specifier = table
                    .get("specifier")
                    .and_then(Item::as_str)
                    .unwrap_or_default();
                let name = table
                    .get("name")
                    .and_then(Item::as_str)
                    .map(std::string::ToString::to_string)
                    .unwrap_or_else(|| dependency_name(specifier));
                let lookup = name.to_ascii_lowercase();
                let Some(updated) = cached_path_updates.get(&lookup) else {
                    continue;
                };
                let Some(artifact) = table.get_mut("artifact").and_then(Item::as_table_mut) else {
                    continue;
                };
                artifact.insert("cached_path", Item::Value(TomlValue::from(updated.clone())));
            }
            fs::write(&snapshot.lock_path, doc.to_string())?;
        }
    }
    write_project_metadata_stub(snapshot, &cas_profile.env_path, ctx.fs())?;
    let env_python = write_python_environment_markers(
        &cas_profile.env_path,
        &runtime,
        &cas_profile.runtime_path,
        ctx.fs(),
    )?;
    ensure_project_pip(ctx, snapshot, &cas_profile.env_path, &runtime, &env_python)?;
    ensure_project_wheel_scripts(
        &ctx.cache().path,
        snapshot,
        &cas_profile.env_path,
        &runtime,
        &env_owner,
        Some(&cas_profile.profile_oid),
    )?;
    let runtime_state = StoredRuntime {
        path: cas_profile.runtime_path.display().to_string(),
        version: runtime.version.clone(),
        platform: runtime.platform.clone(),
    };
    let local_envs = snapshot.root.join(".px").join("envs");
    ctx.fs().create_dir_all(&local_envs)?;
    let current = local_envs.join("current");
    if current.exists() {
        let _ = fs::remove_file(&current).or_else(|_| fs::remove_dir_all(&current));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let _ = symlink(&cas_profile.env_path, &current);
    }
    #[cfg(not(unix))]
    {
        let _ = fs::remove_dir_all(&current);
        let _ = fs::hard_link(&cas_profile.env_path, &current);
    }
    let site_packages = site_packages_dir(&current, &runtime.version);
    let env_state = StoredEnvironment {
        id: cas_profile.profile_oid.clone(),
        lock_id,
        platform: runtime.platform.clone(),
        site_packages: site_packages.display().to_string(),
        env_path: Some(current.display().to_string()),
        profile_oid: Some(cas_profile.profile_oid.clone()),
        python: StoredPython {
            path: env_python.display().to_string(),
            version: runtime.version.clone(),
        },
    };
    persist_project_state(ctx.fs(), &snapshot.root, env_state, runtime_state)?;

    if let Some(prev) = previous_env {
        if let Some(prev_profile) = prev.profile_oid.as_deref() {
            if prev_profile != cas_profile.profile_oid {
                let store = global_store();
                if let Ok(prev_owner_id) =
                    project_env_owner_id(&snapshot.root, &prev.lock_id, &prev.python.version)
                {
                    let prev_owner = OwnerId {
                        owner_type: OwnerType::ProjectEnv,
                        owner_id: prev_owner_id,
                    };
                    if store.remove_ref(&prev_owner, prev_profile)?
                        && store.refs_for(prev_profile)?.is_empty()
                    {
                        let profile_owner = OwnerId {
                            owner_type: OwnerType::Profile,
                            owner_id: prev_profile.to_string(),
                        };
                        let _ = store.remove_owner_refs(&profile_owner)?;
                        let _ = store.remove_env_materialization(prev_profile);
                    }
                }
            }
        }
    }

    let _ = run_gc_with_env_policy(global_store());
    Ok(())
}

pub(in super::super) fn project_site_env(
    ctx: &CommandContext,
    snapshot: &ManifestSnapshot,
    site_dir: &Path,
    env_python: &Path,
) -> Result<Vec<(String, String)>> {
    let paths = build_pythonpath(ctx.fs(), &snapshot.root, Some(site_dir.to_path_buf()))?;
    let allowed = env::join_paths(&paths.allowed_paths)
        .context("allowed path contains invalid UTF-8")?
        .into_string()
        .map_err(|_| anyhow!("allowed path contains non-utf8 data"))?;
    let mut envs = vec![
        ("PYTHONPATH".into(), paths.pythonpath.clone()),
        ("PYTHONUNBUFFERED".into(), "1".into()),
        ("PYTHONDONTWRITEBYTECODE".into(), "1".into()),
        ("PYTHONUSERBASE".into(), site_dir.display().to_string()),
        ("PYTHONNOUSERSITE".into(), "1".into()),
        ("PX_ALLOWED_PATHS".into(), allowed),
        (
            "PX_PROJECT_ROOT".into(),
            snapshot.root.display().to_string(),
        ),
        ("PX_PYTHON".into(), env_python.display().to_string()),
    ];
    if let Some(bin) = &paths.site_bin {
        let mut path_entries = vec![bin.clone()];
        if let Some(site_root) = bin.parent() {
            envs.push(("VIRTUAL_ENV".into(), site_root.display().to_string()));
        }
        if let Ok(existing) = env::var("PATH") {
            path_entries.extend(env::split_paths(&existing));
        }
        if let Ok(joined) = env::join_paths(path_entries) {
            if let Ok(value) = joined.into_string() {
                envs.push(("PATH".into(), value));
            }
        }
    }
    disable_proxy_env(&mut envs);
    Ok(envs)
}

fn ensure_project_pip(
    ctx: &CommandContext,
    snapshot: &ManifestSnapshot,
    site_dir: &Path,
    runtime: &RuntimeMetadata,
    env_python: &Path,
) -> Result<()> {
    fn set_env_pair(envs: &mut Vec<(String, String)>, key: &str, value: String) {
        if let Some((_, existing)) = envs.iter_mut().find(|(k, _)| k == key) {
            *existing = value;
        } else {
            envs.push((key.to_string(), value));
        }
    }

    fn pip_bootstrap_env(envs: &[(String, String)]) -> Vec<(String, String)> {
        let mut sanitized = Vec::with_capacity(envs.len() + 1);
        for (key, value) in envs {
            if key == "PYTHONPATH" {
                continue;
            }
            sanitized.push((key.clone(), value.clone()));
        }
        if !sanitized.iter().any(|(key, _)| key == "PYTHONSAFEPATH") {
            sanitized.push(("PYTHONSAFEPATH".into(), "1".into()));
        }
        sanitized
    }

    let debug_pip = std::env::var("PX_DEBUG_PIP").is_ok();
    let skip_ensurepip = std::env::var("PX_NO_ENSUREPIP")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let site_packages = site_packages_dir(site_dir, &runtime.version);
    let sitecustomize = site_dir.join("sitecustomize.py");
    if !sitecustomize.exists() {
        ctx.fs().create_dir_all(site_dir)?;
        ctx.fs().write(&sitecustomize, SITE_CUSTOMIZE.as_bytes())?;
    }
    let sitecustomize_site_packages = site_packages.join("sitecustomize.py");
    if !sitecustomize_site_packages.exists() {
        ctx.fs().create_dir_all(&site_packages)?;
        ctx.fs()
            .write(&sitecustomize_site_packages, SITE_CUSTOMIZE.as_bytes())?;
    }
    let pip_installed = has_pip_in_site(&site_packages);
    let pip_editable = has_px_editable_stub(site_dir, &normalize_project_name("pip"));
    let pip_entrypoints =
        site_dir.join("bin").join("pip").exists() || site_dir.join("bin").join("pip3").exists();
    if debug_pip {
        eprintln!(
            "pip bootstrap: installed={pip_installed} editable={pip_editable} entrypoints={pip_entrypoints} site={} root={}",
            site_packages.display(),
            site_dir.display()
        );
    }

    if pip_editable {
        return Ok(());
    }

    let envs = project_site_env(ctx, snapshot, site_dir, env_python)?;
    let mut envs = envs;
    set_env_pair(&mut envs, "PX_PYTHON", runtime.path.clone());
    let mut pip_main_available =
        module_available(ctx, snapshot, env_python, &envs, "pip.__main__")?;

    if skip_ensurepip {
        if !pip_entrypoints || !pip_main_available {
            link_runtime_pip(
                &site_packages,
                &site_dir.join("bin"),
                Path::new(&runtime.path),
                &runtime.version,
            )?;
            pip_main_available =
                module_available(ctx, snapshot, env_python, &envs, "pip.__main__")?;
        }
        if !module_available(ctx, snapshot, env_python, &envs, "pip")? || !pip_main_available {
            return Err(InstallUserError::new(
                "environment missing baseline packaging support",
                json!({
                    "missing": ["pip"],
                    "reason": "missing_pip",
                    "hint": "unset PX_NO_ENSUREPIP or ensure the runtime provides pip so px can seed setuptools",
                    "code": diagnostics::cas::MISSING_OR_CORRUPT,
                }),
            )
            .into());
        }
        ensure_setuptools_seed(ctx, snapshot, &site_packages, env_python, &envs)?;
        ensure_uv_seed(ctx, snapshot, site_dir, env_python, &envs)?;
        ensure_build_tooling_seed(ctx, snapshot, &site_packages, env_python, &envs, runtime)?;
        return Ok(());
    }

    if !pip_entrypoints || !pip_main_available {
        if !pip_main_available {
            let bootstrap_envs = pip_bootstrap_env(&envs);
            let output = ctx.python_runtime().run_command(
                env_python
                    .to_str()
                    .ok_or_else(|| anyhow!("invalid python path"))?,
                &[
                    "-m".to_string(),
                    "ensurepip".to_string(),
                    "--default-pip".to_string(),
                    "--upgrade".to_string(),
                    "--user".to_string(),
                ],
                &bootstrap_envs,
                &snapshot.root,
            )?;
            if output.code != 0 {
                let mut message = String::from("failed to bootstrap pip in the px environment");
                if !output.stderr.trim().is_empty() {
                    message.push_str(": ");
                    message.push_str(output.stderr.trim());
                }
                if output.stderr.trim().is_empty() && !output.stdout.trim().is_empty() {
                    message.push_str(": ");
                    message.push_str(output.stdout.trim());
                }
                if debug_pip {
                    eprintln!(
                        "ensurepip failed stdout={}, stderr={}",
                        output.stdout, output.stderr
                    );
                }
                bail!(message);
            }
            if debug_pip {
                eprintln!(
                    "ensurepip ok stdout={}, stderr={}",
                    output.stdout.trim(),
                    output.stderr.trim()
                );
            }
        }

        link_runtime_pip(
            &site_packages,
            &site_dir.join("bin"),
            Path::new(&runtime.path),
            &runtime.version,
        )?;
        pip_main_available = module_available(ctx, snapshot, env_python, &envs, "pip.__main__")?;
        if debug_pip {
            let after_link = has_pip_in_site(&site_packages);
            eprintln!(
                "post-pip-bootstrap pip_present={} entrypoints={} main_available={}",
                after_link, pip_entrypoints, pip_main_available
            );
        }
        if !module_available(ctx, snapshot, env_python, &envs, "pip")? || !pip_main_available {
            bail!("failed to bootstrap pip in the px environment: pip not available");
        }
    }
    ensure_setuptools_seed(ctx, snapshot, &site_packages, env_python, &envs)?;
    ensure_uv_seed(ctx, snapshot, site_dir, env_python, &envs)?;
    ensure_build_tooling_seed(ctx, snapshot, &site_packages, env_python, &envs, runtime)?;

    Ok(())
}

pub(in super::super) fn module_available(
    ctx: &CommandContext,
    snapshot: &ManifestSnapshot,
    env_python: &Path,
    envs: &[(String, String)],
    module: &str,
) -> Result<bool> {
    let script = format!(
        "import importlib.util, sys; sys.exit(0 if importlib.util.find_spec({module:?}) else 1)"
    );
    let output = ctx.python_runtime().run_command(
        env_python
            .to_str()
            .ok_or_else(|| anyhow!("invalid python path"))?,
        &["-c".to_string(), script],
        envs,
        &snapshot.root,
    )?;
    Ok(output.code == 0)
}

fn ensure_setuptools_seed(
    ctx: &CommandContext,
    snapshot: &ManifestSnapshot,
    site_packages: &Path,
    env_python: &Path,
    envs: &[(String, String)],
) -> Result<()> {
    if module_available(ctx, snapshot, env_python, envs, "setuptools")? {
        return Ok(());
    }

    let release = ctx
        .pypi()
        .fetch_release(
            "setuptools",
            SETUPTOOLS_SEED_VERSION,
            &format!("setuptools=={SETUPTOOLS_SEED_VERSION}"),
        )
        .map_err(|err| {
            InstallUserError::new(
                "failed to locate setuptools for the px environment",
                json!({
                    "package": "setuptools",
                    "version": SETUPTOOLS_SEED_VERSION,
                    "error": err.to_string(),
                    "reason": "missing_artifacts",
                    "hint": "ensure network access or prefetch artifacts, then rerun `px sync`",
                }),
            )
        })?;
    let wheel = release
        .urls
        .iter()
        .filter(|file| file.packagetype == "bdist_wheel" && !file.yanked.unwrap_or(false))
        .find(|file| file.filename.ends_with("py3-none-any.whl"))
        .or_else(|| {
            release
                .urls
                .iter()
                .find(|file| file.packagetype == "bdist_wheel" && !file.yanked.unwrap_or(false))
        })
        .cloned()
        .ok_or_else(|| {
            InstallUserError::new(
                "setuptools wheel unavailable for the px environment",
                json!({
                    "package": "setuptools",
                    "version": SETUPTOOLS_SEED_VERSION,
                    "reason": "missing_artifacts",
                    "hint": "rerun with network access to refresh the wheel cache",
                }),
            )
        })?;
    let filename = wheel.filename.clone();
    let url = wheel.url.clone();
    let sha256 = wheel.digests.sha256.clone();
    let request = ArtifactRequest {
        name: "setuptools",
        version: SETUPTOOLS_SEED_VERSION,
        filename: &filename,
        url: &url,
        sha256: &sha256,
    };
    let cached = ctx
        .cache_store()
        .cache_wheel(&ctx.cache().path, &request)
        .map_err(|err| {
            InstallUserError::new(
                "failed to cache setuptools for the px environment",
                json!({
                    "package": "setuptools",
                    "version": SETUPTOOLS_SEED_VERSION,
                    "error": err.to_string(),
                    "reason": "missing_artifacts",
                    "hint": "rerun with network access to refresh the cache",
                }),
            )
        })?;

    let output = ctx.python_runtime().run_command(
        env_python
            .to_str()
            .ok_or_else(|| anyhow!("invalid python path"))?,
        &[
            "-m".to_string(),
            "pip".to_string(),
            "install".to_string(),
            "--no-deps".to_string(),
            "--no-index".to_string(),
            "--disable-pip-version-check".to_string(),
            "--no-compile".to_string(),
            "--no-warn-script-location".to_string(),
            "--target".to_string(),
            site_packages.display().to_string(),
            cached.wheel_path.display().to_string(),
        ],
        envs,
        &snapshot.root,
    )?;
    if output.code != 0 {
        let mut message = String::from("failed to seed setuptools in the px environment");
        if !output.stderr.trim().is_empty() {
            message.push_str(": ");
            message.push_str(output.stderr.trim());
        }
        if output.stderr.trim().is_empty() && !output.stdout.trim().is_empty() {
            message.push_str(": ");
            message.push_str(output.stdout.trim());
        }
        return Err(InstallUserError::new(
            message,
            json!({
                "package": "setuptools",
                "version": SETUPTOOLS_SEED_VERSION,
                "reason": "missing_artifacts",
            }),
        )
        .into());
    }

    if !module_available(ctx, snapshot, env_python, envs, "setuptools")? {
        return Err(InstallUserError::new(
            "setuptools seed did not install correctly",
            json!({
                "package": "setuptools",
                "version": SETUPTOOLS_SEED_VERSION,
                "reason": "missing_artifacts",
                "hint": "rerun `px sync` to refresh the environment",
            }),
        )
        .into());
    }

    Ok(())
}

fn ensure_build_tooling_seed(
    ctx: &CommandContext,
    snapshot: &ManifestSnapshot,
    site_packages: &Path,
    env_python: &Path,
    envs: &[(String, String)],
    runtime: &RuntimeMetadata,
) -> Result<()> {
    if !uses_maturin_backend(&snapshot.manifest_path)? {
        return Ok(());
    }

    let seed = SeedContext {
        ctx,
        snapshot,
        site_packages,
        env_python,
        envs,
    };
    ensure_seeded_tool(&seed, "packaging", PACKAGING_SEED_VERSION, "packaging")?;
    ensure_seeded_tool(
        &seed,
        "pyproject-hooks",
        PYPROJECT_HOOKS_SEED_VERSION,
        "pyproject_hooks",
    )?;
    if parse_python_version(&runtime.version)
        .and_then(|(_, minor)| minor.parse::<u32>().ok())
        .is_some_and(|minor| minor < 11)
    {
        ensure_seeded_tool(&seed, "tomli", TOMLI_SEED_VERSION, "tomli")?;
    }
    ensure_seeded_tool(&seed, "build", BUILD_SEED_VERSION, "build.__main__")?;
    Ok(())
}

struct SeedContext<'ctx, 'global> {
    ctx: &'ctx CommandContext<'global>,
    snapshot: &'ctx ManifestSnapshot,
    site_packages: &'ctx Path,
    env_python: &'ctx Path,
    envs: &'ctx [(String, String)],
}

fn ensure_seeded_tool(
    seed: &SeedContext<'_, '_>,
    package: &str,
    version: &str,
    module: &str,
) -> Result<()> {
    if module_available(
        seed.ctx,
        seed.snapshot,
        seed.env_python,
        seed.envs,
        module,
    )? {
        return Ok(());
    }

    let release = seed
        .ctx
        .pypi()
        .fetch_release(package, version, &format!("{package}=={version}"))
        .map_err(|err| {
            InstallUserError::new(
                format!("failed to locate {package} for the px environment"),
                json!({
                    "package": package,
                    "version": version,
                    "error": err.to_string(),
                    "reason": "missing_artifacts",
                    "hint": "ensure network access or prefetch artifacts, then rerun `px sync`",
                }),
            )
        })?;
    let wheel = release
        .urls
        .iter()
        .filter(|file| file.packagetype == "bdist_wheel" && !file.yanked.unwrap_or(false))
        .find(|file| file.filename.ends_with("py3-none-any.whl"))
        .or_else(|| {
            release
                .urls
                .iter()
                .find(|file| file.packagetype == "bdist_wheel" && !file.yanked.unwrap_or(false))
        })
        .cloned()
        .ok_or_else(|| {
            InstallUserError::new(
                format!("{package} wheel unavailable for the px environment"),
                json!({
                    "package": package,
                    "version": version,
                    "reason": "missing_artifacts",
                    "hint": "rerun with network access to refresh the wheel cache",
                }),
            )
        })?;

    let request = ArtifactRequest {
        name: package,
        version,
        filename: &wheel.filename,
        url: &wheel.url,
        sha256: &wheel.digests.sha256,
    };
    let cached = seed
        .ctx
        .cache_store()
        .cache_wheel(&seed.ctx.cache().path, &request)
        .map_err(|err| {
            InstallUserError::new(
                format!("failed to cache {package} for the px environment"),
                json!({
                    "package": package,
                    "version": version,
                    "error": err.to_string(),
                    "reason": "missing_artifacts",
                    "hint": "rerun with network access to refresh the cache",
                }),
            )
        })?;

    let output = seed.ctx.python_runtime().run_command(
        seed.env_python
            .to_str()
            .ok_or_else(|| anyhow!("invalid python path"))?,
        &[
            "-m".to_string(),
            "pip".to_string(),
            "install".to_string(),
            "--no-deps".to_string(),
            "--no-index".to_string(),
            "--disable-pip-version-check".to_string(),
            "--no-compile".to_string(),
            "--no-warn-script-location".to_string(),
            "--target".to_string(),
            seed.site_packages.display().to_string(),
            cached.wheel_path.display().to_string(),
        ],
        seed.envs,
        &seed.snapshot.root,
    )?;
    if output.code != 0 {
        let mut message = format!("failed to seed {package} in the px environment");
        if !output.stderr.trim().is_empty() {
            message.push_str(": ");
            message.push_str(output.stderr.trim());
        }
        if output.stderr.trim().is_empty() && !output.stdout.trim().is_empty() {
            message.push_str(": ");
            message.push_str(output.stdout.trim());
        }
        return Err(InstallUserError::new(
            message,
            json!({
                "package": package,
                "version": version,
                "reason": "missing_artifacts",
            }),
        )
        .into());
    }

    if !module_available(
        seed.ctx,
        seed.snapshot,
        seed.env_python,
        seed.envs,
        module,
    )? {
        return Err(InstallUserError::new(
            format!("{package} seed did not install correctly"),
            json!({
                "package": package,
                "version": version,
                "reason": "missing_artifacts",
                "hint": "rerun `px sync` to refresh the environment",
            }),
        )
        .into());
    }

    Ok(())
}

pub(in super::super) fn uv_seed_required(snapshot: &ManifestSnapshot) -> bool {
    snapshot.root.join("uv.lock").exists()
}

pub(in super::super) fn uv_cli_candidates(site_dir: &Path) -> Vec<PathBuf> {
    vec![
        site_dir.join("bin").join("uv"),
        site_dir.join("bin").join("uvx"),
        site_dir.join("Scripts").join("uv.exe"),
        site_dir.join("Scripts").join("uvx.exe"),
    ]
}

pub(in super::super) fn has_uv_cli(site_dir: &Path) -> bool {
    uv_cli_candidates(site_dir)
        .into_iter()
        .any(|path| path.exists())
}

pub(in super::super) fn ensure_uv_seed(
    ctx: &CommandContext,
    snapshot: &ManifestSnapshot,
    site_dir: &Path,
    env_python: &Path,
    envs: &[(String, String)],
) -> Result<()> {
    if !uv_seed_required(snapshot) {
        return Ok(());
    }
    if has_uv_cli(site_dir)
        || module_available(ctx, snapshot, env_python, envs, "uv").unwrap_or(false)
    {
        return Ok(());
    }

    let release = ctx
        .pypi()
        .fetch_release("uv", UV_SEED_VERSION, &format!("uv=={UV_SEED_VERSION}"))
        .map_err(|err| {
            InstallUserError::new(
                "failed to locate uv for the px environment",
                json!({
                    "package": "uv",
                    "version": UV_SEED_VERSION,
                    "error": err.to_string(),
                    "reason": "missing_artifacts",
                    "hint": "ensure network access or prefetch artifacts, then rerun `px sync`",
                }),
            )
        })?;
    let tags = detect_interpreter_tags(
        env_python
            .to_str()
            .ok_or_else(|| anyhow!("invalid python path"))?,
    )?;
    let wheel = select_wheel(&release.urls, &tags, &format!("uv=={UV_SEED_VERSION}"))?;
    let request = ArtifactRequest {
        name: "uv",
        version: UV_SEED_VERSION,
        filename: &wheel.filename,
        url: &wheel.url,
        sha256: &wheel.sha256,
    };
    let cached = ctx
        .cache_store()
        .cache_wheel(&ctx.cache().path, &request)
        .map_err(|err| {
            InstallUserError::new(
                "failed to cache uv for the px environment",
                json!({
                    "package": "uv",
                    "version": UV_SEED_VERSION,
                    "error": err.to_string(),
                    "reason": "missing_artifacts",
                    "hint": "ensure network access or prefetch artifacts, then rerun `px sync`",
                }),
            )
        })?;

    let output = ctx.python_runtime().run_command(
        env_python
            .to_str()
            .ok_or_else(|| anyhow!("invalid python path"))?,
        &[
            "-m".into(),
            "pip".into(),
            "install".into(),
            "--no-deps".into(),
            "--no-index".into(),
            "--disable-pip-version-check".into(),
            "--no-compile".into(),
            "--no-warn-script-location".into(),
            "--prefix".into(),
            site_dir.display().to_string(),
            cached.wheel_path.display().to_string(),
        ],
        envs,
        &snapshot.root,
    )?;
    if output.code != 0 {
        let mut message = String::from("failed to seed uv in the px environment");
        if !output.stderr.trim().is_empty() {
            message.push_str(": ");
            message.push_str(output.stderr.trim());
        }
        if output.stderr.trim().is_empty() && !output.stdout.trim().is_empty() {
            message.push_str(": ");
            message.push_str(output.stdout.trim());
        }
        return Err(InstallUserError::new(
            message,
            json!({
                "package": "uv",
                "version": UV_SEED_VERSION,
                "reason": "missing_artifacts",
            }),
        )
        .into());
    }

    if !has_uv_cli(site_dir) {
        return Err(InstallUserError::new(
            "uv seed did not install correctly",
            json!({
                "package": "uv",
                "version": UV_SEED_VERSION,
                "reason": "missing_artifacts",
                "hint": "rerun `px sync` to refresh the environment",
            }),
        )
        .into());
    }

    Ok(())
}

fn has_pip_in_site(site_packages: &Path) -> bool {
    if site_packages.join("pip").exists() {
        return true;
    }
    if let Ok(entries) = fs::read_dir(site_packages) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if let Some(name) = name.to_str() {
                if name.starts_with("pip-") && name.ends_with(".dist-info") {
                    return true;
                }
            }
        }
    }
    false
}

fn has_px_editable_stub(site_root: &Path, normalized_name: &str) -> bool {
    let prefix = format!("{normalized_name}-");
    if let Ok(entries) = fs::read_dir(site_root) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
                continue;
            };
            if !name.starts_with(&prefix) || !name.ends_with(".dist-info") {
                continue;
            }
            if path.join("PX-EDITABLE").exists() {
                return true;
            }
        }
    }
    false
}

fn link_runtime_pip(
    env_site: &Path,
    env_bin: &Path,
    runtime_path: &Path,
    runtime_version: &str,
) -> Result<()> {
    let runtime_root = match runtime_path.parent().and_then(|bin| bin.parent()) {
        Some(root) => root.to_path_buf(),
        None => return Ok(()),
    };
    let Some((major, minor)) = parse_python_version(runtime_version) else {
        return Ok(());
    };
    let runtime_site = runtime_root
        .join("lib")
        .join(format!("python{major}.{minor}"))
        .join("site-packages");
    if runtime_site.exists() {
        let pip_src = runtime_site.join("pip");
        let pip_dest = env_site.join("pip");
        if pip_src.exists() && !pip_dest.exists() {
            symlink_or_copy_dir(&pip_src, &pip_dest)?;
        }

        for entry in fs::read_dir(&runtime_site)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name_str) = name.to_str() else {
                continue;
            };
            if !name_str.starts_with("pip") {
                continue;
            }
            let src = entry.path();
            let dest = env_site.join(name);
            if entry.file_type()?.is_dir() && !dest.exists() {
                symlink_or_copy_dir(&src, &dest)?;
            }
        }
    }

    let runtime_bin = runtime_root.join("bin");
    let mut bin_names = vec!["pip".to_string(), "pip3".to_string()];
    bin_names.push(format!("pip{major}"));
    bin_names.push(format!("pip{major}.{minor}"));
    for name in bin_names {
        let src = runtime_bin.join(&name);
        if !src.exists() {
            continue;
        }
        let dest = env_bin.join(&name);
        let _ = install_python_link(&src, &dest);
    }

    Ok(())
}

fn symlink_or_copy_dir(src: &Path, dest: &Path) -> Result<()> {
    if dest.exists() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        if symlink(src, dest).is_ok() {
            return Ok(());
        }
    }
    copy_tree(src, dest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{GlobalOptions, SystemEffects};
    use crate::pypi::{PypiDigests, PypiFile, PypiReleaseResponse};
    use crate::store::{
        BuiltWheel, CacheLocation, CachePruneResult, CacheUsage, CacheWalk, CachedArtifact,
        PrefetchOptions, PrefetchSpec, PrefetchSummary,
    };
    use sha2::Sha256;
    use std::io::Write as _;
    use std::sync::Arc;
    use zip::write::FileOptions;

    #[cfg(unix)]
    #[test]
    fn ensure_project_pip_skips_ensurepip_when_pip_is_already_available() -> Result<()> {
        let saved_env = vec![
            ("PX_CACHE_PATH", env::var("PX_CACHE_PATH").ok()),
            ("PX_STORE_PATH", env::var("PX_STORE_PATH").ok()),
            ("PX_ENVS_PATH", env::var("PX_ENVS_PATH").ok()),
            ("PX_NO_ENSUREPIP", env::var("PX_NO_ENSUREPIP").ok()),
        ];

        env::remove_var("PX_NO_ENSUREPIP");
        let temp_env = tempfile::tempdir()?;
        env::set_var("PX_CACHE_PATH", temp_env.path().join("cache"));
        env::set_var("PX_STORE_PATH", temp_env.path().join("store"));
        env::set_var("PX_ENVS_PATH", temp_env.path().join("envs"));

        let temp = tempfile::tempdir()?;
        let project_root = temp.path();
        fs::write(
            project_root.join("pyproject.toml"),
            r#"[project]
name = "pip-skip-ensurepip"
version = "0.0.0"
requires-python = ">=3.11"

[build-system]
requires = ["setuptools"]
build-backend = "setuptools.build_meta"
"#,
        )?;
        let snapshot = ManifestSnapshot::read_from(project_root)?;

        let site_temp = tempfile::tempdir()?;
        let site_dir = site_temp.path();
        fs::create_dir_all(site_dir.join("bin"))?;
        fs::write(site_dir.join("bin").join("pip"), "")?;

        let runtime = RuntimeMetadata {
            path: "/usr/bin/python3.11".to_string(),
            version: "3.11.14".to_string(),
            platform: "any".to_string(),
        };
        let site_packages = site_packages_dir(site_dir, &runtime.version);
        fs::create_dir_all(&site_packages)?;

        let tooling_root = project_root.join("tooling");
        fs::create_dir_all(tooling_root.join("pip"))?;
        fs::write(tooling_root.join("pip").join("__init__.py"), "")?;
        fs::write(tooling_root.join("pip").join("__main__.py"), "")?;
        fs::create_dir_all(tooling_root.join("setuptools"))?;
        fs::write(
            tooling_root.join("setuptools").join("__init__.py"),
            "__version__ = \"0.0.0\"\n",
        )?;
        fs::write(
            site_dir.join("px.pth"),
            format!("{}\n", tooling_root.display()),
        )?;

        let log_path = project_root.join("ensurepip.log");
        let wrapper = project_root.join("python-wrapper.sh");
        fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nif [ \"$1\" = \"-m\" ] && [ \"$2\" = \"ensurepip\" ]; then echo ensurepip >> \"{}\"; exit 17; fi\nexec /usr/bin/python3.11 \"$@\"\n",
                log_path.display()
            ),
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&wrapper)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&wrapper, perms)?;
        }

        let global = GlobalOptions::default();
        let ctx = CommandContext::new(&global, Arc::new(SystemEffects::new()))?;
        ensure_project_pip(&ctx, &snapshot, site_dir, &runtime, &wrapper)?;
        assert!(
            !log_path.exists(),
            "ensurepip should not be invoked when pip.__main__ is already importable"
        );

        for (key, value) in saved_env {
            match value {
                Some(prev) => env::set_var(key, prev),
                None => env::remove_var(key),
            }
        }

        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn ensure_project_pip_seeds_build_tools_for_maturin_projects() -> Result<()> {
        fn wheel_sha256(path: &Path) -> Result<String> {
            let data = fs::read(path)?;
            Ok(hex::encode(Sha256::digest(&data)))
        }

        fn write_seed_wheel(
            dir: &Path,
            dist_name: &str,
            version: &str,
            files: &[(&str, &str)],
        ) -> Result<PathBuf> {
            let filename = format!(
                "{}-{}-py3-none-any.whl",
                dist_name.replace('-', "_"),
                version
            );
            let wheel_path = dir.join(filename);
            let file = fs::File::create(&wheel_path)?;
            let mut zip = zip::ZipWriter::new(file);
            let opts = FileOptions::default();
            let dist_info = format!("{}-{}.dist-info", dist_name.replace('-', "_"), version);

            zip.start_file(format!("{dist_info}/METADATA"), opts)?;
            zip.write_all(
                format!(
                    "Metadata-Version: 2.1\nName: {dist_name}\nVersion: {version}\n\n"
                )
                .as_bytes(),
            )?;
            zip.start_file(format!("{dist_info}/WHEEL"), opts)?;
            zip.write_all(
                b"Wheel-Version: 1.0\nGenerator: px-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n\n",
            )?;

            let mut record = Vec::new();
            for (path, contents) in files {
                zip.start_file(*path, opts)?;
                zip.write_all(contents.as_bytes())?;
                record.push(format!("{path},,"));
            }
            record.push(format!("{dist_info}/METADATA,,"));
            record.push(format!("{dist_info}/WHEEL,,"));
            record.push(format!("{dist_info}/RECORD,,"));
            zip.start_file(format!("{dist_info}/RECORD"), opts)?;
            zip.write_all(record.join("\n").as_bytes())?;
            zip.finish()?;
            Ok(wheel_path)
        }

        #[derive(Default)]
        struct MapPypiClient {
            releases: std::collections::HashMap<String, Vec<PypiFile>>,
        }

        impl effects::PypiClient for MapPypiClient {
            fn fetch_release(
                &self,
                normalized: &str,
                _version: &str,
                _specifier: &str,
            ) -> Result<PypiReleaseResponse> {
                Ok(PypiReleaseResponse {
                    urls: self
                        .releases
                        .get(normalized)
                        .cloned()
                        .unwrap_or_default(),
                })
            }
        }

        struct MapCacheStore {
            cache_root: PathBuf,
            wheels: std::collections::HashMap<String, PathBuf>,
        }

        impl effects::CacheStore for MapCacheStore {
            fn resolve_store_path(&self) -> Result<CacheLocation> {
                Ok(CacheLocation {
                    path: self.cache_root.clone(),
                    source: "test",
                })
            }

            fn compute_usage(&self, _path: &Path) -> Result<CacheUsage> {
                Ok(CacheUsage {
                    exists: true,
                    total_entries: 0,
                    total_size_bytes: 0,
                })
            }

            fn collect_walk(&self, _path: &Path) -> Result<CacheWalk> {
                Ok(CacheWalk::default())
            }

            fn prune(&self, _walk: &CacheWalk) -> CachePruneResult {
                CachePruneResult::default()
            }

            fn prefetch(
                &self,
                _cache: &Path,
                _specs: &[PrefetchSpec<'_>],
                _options: PrefetchOptions,
            ) -> Result<PrefetchSummary> {
                Ok(PrefetchSummary::default())
            }

            fn cache_wheel(&self, _cache: &Path, request: &ArtifactRequest) -> Result<CachedArtifact> {
                let wheel_path = self
                    .wheels
                    .get(request.filename)
                    .cloned()
                    .ok_or_else(|| anyhow!("unexpected wheel request: {}", request.filename))?;
                Ok(CachedArtifact {
                    size: fs::metadata(&wheel_path).map(|m| m.len()).unwrap_or(0),
                    dist_path: wheel_path.with_extension("dist"),
                    wheel_path,
                })
            }

            fn ensure_sdist_build(&self, _cache: &Path, _request: &SdistRequest) -> Result<BuiltWheel> {
                panic!("unexpected sdist build while seeding tooling wheels")
            }
        }

        struct TestEffects {
            system: SystemEffects,
            cache: MapCacheStore,
            pypi: MapPypiClient,
        }

        impl effects::Effects for TestEffects {
            fn python(&self) -> &dyn effects::PythonRuntime {
                self.system.python()
            }

            fn git(&self) -> &dyn effects::GitClient {
                self.system.git()
            }

            fn fs(&self) -> &dyn effects::FileSystem {
                self.system.fs()
            }

            fn cache(&self) -> &dyn effects::CacheStore {
                &self.cache
            }

            fn pypi(&self) -> &dyn effects::PypiClient {
                &self.pypi
            }
        }

        let saved_env = vec![("PX_NO_ENSUREPIP", env::var("PX_NO_ENSUREPIP").ok())];
        env::remove_var("PX_NO_ENSUREPIP");

        let temp = tempfile::tempdir()?;
        let project_root = temp.path().join("maturin-seed");
        fs::create_dir_all(&project_root)?;
        fs::write(
            project_root.join("pyproject.toml"),
            r#"[project]
name = "maturin-seed"
version = "0.1.0"
requires-python = ">=3.11"

[build-system]
requires = ["maturin>=1.0"]
build-backend = "maturin"
"#,
        )?;
        let snapshot = ManifestSnapshot::read_from(&project_root)?;

        let site_temp = tempfile::tempdir()?;
        let site_dir = site_temp.path();
        fs::create_dir_all(site_dir.join("bin"))?;

        let runtime = RuntimeMetadata {
            path: "/usr/bin/python3.11".to_string(),
            version: "3.11.14".to_string(),
            platform: "test".to_string(),
        };
        let site_packages = site_packages_dir(site_dir, &runtime.version);
        fs::create_dir_all(&site_packages)?;
        fs::create_dir_all(site_packages.join("setuptools"))?;
        fs::write(
            site_packages.join("setuptools").join("__init__.py"),
            "__version__ = '0.0.0'\n",
        )?;

        let wheel_dir = temp.path().join("wheels");
        fs::create_dir_all(&wheel_dir)?;

        let packaging_wheel =
            write_seed_wheel(&wheel_dir, "packaging", PACKAGING_SEED_VERSION, &[(
                "packaging/__init__.py",
                "__version__ = '0.0.0'\n",
            )])?;
        let pyproject_hooks_wheel = write_seed_wheel(
            &wheel_dir,
            "pyproject-hooks",
            PYPROJECT_HOOKS_SEED_VERSION,
            &[("pyproject_hooks/__init__.py", "__version__ = '0.0.0'\n")],
        )?;
        let build_wheel = write_seed_wheel(
            &wheel_dir,
            "build",
            BUILD_SEED_VERSION,
            &[
                ("build/__init__.py", "__version__ = '0.0.0'\n"),
                ("build/__main__.py", "def main():\n    return 0\n"),
            ],
        )?;

        let packaging_filename = packaging_wheel
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap()
            .to_string();
        let pyproject_hooks_filename = pyproject_hooks_wheel
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap()
            .to_string();
        let build_filename = build_wheel
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap()
            .to_string();

        let mut releases = std::collections::HashMap::new();
        releases.insert(
            "packaging".to_string(),
            vec![PypiFile {
                filename: packaging_filename.clone(),
                url: "https://example.invalid/packaging".to_string(),
                packagetype: "bdist_wheel".to_string(),
                yanked: None,
                digests: PypiDigests {
                    sha256: wheel_sha256(&packaging_wheel)?,
                },
            }],
        );
        releases.insert(
            "pyproject-hooks".to_string(),
            vec![PypiFile {
                filename: pyproject_hooks_filename.clone(),
                url: "https://example.invalid/pyproject-hooks".to_string(),
                packagetype: "bdist_wheel".to_string(),
                yanked: None,
                digests: PypiDigests {
                    sha256: wheel_sha256(&pyproject_hooks_wheel)?,
                },
            }],
        );
        releases.insert(
            "build".to_string(),
            vec![PypiFile {
                filename: build_filename.clone(),
                url: "https://example.invalid/build".to_string(),
                packagetype: "bdist_wheel".to_string(),
                yanked: None,
                digests: PypiDigests {
                    sha256: wheel_sha256(&build_wheel)?,
                },
            }],
        );

        let mut wheels = std::collections::HashMap::new();
        wheels.insert(packaging_filename, packaging_wheel);
        wheels.insert(pyproject_hooks_filename, pyproject_hooks_wheel);
        wheels.insert(build_filename, build_wheel);

        let global = GlobalOptions::default();
        let effects = TestEffects {
            system: SystemEffects::new(),
            cache: MapCacheStore {
                cache_root: temp.path().join("cache"),
                wheels,
            },
            pypi: MapPypiClient { releases },
        };
        let ctx = CommandContext::new(&global, Arc::new(effects))?;
        ensure_project_pip(&ctx, &snapshot, site_dir, &runtime, Path::new(&runtime.path))?;

        let envs = project_site_env(&ctx, &snapshot, site_dir, Path::new(&runtime.path))?;
        assert!(
            module_available(&ctx, &snapshot, Path::new(&runtime.path), &envs, "build.__main__")?,
            "expected build tooling to be seeded for maturin projects"
        );

        for (key, value) in saved_env {
            match value {
                Some(prev) => env::set_var(key, prev),
                None => env::remove_var(key),
            }
        }

        Ok(())
    }
}
