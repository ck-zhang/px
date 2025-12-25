// Runtime probing + metadata helpers.
use super::*;

#[derive(Clone, Debug)]
pub(crate) struct RuntimeMetadata {
    pub(crate) path: String,
    pub(crate) version: String,
    pub(crate) platform: String,
}

pub(crate) fn prepare_project_runtime(
    snapshot: &ManifestSnapshot,
) -> Result<runtime_manager::RuntimeSelection> {
    if let Ok(explicit) = env::var("PX_RUNTIME_PYTHON") {
        if let Ok(details) = runtime_manager::inspect_python(Path::new(&explicit)) {
            let requirement = snapshot
                .python_override
                .as_deref()
                .unwrap_or(&snapshot.python_requirement);
            if let (Some(specs), Ok(version)) = (
                runtime_requirement_specs(requirement),
                pep440_rs::Version::from_str(&details.full_version),
            ) {
                if specs.contains(&version) {
                    let channel = runtime_manager::format_channel(&details.full_version)
                        .unwrap_or_else(|_| requirement.to_string());
                    let record = runtime_manager::RuntimeRecord {
                        version: channel,
                        full_version: details.full_version,
                        path: details.executable,
                        default: false,
                    };
                    let selection = runtime_manager::RuntimeSelection {
                        record,
                        source: runtime_manager::RuntimeSource::Explicit,
                    };
                    env::set_var("PX_RUNTIME_PYTHON", &selection.record.path);
                    return Ok(selection);
                }
            }
        }
    }

    let selection = runtime_manager::resolve_runtime(
        snapshot.python_override.as_deref(),
        &snapshot.python_requirement,
    )
    .map_err(|err| {
        let requirement = snapshot
            .python_override
            .as_deref()
            .unwrap_or(&snapshot.python_requirement);
        let install_version = suggest_install_runtime_version(requirement);
        InstallUserError::new(
            "python runtime unavailable",
            json!({
                "install_version": install_version,
                "python_requirement": snapshot.python_requirement.as_str(),
                "python_override": snapshot.python_override.as_deref(),
                "hint": err.to_string(),
                "reason": "missing_runtime",
            }),
        )
    })?;
    env::set_var("PX_RUNTIME_PYTHON", &selection.record.path);
    Ok(selection)
}

fn suggest_install_runtime_version(requirement: &str) -> String {
    const DEFAULT: &str = "3.12";
    let Some(specs) = runtime_requirement_specs(requirement) else {
        return DEFAULT.to_string();
    };
    for channel in ["3.12", "3.13", "3.11", "3.10", "3.9", "3.8"] {
        let candidate = format!("{channel}.0");
        if let Ok(version) = pep440_rs::Version::from_str(&candidate) {
            if specs.contains(&version) {
                return channel.to_string();
            }
        }
    }
    DEFAULT.to_string()
}

fn runtime_requirement_specs(requirement: &str) -> Option<pep440_rs::VersionSpecifiers> {
    if let Ok(specs) = pep440_rs::VersionSpecifiers::from_str(requirement) {
        return Some(specs);
    }
    if let Ok(channel) = runtime_manager::normalize_channel(requirement) {
        let spec = format!("=={channel}.*");
        return pep440_rs::VersionSpecifiers::from_str(&spec).ok();
    }
    None
}

pub(crate) fn detect_runtime_metadata(
    ctx: &CommandContext,
    snapshot: &ManifestSnapshot,
) -> Result<RuntimeMetadata> {
    let path = ctx.python_runtime().detect_interpreter()?;
    let version = probe_python_version(ctx, snapshot, &path)?;
    let tags = detect_interpreter_tags(&path)?;
    let platform = tags
        .platform
        .first()
        .cloned()
        .unwrap_or_else(|| "any".to_string());
    Ok(RuntimeMetadata {
        path,
        version,
        platform,
    })
}

fn probe_python_version(
    ctx: &CommandContext,
    snapshot: &ManifestSnapshot,
    python: &str,
) -> Result<String> {
    const SCRIPT: &str =
        "import json, platform; print(json.dumps({'version': platform.python_version()}))";
    let args = vec!["-c".to_string(), SCRIPT.to_string()];
    let output = ctx
        .python_runtime()
        .run_command(python, &args, &[], &snapshot.root)?;
    if output.code != 0 {
        return Err(anyhow!("python exited with {}", output.code));
    }
    let payload: RuntimeProbe =
        serde_json::from_str(output.stdout.trim()).context("invalid runtime probe payload")?;
    Ok(payload.version)
}

#[derive(Deserialize)]
struct RuntimeProbe {
    version: String,
}
