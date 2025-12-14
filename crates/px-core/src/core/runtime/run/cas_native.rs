use super::*;

pub(super) const CONSOLE_SCRIPT_DISPATCH: &str = r#"
import sys
from importlib.metadata import distribution

def _main():
    if len(sys.argv) < 3:
        raise SystemExit("px: console script dispatch requires <script> <dist> [args...]")
    script = sys.argv[1]
    dist_name = sys.argv[2]
    args = sys.argv[3:]
    dist = distribution(dist_name)
    eps = [ep for ep in dist.entry_points if ep.group == "console_scripts" and ep.name == script]
    if not eps:
        raise SystemExit(f"px: console script '{script}' not found in distribution '{dist_name}'")
    if len(eps) > 1:
        names = ", ".join(sorted({ep.value for ep in eps}))
        raise SystemExit(
            f"px: console script '{script}' is ambiguous within distribution '{dist_name}': {names}"
        )
    ep = eps[0]
    func = ep.load()
    sys.argv[:] = [script] + args
    raise SystemExit(func())

if __name__ == "__main__":
    _main()
"#;

pub(super) struct CasNativeRunContext {
    pub(super) py_ctx: PythonContext,
    pub(super) profile_oid: String,
    pub(super) runtime_path: PathBuf,
    pub(super) sys_path_entries: Vec<PathBuf>,
    pub(super) env_vars: BTreeMap<String, Value>,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum CasNativeFallbackReason {
    AmbiguousConsoleScript,
    ConsoleScriptIndexFailed,
    MissingArtifacts,
    UnresolvedConsoleScript,
    NativeSiteSetupFailed,
}

impl CasNativeFallbackReason {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::AmbiguousConsoleScript => "ambiguous_console_script",
            Self::ConsoleScriptIndexFailed => "cas_native_console_script_index_failed",
            Self::MissingArtifacts => "missing_artifacts",
            Self::UnresolvedConsoleScript => "cas_native_unresolved_console_script",
            Self::NativeSiteSetupFailed => "cas_native_site_setup_failed",
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct CasNativeFallback {
    pub(super) reason: CasNativeFallbackReason,
    pub(super) summary: String,
}

pub(super) struct ProcessPlan {
    pub(super) runtime_path: PathBuf,
    pub(super) sys_path_entries: Vec<PathBuf>,
    pub(super) cwd: PathBuf,
    pub(super) envs: EnvPairs,
    pub(super) argv: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct ConsoleScriptIndex {
    pub(super) version: u32,
    pub(super) scripts: BTreeMap<String, Vec<ConsoleScriptCandidate>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct ConsoleScriptCandidate {
    pub(super) dist: String,
    #[serde(default)]
    pub(super) dist_version: Option<String>,
    pub(super) entry_point: String,
}

pub(super) fn is_python_alias_target(entry: &str) -> bool {
    let lower = entry.to_ascii_lowercase();
    lower == "python"
        || lower == "python3"
        || lower.starts_with("python3.")
        || lower == "py"
        || lower == "py3"
}

pub(super) fn prepare_cas_native_run_context(
    ctx: &CommandContext,
    snapshot: &ManifestSnapshot,
) -> Result<CasNativeRunContext, ExecutionOutcome> {
    let Some(lock) = load_lockfile_optional(&snapshot.lock_path).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to load px.lock",
            json!({
                "lockfile": snapshot.lock_path.display().to_string(),
                "error": err.to_string(),
            }),
        )
    })?
    else {
        return Err(ExecutionOutcome::user_error(
            "px.lock not found",
            json!({
                "reason": "missing_lock",
                "lockfile": snapshot.lock_path.display().to_string(),
                "hint": "Run `px sync` to generate px.lock before running commands.",
            }),
        ));
    };

    if lock.manifest_fingerprint.as_deref() != Some(snapshot.manifest_fingerprint.as_str()) {
        return Err(ExecutionOutcome::user_error(
            "Project manifest has changed since px.lock was created",
            json!({
                "code": "PX120",
                "reason": "lock_drift",
                "lockfile": snapshot.lock_path.display().to_string(),
                "manifest_fingerprint": snapshot.manifest_fingerprint.clone(),
                "lock_fingerprint": lock.manifest_fingerprint.clone(),
                "hint": "Run `px sync` to update px.lock and dependencies before running commands.",
            }),
        ));
    }

    let marker_env = marker_env_for_snapshot(snapshot);
    let drift = detect_lock_drift(snapshot, &lock, marker_env.as_ref());
    if !drift.is_empty() {
        return Err(ExecutionOutcome::user_error(
            "px.lock is out of date for this project",
            json!({
                "reason": "lock_drift",
                "lockfile": snapshot.lock_path.display().to_string(),
                "drift": drift,
                "hint": "Run `px sync` to update px.lock and dependencies before running commands.",
            }),
        ));
    }

    let missing = verify_locked_artifacts(&lock);
    if !missing.is_empty() {
        return Err(ExecutionOutcome::user_error(
            "cached artifacts missing",
            json!({
                "reason": "missing_artifacts",
                "lockfile": snapshot.lock_path.display().to_string(),
                "missing": missing,
                "hint": "Run `px sync` to rehydrate cached artifacts before running commands.",
            }),
        ));
    }

    let _ = prepare_project_runtime(snapshot).map_err(|err| {
        install_error_outcome(err, "python runtime unavailable for native CAS execution")
    })?;
    let runtime = detect_runtime_metadata(ctx, snapshot).map_err(|err| {
        install_error_outcome(err, "python runtime unavailable for native CAS execution")
    })?;
    let lock_id = lock.lock_id.clone().unwrap_or_else(|| {
        compute_lock_hash_bytes(&fs::read(&snapshot.lock_path).unwrap_or_else(|_| Vec::new()))
    });
    let env_owner = OwnerId {
        owner_type: OwnerType::ProjectEnv,
        owner_id: project_env_owner_id(&snapshot.root, &lock_id, &runtime.version).map_err(
            |err| {
                ExecutionOutcome::failure(
                    "failed to compute project environment identity",
                    json!({ "error": err.to_string() }),
                )
            },
        )?,
    };
    let cas_profile = ensure_profile_manifest(ctx, snapshot, &lock, &runtime, &env_owner)
        .map_err(|err| install_error_outcome(err, "failed to prepare CAS profile for execution"))?;

    let sys_path_entries = materialize_profile_sys_path(&cas_profile.header).map_err(|err| {
        let mut details = error_details_with_code(&err);
        if let Value::Object(map) = &mut details {
            map.insert("profile_oid".into(), json!(cas_profile.profile_oid.clone()));
            map.insert("reason".into(), json!("cas_native_profile_sys_path_failed"));
        }
        ExecutionOutcome::failure("failed to materialize CAS profile sys.path", details)
    })?;
    let site_dir = ensure_cas_native_site_dir(
        &ctx.cache().path,
        &cas_profile.profile_oid,
        &runtime.version,
        &sys_path_entries,
    )
    .map_err(|err| {
        ExecutionOutcome::failure(
            "failed to prepare native execution site",
            json!({
                "reason": "cas_native_site_setup_failed",
                "error": err.to_string(),
                "profile_oid": cas_profile.profile_oid,
            }),
        )
    })?;
    let paths = build_pythonpath(ctx.fs(), &snapshot.root, Some(site_dir)).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to assemble PYTHONPATH for native CAS execution",
            json!({ "error": err.to_string() }),
        )
    })?;

    let pyc_cache_prefix = if env::var_os("PYTHONPYCACHEPREFIX").is_some() {
        None
    } else {
        match crate::store::ensure_pyc_cache_prefix(&ctx.cache().path, &cas_profile.profile_oid) {
            Ok(prefix) => Some(prefix),
            Err(err) => {
                let prefix =
                    crate::store::pyc_cache_prefix(&ctx.cache().path, &cas_profile.profile_oid);
                return Err(ExecutionOutcome::user_error(
                    "python bytecode cache directory is not writable",
                    json!({
                        "reason": "pyc_cache_unwritable",
                        "cache_dir": prefix.display().to_string(),
                        "error": err.to_string(),
                        "hint": "ensure the directory is writable or set PX_CACHE_PATH to a writable location",
                    }),
                ));
            }
        }
    };

    let py_ctx = PythonContext {
        project_root: snapshot.root.clone(),
        project_name: snapshot.name.clone(),
        python: cas_profile.runtime_path.display().to_string(),
        pythonpath: paths.pythonpath,
        allowed_paths: paths.allowed_paths,
        site_bin: paths.site_bin,
        pep582_bin: paths.pep582_bin,
        pyc_cache_prefix,
        px_options: snapshot.px_options.clone(),
    };
    Ok(CasNativeRunContext {
        py_ctx,
        profile_oid: cas_profile.profile_oid,
        runtime_path: cas_profile.runtime_path,
        sys_path_entries,
        env_vars: cas_profile.header.env_vars,
    })
}

pub(super) fn prepare_cas_native_workspace_run_context(
    ctx: &CommandContext,
    workspace: &crate::workspace::WorkspaceSnapshot,
    member_root: &Path,
) -> Result<CasNativeRunContext, ExecutionOutcome> {
    let snapshot = workspace.lock_snapshot();
    let Some(lock) = load_lockfile_optional(&snapshot.lock_path).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to load workspace lockfile",
            json!({
                "lockfile": snapshot.lock_path.display().to_string(),
                "error": err.to_string(),
            }),
        )
    })?
    else {
        return Err(ExecutionOutcome::user_error(
            "workspace lockfile not found",
            json!({
                "reason": "missing_lock",
                "lockfile": snapshot.lock_path.display().to_string(),
                "hint": "Run `px sync` to generate the workspace lock before running commands.",
            }),
        ));
    };

    if lock.manifest_fingerprint.as_deref() != Some(snapshot.manifest_fingerprint.as_str()) {
        return Err(ExecutionOutcome::user_error(
            "Workspace manifest has changed since the lockfile was created",
            json!({
                "code": "PX120",
                "reason": "lock_drift",
                "lockfile": snapshot.lock_path.display().to_string(),
                "manifest_fingerprint": snapshot.manifest_fingerprint.clone(),
                "lock_fingerprint": lock.manifest_fingerprint.clone(),
                "hint": "Run `px sync` to update the lockfile before running commands.",
            }),
        ));
    }

    let marker_env = marker_env_for_snapshot(&snapshot);
    let drift = detect_lock_drift(&snapshot, &lock, marker_env.as_ref());
    if !drift.is_empty() {
        return Err(ExecutionOutcome::user_error(
            "workspace lockfile is out of date",
            json!({
                "reason": "lock_drift",
                "lockfile": snapshot.lock_path.display().to_string(),
                "drift": drift,
                "hint": "Run `px sync` to update the workspace lock before running commands.",
            }),
        ));
    }

    let missing = verify_locked_artifacts(&lock);
    if !missing.is_empty() {
        return Err(ExecutionOutcome::user_error(
            "cached artifacts missing",
            json!({
                "reason": "missing_artifacts",
                "lockfile": snapshot.lock_path.display().to_string(),
                "missing": missing,
                "hint": "Run `px sync` to rehydrate cached artifacts before running commands.",
            }),
        ));
    }

    let _ = prepare_project_runtime(&snapshot).map_err(|err| {
        install_error_outcome(err, "python runtime unavailable for native CAS execution")
    })?;
    let runtime = detect_runtime_metadata(ctx, &snapshot).map_err(|err| {
        install_error_outcome(err, "python runtime unavailable for native CAS execution")
    })?;
    let lock_id = lock.lock_id.clone().unwrap_or_else(|| {
        compute_lock_hash_bytes(&fs::read(&snapshot.lock_path).unwrap_or_else(|_| Vec::new()))
    });
    let env_owner = OwnerId {
        owner_type: OwnerType::WorkspaceEnv,
        owner_id: workspace_env_owner_id(&snapshot.root, &lock_id, &runtime.version).map_err(
            |err| {
                ExecutionOutcome::failure(
                    "failed to compute workspace environment identity",
                    json!({ "error": err.to_string() }),
                )
            },
        )?,
    };
    let cas_profile = ensure_profile_manifest(ctx, &snapshot, &lock, &runtime, &env_owner)
        .map_err(|err| install_error_outcome(err, "failed to prepare CAS profile for execution"))?;

    let sys_path_entries = materialize_profile_sys_path(&cas_profile.header).map_err(|err| {
        let mut details = error_details_with_code(&err);
        if let Value::Object(map) = &mut details {
            map.insert("profile_oid".into(), json!(cas_profile.profile_oid.clone()));
            map.insert("reason".into(), json!("cas_native_profile_sys_path_failed"));
        }
        ExecutionOutcome::failure("failed to materialize CAS profile sys.path", details)
    })?;
    let site_dir = ensure_cas_native_site_dir(
        &ctx.cache().path,
        &cas_profile.profile_oid,
        &runtime.version,
        &sys_path_entries,
    )
    .map_err(|err| {
        ExecutionOutcome::failure(
            "failed to prepare native execution site",
            json!({
                "reason": "cas_native_site_setup_failed",
                "error": err.to_string(),
                "profile_oid": cas_profile.profile_oid,
            }),
        )
    })?;

    let paths = build_pythonpath(ctx.fs(), member_root, Some(site_dir.clone())).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to build workspace PYTHONPATH for native execution",
            json!({ "error": err.to_string() }),
        )
    })?;

    let mut combined = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut push_unique = |path: PathBuf| {
        if seen.insert(path.clone()) {
            combined.push(path);
        }
    };

    let current_src = member_root.join("src");
    if current_src.exists() {
        push_unique(current_src);
    }
    push_unique(member_root.to_path_buf());
    for member in &workspace.config.members {
        let abs = workspace.config.root.join(member);
        let src = abs.join("src");
        if src.exists() {
            push_unique(src);
        }
        push_unique(abs);
    }
    for path in paths.allowed_paths {
        push_unique(path);
    }
    let allowed_paths = combined;
    let pythonpath = env::join_paths(&allowed_paths)
        .map_err(|err| {
            ExecutionOutcome::failure(
                "failed to assemble workspace PYTHONPATH",
                json!({ "error": err.to_string() }),
            )
        })?
        .into_string()
        .map_err(|_| {
            ExecutionOutcome::failure(
                "failed to assemble workspace PYTHONPATH",
                json!({ "error": "contains non-utf8 data" }),
            )
        })?;

    let member_data = workspace
        .members
        .iter()
        .find(|member| member.root == member_root);
    let px_options = member_data
        .map(|member| member.snapshot.px_options.clone())
        .unwrap_or_default();
    let project_name = member_data
        .map(|member| member.snapshot.name.clone())
        .or_else(|| {
            member_root
                .file_name()
                .and_then(|name| name.to_str())
                .map(std::string::ToString::to_string)
        })
        .unwrap_or_default();
    let profile_oid = cas_profile.profile_oid.clone();
    let pyc_cache_prefix = if env::var_os("PYTHONPYCACHEPREFIX").is_some() {
        None
    } else {
        match crate::store::ensure_pyc_cache_prefix(&ctx.cache().path, &profile_oid) {
            Ok(prefix) => Some(prefix),
            Err(err) => {
                let prefix = crate::store::pyc_cache_prefix(&ctx.cache().path, &profile_oid);
                return Err(ExecutionOutcome::user_error(
                    "python bytecode cache directory is not writable",
                    json!({
                        "reason": "pyc_cache_unwritable",
                        "cache_dir": prefix.display().to_string(),
                        "error": err.to_string(),
                        "hint": "ensure the directory is writable or set PX_CACHE_PATH to a writable location",
                    }),
                ));
            }
        }
    };

    let py_ctx = PythonContext {
        project_root: member_root.to_path_buf(),
        project_name,
        python: cas_profile.runtime_path.display().to_string(),
        pythonpath,
        allowed_paths,
        site_bin: paths.site_bin,
        pep582_bin: paths.pep582_bin,
        pyc_cache_prefix,
        px_options,
    };
    Ok(CasNativeRunContext {
        py_ctx,
        profile_oid: cas_profile.profile_oid,
        runtime_path: cas_profile.runtime_path,
        sys_path_entries,
        env_vars: cas_profile.header.env_vars,
    })
}

fn materialize_profile_sys_path(header: &crate::store::cas::ProfileHeader) -> Result<Vec<PathBuf>> {
    let store = crate::store::cas::global_store();
    let ordered: Vec<String> = if header.sys_path_order.is_empty() {
        header
            .packages
            .iter()
            .map(|pkg| pkg.pkg_build_oid.clone())
            .collect()
    } else {
        header.sys_path_order.clone()
    };
    let mut paths = Vec::new();
    let mut seen = HashSet::new();

    let mut push_oid = |oid: &str| -> Result<()> {
        if !seen.insert(oid.to_string()) {
            return Ok(());
        }
        let loaded = store.load(oid)?;
        let crate::LoadedObject::PkgBuild { archive, .. } = loaded else {
            bail!("CAS object {oid} is not a pkg-build archive");
        };
        let root = materialize_pkg_archive(oid, &archive)?;
        let site = root.join("site-packages");
        if site.exists() {
            paths.push(site);
        } else {
            paths.push(root);
        }
        Ok(())
    };

    for oid in ordered {
        push_oid(&oid)?;
    }
    for pkg in &header.packages {
        push_oid(&pkg.pkg_build_oid)?;
    }

    Ok(paths)
}

fn ensure_cas_native_site_dir(
    cache_root: &Path,
    profile_oid: &str,
    runtime_version: &str,
    sys_path_entries: &[PathBuf],
) -> Result<PathBuf> {
    let site_dir = cache_root
        .join("native")
        .join("profiles")
        .join(profile_oid)
        .join("site");
    let temp_root = site_dir.with_extension("partial");
    if temp_root.exists() {
        let _ = fs::remove_dir_all(&temp_root);
    }
    fs::create_dir_all(&temp_root)?;
    fs::create_dir_all(temp_root.join("bin"))?;
    let site_packages = crate::site_packages_dir(&temp_root, runtime_version);
    fs::create_dir_all(&site_packages)?;

    let mut pth_body = String::new();
    for entry in sys_path_entries {
        pth_body.push_str(&entry.display().to_string());
        pth_body.push('\n');
    }
    fs::write(temp_root.join("px.pth"), pth_body.as_bytes())?;
    fs::write(site_packages.join("px.pth"), pth_body.as_bytes())?;

    fs::write(
        temp_root.join("sitecustomize.py"),
        crate::SITE_CUSTOMIZE.as_bytes(),
    )?;
    fs::write(
        site_packages.join("sitecustomize.py"),
        crate::SITE_CUSTOMIZE.as_bytes(),
    )?;

    let backup_root = site_dir.with_extension("backup");
    if backup_root.exists() {
        let _ = fs::remove_dir_all(&backup_root);
    }
    if site_dir.exists() {
        fs::rename(&site_dir, &backup_root)?;
    }
    if let Err(err) = fs::rename(&temp_root, &site_dir) {
        let _ = fs::remove_dir_all(&temp_root);
        if backup_root.exists() {
            let _ = fs::rename(&backup_root, &site_dir);
        }
        return Err(err).with_context(|| {
            format!(
                "failed to finalize native site directory at {}",
                site_dir.display()
            )
        });
    }
    let _ = fs::remove_dir_all(&backup_root);

    Ok(site_dir)
}

fn console_script_index_path(cache_root: &Path, profile_oid: &str) -> PathBuf {
    cache_root
        .join("native")
        .join("profiles")
        .join(profile_oid)
        .join("console_scripts.json")
}

fn load_console_script_index(path: &Path) -> Option<ConsoleScriptIndex> {
    let contents = fs::read_to_string(path).ok()?;
    let parsed: ConsoleScriptIndex = serde_json::from_str(&contents).ok()?;
    if parsed.version != 1 {
        return None;
    }
    Some(parsed)
}

fn write_console_script_index(path: &Path, index: &ConsoleScriptIndex) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut bytes = serde_json::to_vec_pretty(index)?;
    bytes.push(b'\n');
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

pub(super) fn load_or_build_console_script_index(
    cache_root: &Path,
    native: &CasNativeRunContext,
) -> Result<ConsoleScriptIndex> {
    let path = console_script_index_path(cache_root, &native.profile_oid);
    if let Some(index) = load_console_script_index(&path) {
        return Ok(index);
    }
    let index = build_console_script_index(&native.sys_path_entries)?;
    write_console_script_index(&path, &index)?;
    Ok(index)
}

fn build_console_script_index(sys_path_entries: &[PathBuf]) -> Result<ConsoleScriptIndex> {
    let mut scripts: BTreeMap<String, Vec<ConsoleScriptCandidate>> = BTreeMap::new();
    for sys_path in sys_path_entries {
        if !sys_path.is_dir() {
            continue;
        }
        let Ok(entries) = fs::read_dir(sys_path) else {
            continue;
        };
        let mut dist_infos = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("dist-info"))
                && path.is_dir()
            {
                dist_infos.push(path);
            }
        }
        dist_infos.sort();
        for dist_info in dist_infos {
            let entry_points = dist_info.join("entry_points.txt");
            if !entry_points.exists() {
                continue;
            }
            let contents = match fs::read_to_string(&entry_points) {
                Ok(contents) => contents,
                Err(_) => continue,
            };
            let candidates = parse_console_scripts_from_entry_points(&contents);
            if candidates.is_empty() {
                continue;
            }
            let (dist, dist_version) = read_dist_metadata_name_version(&dist_info);
            for (name, entry_point) in candidates {
                scripts
                    .entry(name)
                    .or_default()
                    .push(ConsoleScriptCandidate {
                        dist: dist.clone(),
                        dist_version: dist_version.clone(),
                        entry_point,
                    });
            }
        }
    }
    for candidates in scripts.values_mut() {
        candidates.sort_by(|a, b| {
            a.dist
                .cmp(&b.dist)
                .then(a.dist_version.cmp(&b.dist_version))
                .then(a.entry_point.cmp(&b.entry_point))
        });
    }
    Ok(ConsoleScriptIndex {
        version: 1,
        scripts,
    })
}

fn parse_console_scripts_from_entry_points(contents: &str) -> Vec<(String, String)> {
    let mut in_console_scripts = false;
    let mut scripts = Vec::new();
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            let section = &trimmed[1..trimmed.len() - 1];
            in_console_scripts = section.trim() == "console_scripts";
            continue;
        }
        if !in_console_scripts {
            continue;
        }
        let Some((name, value)) = trimmed.split_once('=') else {
            continue;
        };
        let name = name.trim();
        let value = value.trim();
        if name.is_empty() || value.is_empty() {
            continue;
        }
        scripts.push((name.to_string(), value.to_string()));
    }
    scripts
}

fn read_dist_metadata_name_version(dist_info: &Path) -> (String, Option<String>) {
    let metadata = dist_info.join("METADATA");
    let mut name = None;
    let mut version = None;
    if let Ok(contents) = fs::read_to_string(&metadata) {
        for line in contents.lines() {
            if name.is_none() {
                if let Some(value) = line.strip_prefix("Name:") {
                    let trimmed = value.trim();
                    if !trimmed.is_empty() {
                        name = Some(trimmed.to_string());
                    }
                }
            }
            if version.is_none() {
                if let Some(value) = line.strip_prefix("Version:") {
                    let trimmed = value.trim();
                    if !trimmed.is_empty() {
                        version = Some(trimmed.to_string());
                    }
                }
            }
            if name.is_some() && version.is_some() {
                break;
            }
        }
    }
    let fallback = dist_info
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown.dist-info")
        .to_string();
    (name.unwrap_or(fallback), version)
}
