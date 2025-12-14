use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use pep508_rs::MarkerEnvironment;
use px_domain::api::ResolverEnv;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::from_slice;
use which::which;

const MARKER_SCRIPT: &str = r#"import json, os, platform, sys
impl_name = getattr(sys.implementation, "name", "cpython")
impl_version = platform.python_version()
python_full = platform.python_version()
python_short = f"{sys.version_info[0]}.{sys.version_info[1]}"
data = {
    "implementation_name": impl_name,
    "implementation_version": impl_version,
    "os_name": os.name,
    "platform_machine": platform.machine(),
    "platform_python_implementation": platform.python_implementation(),
    "platform_release": platform.release(),
    "platform_system": platform.system(),
    "platform_version": platform.version(),
    "python_full_version": python_full,
    "python_version": python_short,
    "sys_platform": sys.platform,
}
print(json.dumps(data))
"#;

const TAGS_SCRIPT: &str = r#"import json, sys, sysconfig
major = sys.version_info[0]
minor = sys.version_info[1]
py = [f"cp{major}{minor}", f"py{major}{minor}", f"py{major}", "py3"]
abi = [f"cp{major}{minor}", "abi3", "none"]
plat = sysconfig.get_platform().lower().replace("-", "_").replace(".", "_")
data = {"python": py, "abi": abi, "platform": [plat, "any"], "tags": []}

def collect_tags():
    try:
        from pip._internal.utils.compatibility_tags import get_supported
        return list(get_supported())
    except Exception:
        try:
            from packaging import tags as packaging_tags
        except Exception:
            return []
        else:
            return list(packaging_tags.sys_tags())

tags = collect_tags()
if tags:
    data["tags"] = [
        {
            "python": str(tag.interpreter).lower(),
            "abi": str(tag.abi).lower(),
            "platform": str(tag.platform).lower(),
        }
        for tag in tags
    ]

print(json.dumps(data))
"#;

/// Detects the Python interpreter path used by px.
///
/// # Errors
///
/// Returns an error when no compatible interpreter can be found or the detected
/// path is not valid UTF-8.
pub fn detect_interpreter() -> Result<String> {
    if let Ok(explicit) = std::env::var("PX_RUNTIME_PYTHON") {
        return Ok(explicit);
    }

    for candidate in ["python3", "python"] {
        if let Ok(path) = which(candidate) {
            return path
                .into_os_string()
                .into_string()
                .map_err(|_| anyhow!("non-utf8 path"));
        }
    }

    bail!("no python interpreter found; set PX_RUNTIME_PYTHON")
}

/// Probes the marker environment for the given interpreter.
///
/// # Errors
///
/// Returns an error when the interpreter cannot be invoked or the payload is
/// malformed.
pub fn detect_marker_environment(python: &str) -> Result<ResolverEnv> {
    let payload: MarkerEnvPayload = probe_python(python, MARKER_SCRIPT, "marker environment")?;
    Ok(ResolverEnv {
        implementation_name: payload.implementation_name,
        implementation_version: payload.implementation_version,
        os_name: payload.os_name,
        platform_machine: payload.platform_machine,
        platform_python_implementation: payload.platform_python_implementation,
        platform_release: payload.platform_release,
        platform_system: payload.platform_system,
        platform_version: payload.platform_version,
        python_full_version: payload.python_full_version,
        python_version: payload.python_version,
        sys_platform: payload.sys_platform,
    })
}

/// Detects the marker environment for the default interpreter.
///
/// # Errors
///
/// Returns an error when the interpreter cannot be determined or the marker
/// payload cannot be parsed.
#[allow(dead_code)]
pub fn current_marker_environment() -> Result<MarkerEnvironment> {
    let python = detect_interpreter()?;
    let resolver_env = detect_marker_environment(&python)?;
    resolver_env.to_marker_environment()
}

/// Probes the interpreter tags supported by the given interpreter.
///
/// # Errors
///
/// Returns an error when the interpreter cannot be executed or the payload is
/// malformed.
pub fn detect_interpreter_tags(python: &str) -> Result<InterpreterTags> {
    let payload: InterpreterTagsPayload = probe_python(python, TAGS_SCRIPT, "interpreter tags")?;
    let supported = payload
        .tags
        .into_iter()
        .map(|tag| InterpreterSupportedTag {
            python: tag.python,
            abi: tag.abi,
            platform: tag.platform,
        })
        .collect();
    Ok(InterpreterTags {
        python: payload.python,
        abi: payload.abi,
        platform: payload.platform,
        supported,
    })
}

fn probe_python<T>(python: &str, script: &str, guide: &str) -> Result<T>
where
    T: DeserializeOwned,
{
    let cmd = Command::new(python)
        .arg("-c")
        .arg(script)
        .output()
        .with_context(|| format!("failed to probe {guide} via {python}"))?;
    if !cmd.status.success() {
        let stderr = String::from_utf8_lossy(&cmd.stderr);
        bail!("python {guide} probe failed: {stderr}");
    }
    from_slice(&cmd.stdout).context(format!("invalid {guide} payload"))
}

#[derive(Clone, Debug)]
pub struct InterpreterTags {
    pub python: Vec<String>,
    pub abi: Vec<String>,
    pub platform: Vec<String>,
    pub supported: Vec<InterpreterSupportedTag>,
}

impl InterpreterTags {
    #[must_use]
    pub fn supports_triple(&self, py: &str, abi: &str, platform: &str) -> bool {
        if self.supported.is_empty() {
            return false;
        }
        self.supported
            .iter()
            .any(|tag| tag.python == py && tag.abi == abi && tag.platform == platform)
    }
}

#[derive(Clone, Debug)]
pub struct InterpreterSupportedTag {
    pub python: String,
    pub abi: String,
    pub platform: String,
}

#[derive(Deserialize)]
struct InterpreterTagsPayload {
    python: Vec<String>,
    abi: Vec<String>,
    platform: Vec<String>,
    #[serde(default)]
    tags: Vec<InterpreterTagPayload>,
}

#[derive(Deserialize)]
struct InterpreterTagPayload {
    python: String,
    abi: String,
    platform: String,
}

#[derive(Deserialize)]
struct MarkerEnvPayload {
    implementation_name: String,
    implementation_version: String,
    os_name: String,
    platform_machine: String,
    platform_python_implementation: String,
    platform_release: String,
    platform_system: String,
    platform_version: String,
    python_full_version: String,
    python_version: String,
    sys_platform: String,
}
