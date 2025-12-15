use anyhow::Result;
use which::which;

use crate::core::runtime::runtime_manager;

pub(crate) fn fallback_runtime_by_channel(
    channel: &str,
) -> Result<runtime_manager::RuntimeSelection> {
    let normalized = runtime_manager::normalize_channel(channel)?;
    let candidates = [
        format!("python{}", normalized.replace('.', "")),
        format!("python{normalized}"),
    ];
    for candidate in candidates {
        if let Ok(path) = which(&candidate) {
            if let Ok(details) = runtime_manager::inspect_python(&path) {
                if runtime_manager::format_channel(&details.full_version).ok()
                    == Some(normalized.clone())
                {
                    let record = runtime_manager::RuntimeRecord {
                        version: normalized.clone(),
                        full_version: details.full_version,
                        path: details.executable,
                        default: false,
                    };
                    return Ok(runtime_manager::RuntimeSelection {
                        record,
                        source: runtime_manager::RuntimeSource::Explicit,
                    });
                }
            }
        }
    }
    Err(anyhow::anyhow!(
        "python runtime {channel} is not installed; run `px python install {channel}`"
    ))
}
