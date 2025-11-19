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
use which::which;

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
    System,
}

impl RuntimeRegistry {
    fn find(&self, version: &str) -> Option<RuntimeRecord> {
        self.runtimes
            .iter()
            .find(|runtime| runtime.version == version)
            .cloned()
    }

    fn best_for_requirement(&self, requirement: &str) -> Option<RuntimeRecord> {
        let specifiers = VersionSpecifiers::from_str(requirement).ok()?;
        self.runtimes
            .iter()
            .filter_map(|record| {
                Version::from_str(&record.full_version)
                    .ok()
                    .map(|version| (version, record.clone()))
            })
            .filter(|(version, _)| specifiers.contains(version))
            .max_by(|(left, _), (right, _)| left.cmp(right))
            .map(|(_, record)| record)
    }

    fn default_runtime(&self) -> Option<RuntimeRecord> {
        self.runtimes
            .iter()
            .find(|runtime| runtime.default)
            .cloned()
            .or_else(|| self.runtimes.first().cloned())
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
        return Ok(PathBuf::from(path));
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
    Ok(serde_json::from_str(&contents).unwrap_or_default())
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
    let interpreted_path = if let Some(path) = explicit_path {
        PathBuf::from(path)
    } else {
        discover_python_binary(requested_version)?
    };
    if !interpreted_path.exists() {
        bail!(
            "python interpreter not found at {}",
            interpreted_path.display()
        );
    }
    let details = inspect_python(&interpreted_path)?;
    let channel = format_channel(&details.full_version)?;
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

pub fn resolve_runtime(
    override_version: Option<&str>,
    requirement: &str,
) -> Result<RuntimeSelection> {
    let registry = load_registry()?;
    if let Some(version) = override_version {
        if let Some(record) = registry.find(version) {
            return Ok(RuntimeSelection {
                record,
                source: RuntimeSource::Explicit,
            });
        }
        bail!("px runtime {version} is not installed; run `px python install {version}`");
    }

    if let Some(record) = registry.best_for_requirement(requirement) {
        return Ok(RuntimeSelection {
            record,
            source: RuntimeSource::Requirement,
        });
    }

    if let Some(record) = registry.default_runtime() {
        if requirement_allows(requirement, &record.full_version) {
            return Ok(RuntimeSelection {
                record,
                source: RuntimeSource::Default,
            });
        }
    }

    let system = inspect_system_python()?;
    if requirement_allows(requirement, &system.full_version) {
        return Ok(RuntimeSelection {
            record: system,
            source: RuntimeSource::System,
        });
    }

    bail!("no python runtime satisfies `{requirement}`");
}

fn requirement_allows(requirement: &str, version: &str) -> bool {
    VersionSpecifiers::from_str(requirement)
        .ok()
        .and_then(|specifiers| {
            Version::from_str(version)
                .ok()
                .map(|ver| specifiers.contains(&ver))
        })
        .unwrap_or(true)
}

fn discover_python_binary(version: &str) -> Result<PathBuf> {
    let (major, minor) = parse_channel(version)?;
    let candidates = [
        format!("python{major}.{minor}"),
        format!("python{major}"),
        "python3".to_string(),
        "python".to_string(),
    ];
    for candidate in candidates {
        if let Ok(path) = which(&candidate) {
            return Ok(path);
        }
    }
    bail!("unable to locate python {version}; pass --path to px python install")
}

fn inspect_system_python() -> Result<RuntimeRecord> {
    let path = which("python3").or_else(|_| which("python"))?;
    let details = inspect_python(&path)?;
    let channel = format_channel(&details.full_version)?;
    Ok(RuntimeRecord {
        version: channel,
        full_version: details.full_version,
        path: details.executable,
        default: false,
    })
}

struct PythonDetails {
    full_version: String,
    executable: String,
}

fn inspect_python(path: &Path) -> Result<PythonDetails> {
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

fn format_channel(version: &str) -> Result<String> {
    let (major, minor) = parse_channel(version)?;
    Ok(format!("{major}.{minor}"))
}
