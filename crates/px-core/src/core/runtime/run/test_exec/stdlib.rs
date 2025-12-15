use std::env;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use flate2::read::GzDecoder;
use serde_json::Value;
use tar::Archive;
use tracing::{debug, warn};

use crate::PythonContext;

pub(super) fn ensure_stdlib_tests_available(py_ctx: &PythonContext) -> Result<Option<PathBuf>> {
    const DISCOVER_SCRIPT: &str =
        "import json, sys, sysconfig; print(json.dumps({'version': sys.version.split()[0], 'stdlib': sysconfig.get_path('stdlib')}))";
    let output = Command::new(&py_ctx.python)
        .arg("-c")
        .arg(DISCOVER_SCRIPT)
        .output()
        .context("probing python stdlib path")?;
    if !output.status.success() {
        bail!(
            "python exited with {} while probing stdlib",
            output.status.code().unwrap_or(-1)
        );
    }
    let payload: Value =
        serde_json::from_slice(&output.stdout).context("invalid stdlib probe payload")?;
    let runtime_version = payload
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let Some((major, minor)) = parse_python_version(&runtime_version) else {
        return Ok(None);
    };
    let stdlib = payload
        .get("stdlib")
        .and_then(Value::as_str)
        .context("python stdlib path unavailable")?;
    let tests_dir = PathBuf::from(stdlib).join("test");
    if tests_dir.exists() {
        return Ok(None);
    }

    // Avoid mutating the system stdlib; stage tests under the project .px directory.
    let staging_base = env::var_os("PX_STDLIB_STAGING_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| py_ctx.state_root.join(".px").join("stdlib-tests"));
    let staged_root = staging_base.join(format!("{major}.{minor}"));
    let staged_tests = staged_root.join("test");
    if staged_tests.exists() {
        return Ok(Some(staged_root));
    }

    if let Some((host_python, source_tests)) = host_stdlib_tests(&major, &minor, &runtime_version) {
        if copy_stdlib_tests(&source_tests, &staged_tests, &host_python).is_ok() {
            return Ok(Some(staged_root));
        }
    }
    if download_stdlib_tests(&runtime_version, &staged_tests)? {
        return Ok(Some(staged_root));
    }

    warn!(
        runtime = %runtime_version,
        tests_dir = %tests_dir.display(),
        "stdlib test suite missing; proceeding without staging tests"
    );
    Ok(None)
}

fn host_stdlib_tests(
    major: &str,
    minor: &str,
    runtime_version: &str,
) -> Option<(PathBuf, PathBuf)> {
    let candidates = [
        format!("python{major}.{minor}"),
        format!("python{major}"),
        "python".to_string(),
    ];
    for candidate in candidates {
        let output = Command::new(&candidate)
            .arg("-c")
            .arg(
                "import json, sys, sysconfig; print(json.dumps({'stdlib': sysconfig.get_path('stdlib'), 'version': sys.version.split()[0], 'executable': sys.executable}))",
            )
            .output();
        let Ok(output) = output else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        let Ok(payload) = serde_json::from_slice::<Value>(&output.stdout) else {
            continue;
        };
        let detected_version = payload
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if !detected_version.starts_with(&format!("{major}.{minor}")) {
            continue;
        }
        let stdlib = payload
            .get("stdlib")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if stdlib.is_empty() {
            continue;
        }
        let tests = PathBuf::from(stdlib).join("test");
        if !tests.exists() {
            continue;
        }
        let exe = payload
            .get("executable")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(candidate.clone()));
        debug!(
            version = %runtime_version,
            source = %exe.display(),
            tests = %tests.display(),
            "found host stdlib tests"
        );
        return Some((exe, tests));
    }
    None
}

fn download_stdlib_tests(version: &str, dest: &Path) -> Result<bool> {
    let url = format!("https://www.python.org/ftp/python/{version}/Python-{version}.tgz");
    let client = crate::core::runtime::build_http_client()?;
    let response = match client.get(&url).send() {
        Ok(resp) => resp,
        Err(err) => {
            debug!(error = %err, url = %url, "failed to download cpython sources");
            return Ok(false);
        }
    };
    if !response.status().is_success() {
        debug!(
            status = %response.status(),
            url = %url,
            "cpython source archive unavailable for stdlib tests"
        );
        return Ok(false);
    }
    let bytes = match response.bytes() {
        Ok(bytes) => bytes,
        Err(err) => {
            debug!(%err, url = %url, "failed to read cpython source archive for stdlib tests");
            return Ok(false);
        }
    };
    let mut archive = Archive::new(GzDecoder::new(Cursor::new(bytes)));
    if dest.exists() {
        fs::remove_dir_all(dest)
            .with_context(|| format!("clearing existing stdlib tests at {}", dest.display()))?;
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating stdlib parent {}", parent.display()))?;
    }
    let prefix = PathBuf::from(format!("Python-{version}/Lib/test"));
    let mut extracted = false;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?;
        let Ok(rel) = path.strip_prefix(&prefix) else {
            continue;
        };
        let dest_path = dest.join(rel);
        if let Some(parent) = dest_path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        entry.unpack(&dest_path)?;
        extracted = true;
    }
    Ok(extracted)
}

fn copy_stdlib_tests(source: &Path, dest: &Path, python: &Path) -> Result<()> {
    let script = r#"
import shutil
import sys
from pathlib import Path

src = Path(sys.argv[1])
dest = Path(sys.argv[2])
shutil.copytree(src, dest, dirs_exist_ok=True, symlinks=True)
"#;
    if dest.exists() {
        fs::remove_dir_all(dest)
            .with_context(|| format!("removing previous stdlib tests at {}", dest.display()))?;
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating stdlib parent {}", parent.display()))?;
    }
    let status = Command::new(python)
        .arg("-c")
        .arg(script)
        .arg(source.as_os_str())
        .arg(dest.as_os_str())
        .status()
        .with_context(|| format!("copying stdlib tests using {}", python.display()))?;
    if !status.success() {
        bail!(
            "python exited with {} while copying stdlib tests",
            status.code().unwrap_or(-1)
        );
    }
    Ok(())
}

fn parse_python_version(version: &str) -> Option<(String, String)> {
    let mut parts = version.split('.');
    let major = parts.next()?.to_string();
    let minor = parts.next().unwrap_or_default().to_string();
    if major.is_empty() || minor.is_empty() {
        return None;
    }
    Some((major, minor))
}
