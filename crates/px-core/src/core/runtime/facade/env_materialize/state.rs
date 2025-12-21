// state.json persistence + helpers.
use super::*;

pub(crate) fn persist_project_state(
    filesystem: &dyn effects::FileSystem,
    project_root: &Path,
    env: StoredEnvironment,
    runtime: StoredRuntime,
) -> Result<()> {
    let mut state = load_project_state(filesystem, project_root)?;
    state.current_env = Some(env);
    state.runtime = Some(runtime);
    write_project_state(filesystem, project_root, &state)
}

pub(crate) fn load_project_state(
    filesystem: &dyn effects::FileSystem,
    project_root: &Path,
) -> Result<ProjectState> {
    let path = project_root.join(".px").join("state.json");
    match filesystem.read_to_string(&path) {
        Ok(contents) => {
            let state: ProjectState = serde_json::from_str(&contents)
                .with_context(|| format!("failed to parse {}", path.display()))?;
            validate_project_state(&state)?;
            Ok(state)
        }
        Err(err) => {
            if filesystem.metadata(&path).is_ok() {
                Err(err)
            } else {
                Ok(ProjectState::default())
            }
        }
    }
}

fn write_project_state(
    filesystem: &dyn effects::FileSystem,
    project_root: &Path,
    state: &ProjectState,
) -> Result<()> {
    let path = project_root.join(".px").join("state.json");
    let mut contents = serde_json::to_vec_pretty(state)?;
    contents.push(b'\n');
    if let Some(dir) = path.parent() {
        filesystem.create_dir_all(dir)?;
    }
    let tmp_path = path.with_extension("json.tmp");
    filesystem.write(&tmp_path, &contents)?;
    std::fs::rename(&tmp_path, &path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn validate_project_state(state: &ProjectState) -> Result<()> {
    if let Some(env) = &state.current_env {
        if env.id.trim().is_empty() || env.lock_id.trim().is_empty() {
            bail!("invalid project state: missing environment identity");
        }
        let has_site = !env.site_packages.trim().is_empty()
            || env
                .env_path
                .as_ref()
                .is_some_and(|path| !path.trim().is_empty());
        if !has_site {
            bail!("invalid project state: missing site-packages path");
        }
        if env.python.path.trim().is_empty() || env.python.version.trim().is_empty() {
            bail!("invalid project state: missing python metadata");
        }
    }
    if let Some(runtime) = &state.runtime {
        if runtime.path.trim().is_empty()
            || runtime.version.trim().is_empty()
            || runtime.platform.trim().is_empty()
        {
            bail!("invalid project runtime metadata");
        }
    }
    Ok(())
}

pub(in super::super) fn resolve_project_site(
    filesystem: &dyn effects::FileSystem,
    project_root: &Path,
) -> Result<PathBuf> {
    let state = load_project_state(filesystem, project_root)?;
    let env = state.current_env.ok_or_else(|| {
        InstallUserError::new(
            "project environment missing",
            json!({
                "project_root": project_root.display().to_string(),
                "reason": "missing_env",
                "hint": "run `px sync` to rebuild the environment",
            }),
        )
    })?;
    let root = env
        .env_path
        .as_ref()
        .filter(|path| !path.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            let trimmed = env.site_packages.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(PathBuf::from(trimmed))
            }
        })
        .ok_or_else(|| {
            InstallUserError::new(
                "project environment missing",
                json!({
                    "project_root": project_root.display().to_string(),
                    "reason": "missing_env",
                    "hint": "run `px sync` to rebuild the environment",
                }),
            )
        })?;
    if !root.exists() {
        return Err(InstallUserError::new(
            "project environment missing",
            json!({
                "project_root": project_root.display().to_string(),
                "env_path": env.env_path.as_ref().map(|path| path.to_string()),
                "site_packages": env.site_packages,
                "resolved_site": root.display().to_string(),
                "reason": "missing_env",
                "hint": "run `px sync` to rebuild the environment",
            }),
        )
        .into());
    }
    Ok(root)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct ProjectState {
    #[serde(default)]
    pub(crate) current_env: Option<StoredEnvironment>,
    #[serde(default)]
    pub(crate) runtime: Option<StoredRuntime>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StoredEnvironment {
    pub(crate) id: String,
    #[serde(alias = "lock_hash")]
    pub(crate) lock_id: String,
    pub(crate) platform: String,
    pub(crate) site_packages: String,
    #[serde(default)]
    pub(crate) env_path: Option<String>,
    #[serde(default)]
    pub(crate) profile_oid: Option<String>,
    pub(crate) python: StoredPython,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct StoredRuntime {
    pub(crate) path: String,
    pub(crate) version: String,
    pub(crate) platform: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StoredPython {
    pub(crate) path: String,
    pub(crate) version: String,
}
