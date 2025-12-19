use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    str::FromStr,
};

use anyhow::{anyhow, bail, Context, Result};
use dirs_next::home_dir;
use pep440_rs::{Version, VersionSpecifiers};
use serde::{Deserialize, Serialize};

use crate::python_build;

const REGISTRY_ENV: &str = "PX_RUNTIME_REGISTRY";
const REGISTRY_FILENAME: &str = "runtimes.json";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RuntimeRecord {
    pub version: String,
    pub full_version: String,
    pub path: String,
    pub default: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct RuntimeRegistry {
    runtimes: Vec<RuntimeRecord>,
}

#[derive(Clone, Debug)]
pub struct RuntimeSelection {
    pub record: RuntimeRecord,
    pub source: RuntimeSource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeSource {
    Explicit,
    Requirement,
    Default,
}

impl RuntimeRegistry {
    fn best_for_requirement(&self, requirement: &VersionSpecifiers) -> Option<RuntimeRecord> {
        self.runtimes
            .iter()
            .filter_map(|record| {
                Version::from_str(&record.full_version)
                    .ok()
                    .map(|version| (version, record.clone()))
            })
            .filter(|(version, _)| requirement.contains(version))
            .max_by(|(left, _), (right, _)| left.cmp(right))
            .map(|(_, record)| record)
    }

    fn highest_version(&self, filter: Option<&VersionSpecifiers>) -> Option<RuntimeRecord> {
        let mut candidates: Vec<(Version, RuntimeRecord)> = self
            .runtimes
            .iter()
            .filter_map(|record| {
                Version::from_str(&record.full_version)
                    .ok()
                    .map(|version| (version, record.clone()))
            })
            .collect();
        if let Some(specs) = filter {
            candidates.retain(|(version, _)| specs.contains(version));
        }
        if candidates.is_empty() {
            return None;
        }
        if let Some((_, record)) = candidates
            .iter()
            .find(|(_, record)| record.default)
            .cloned()
        {
            return Some(record);
        }
        candidates
            .into_iter()
            .max_by(|(left, _), (right, _)| left.cmp(right))
            .map(|(_, record)| record)
    }

    fn default_runtime(&self, requirement: Option<&VersionSpecifiers>) -> Option<RuntimeRecord> {
        self.highest_version(requirement)
    }

    fn insert_or_replace(&mut self, record: RuntimeRecord) {
        if let Some(pos) = self
            .runtimes
            .iter()
            .position(|runtime| runtime.version == record.version)
        {
            self.runtimes[pos] = record;
        } else {
            self.runtimes.push(record);
        }
        self.runtimes.sort_by(|a, b| a.version.cmp(&b.version));
    }
}

pub fn registry_path() -> Result<PathBuf> {
    if let Some(path) = env::var_os(REGISTRY_ENV) {
        let path = PathBuf::from(path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        return Ok(path);
    }
    let home = home_dir().ok_or_else(|| anyhow!("home directory not found"))?;
    let dir = home.join(".px");
    fs::create_dir_all(&dir)?;
    Ok(dir.join(REGISTRY_FILENAME))
}

fn load_registry() -> Result<RuntimeRegistry> {
    let path = registry_path()?;
    if !path.exists() {
        return Ok(RuntimeRegistry::default());
    }
    let contents = fs::read_to_string(&path)
        .with_context(|| format!("reading runtime registry at {}", path.display()))?;
    let registry: RuntimeRegistry = serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse runtime registry at {}", path.display()))?;
    Ok(registry)
}

fn save_registry(registry: &RuntimeRegistry) -> Result<()> {
    let path = registry_path()?;
    let contents = serde_json::to_string_pretty(registry)?;
    fs::write(&path, contents + "\n")
        .with_context(|| format!("writing runtime registry at {}", path.display()))
}

pub fn list_runtimes() -> Result<Vec<RuntimeRecord>> {
    let mut registry = load_registry()?;
    registry
        .runtimes
        .retain(|runtime| Path::new(&runtime.path).exists());
    save_registry(&registry)?;
    Ok(registry.runtimes)
}

pub fn install_runtime(
    requested_version: &str,
    explicit_path: Option<&str>,
    set_default: bool,
) -> Result<RuntimeRecord> {
    let requested_channel = normalize_channel(requested_version)?;
    let interpreter_path = if let Some(path) = explicit_path {
        fs::canonicalize(path)
            .with_context(|| format!("unable to resolve python path at {path}"))?
    } else {
        python_build::install_python(&requested_channel)?
    };
    if !interpreter_path.exists() {
        bail!(
            "python interpreter not found at {}",
            interpreter_path.display()
        );
    }
    let details = inspect_python(&interpreter_path)?;
    let channel = format_channel(&details.full_version)?;
    if channel != requested_channel {
        if explicit_path.is_some() {
            bail!(
                "python at {} reports version {} but `{}` was requested",
                details.executable,
                details.full_version,
                requested_channel
            );
        } else {
            bail!(
                "downloaded python runtime reports version {} but `{}` was requested",
                details.full_version,
                requested_channel
            );
        }
    }
    let mut registry = load_registry()?;
    let mut record = RuntimeRecord {
        version: channel,
        full_version: details.full_version,
        path: details.executable,
        default: false,
    };
    if set_default || registry.runtimes.is_empty() {
        record.default = true;
        for runtime in &mut registry.runtimes {
            runtime.default = false;
        }
    }
    registry.insert_or_replace(record.clone());
    save_registry(&registry)?;
    Ok(record)
}

fn managed_runtimes_root() -> Option<PathBuf> {
    registry_path()
        .ok()
        .and_then(|path| path.parent().map(|parent| parent.join("runtimes")))
}

fn is_px_managed(record: &RuntimeRecord) -> bool {
    let Some(root) = managed_runtimes_root() else {
        return false;
    };
    Path::new(&record.path).starts_with(root)
}

pub fn resolve_runtime(
    override_version: Option<&str>,
    requirement: &str,
) -> Result<RuntimeSelection> {
    let specifiers = VersionSpecifiers::from_str(requirement)
        .map_err(|err| anyhow!("invalid requires-python `{requirement}`: {err}"))?;
    let registry = load_registry()?;
    let mut managed: Vec<RuntimeRecord> = Vec::new();
    let mut external: Vec<RuntimeRecord> = Vec::new();
    for runtime in registry.runtimes {
        if is_px_managed(&runtime) {
            managed.push(runtime);
        } else {
            external.push(runtime);
        }
    }

    if let Some(version) = override_version {
        let requested = normalize_channel(version)?;
        if let Some(record) = managed
            .iter()
            .find(|runtime| runtime.version == requested)
            .cloned()
            .or_else(|| {
                external
                    .iter()
                    .find(|runtime| runtime.version == requested)
                    .cloned()
            })
        {
            if let Ok(runtime_version) = Version::from_str(&record.full_version) {
                if !specifiers.contains(&runtime_version) {
                    bail!(
                        "px runtime {requested} ({}) does not satisfy requires-python `{requirement}`",
                        record.full_version
                    );
                }
            }
            return Ok(RuntimeSelection {
                record,
                source: RuntimeSource::Explicit,
            });
        }
        bail!("px runtime {requested} is not installed; run `px python install {requested}`");
    }

    let managed_registry = RuntimeRegistry {
        runtimes: managed.clone(),
    };
    let external_registry = RuntimeRegistry { runtimes: external };
    if let Some(record) = managed_registry.best_for_requirement(&specifiers) {
        return Ok(RuntimeSelection {
            record,
            source: RuntimeSource::Requirement,
        });
    }

    if let Some(record) = external_registry.best_for_requirement(&specifiers) {
        return Ok(RuntimeSelection {
            record,
            source: RuntimeSource::Requirement,
        });
    }

    if let Some(record) = managed_registry.default_runtime(Some(&specifiers)) {
        return Ok(RuntimeSelection {
            record,
            source: RuntimeSource::Default,
        });
    }

    if let Some(record) = external_registry.default_runtime(Some(&specifiers)) {
        return Ok(RuntimeSelection {
            record,
            source: RuntimeSource::Default,
        });
    }

    bail!("no px runtime satisfies `{requirement}`; run `px python install <version>`");
}

pub(crate) struct PythonDetails {
    pub(crate) full_version: String,
    pub(crate) executable: String,
}

pub(crate) fn inspect_python(path: &Path) -> Result<PythonDetails> {
    const SCRIPT: &str =
        "import json, platform, sys; print(json.dumps({'version': platform.python_version(), 'executable': sys.executable}))";
    let output = Command::new(path)
        .arg("-c")
        .arg(SCRIPT)
        .output()
        .with_context(|| format!("failed to inspect python at {}", path.display()))?;
    if !output.status.success() {
        bail!(
            "python exited with {} while probing",
            output.status.code().unwrap_or(-1)
        )
    }
    let payload: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("invalid runtime inspection payload")?;
    let version = payload
        .get("version")
        .and_then(|value| value.as_str())
        .ok_or_else(|| anyhow!("python inspection missing version"))?
        .to_string();
    let executable = payload
        .get("executable")
        .and_then(|value| value.as_str())
        .ok_or_else(|| anyhow!("python inspection missing executable"))?
        .to_string();
    Ok(PythonDetails {
        full_version: version,
        executable,
    })
}

fn parse_channel(input: &str) -> Result<(u64, u64)> {
    let mut parts = input.split('.');
    let major = parts
        .next()
        .ok_or_else(|| anyhow!("python version missing major"))?
        .parse::<u64>()?;
    let minor = parts
        .next()
        .ok_or_else(|| anyhow!("python version missing minor"))?
        .parse::<u64>()?;
    Ok((major, minor))
}

pub(crate) fn format_channel(version: &str) -> Result<String> {
    let (major, minor) = parse_channel(version)?;
    Ok(format!("{major}.{minor}"))
}

pub fn normalize_channel(version: &str) -> Result<String> {
    let (major, minor) = parse_channel(version)?;
    Ok(format!("{major}.{minor}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::env;
    use std::fs;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    struct RegistryEnvGuard;

    impl RegistryEnvGuard {
        fn set(path: &Path) -> Self {
            env::set_var(REGISTRY_ENV, path);
            Self
        }
    }

    impl Drop for RegistryEnvGuard {
        fn drop(&mut self) {
            env::remove_var(REGISTRY_ENV);
        }
    }

    fn registry_path_in(temp: &TempDir, name: &str) -> (PathBuf, RegistryEnvGuard) {
        let path = temp.path().join(name);
        let guard = RegistryEnvGuard::set(&path);
        (path, guard)
    }

    fn write_registry(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create registry dir");
        }
        fs::write(path, contents).expect("write registry");
    }

    #[test]
    #[serial]
    fn resolve_runtime_rejects_invalid_requirement() {
        let temp = TempDir::new().unwrap();
        let (registry, _guard) = registry_path_in(&temp, "runtimes/registry.json");
        write_registry(
            &registry,
            r#"{"runtimes":[{"version":"3.13","full_version":"3.13.7","path":"/usr/bin/python3","default":true}]}"#,
        );
        let err = resolve_runtime(None, "not-a-spec").unwrap_err();
        assert!(
            err.to_string().contains("invalid requires-python"),
            "error message should point to invalid requirement: {err}"
        );
    }

    #[test]
    #[serial]
    fn resolve_runtime_prefers_highest_available_default() {
        let temp = TempDir::new().unwrap();
        let (registry, _guard) = registry_path_in(&temp, "runtimes.json");
        write_registry(
            &registry,
            r#"{"runtimes":[
                {"version":"3.10","full_version":"3.10.12","path":"/usr/bin/python3.10","default":false},
                {"version":"3.12","full_version":"3.12.2","path":"/usr/bin/python3.12","default":false}
            ]}"#,
        );
        let selection = resolve_runtime(None, ">=3.9").expect("select runtime");
        assert_eq!(selection.record.version, "3.12");
        assert_eq!(selection.source, RuntimeSource::Requirement);
    }

    #[test]
    #[serial]
    fn resolve_runtime_allows_external_runtimes() {
        let temp = TempDir::new().unwrap();
        let (registry, _guard) = registry_path_in(&temp, "registry.json");
        write_registry(
            &registry,
            r#"{"runtimes":[{"version":"3.13","full_version":"3.13.7","path":"/usr/bin/python3","default":true}]}"#,
        );
        let selection = resolve_runtime(None, ">=3.0").expect("select runtime");
        assert_eq!(selection.record.path, "/usr/bin/python3");
    }

    #[test]
    #[serial]
    fn resolve_runtime_rejects_explicit_incompatible_runtime() {
        let temp = TempDir::new().unwrap();
        let (registry, _guard) = registry_path_in(&temp, "registry.json");
        write_registry(
            &registry,
            r#"{"runtimes":[{"version":"3.9","full_version":"3.9.25","path":"/usr/bin/python3.9","default":true}]}"#,
        );
        let err = resolve_runtime(Some("3.9"), ">=3.11").unwrap_err();
        assert!(
            err.to_string()
                .contains("does not satisfy requires-python `>=3.11`"),
            "expected incompatibility error, got {err}"
        );
    }

    #[test]
    #[serial]
    fn list_runtimes_reports_registry_parse_errors() {
        let temp = TempDir::new().unwrap();
        let (registry, _guard) = registry_path_in(&temp, "broken.json");
        write_registry(&registry, "not-json");
        let err = list_runtimes().unwrap_err();
        assert!(
            err.to_string().contains("failed to parse runtime registry"),
            "expected parse failure, got {err}"
        );
    }

    #[test]
    #[serial]
    fn install_runtime_creates_registry_parents() {
        let temp = TempDir::new().unwrap();
        let (registry, _guard) = registry_path_in(&temp, "nested/path/registry.json");
        let python = which::which("python3").unwrap();
        let details = inspect_python(&python).unwrap();
        let channel = format_channel(&details.full_version).unwrap();
        let record = install_runtime(
            &channel,
            Some(python.to_str().expect("python path utf8")),
            true,
        )
        .expect("install runtime");
        assert!(PathBuf::from(record.path).is_absolute());
        assert!(registry.exists());
    }

    #[test]
    #[cfg(unix)]
    #[serial]
    fn install_runtime_canonicalizes_relative_paths() {
        let temp = TempDir::new().unwrap();
        let (_registry, _guard) = registry_path_in(&temp, "runtimes.json");
        let target = which::which("python3").unwrap();
        let link = temp.path().join("py");
        symlink(&target, &link).unwrap();
        let cwd = env::current_dir().unwrap();
        env::set_current_dir(temp.path()).unwrap();
        let details = inspect_python(&target).unwrap();
        let channel = format_channel(&details.full_version).unwrap();
        let record = install_runtime(&channel, Some("./py"), true).expect("install runtime");
        env::set_current_dir(cwd).unwrap();
        assert!(PathBuf::from(record.path).is_absolute());
    }
}
