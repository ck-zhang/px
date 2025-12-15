use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde_json::json;
use toml_edit::DocumentMut;
use walkdir::WalkDir;

use crate::{
    is_missing_project_error, manifest_snapshot, missing_project_outcome, python_context,
    relative_path_str, CommandContext, ExecutionOutcome, ManifestSnapshot, PythonContext,
};

use super::artifacts::{
    collect_artifact_summaries, format_bytes, summarize_selected_artifacts, BuildTargets,
};
use super::plan::{plan_build, BuildRequest};
use super::uv;

/// Builds the configured project artifacts.
///
/// # Errors
/// Returns an error if the build environment is unavailable or packaging fails.
pub fn build_project(ctx: &CommandContext, request: &BuildRequest) -> Result<ExecutionOutcome> {
    build_project_outcome(ctx, request)
}

fn build_project_outcome(ctx: &CommandContext, request: &BuildRequest) -> Result<ExecutionOutcome> {
    if request.dry_run {
        let snapshot = match manifest_snapshot() {
            Ok(snapshot) => snapshot,
            Err(err) => {
                if is_missing_project_error(&err) {
                    return Ok(missing_project_outcome());
                }
                return Err(err);
            }
        };
        let py_ctx = dry_run_context(&snapshot);
        let plan = plan_build(&py_ctx, request);
        let artifacts = collect_artifact_summaries(&plan.out_dir, None, &py_ctx)?;
        let details = json!({
            "artifacts": artifacts,
            "out_dir": relative_path_str(&plan.out_dir, &py_ctx.project_root),
            "format": plan.targets.label(),
            "dry_run": true,
        });
        let message = format!(
            "px build: dry-run (format={}, out={})",
            plan.targets.label(),
            relative_path_str(&plan.out_dir, &py_ctx.project_root)
        );
        return Ok(ExecutionOutcome::success(message, details));
    }

    let py_ctx = match python_context(ctx) {
        Ok(py) => py,
        Err(outcome) => return Ok(outcome),
    };
    let plan = plan_build(&py_ctx, request);

    ctx.fs()
        .create_dir_all(&plan.out_dir)
        .with_context(|| format!("creating output directory at {}", plan.out_dir.display()))?;
    let produced = build_with_uv(ctx, &py_ctx, plan.targets, &plan.out_dir)?;

    let artifacts = summarize_selected_artifacts(&produced, &py_ctx)?;
    if artifacts.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            "px build: build completed but produced no artifacts",
            json!({
                "out_dir": relative_path_str(&plan.out_dir, &py_ctx.project_root),
                "format": plan.targets.label(),
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
        "out_dir": relative_path_str(&plan.out_dir, &py_ctx.project_root),
        "format": plan.targets.label(),
        "dry_run": false,
        "skip_tests": ctx.config().test.skip_tests_flag.clone(),
    });
    Ok(ExecutionOutcome::success(message, details))
}

fn dry_run_context(snapshot: &ManifestSnapshot) -> PythonContext {
    PythonContext {
        state_root: snapshot.root.clone(),
        project_root: snapshot.root.clone(),
        project_name: snapshot.name.clone(),
        python: String::new(),
        pythonpath: String::new(),
        allowed_paths: vec![snapshot.root.clone()],
        site_bin: None,
        pep582_bin: Vec::new(),
        pyc_cache_prefix: None,
        px_options: snapshot.px_options.clone(),
    }
}

fn build_with_uv(
    ctx: &CommandContext,
    py_ctx: &PythonContext,
    targets: BuildTargets,
    out_dir: &Path,
) -> Result<Vec<PathBuf>> {
    ctx.fs()
        .create_dir_all(out_dir)
        .with_context(|| format!("creating output directory at {}", out_dir.display()))?;
    ensure_package_stub(&py_ctx.project_root)?;
    uv::build_distributions(&py_ctx.project_root, targets, out_dir)
}

fn ensure_package_stub(project_root: &Path) -> Result<()> {
    let pyproject_path = project_root.join("pyproject.toml");
    let contents = fs::read_to_string(&pyproject_path)
        .with_context(|| format!("reading {}", pyproject_path.display()))?;
    let doc: DocumentMut = contents.parse()?;
    let name = doc["project"]["name"]
        .as_str()
        .ok_or_else(|| anyhow!("pyproject missing [project].name"))?
        .to_string();
    let package = name.replace('-', "_");
    let src_root = project_root.join("src");
    let module_dir = src_root.join(&package);
    let existing_package = project_root.join(&package);
    if !module_dir.exists() && existing_package.exists() {
        copy_package_tree(&existing_package, &module_dir)?;
    }
    fs::create_dir_all(&module_dir)?;
    let init_py = module_dir.join("__init__.py");
    if !init_py.exists() {
        fs::write(&init_py, b"")?;
    }
    Ok(())
}

fn copy_package_tree(from: &Path, to: &Path) -> Result<()> {
    for entry in WalkDir::new(from) {
        let entry = entry?;
        let path = entry.path();
        let relative = path.strip_prefix(from).unwrap_or(path);
        let dest = to.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&dest)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(path, &dest)?;
        }
    }
    Ok(())
}
