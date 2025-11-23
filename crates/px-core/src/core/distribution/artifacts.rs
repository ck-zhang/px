use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{relative_path_str, PythonContext};

#[derive(Clone, Copy, Debug)]
pub(crate) struct BuildTargets {
    pub(crate) sdist: bool,
    pub(crate) wheel: bool,
}

impl BuildTargets {
    pub(crate) fn label(self) -> &'static str {
        match (self.sdist, self.wheel) {
            (true, true) => "both",
            (true, false) => "sdist",
            (false, true) => "wheel",
            (false, false) => "none",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ArtifactSummary {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}

pub(crate) fn collect_artifact_summaries(
    dir: &Path,
    targets: Option<BuildTargets>,
    ctx: &PythonContext,
) -> Result<Vec<ArtifactSummary>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if !path.is_file() {
            continue;
        }
        if let Some(targets) = targets {
            if !artifact_matches_format(&path, targets) {
                continue;
            }
        }
        let bytes = fs::metadata(&path)?.len();
        let sha256 = compute_file_sha256(&path)?;
        entries.push(ArtifactSummary {
            path: relative_path_str(&path, &ctx.project_root),
            bytes,
            sha256,
        });
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(entries)
}

pub(crate) fn summarize_selected_artifacts(
    paths: &[PathBuf],
    ctx: &PythonContext,
) -> Result<Vec<ArtifactSummary>> {
    let mut entries = Vec::new();
    for path in paths {
        let bytes = fs::metadata(path)?.len();
        let sha256 = compute_file_sha256(path)?;
        entries.push(ArtifactSummary {
            path: relative_path_str(path, &ctx.project_root),
            bytes,
            sha256,
        });
    }
    Ok(entries)
}

pub(crate) fn compute_file_sha256(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    io::copy(&mut file, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

pub(crate) fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;

    fn format_scaled(value: u64, unit: u64, suffix: &str) -> String {
        let whole = value / unit;
        let remainder = value % unit;
        let tenths = (remainder * 10) / unit;
        format!("{whole}.{tenths} {suffix}")
    }

    if bytes >= MB {
        format_scaled(bytes, MB, "MB")
    } else if bytes >= KB {
        format_scaled(bytes, KB, "KB")
    } else {
        format!("{bytes} B")
    }
}

pub(crate) fn artifact_matches_format(path: &Path, targets: BuildTargets) -> bool {
    if targets.sdist {
        if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
            if ext.eq_ignore_ascii_case("gz") {
                return true;
            }
        }
    }
    if targets.wheel {
        if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
            if ext.eq_ignore_ascii_case("whl") {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_matches_format_respects_targets() {
        let sdist = PathBuf::from("dist/demo-0.1.0.tar.gz");
        let wheel = PathBuf::from("dist/demo-0.1.0-py3-none-any.whl");

        let sdist_only = BuildTargets {
            sdist: true,
            wheel: false,
        };
        assert!(artifact_matches_format(&sdist, sdist_only));
        assert!(!artifact_matches_format(&wheel, sdist_only));

        let wheel_only = BuildTargets {
            sdist: false,
            wheel: true,
        };
        assert!(artifact_matches_format(&wheel, wheel_only));
        assert!(!artifact_matches_format(&sdist, wheel_only));

        let both = BuildTargets {
            sdist: true,
            wheel: true,
        };
        assert!(artifact_matches_format(&sdist, both));
        assert!(artifact_matches_format(&wheel, both));
    }

    #[test]
    fn format_bytes_scales_values() {
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(2048), "2.0 KB");
        assert_eq!(format_bytes(1_572_864), "1.5 MB");
    }
}
