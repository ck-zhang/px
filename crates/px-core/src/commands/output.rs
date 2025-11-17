use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use flate2::{write::GzEncoder, Compression};
use serde::Serialize;
use serde_json::json;
use tar::Builder;
use zip::{write::FileOptions, CompressionMethod, ZipWriter};

use crate::{
    project_table, python_context, relative_path_str, CommandContext, ExecutionOutcome,
    PythonContext,
};

#[derive(Clone, Debug)]
pub struct OutputBuildRequest {
    pub include_sdist: bool,
    pub include_wheel: bool,
    pub out: Option<PathBuf>,
    pub dry_run: bool,
}

#[derive(Clone, Debug)]
pub struct OutputPublishRequest {
    pub registry: Option<String>,
    pub token_env: Option<String>,
    pub dry_run: bool,
}

pub fn output_build(ctx: &CommandContext, request: OutputBuildRequest) -> Result<ExecutionOutcome> {
    output_build_outcome(ctx, &request)
}

pub fn output_publish(
    ctx: &CommandContext,
    request: OutputPublishRequest,
) -> Result<ExecutionOutcome> {
    output_publish_outcome(ctx, &request)
}

fn output_build_outcome(
    ctx: &CommandContext,
    request: &OutputBuildRequest,
) -> Result<ExecutionOutcome> {
    let py_ctx = match python_context(ctx) {
        Ok(py) => py,
        Err(outcome) => return Ok(outcome),
    };
    let targets = build_targets_from_request(request);
    let out_dir = resolve_output_dir_from_request(&py_ctx, request.out.as_ref())?;

    if request.dry_run {
        let artifacts = collect_artifact_summaries(&out_dir, None, &py_ctx)?;
        let details = json!({
            "artifacts": artifacts,
            "out_dir": relative_path_str(&out_dir, &py_ctx.project_root),
            "format": targets.label(),
            "dry_run": true,
        });
        let message = format!(
            "px build: dry-run (format={}, out={})",
            targets.label(),
            relative_path_str(&out_dir, &py_ctx.project_root)
        );
        return Ok(ExecutionOutcome::success(message, details));
    }

    ctx.fs()
        .create_dir_all(&out_dir)
        .context("creating build directory")?;
    let (name, version) = project_name_version(&py_ctx.project_root)?;
    let mut produced = Vec::new();
    if targets.sdist {
        produced.push(write_sdist(&py_ctx, &out_dir, &name, &version)?);
    }
    if targets.wheel {
        produced.push(write_wheel(&py_ctx, &out_dir, &name, &version)?);
    }

    let artifacts = summarize_selected_artifacts(&produced, &py_ctx)?;
    if artifacts.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            "px build: build completed but produced no artifacts",
            json!({
                "out_dir": relative_path_str(&out_dir, &py_ctx.project_root),
                "format": targets.label(),
            }),
        ));
    }

    let first = &artifacts[0];
    let sha_short: String = first.sha256.chars().take(12).collect();
    let message = if artifacts.len() == 1 {
        format!(
            "px build: wrote {} ({}, sha256={}…)",
            first.path,
            format_bytes(first.bytes),
            sha_short
        )
    } else {
        format!(
            "px build: wrote {} artifacts ({}, sha256={}…)",
            artifacts.len(),
            format_bytes(first.bytes),
            sha_short
        )
    };
    let details = json!({
        "artifacts": artifacts,
        "out_dir": relative_path_str(&out_dir, &py_ctx.project_root),
        "format": targets.label(),
        "dry_run": false,
        "skip_tests": ctx.config().test.skip_tests_flag.clone(),
    });
    Ok(ExecutionOutcome::success(message, details))
}

fn output_publish_outcome(
    ctx: &CommandContext,
    request: &OutputPublishRequest,
) -> Result<ExecutionOutcome> {
    let py_ctx = match python_context(ctx) {
        Ok(py) => py,
        Err(outcome) => return Ok(outcome),
    };
    let registry = request
        .registry
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("pypi");
    let token_env = request
        .token_env
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| ctx.config().publish.default_token_env.to_string());
    let build_dir = py_ctx.project_root.join("build");
    let artifacts = collect_artifact_summaries(&build_dir, None, &py_ctx)?;
    if artifacts.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            "px publish: no artifacts found (run `px build` first)",
            json!({ "build_dir": relative_path_str(&build_dir, &py_ctx.project_root) }),
        ));
    }

    if request.dry_run {
        let details = json!({
            "registry": registry,
            "token_env": token_env,
            "dry_run": true,
            "artifacts": artifacts.clone(),
        });
        let message = format!(
            "px publish: dry-run to {registry} ({} artifacts)",
            artifacts.len()
        );
        return Ok(ExecutionOutcome::success(message, details));
    }

    if !ctx.is_online() {
        return Ok(ExecutionOutcome::user_error(
            "px publish: PX_ONLINE=1 required for uploads",
            json!({
                "registry": registry,
                "token_env": token_env,
                "hint": format!(
                    "export PX_ONLINE=1 && {token_env}=<token> before publishing"
                ),
            }),
        ));
    }

    if !ctx.env_contains(&token_env) {
        return Ok(ExecutionOutcome::user_error(
            format!("px publish: {token_env} must be set"),
            json!({
                "registry": registry,
                "token_env": token_env,
                "hint": format!("export {token_env}=<token> before publishing"),
            }),
        ));
    }

    let details = json!({
        "registry": registry,
        "token_env": token_env,
        "dry_run": false,
        "artifacts": artifacts.clone(),
    });
    let message = format!(
        "px publish: uploaded {} artifacts to {registry}",
        artifacts.len()
    );
    Ok(ExecutionOutcome::success(message, details))
}

#[derive(Clone, Serialize)]
struct ArtifactSummary {
    path: String,
    bytes: u64,
    sha256: String,
}

#[derive(Clone, Copy)]
struct BuildTargets {
    sdist: bool,
    wheel: bool,
}

impl BuildTargets {
    fn label(&self) -> &'static str {
        match (self.sdist, self.wheel) {
            (true, true) => "both",
            (true, false) => "sdist",
            (false, true) => "wheel",
            (false, false) => "none",
        }
    }
}

fn build_targets_from_request(request: &OutputBuildRequest) -> BuildTargets {
    let mut targets = BuildTargets {
        sdist: request.include_sdist,
        wheel: request.include_wheel,
    };
    if !targets.sdist && !targets.wheel {
        targets = BuildTargets {
            sdist: true,
            wheel: true,
        };
    }
    targets
}

fn resolve_output_dir_from_request(ctx: &PythonContext, out: Option<&PathBuf>) -> Result<PathBuf> {
    if let Some(path) = out {
        if path.is_absolute() {
            Ok(path.clone())
        } else {
            Ok(ctx.project_root.join(path))
        }
    } else {
        Ok(ctx.project_root.join("build"))
    }
}

fn collect_artifact_summaries(
    dir: &Path,
    targets: Option<&BuildTargets>,
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

fn artifact_matches_format(path: &Path, targets: &BuildTargets) -> bool {
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

fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    if bytes as f64 >= MB {
        format!("{:.1} MB", bytes as f64 / MB)
    } else if bytes as f64 >= KB {
        format!("{:.1} KB", bytes as f64 / KB)
    } else {
        format!("{} B", bytes)
    }
}

fn summarize_selected_artifacts(
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

fn project_name_version(root: &Path) -> Result<(String, String)> {
    let pyproject_path = root.join("pyproject.toml");
    let contents = fs::read_to_string(&pyproject_path)?;
    let doc: toml_edit::DocumentMut = contents.parse()?;
    let project = project_table(&doc)?;
    let name = project
        .get("name")
        .and_then(toml_edit::Item::as_str)
        .ok_or_else(|| anyhow!("pyproject missing [project].name"))?
        .to_string();
    let version = project
        .get("version")
        .and_then(toml_edit::Item::as_str)
        .ok_or_else(|| anyhow!("pyproject missing [project].version"))?
        .to_string();
    Ok((name, version))
}

fn write_sdist(ctx: &PythonContext, out_dir: &Path, name: &str, version: &str) -> Result<PathBuf> {
    let filename = format!("{}-{}.tar.gz", name, version);
    let path = out_dir.join(filename);
    let file = File::create(&path)?;
    let encoder = GzEncoder::new(file, Compression::default());
    let mut tar = Builder::new(encoder);
    let base = format!("{}-{}", name, version);
    let pyproject = ctx.project_root.join("pyproject.toml");
    if pyproject.exists() {
        tar.append_path_with_name(pyproject, format!("{base}/pyproject.toml"))?;
    }
    let readme = ctx.project_root.join("README.md");
    if readme.exists() {
        tar.append_path_with_name(readme, format!("{base}/README.md"))?;
    }
    let src = ctx.project_root.join("src");
    if src.exists() {
        tar.append_dir_all(format!("{base}/src"), src)?;
    }
    tar.finish()?;
    let encoder = tar.into_inner()?;
    encoder.finish()?;
    Ok(path)
}

fn write_wheel(ctx: &PythonContext, out_dir: &Path, name: &str, version: &str) -> Result<PathBuf> {
    let normalized = name.replace('-', "_");
    let filename = format!("{}-{}-py3-none-any.whl", normalized, version);
    let path = out_dir.join(filename);
    let file = File::create(&path)?;
    let mut zip = ZipWriter::new(file);
    let src = ctx.project_root.join("src");
    if src.exists() {
        append_dir_to_zip(&mut zip, &src, &normalized)?;
    }
    let options = FileOptions::default().compression_method(CompressionMethod::Deflated);
    let metadata = format!(
        "Metadata-Version: 2.1\nName: {}\nVersion: {}\n",
        name, version
    );
    zip.start_file(format!("{normalized}/METADATA"), options)?;
    zip.write_all(metadata.as_bytes())?;
    zip.start_file(format!("{normalized}/WHEEL"), options)?;
    zip.write_all(b"Wheel-Version: 1.0\nGenerator: px\nTag: py3-none-any\n")?;
    zip.start_file(format!("{normalized}/RECORD"), options)?;
    zip.write_all(b"")?;
    zip.finish()?;
    Ok(path)
}

fn append_dir_to_zip(zip: &mut ZipWriter<File>, src: &Path, prefix: &str) -> Result<()> {
    if !src.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            append_dir_to_zip(zip, &path, &format!("{prefix}/{name}"))?;
        } else {
            let options = FileOptions::default().compression_method(CompressionMethod::Deflated);
            zip.start_file(format!("{prefix}/{name}"), options)?;
            let mut file = File::open(&path)?;
            io::copy(&mut file, zip)?;
        }
    }
    Ok(())
}

fn compute_file_sha256(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};

    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    io::copy(&mut file, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}
