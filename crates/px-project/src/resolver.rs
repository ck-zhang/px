use std::{process::Command, str::FromStr};

use anyhow::{bail, Context, Result};
use pep508_rs::{MarkerEnvironment, Requirement as PepRequirement};
use px_python;
use px_resolver::ResolverEnv;
use serde::Deserialize;

use crate::manifest::{canonicalize_marker, dependency_name, strip_wrapping_quotes};

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

pub fn merge_resolved_dependencies(
    original: &[String],
    resolved: &[String],
    marker_env: &MarkerEnvironment,
) -> Vec<String> {
    let mut merged = Vec::with_capacity(original.len());
    let mut resolved_iter = resolved.iter();
    for spec in original {
        if spec_requires_pin(spec) && marker_applies(spec, marker_env) {
            if let Some(pinned) = resolved_iter.next() {
                merged.push(pinned.clone());
            } else {
                merged.push(spec.clone());
            }
        } else {
            merged.push(spec.clone());
        }
    }
    merged
}

pub fn marker_applies(spec: &str, marker_env: &MarkerEnvironment) -> bool {
    let cleaned = strip_wrapping_quotes(spec.trim());
    match PepRequirement::from_str(cleaned) {
        Ok(req) => req.evaluate_markers(marker_env, &[]),
        Err(_) => true,
    }
}

pub fn spec_requires_pin(spec: &str) -> bool {
    let head = spec.split(';').next().unwrap_or(spec).trim();
    !head.contains("==")
}

pub fn autopin_spec_key(spec: &str) -> String {
    match PepRequirement::from_str(spec.trim()) {
        Ok(req) => {
            let name = req.name.to_string().to_ascii_lowercase();
            let mut extras = req
                .extras
                .iter()
                .map(|extra| extra.to_string().to_ascii_lowercase())
                .collect::<Vec<_>>();
            extras.sort();
            let extras_part = extras.join(",");
            let marker_part = req
                .marker
                .as_ref()
                .map(|m| canonicalize_marker(&m.to_string()))
                .unwrap_or_default();
            format!("{name}|{extras_part}|{marker_part}")
        }
        Err(_) => {
            let name = dependency_name(spec);
            format!("{name}||")
        }
    }
}

pub fn autopin_pin_key(pin: &PinSpec) -> String {
    let mut extras = pin
        .extras
        .iter()
        .map(|extra| extra.to_ascii_lowercase())
        .collect::<Vec<_>>();
    extras.sort();
    let extras_part = extras.join(",");
    let marker_part = pin
        .marker
        .as_deref()
        .map(canonicalize_marker)
        .unwrap_or_default();
    format!("{}|{extras_part}|{marker_part}", pin.normalized)
}
