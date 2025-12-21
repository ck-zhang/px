use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::json;
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
    let build_root = prepare_build_root(py_ctx, out_dir)?;
    uv::build_distributions(build_root.path(), targets, out_dir)
}

struct BuildRoot {
    path: PathBuf,
    _temp: Option<tempfile::TempDir>,
}

impl BuildRoot {
    fn path(&self) -> &Path {
        &self.path
    }
}

fn prepare_build_root(py_ctx: &PythonContext, out_dir: &Path) -> Result<BuildRoot> {
    let package = py_ctx.project_name.replace('-', "_");
    let expected_init = py_ctx
        .project_root
        .join("src")
        .join(&package)
        .join("__init__.py");
    if expected_init.exists() {
        return Ok(BuildRoot {
            path: py_ctx.project_root.clone(),
            _temp: None,
        });
    }

    let temp = tempfile::Builder::new()
        .prefix(".px-build-")
        .tempdir_in(out_dir)
        .context("creating build staging directory")?;
    let root = temp.path().to_path_buf();

    for filename in [
        "pyproject.toml",
        "setup.cfg",
        "setup.py",
        "MANIFEST.in",
        "README.md",
        "README.rst",
        "README.txt",
        "LICENSE",
        "LICENSE.txt",
    ] {
        let src = py_ctx.project_root.join(filename);
        if src.exists() {
            fs::copy(&src, root.join(filename))
                .with_context(|| format!("copying {} into build staging root", src.display()))?;
        }
    }

    let src_dir = py_ctx.project_root.join("src");
    if src_dir.exists() {
        copy_tree(&src_dir, &root.join("src"))
            .with_context(|| format!("copying {} into build staging root", src_dir.display()))?;
    }

    let module_dir = root.join("src").join(&package);
    let existing_package = py_ctx.project_root.join(&package);
    if !module_dir.exists() && existing_package.exists() {
        copy_tree(&existing_package, &module_dir).with_context(|| {
            format!(
                "copying {} into build staging root",
                existing_package.display()
            )
        })?;
    }

    fs::create_dir_all(&module_dir)?;
    let init_py = module_dir.join("__init__.py");
    if !init_py.exists() {
        fs::write(&init_py, b"")?;
    }

    Ok(BuildRoot {
        path: root,
        _temp: Some(temp),
    })
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use px_domain::api::PxOptions;

    #[test]
    fn prepare_build_root_stages_stub_without_touching_project() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project_root = temp.path().join("proj");
        fs::create_dir_all(&project_root).expect("project root");
        fs::write(project_root.join("pyproject.toml"), "[project]\nname = \"proj\"\n")
            .expect("pyproject");
        let out_dir = project_root.join("dist");
        fs::create_dir_all(&out_dir).expect("dist");

        let py_ctx = PythonContext {
            state_root: project_root.clone(),
            project_root: project_root.clone(),
            project_name: "proj".to_string(),
            python: String::new(),
            pythonpath: String::new(),
            allowed_paths: vec![project_root.clone()],
            site_bin: None,
            pep582_bin: Vec::new(),
            pyc_cache_prefix: None,
            px_options: PxOptions::default(),
        };

        assert!(
            !project_root.join("src").exists(),
            "fixture should not create src/"
        );

        let build_root = prepare_build_root(&py_ctx, &out_dir).expect("build root");
        let staged_path = build_root.path().to_path_buf();
        assert_ne!(
            staged_path, project_root,
            "expected staging build root when module is missing"
        );

        assert!(
            !project_root.join("src").exists(),
            "prepare_build_root must not create src/ in the project"
        );
        assert!(
            staged_path.join("src").join("proj").join("__init__.py").exists(),
            "staged build root should contain a stub package"
        );

        drop(build_root);
        assert!(
            !staged_path.exists(),
            "staging directory should be cleaned up"
        );
    }
}
