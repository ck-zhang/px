use std::{fs, path::PathBuf, process::Command};

use anyhow::{anyhow, bail, Result};
use serde_json::json;
use sha2::{Digest, Sha256};
use tracing::debug;

use crate::core::runtime::facade::RuntimeMetadata;
use crate::store::cas::{archive_selected_filtered, RuntimeHeader};

pub(super) fn runtime_header(runtime: &RuntimeMetadata) -> Result<RuntimeHeader> {
    let tags = crate::python_sys::detect_interpreter_tags(&runtime.path)?;
    let abi = tags
        .abi
        .first()
        .cloned()
        .unwrap_or_else(|| "none".to_string());
    let platform = tags
        .platform
        .first()
        .cloned()
        .unwrap_or_else(|| "any".to_string());
    let exe_path = {
        let python_path = PathBuf::from(&runtime.path);
        let python_path = fs::canonicalize(&python_path).unwrap_or(python_path);
        python_path
            .parent()
            .and_then(|bin| bin.parent())
            .and_then(|root| python_path.strip_prefix(root).ok())
            .map(|rel| rel.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|| {
                let name = python_path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "python".to_string());
                format!("bin/{name}")
            })
    };
    Ok(RuntimeHeader {
        version: runtime.version.clone(),
        abi,
        platform,
        build_config_hash: runtime_config_hash(&tags),
        exe_path,
    })
}

pub(super) fn runtime_archive(runtime: &RuntimeMetadata) -> Result<Vec<u8>> {
    let python_path = PathBuf::from(&runtime.path);
    let python_path = fs::canonicalize(&python_path).unwrap_or(python_path);
    let Some(bin_dir) = python_path.parent() else {
        return Err(anyhow!(
            "unable to resolve runtime root for {}",
            runtime.path
        ));
    };
    let root_dir = bin_dir
        .parent()
        .ok_or_else(|| anyhow!("unable to resolve runtime root for {}", runtime.path))?;
    let root_dir = fs::canonicalize(root_dir).unwrap_or_else(|_| root_dir.to_path_buf());
    let mut include_paths = python_runtime_paths(runtime)?;
    let probed = !include_paths.is_empty();
    include_paths.push(python_path.clone());
    if !probed {
        let version_tag = runtime
            .version
            .split('.')
            .take(2)
            .collect::<Vec<_>>()
            .join(".");
        let version_dir = format!("python{version_tag}");
        include_paths.extend([
            root_dir.join("lib").join(&version_dir),
            root_dir.join("lib64").join(&version_dir),
            root_dir.join("include"),
            root_dir.join("include").join(&version_dir),
            root_dir.join("Include").join(&version_dir),
            root_dir.join("Lib").join(&version_dir),
            root_dir.join("pyvenv.cfg"),
        ]);
    }
    include_paths.retain(|path| path.exists());
    include_paths = include_paths
        .into_iter()
        .map(|path| fs::canonicalize(&path).unwrap_or(path))
        .collect();
    include_paths.sort();
    include_paths.dedup();
    if include_paths.is_empty() {
        bail!("no runtime paths found to archive for {}", runtime.path);
    }
    archive_selected_filtered(&root_dir, &include_paths, |entry| {
        let name = entry.file_name().to_str().unwrap_or_default();
        !matches!(name, "site-packages" | "dist-packages" | "__pycache__")
    })
}

fn runtime_config_hash(tags: &crate::python_sys::InterpreterTags) -> String {
    let payload = json!({
        "python": tags.python,
        "abi": tags.abi,
        "platform": tags.platform,
    });
    let bytes = serde_json::to_vec(&payload).unwrap_or_default();
    hex::encode(Sha256::digest(bytes))
}

pub(super) fn default_build_options_hash(runtime: &RuntimeMetadata) -> String {
    let payload = json!({
        "runtime": runtime.version,
        "platform": runtime.platform,
        "kind": "default",
    });
    hex::encode(Sha256::digest(
        serde_json::to_vec(&payload).unwrap_or_default(),
    ))
}

fn python_runtime_paths(runtime: &RuntimeMetadata) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    let script = r#"
import json, sys, sysconfig
paths = {
    "executable": sys.executable,
    "stdlib": sysconfig.get_path("stdlib"),
    "platstdlib": sysconfig.get_path("platstdlib"),
    "include": sysconfig.get_config_var("INCLUDEPY"),
}
print(json.dumps(paths))
"#;
    match Command::new(&runtime.path).arg("-c").arg(script).output() {
        Ok(output) if output.status.success() => {
            if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&output.stdout) {
                let entries = [
                    value.get("executable"),
                    value.get("stdlib"),
                    value.get("platstdlib"),
                    value.get("include"),
                ];
                for entry in entries.into_iter().flatten() {
                    if let Some(s) = entry.as_str() {
                        if !s.is_empty() {
                            paths.push(PathBuf::from(s));
                        }
                    }
                }
            }
        }
        Ok(_) => {}
        Err(err) => {
            debug!(
                %err,
                python = %runtime.path,
                "failed to probe runtime paths; falling back to interpreter only"
            );
        }
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn runtime_archive_excludes_site_packages() -> Result<()> {
        let interpreter = which::which("python3")
            .or_else(|_| which::which("python"))
            .map_err(|err| anyhow!("no system python found: {err}"))?;
        let details = crate::runtime_manager::inspect_python(&interpreter)?;
        let runtime = RuntimeMetadata {
            path: details.executable,
            version: details.full_version,
            platform: "any".to_string(),
        };
        let bytes = runtime_archive(&runtime)?;
        let decoder = flate2::read::GzDecoder::new(bytes.as_slice());
        let mut archive = tar::Archive::new(decoder);
        for entry in archive.entries()? {
            let entry = entry?;
            let path = entry.path()?;
            let path = path.to_string_lossy();
            assert!(
                !path.contains("site-packages"),
                "runtime archive should not include {path}"
            );
            assert!(
                !path.contains("dist-packages"),
                "runtime archive should not include {path}"
            );
            assert!(
                !path.contains("__pycache__"),
                "runtime archive should not include {path}"
            );
        }
        Ok(())
    }
}
