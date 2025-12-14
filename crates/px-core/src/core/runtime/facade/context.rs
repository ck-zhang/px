use std::env;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use toml_edit::{Item, Value as TomlValue};
use tracing::warn;

use crate::context::CommandContext;
use crate::effects;
use crate::outcome::{ExecutionOutcome, InstallUserError};
use crate::tools::disable_proxy_env;
use px_domain::api::PxOptions;

use super::env_materialize::resolve_project_site;
use super::{
    ensure_project_environment_synced, install_snapshot, is_missing_project_error,
    load_project_state, manifest_snapshot_at, missing_project_outcome, prepare_project_runtime,
    refresh_project_site, select_python_from_site, ManifestSnapshot,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CommandGroup {
    Init,
    Add,
    Remove,
    Sync,
    Update,
    Run,
    Test,
    Fmt,
    Explain,
    Build,
    Publish,
    Pack,
    Migrate,
    Status,
    Why,
    Tool,
    Python,
    Completions,
}

impl fmt::Display for CommandGroup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            CommandGroup::Init => "init",
            CommandGroup::Add => "add",
            CommandGroup::Remove => "remove",
            CommandGroup::Sync => "sync",
            CommandGroup::Update => "update",
            CommandGroup::Run => "run",
            CommandGroup::Test => "test",
            CommandGroup::Fmt => "fmt",
            CommandGroup::Explain => "explain",
            CommandGroup::Build => "build",
            CommandGroup::Publish => "publish",
            CommandGroup::Pack => "pack",
            CommandGroup::Migrate => "migrate",
            CommandGroup::Status => "status",
            CommandGroup::Why => "why",
            CommandGroup::Tool => "tool",
            CommandGroup::Python => "python",
            CommandGroup::Completions => "completions",
        };
        f.write_str(name)
    }
}

pub(crate) struct PythonContext {
    pub(crate) project_root: PathBuf,
    pub(crate) project_name: String,
    pub(crate) python: String,
    pub(crate) pythonpath: String,
    pub(crate) allowed_paths: Vec<PathBuf>,
    pub(crate) site_bin: Option<PathBuf>,
    pub(crate) pep582_bin: Vec<PathBuf>,
    pub(crate) pyc_cache_prefix: Option<PathBuf>,
    pub(crate) px_options: PxOptions,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum EnvGuard {
    Strict,
    AutoSync,
}

#[derive(Clone, Debug)]
pub(crate) struct EnvironmentSyncReport {
    action: &'static str,
    note: String,
}

impl EnvironmentSyncReport {
    pub(crate) fn new(issue: EnvironmentIssue) -> Self {
        Self {
            action: issue.action_key(),
            note: issue.note().to_string(),
        }
    }

    pub(crate) fn action(&self) -> &str {
        self.action
    }

    fn to_json(&self) -> Value {
        json!({
            "action": self.action,
            "note": self.note,
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum EnvironmentIssue {
    MissingLock,
    LockDrift,
    MissingArtifacts,
    MissingEnv,
    EnvOutdated,
    RuntimeMismatch,
}

impl EnvironmentIssue {
    fn from_details(details: &Value) -> Option<Self> {
        let reason = details
            .as_object()
            .and_then(|map| map.get("reason"))
            .and_then(Value::as_str)?;
        match reason {
            "missing_lock" => Some(EnvironmentIssue::MissingLock),
            "lock_drift" => Some(EnvironmentIssue::LockDrift),
            "missing_artifacts" => Some(EnvironmentIssue::MissingArtifacts),
            "missing_env" => Some(EnvironmentIssue::MissingEnv),
            "env_outdated" => Some(EnvironmentIssue::EnvOutdated),
            "runtime_mismatch" => Some(EnvironmentIssue::RuntimeMismatch),
            _ => None,
        }
    }

    fn note(self) -> &'static str {
        self.lock_message().unwrap_or_else(|| self.env_message())
    }

    fn lock_message(self) -> Option<&'static str> {
        match self {
            EnvironmentIssue::MissingLock => Some("Updating px.lock (missing lock)"),
            EnvironmentIssue::LockDrift => Some("Updating px.lock (manifest changed)"),
            _ => None,
        }
    }

    fn env_message(self) -> &'static str {
        match self {
            EnvironmentIssue::MissingLock | EnvironmentIssue::LockDrift => "Syncing environment…",
            EnvironmentIssue::MissingArtifacts => "Syncing environment (rehydrating cache)…",
            EnvironmentIssue::MissingEnv => "Syncing environment…",
            EnvironmentIssue::EnvOutdated => "Syncing environment…",
            EnvironmentIssue::RuntimeMismatch => "Syncing environment (runtime changed)…",
        }
    }

    fn needs_lock_resolution(self) -> bool {
        self.lock_message().is_some()
    }

    fn action_key(self) -> &'static str {
        match self {
            EnvironmentIssue::MissingLock => "lock-bootstrap",
            EnvironmentIssue::LockDrift => "lock-sync",
            EnvironmentIssue::MissingArtifacts => "env-rehydrate",
            EnvironmentIssue::MissingEnv => "env-recreate",
            EnvironmentIssue::EnvOutdated => "env-refresh",
            EnvironmentIssue::RuntimeMismatch => "env-runtime",
        }
    }

    fn auto_fixable(self) -> bool {
        matches!(
            self,
            EnvironmentIssue::MissingLock
                | EnvironmentIssue::LockDrift
                | EnvironmentIssue::MissingArtifacts
                | EnvironmentIssue::MissingEnv
                | EnvironmentIssue::EnvOutdated
                | EnvironmentIssue::RuntimeMismatch
        )
    }
}
#[allow(dead_code)]
pub(crate) fn issue_from_details(details: &Value) -> Option<EnvironmentIssue> {
    EnvironmentIssue::from_details(details)
}

impl PythonContext {
    fn new_with_guard(
        ctx: &CommandContext,
        guard: EnvGuard,
    ) -> Result<(Self, Option<EnvironmentSyncReport>)> {
        let project_root = ctx.project_root()?;
        let manifest_path = project_root.join("pyproject.toml");
        if !manifest_path.exists() {
            return Err(InstallUserError::new(
                format!("pyproject.toml not found in {}", project_root.display()),
                json!({
                    "pyproject": manifest_path.display().to_string(),
                    "hint": "run `px migrate --apply` or create pyproject.toml first",
                    "reason": "missing_manifest",
                }),
            )
            .into());
        }
        ensure_version_file(&manifest_path)?;
        let snapshot = manifest_snapshot_at(&project_root)?;
        let runtime = prepare_project_runtime(&snapshot)?;
        let sync_report = ensure_environment_with_guard(ctx, &snapshot, guard)?;
        let state = load_project_state(ctx.fs(), &project_root)?;
        let profile_oid = state
            .current_env
            .as_ref()
            .and_then(|env| env.profile_oid.clone().or_else(|| Some(env.id.clone())));
        let pyc_cache_prefix = if env::var_os("PYTHONPYCACHEPREFIX").is_some() {
            None
        } else if let Some(oid) = profile_oid.as_deref() {
            match crate::store::ensure_pyc_cache_prefix(&ctx.cache().path, oid) {
                Ok(prefix) => Some(prefix),
                Err(err) => {
                    let prefix = crate::store::pyc_cache_prefix(&ctx.cache().path, oid);
                    return Err(InstallUserError::new(
                        "python bytecode cache directory is not writable",
                        json!({
                            "reason": "pyc_cache_unwritable",
                            "cache_dir": prefix.display().to_string(),
                            "error": err.to_string(),
                            "hint": "ensure the directory is writable or set PX_CACHE_PATH to a writable location",
                        }),
                    )
                    .into());
                }
            }
        } else {
            None
        };
        let paths = build_pythonpath(ctx.fs(), &project_root, None)?;
        let python = select_python_from_site(
            &paths.site_bin,
            &runtime.record.path,
            &runtime.record.full_version,
        );
        Ok((
            Self {
                project_root,
                project_name: snapshot.name.clone(),
                python,
                pythonpath: paths.pythonpath,
                allowed_paths: paths.allowed_paths,
                site_bin: paths.site_bin,
                pep582_bin: paths.pep582_bin,
                pyc_cache_prefix,
                px_options: snapshot.px_options.clone(),
            },
            sync_report,
        ))
    }

    pub(crate) fn base_env(&self, command_args: &Value) -> Result<Vec<(String, String)>> {
        let mut allowed_paths = self.allowed_paths.clone();
        if !self.pythonpath.is_empty() {
            for entry in env::split_paths(&self.pythonpath) {
                if !allowed_paths.contains(&entry) {
                    allowed_paths.push(entry);
                }
            }
        }
        let allowed = env::join_paths(&allowed_paths)
            .context("allowed path contains invalid UTF-8")?
            .into_string()
            .map_err(|_| anyhow!("allowed path contains non-utf8 data"))?;
        let mut python_paths: Vec<_> = env::split_paths(&allowed).collect();
        if !self.pythonpath.is_empty() {
            python_paths.extend(env::split_paths(&self.pythonpath));
        }
        let pythonpath = env::join_paths(&python_paths)
            .context("failed to assemble PYTHONPATH")?
            .into_string()
            .map_err(|_| anyhow!("pythonpath contains non-utf8 data"))?;
        let mut envs = vec![
            ("PYTHONPATH".into(), pythonpath),
            ("PYTHONUNBUFFERED".into(), "1".into()),
            ("PYTHONSAFEPATH".into(), "1".into()),
        ];
        if env::var_os("PYTHONPYCACHEPREFIX").is_none() {
            if let Some(prefix) = self.pyc_cache_prefix.as_ref() {
                fs::create_dir_all(prefix).with_context(|| {
                    format!(
                        "failed to create python bytecode cache directory {}",
                        prefix.display()
                    )
                })?;
                envs.push(("PYTHONPYCACHEPREFIX".into(), prefix.display().to_string()));
            }
        }
        envs.push(("PX_ALLOWED_PATHS".into(), allowed));
        envs.push((
            "PX_PROJECT_ROOT".into(),
            self.project_root.display().to_string(),
        ));
        envs.push(("PX_PYTHON".into(), self.python.clone()));
        envs.push(("PX_COMMAND_JSON".into(), command_args.to_string()));
        if let Ok(debug_site) = env::var("PX_DEBUG_SITE_PATHS") {
            envs.push(("PX_DEBUG_SITE_PATHS".into(), debug_site));
        }
        if let Some(alias) = self.px_options.manage_command.as_ref() {
            let trimmed = alias.trim();
            if !trimmed.is_empty() {
                envs.push(("PYAPP_COMMAND_NAME".into(), trimmed.to_string()));
            }
        }
        if let Some(bin) = &self.site_bin {
            if let Some(site_dir) = bin.parent() {
                let virtual_env = site_dir
                    .canonicalize()
                    .unwrap_or_else(|_| site_dir.to_path_buf());
                envs.push(("VIRTUAL_ENV".into(), virtual_env.display().to_string()));
            }
        }
        let mut path_entries = Vec::new();
        if let Some(bin) = &self.site_bin {
            path_entries.push(bin.clone());
        }
        path_entries.extend(self.pep582_bin.iter().cloned());
        if let Some(python_dir) = Path::new(&self.python).parent() {
            path_entries.push(python_dir.to_path_buf());
        }
        if let Ok(existing) = env::var("PATH") {
            path_entries.extend(env::split_paths(&existing));
        }
        let mut unique = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for entry in path_entries.into_iter().filter(|p| p.exists()) {
            if seen.insert(entry.clone()) {
                unique.push(entry);
            }
        }
        if !unique.is_empty() {
            if let Ok(joined) = env::join_paths(&unique) {
                if let Ok(value) = joined.into_string() {
                    envs.push(("PATH".into(), value));
                }
            }
        }
        disable_proxy_env(&mut envs);
        Ok(envs)
    }
}

pub(crate) fn ensure_version_file(manifest_path: &Path) -> Result<()> {
    let contents = fs::read_to_string(manifest_path)?;
    let doc: toml_edit::DocumentMut = contents.parse()?;
    let manifest_dir = manifest_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();

    let hatch_describe = hatch_git_describe_command(&doc);
    let hatch_simplified_semver = hatch_prefers_simplified_semver(&doc);
    let hatch_drop_local = hatch_drops_local_version(&doc);
    if let Some(version_file) = hatch_version_file(&doc) {
        ensure_version_stub(
            &manifest_dir,
            &version_file,
            VersionFileStyle::HatchVcsHook,
            VersionDeriveOptions {
                git_describe_command: hatch_describe.as_deref(),
                simplified_semver: hatch_simplified_semver,
                drop_local: hatch_drop_local,
            },
        )?;
    }

    if let Some(version_file) = setuptools_scm_version_file(&doc) {
        ensure_version_stub(
            &manifest_dir,
            &version_file,
            VersionFileStyle::SetuptoolsScm,
            VersionDeriveOptions::default(),
        )?;
    }

    if let Some(version_file) = pdm_version_file(&doc) {
        ensure_version_stub(
            &manifest_dir,
            &version_file,
            VersionFileStyle::Plain,
            VersionDeriveOptions::default(),
        )?;
    }

    ensure_inline_version_module(&manifest_dir, &doc)?;

    Ok(())
}

pub(super) fn uses_hatch_vcs(doc: &toml_edit::DocumentMut) -> bool {
    doc.get("tool")
        .and_then(|tool| tool.get("hatch"))
        .and_then(|hatch| hatch.get("version"))
        .and_then(|version| version.get("source"))
        .and_then(|value| value.as_str())
        .map(|value| value.eq_ignore_ascii_case("vcs"))
        .unwrap_or(false)
}

pub(super) fn hatch_version_file(doc: &toml_edit::DocumentMut) -> Option<PathBuf> {
    doc.get("tool")
        .and_then(|tool| tool.get("hatch"))
        .and_then(|hatch| hatch.get("build"))
        .and_then(|build| build.get("hooks"))
        .and_then(|hooks| hooks.get("vcs"))
        .and_then(|vcs| vcs.get("version-file"))
        .and_then(|item| item.as_str())
        .map(PathBuf::from)
}

pub(super) fn hatch_git_describe_command(doc: &toml_edit::DocumentMut) -> Option<Vec<String>> {
    doc.get("tool")
        .and_then(|tool| tool.get("hatch"))
        .and_then(|hatch| hatch.get("version"))
        .and_then(|version| version.get("raw-options"))
        .and_then(|raw| raw.get("git_describe_command"))
        .and_then(string_vec_from_item)
}

pub(super) fn hatch_prefers_simplified_semver(doc: &toml_edit::DocumentMut) -> bool {
    hatch_version_raw_option(doc, "version_scheme")
        .map(|value| value == "python-simplified-semver")
        .unwrap_or(false)
}

pub(super) fn hatch_drops_local_version(doc: &toml_edit::DocumentMut) -> bool {
    hatch_version_raw_option(doc, "local_scheme")
        .map(|value| value == "no-local-version")
        .unwrap_or(false)
}

fn hatch_version_raw_option(doc: &toml_edit::DocumentMut, key: &str) -> Option<String> {
    doc.get("tool")
        .and_then(|tool| tool.get("hatch"))
        .and_then(|hatch| hatch.get("version"))
        .and_then(|version| version.get("raw-options"))
        .and_then(|raw| raw.get(key))
        .and_then(|item| item.as_str())
        .map(str::to_string)
}

fn string_vec_from_item(item: &Item) -> Option<Vec<String>> {
    match item {
        Item::Value(TomlValue::Array(items)) => {
            let mut values = Vec::new();
            for entry in items.iter() {
                let value = entry.as_str()?.to_string();
                values.push(value);
            }
            if values.is_empty() {
                None
            } else {
                Some(values)
            }
        }
        Item::Value(TomlValue::String(value)) => {
            let values: Vec<String> = value
                .value()
                .split_whitespace()
                .map(|entry| entry.to_string())
                .collect();
            if values.is_empty() {
                None
            } else {
                Some(values)
            }
        }
        _ => None,
    }
}

pub(super) fn setuptools_scm_version_file(doc: &toml_edit::DocumentMut) -> Option<PathBuf> {
    doc.get("tool")
        .and_then(|tool| tool.get("setuptools_scm"))
        .and_then(|cfg| cfg.get("write_to").or_else(|| cfg.get("version_file")))
        .and_then(|item| item.as_str())
        .map(PathBuf::from)
}

pub(super) fn pdm_version_file(doc: &toml_edit::DocumentMut) -> Option<PathBuf> {
    doc.get("tool")
        .and_then(|tool| tool.get("pdm"))
        .and_then(|pdm| pdm.get("version"))
        .and_then(|version| version.get("write_to"))
        .and_then(|item| item.as_str())
        .map(PathBuf::from)
}

fn ensure_inline_version_module(manifest_dir: &Path, doc: &toml_edit::DocumentMut) -> Result<()> {
    let Some(project) = doc.get("project").and_then(Item::as_table) else {
        return Ok(());
    };
    if project
        .get("dynamic")
        .and_then(Item::as_array)
        .is_some_and(|items| {
            items
                .iter()
                .any(|item| item.as_str().is_some_and(|value| value == "version"))
        })
    {
        return Ok(());
    }

    let Some(name) = project.get("name").and_then(Item::as_str) else {
        return Ok(());
    };
    let Some(version) = project.get("version").and_then(Item::as_str) else {
        return Ok(());
    };

    let module = name.replace(['-', '.'], "_").to_lowercase();
    let candidates = [
        manifest_dir.join("src").join(&module),
        manifest_dir.join("python").join(&module),
        manifest_dir.join(&module),
    ];
    let Some(package_dir) = candidates.iter().find(|path| path.exists()) else {
        return Ok(());
    };
    let version_pyi = package_dir.join("version.pyi");
    if !version_pyi.exists() {
        return Ok(());
    }
    let version_py = package_dir.join("version.py");

    let (version_value, git_revision) = inline_version_values(manifest_dir, version);
    let release_flag = if !version_value.contains("dev") && !version_value.contains('+') {
        "True"
    } else {
        "False"
    };
    let contents = format!(
        "\"\"\"\nModule to expose more detailed version info for the installed `{name}`\n\"\"\"\n\
version = \"{version_value}\"\n\
__version__ = version\n\
full_version = version\n\n\
git_revision = \"{git_revision}\"\n\
release = {release_flag}\n\
short_version = version.split(\"+\")[0]\n"
    );

    if let Some(parent) = version_py.parent() {
        fs::create_dir_all(parent)?;
    }
    if version_py.exists() {
        if let Ok(current) = fs::read_to_string(&version_py) {
            if current == contents {
                return Ok(());
            }
        }
    }
    fs::write(&version_py, contents)?;
    Ok(())
}

fn inline_version_values(manifest_dir: &Path, version: &str) -> (String, String) {
    let mut version_value = version.to_string();
    let mut git_revision = String::new();

    if let Some((hash, date)) = latest_git_commit(manifest_dir) {
        git_revision = hash.clone();
        if version_value.contains("dev") && !date.is_empty() {
            let short = hash.chars().take(7).collect::<String>();
            if !short.is_empty() {
                version_value = format!("{version_value}+git{date}.{short}");
            }
        }
    }

    (version_value, git_revision)
}

fn latest_git_commit(manifest_dir: &Path) -> Option<(String, String)> {
    let output = Command::new("git")
        .args([
            "-c",
            "log.showSignature=false",
            "log",
            "-1",
            "--format=\"%H %aI\"",
        ])
        .current_dir(manifest_dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut parts = stdout.trim().trim_matches('"').split_whitespace();
    let hash = parts.next().unwrap_or_default();
    if hash.is_empty() {
        return None;
    }
    let timestamp = parts.next().unwrap_or_default();
    let date = timestamp
        .split('T')
        .next()
        .unwrap_or_default()
        .replace('-', "");
    Some((hash.to_string(), date))
}

#[derive(Clone, Copy)]
enum VersionFileStyle {
    HatchVcsHook,
    SetuptoolsScm,
    Plain,
}

#[derive(Default)]
pub(super) struct VersionDeriveOptions<'a> {
    pub(super) git_describe_command: Option<&'a [String]>,
    pub(super) simplified_semver: bool,
    pub(super) drop_local: bool,
}

fn ensure_version_stub(
    root: &Path,
    target: &Path,
    style: VersionFileStyle,
    derive_opts: VersionDeriveOptions<'_>,
) -> Result<()> {
    let version_path = root.join(target);
    let mut rewrite = false;
    if version_path.exists() {
        match style {
            VersionFileStyle::HatchVcsHook => {
                if let Ok(contents) = fs::read_to_string(&version_path) {
                    let has_version = contents.contains("version =");
                    let has_alias = contents.contains("__version__");
                    let fallback_version = contents.lines().find_map(|line| {
                        let trimmed = line.trim_start();
                        if !trimmed.starts_with("version =") {
                            return None;
                        }
                        let value = trimmed
                            .split_once('=')
                            .map(|(_, rhs)| rhs.trim().trim_matches('"'))
                            .unwrap_or_default();
                        Some(
                            value == "unknown"
                                || value.starts_with("0.0.0+")
                                || value.starts_with("0+"),
                        )
                    });
                    let needs_upgrade = fallback_version.unwrap_or(false);
                    if !(has_version && has_alias) || needs_upgrade {
                        rewrite = true;
                    }
                } else {
                    rewrite = true;
                }
            }
            VersionFileStyle::SetuptoolsScm => {
                if let Ok(contents) = fs::read_to_string(&version_path) {
                    let has_version = contents.contains("version =");
                    let has_alias = contents.contains("__version__");
                    let has_tuple = contents.contains("version_tuple = tuple(_v.release)");
                    let has_packaging =
                        contents.contains("from packaging.version import Version as _Version");
                    let fallback_version = contents.lines().find_map(|line| {
                        let trimmed = line.trim_start();
                        if !trimmed.starts_with("version =") {
                            return None;
                        }
                        let value = trimmed
                            .split_once('=')
                            .map(|(_, rhs)| rhs.trim().trim_matches('"'))
                            .unwrap_or_default();
                        Some(
                            value == "unknown"
                                || value.starts_with("0.0.0+")
                                || value.starts_with("0+"),
                        )
                    });
                    let needs_upgrade = fallback_version.unwrap_or(false);
                    if !(has_version && has_alias && has_tuple && has_packaging) || needs_upgrade {
                        rewrite = true;
                    }
                } else {
                    rewrite = true;
                }
            }
            VersionFileStyle::Plain => {
                if let Ok(contents) = fs::read_to_string(&version_path) {
                    let trimmed = contents.trim();
                    if trimmed.is_empty()
                        || trimmed == "unknown"
                        || trimmed.starts_with("0.0.0+")
                        || trimmed.starts_with("0+")
                    {
                        rewrite = true;
                    }
                } else {
                    rewrite = true;
                }
            }
        }
        if !rewrite {
            return Ok(());
        }
    }
    if !version_path.exists() || rewrite {
        if let Some(parent) = version_path.parent() {
            fs::create_dir_all(parent)?;
        }
    }

    let derived = match derive_vcs_version(root, &derive_opts) {
        Ok(version) => version,
        Err(err) => {
            warn!(
                error = %err,
                path = %root.display(),
                "git metadata unavailable; writing fallback vcs version"
            );
            if derive_opts.drop_local {
                "0.0.0".to_string()
            } else {
                "0.0.0+unknown".to_string()
            }
        }
    };

    let contents = match style {
        VersionFileStyle::HatchVcsHook => format!(
            "version = \"{derived}\"\n\
__version__ = version\n\
__all__ = [\"__version__\", \"version\"]\n"
        ),
        VersionFileStyle::SetuptoolsScm => format!(
            "from packaging.version import Version as _Version\n\
version = \"{derived}\"\n\
__version__ = version\n\
_v = _Version(version)\n\
version_tuple = tuple(_v.release)\n\
__all__ = [\"__version__\", \"version\", \"version_tuple\"]\n"
        ),
        VersionFileStyle::Plain => format!("{derived}\n"),
    };
    if rewrite || !version_path.exists() {
        fs::write(&version_path, contents)?;
    }
    Ok(())
}

pub(super) fn derive_vcs_version(
    manifest_dir: &Path,
    derive_opts: &VersionDeriveOptions<'_>,
) -> Result<String> {
    if let Some(command) = derive_opts.git_describe_command {
        if let Some(info) = describe_with_command(command, manifest_dir) {
            if let Some(version) = format_version_from_describe(&info, derive_opts) {
                return Ok(version);
            }
        }
    }

    let default_describe = [
        "git".to_string(),
        "describe".to_string(),
        "--tags".to_string(),
        "--dirty".to_string(),
        "--long".to_string(),
    ];
    if let Some(info) = describe_with_command(&default_describe, manifest_dir) {
        if let Some(version) = format_version_from_describe(&info, derive_opts) {
            return Ok(version);
        }
    }

    if let Ok(output) = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(manifest_dir)
        .output()
    {
        if output.status.success() {
            let hash = String::from_utf8_lossy(&output.stdout)
                .trim()
                .trim_start_matches('g')
                .to_string();
            if !hash.is_empty() {
                if derive_opts.drop_local {
                    return Ok("0.0.0".to_string());
                }
                return Ok(format!("0.0.0+g{hash}"));
            }
        }
    }

    Err(anyhow!(
        "unable to derive version from git; add tags or version-file"
    ))
}

fn describe_with_command(command: &[String], manifest_dir: &Path) -> Option<GitDescribeInfo> {
    let (program, args) = command.split_first()?;
    if program.trim().is_empty() {
        return None;
    }
    let output = Command::new(program)
        .args(args)
        .current_dir(manifest_dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_git_describe(String::from_utf8_lossy(&output.stdout).trim())
}

fn format_version_from_describe(
    info: &GitDescribeInfo,
    derive_opts: &VersionDeriveOptions<'_>,
) -> Option<String> {
    if derive_opts.simplified_semver {
        if let Some(version) = simplified_semver_from_describe(info, derive_opts.drop_local) {
            return Some(version);
        }
    }
    pep440_from_info(info, derive_opts.drop_local)
}

#[cfg(test)]
pub(super) fn pep440_from_describe(desc: &str) -> Option<String> {
    parse_git_describe(desc).and_then(|info| pep440_from_info(&info, false))
}

fn pep440_from_info(info: &GitDescribeInfo, drop_local: bool) -> Option<String> {
    let tag = info.tag.trim_start_matches('v');
    let mut version = tag.to_string();
    if !drop_local {
        version.push_str(&format!("+{}.g{}", info.commits_since_tag, info.sha));
        if info.dirty {
            version.push_str(".dirty");
        }
    }
    Some(version)
}

fn simplified_semver_from_describe(info: &GitDescribeInfo, drop_local: bool) -> Option<String> {
    let numeric_start = info
        .tag
        .find(|ch: char| ch.is_ascii_digit())
        .or_else(|| info.tag.find('v').map(|index| index + 1))?;
    let base = info.tag[numeric_start..]
        .trim_start_matches('v')
        .to_string();
    if base.is_empty() {
        return None;
    }
    let mut release_parts: Vec<u64> = base
        .split('.')
        .map(|part| part.parse::<u64>().ok())
        .collect::<Option<Vec<_>>>()?;
    if release_parts.is_empty() {
        return None;
    }
    if info.commits_since_tag > 0 {
        if let Some(last) = release_parts.last_mut() {
            *last += 1;
        }
    }
    let mut version = release_parts
        .iter()
        .map(|part| part.to_string())
        .collect::<Vec<_>>()
        .join(".");
    if info.commits_since_tag > 0 {
        version.push_str(&format!(".dev{}", info.commits_since_tag));
    }
    let has_local = !drop_local && (info.commits_since_tag > 0 || info.dirty);
    if has_local {
        version.push_str(&format!("+g{}", info.sha));
        if info.dirty {
            version.push_str(".dirty");
        }
    }
    Some(version)
}

fn parse_git_describe(desc: &str) -> Option<GitDescribeInfo> {
    let trimmed = desc.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut dirty = false;
    let mut core = trimmed.to_string();
    if core.ends_with("-dirty") {
        dirty = true;
        core = core.trim_end_matches("-dirty").to_string();
    }
    let mut iter = core.rsplitn(3, '-');
    let sha_part = iter.next()?;
    let commits_part = iter.next()?;
    let tag_part = iter.next()?;

    Some(GitDescribeInfo {
        tag: tag_part.to_string(),
        commits_since_tag: commits_part.parse::<usize>().ok()?,
        sha: sha_part.trim_start_matches('g').to_string(),
        dirty,
    })
}

struct GitDescribeInfo {
    tag: String,
    commits_since_tag: usize,
    sha: String,
    dirty: bool,
}

pub(crate) struct PythonPathInfo {
    pub(crate) pythonpath: String,
    pub(crate) allowed_paths: Vec<PathBuf>,
    pub(crate) site_bin: Option<PathBuf>,
    pub(crate) pep582_bin: Vec<PathBuf>,
}

pub(super) fn detect_local_site_packages(
    fs: &dyn effects::FileSystem,
    site_dir: &Path,
) -> Option<PathBuf> {
    let lib_dir = site_dir.join("lib");
    if let Ok(entries) = fs.read_dir(&lib_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if let Some(name) = path.file_name().and_then(|value| value.to_str()) {
                if !name.starts_with("python") {
                    continue;
                }
            }
            let candidate = path.join("site-packages");
            if fs.metadata(&candidate).is_ok() {
                return Some(candidate);
            }
        }
    }
    let fallback = site_dir.join("site-packages");
    fs.metadata(&fallback).ok().map(|_| fallback)
}

fn discover_code_generator_paths(
    fs: &dyn effects::FileSystem,
    project_root: &Path,
    max_depth: usize,
) -> Vec<PathBuf> {
    let mut extras = Vec::new();
    let mut stack = vec![(project_root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = fs.read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = entry.file_name();
            if name
                .to_str()
                .is_some_and(|value| value == "code_generators")
            {
                extras.push(path.clone());
                continue;
            }
            if depth < max_depth {
                stack.push((path, depth + 1));
            }
        }
    }
    extras
}

pub(crate) fn build_pythonpath(
    fs: &dyn effects::FileSystem,
    project_root: &Path,
    site_override: Option<PathBuf>,
) -> Result<PythonPathInfo> {
    let site_dir = match site_override {
        Some(dir) => dir,
        None => resolve_project_site(fs, project_root)?,
    };

    let mut site_paths = Vec::new();
    let mut site_packages_used = None;
    let code_paths = discover_code_generator_paths(fs, project_root, 3);

    let canonical = fs.canonicalize(&site_dir).unwrap_or(site_dir.clone());
    let site_dir_used = Some(canonical.clone());
    site_paths.push(canonical.clone());
    if let Some(site_packages) = detect_local_site_packages(fs, &canonical) {
        site_packages_used = Some(site_packages.clone());
        site_paths.push(site_packages.clone());
        if let Ok(canon) = fs.canonicalize(&site_packages) {
            if canon != site_packages {
                site_paths.push(canon);
            }
        }
    }
    let pth = canonical.join("px.pth");
    if pth.exists() {
        if let Ok(contents) = fs.read_to_string(&pth) {
            for line in contents.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let entry_path = PathBuf::from(trimmed);
                if entry_path.exists() {
                    site_paths.push(entry_path);
                }
            }
        }
    }

    let mut project_paths = Vec::new();
    let src = project_root.join("src");
    if src.exists() {
        project_paths.push(src);
    }
    let python_dir = project_root.join("python");
    if python_dir.exists() {
        project_paths.push(python_dir);
    }
    let mut child_projects = Vec::new();
    if let Ok(entries) = fs.read_dir(project_root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let manifest = path.join("pyproject.toml");
            if fs.metadata(&manifest).is_ok() {
                child_projects.push(path);
            }
        }
    }
    child_projects.sort();
    for path in child_projects {
        if path != project_root {
            project_paths.push(path);
        }
    }
    project_paths.push(project_root.to_path_buf());

    let mut pep582_libs = Vec::new();
    let mut pep582_bins = Vec::new();
    let pep582_root = project_root.join("__pypackages__");
    if pep582_root.exists() {
        if let Ok(entries) = fs.read_dir(&pep582_root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let lib = path.join("lib");
                if lib.exists() {
                    pep582_libs.push(lib);
                } else {
                    pep582_libs.push(path.clone());
                }
                let bin = path.join("bin");
                if bin.exists() {
                    pep582_bins.push(bin);
                }
            }
        }
    }

    let mut paths = Vec::new();
    if let Some(dir) = site_dir_used.as_ref() {
        paths.push(dir.clone());
    }
    paths.extend(code_paths.clone());
    paths.extend(project_paths.clone());
    if let Some(pkgs) = site_packages_used.as_ref() {
        paths.push(pkgs.clone());
    }
    for path in site_paths {
        if Some(&path) == site_dir_used.as_ref() {
            continue;
        }
        if site_packages_used
            .as_ref()
            .is_some_and(|pkgs| pkgs == &path)
        {
            continue;
        }
        if project_paths.iter().any(|pkg| pkg == &path) {
            continue;
        }
        if code_paths.iter().any(|extra| extra == &path) {
            continue;
        }
        paths.push(path);
    }
    paths.extend(pep582_libs);
    paths.retain(|p| p.exists());
    if paths.is_empty() {
        paths.push(project_root.to_path_buf());
    }

    let joined = env::join_paths(&paths).context("failed to build PYTHONPATH")?;
    let pythonpath = joined
        .into_string()
        .map_err(|_| anyhow!("pythonpath contains non-UTF paths"))?;
    let site_bin = site_dir_used
        .map(|dir| dir.join("bin"))
        .filter(|bin| bin.exists());
    Ok(PythonPathInfo {
        pythonpath,
        allowed_paths: paths,
        site_bin,
        pep582_bin: pep582_bins,
    })
}

pub(crate) fn ensure_environment_with_guard(
    ctx: &CommandContext,
    snapshot: &ManifestSnapshot,
    guard: EnvGuard,
) -> Result<Option<EnvironmentSyncReport>> {
    match ensure_project_environment_synced(ctx, snapshot) {
        Ok(()) => Ok(None),
        Err(err) => match err.downcast::<InstallUserError>() {
            Ok(user) => match guard {
                EnvGuard::Strict => Err(user.into()),
                EnvGuard::AutoSync => {
                    if let Some(issue) = EnvironmentIssue::from_details(&user.details) {
                        if issue.auto_fixable() {
                            auto_sync_environment(ctx, snapshot, issue)
                        } else {
                            Err(user.into())
                        }
                    } else {
                        Err(user.into())
                    }
                }
            },
            Err(err) => Err(err),
        },
    }
}

fn log_autosync_step(message: &str) {
    eprintln!("px ▸ {message}");
}

pub(crate) fn auto_sync_environment(
    ctx: &CommandContext,
    snapshot: &ManifestSnapshot,
    issue: EnvironmentIssue,
) -> Result<Option<EnvironmentSyncReport>> {
    if issue.needs_lock_resolution() {
        if let Some(message) = issue.lock_message() {
            log_autosync_step(message);
        }
        install_snapshot(ctx, snapshot, false, None)?;
    }
    log_autosync_step(issue.env_message());
    refresh_project_site(snapshot, ctx)?;
    Ok(Some(EnvironmentSyncReport::new(issue)))
}

pub(crate) fn attach_autosync_details(
    outcome: &mut ExecutionOutcome,
    report: Option<EnvironmentSyncReport>,
) {
    let Some(report) = report else {
        return;
    };
    let autosync = report.to_json();
    match outcome.details {
        Value::Object(ref mut map) => {
            map.insert("autosync".to_string(), autosync);
        }
        Value::Null => {
            outcome.details = json!({ "autosync": autosync });
        }
        ref mut other => {
            let previous = other.take();
            outcome.details = json!({
                "value": previous,
                "autosync": autosync,
            });
        }
    }
}

pub(crate) fn python_context(ctx: &CommandContext) -> Result<PythonContext, ExecutionOutcome> {
    python_context_with_mode(ctx, EnvGuard::Strict).map(|(py, _)| py)
}

pub(crate) fn python_context_with_mode(
    ctx: &CommandContext,
    guard: EnvGuard,
) -> Result<(PythonContext, Option<EnvironmentSyncReport>), ExecutionOutcome> {
    match PythonContext::new_with_guard(ctx, guard) {
        Ok(result) => Ok(result),
        Err(err) => {
            if is_missing_project_error(&err) {
                return Err(missing_project_outcome());
            }
            match err.downcast::<InstallUserError>() {
                Ok(user) => Err(ExecutionOutcome::user_error(user.message, user.details)),
                Err(err) => Err(ExecutionOutcome::failure(
                    "failed to prepare python environment",
                    json!({ "error": err.to_string() }),
                )),
            }
        }
    }
}
