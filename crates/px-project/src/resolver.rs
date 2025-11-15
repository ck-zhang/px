use std::process::Command;

use anyhow::{bail, Context, Result};
use pep508_rs::MarkerEnvironment;
use px_python;
use px_resolver::ResolverEnv;
use serde::Deserialize;

#[derive(Clone, Debug)]
pub struct PinSpec {
    pub name: String,
    pub specifier: String,
    pub version: String,
    pub normalized: String,
    pub extras: Vec<String>,
    pub marker: Option<String>,
}

#[derive(Clone, Debug)]
pub struct InstallOverride {
    pub dependencies: Vec<String>,
    pub pins: Vec<PinSpec>,
}

#[derive(Clone, Debug)]
pub struct ResolvedSpecOutput {
    pub specs: Vec<String>,
    pub pins: Vec<PinSpec>,
}

pub fn current_marker_environment() -> Result<MarkerEnvironment> {
    let python = px_python::detect_interpreter()?;
    let resolver_env = detect_marker_environment_with(&python)?;
    resolver_env.to_marker_environment()
}

fn detect_marker_environment_with(python: &str) -> Result<ResolverEnv> {
    let script = r#"import json, os, platform, sys
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
    let cmd = Command::new(python)
        .arg("-c")
        .arg(script)
        .output()
        .with_context(|| format!("failed to probe marker environment via {python}"))?;
    if !cmd.status.success() {
        let stderr = String::from_utf8_lossy(&cmd.stderr);
        bail!("python marker probe failed: {stderr}");
    }
    let payload: MarkerEnvPayload =
        serde_json::from_slice(&cmd.stdout).context("invalid marker env payload")?;
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
