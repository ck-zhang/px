use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::json;
use toml_edit::{DocumentMut, Item, Table, Value as TomlValue};
use walkdir::WalkDir;

use crate::{
    is_missing_project_error, manifest_snapshot, missing_project_outcome, relative_path_str,
    CommandContext, ExecutionOutcome, ManifestSnapshot, PythonContext,
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

    if request.dry_run {
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

    ctx.fs()
        .create_dir_all(&plan.out_dir)
        .with_context(|| format!("creating output directory at {}", plan.out_dir.display()))?;
    let produced = match build_with_uv(ctx, &py_ctx, plan.targets, &plan.out_dir) {
        Ok(produced) => produced,
        Err(err) => {
            if let Some(outcome) = build_user_error(&err, &py_ctx) {
                return Ok(outcome);
            }
            return Err(err);
        }
    };

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

#[derive(Debug)]
struct BuildRoot {
    path: PathBuf,
    _temp: Option<tempfile::TempDir>,
}

impl BuildRoot {
    fn path(&self) -> &Path {
        &self.path
    }
}

#[derive(Debug)]
struct BuildLayoutError {
    package: String,
    expected_src_init: PathBuf,
    expected_flat_init: PathBuf,
}

impl std::fmt::Display for BuildLayoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "expected a Python module at {} or {}",
            self.expected_src_init.display(),
            self.expected_flat_init.display()
        )
    }
}

impl std::error::Error for BuildLayoutError {}

fn prepare_build_root(py_ctx: &PythonContext, out_dir: &Path) -> Result<BuildRoot> {
    let package = py_ctx.project_name.replace('-', "_");
    let expected_src_init = py_ctx
        .project_root
        .join("src")
        .join(&package)
        .join("__init__.py");
    let expected_src_init_pyi = py_ctx
        .project_root
        .join("src")
        .join(&package)
        .join("__init__.pyi");

    // If the project explicitly configures the uv build backend, respect that configuration by
    // building from the real project root.
    if project_declares_uv_build_backend(&py_ctx.project_root.join("pyproject.toml"))? {
        return Ok(BuildRoot {
            path: py_ctx.project_root.clone(),
            _temp: None,
        });
    }

    if expected_src_init.exists() || expected_src_init_pyi.exists() {
        return Ok(BuildRoot {
            path: py_ctx.project_root.clone(),
            _temp: None,
        });
    }

    let expected_flat_init = py_ctx.project_root.join(&package).join("__init__.py");
    let expected_flat_init_pyi = py_ctx.project_root.join(&package).join("__init__.pyi");
    if !expected_flat_init.exists() && !expected_flat_init_pyi.exists() {
        return Err(BuildLayoutError {
            package,
            expected_src_init,
            expected_flat_init,
        }
        .into());
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

    let existing_package = py_ctx.project_root.join(&package);
    copy_tree(&existing_package, &root.join(&package)).with_context(|| {
        format!(
            "copying {} into build staging root",
            existing_package.display()
        )
    })?;
    let pyproject_path = root.join("pyproject.toml");
    ensure_uv_build_backend_module_root(&pyproject_path, "")?;

    Ok(BuildRoot {
        path: root,
        _temp: Some(temp),
    })
}

fn project_declares_uv_build_backend(pyproject_path: &Path) -> Result<bool> {
    let contents = match fs::read_to_string(pyproject_path) {
        Ok(contents) => contents,
        Err(_) => return Ok(false),
    };
    let doc: DocumentMut = match contents.parse() {
        Ok(doc) => doc,
        Err(_) => return Ok(false),
    };
    let configured = doc
        .get("tool")
        .and_then(Item::as_table)
        .and_then(|tool| tool.get("uv"))
        .and_then(Item::as_table)
        .and_then(|uv| uv.get("build-backend"))
        .is_some_and(Item::is_table);
    Ok(configured)
}

fn ensure_uv_build_backend_module_root(pyproject_path: &Path, module_root: &str) -> Result<()> {
    let contents = fs::read_to_string(pyproject_path)
        .with_context(|| format!("reading {}", pyproject_path.display()))?;
    let mut doc: DocumentMut = contents
        .parse()
        .with_context(|| format!("parsing {}", pyproject_path.display()))?;

    let tool_entry = doc.entry("tool").or_insert(Item::Table(Table::new()));
    if !tool_entry.is_table() {
        *tool_entry = Item::Table(Table::new());
    }
    let tool_table = tool_entry.as_table_mut().expect("tool table");

    let uv_entry = tool_table.entry("uv").or_insert(Item::Table(Table::new()));
    if !uv_entry.is_table() {
        *uv_entry = Item::Table(Table::new());
    }
    let uv_table = uv_entry.as_table_mut().expect("uv table");

    let build_entry = uv_table
        .entry("build-backend")
        .or_insert(Item::Table(Table::new()));
    if !build_entry.is_table() {
        *build_entry = Item::Table(Table::new());
    }
    let build_table = build_entry.as_table_mut().expect("build-backend table");

    build_table.insert(
        "module-root",
        Item::Value(TomlValue::from(module_root)),
    );

    fs::write(pyproject_path, doc.to_string())
        .with_context(|| format!("writing {}", pyproject_path.display()))?;
    Ok(())
}

fn build_user_error(err: &anyhow::Error, py_ctx: &PythonContext) -> Option<ExecutionOutcome> {
    if let Some(layout) = err
        .chain()
        .find_map(|cause| cause.downcast_ref::<BuildLayoutError>())
    {
        return Some(ExecutionOutcome::user_error(
            "px build: project has no Python module to package",
            json!({
                "reason": "missing_module",
                "module": layout.package,
                "expected_src_init": layout.expected_src_init.display().to_string(),
                "expected_flat_init": layout.expected_flat_init.display().to_string(),
                "hint": format!(
                    "Create `{}` (src layout) or `{}` (flat layout), or configure `[tool.uv.build-backend]` (module-root/module-name).",
                    relative_path_str(&layout.expected_src_init, &py_ctx.project_root),
                    relative_path_str(&layout.expected_flat_init, &py_ctx.project_root),
                ),
            }),
        ));
    }

    let uv_err = err
        .chain()
        .find_map(|cause| cause.downcast_ref::<uv_build_backend::Error>())?;

    let package = py_ctx.project_name.replace('-', "_");
    let expected_src_init = py_ctx
        .project_root
        .join("src")
        .join(&package)
        .join("__init__.py");
    let expected_flat_init = py_ctx.project_root.join(&package).join("__init__.py");

    match uv_err {
        uv_build_backend::Error::MissingInitPy(path) => Some(ExecutionOutcome::user_error(
            "px build: project layout is not buildable",
            json!({
                "reason": "missing_init_py",
                "error": uv_err.to_string(),
                "missing": path.display().to_string(),
                "hint": format!(
                    "Ensure a Python package exists at `{}` (src layout) or `{}` (flat layout), or configure `[tool.uv.build-backend]`.",
                    relative_path_str(&expected_src_init, &py_ctx.project_root),
                    relative_path_str(&expected_flat_init, &py_ctx.project_root),
                ),
            }),
        )),
        uv_build_backend::Error::VenvInSourceTree(path) => Some(ExecutionOutcome::user_error(
            "px build: virtual environment detected in source tree",
            json!({
                "reason": "venv_in_source_tree",
                "error": uv_err.to_string(),
                "path": path.display().to_string(),
                "hint": "Remove the virtual environment directory or exclude it from the build inputs.",
            }),
        )),
        _ => Some(ExecutionOutcome::user_error(
            format!("px build: {uv_err}"),
            json!({
                "reason": "build_failed",
                "error": uv_err.to_string(),
            }),
        )),
    }
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
    fn prepare_build_root_stages_flat_layout_without_touching_project() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project_root = temp.path().join("proj");
        fs::create_dir_all(&project_root).expect("project root");
        fs::write(
            project_root.join("pyproject.toml"),
            "[project]\nname = \"proj\"\n",
        )
        .expect("pyproject");
        fs::create_dir_all(project_root.join("proj")).expect("package dir");
        fs::write(project_root.join("proj").join("__init__.py"), b"").expect("init");
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
            staged_path
                .join("proj")
                .join("__init__.py")
                .exists(),
            "staged build root should contain the real package"
        );
        assert!(
            !staged_path.join("src").exists(),
            "staged build root should preserve flat layout"
        );

        drop(build_root);
        assert!(
            !staged_path.exists(),
            "staging directory should be cleaned up"
        );
    }

    #[test]
    fn prepare_build_root_errors_without_module() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project_root = temp.path().join("proj");
        fs::create_dir_all(&project_root).expect("project root");
        fs::write(
            project_root.join("pyproject.toml"),
            "[project]\nname = \"proj\"\n",
        )
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

        let err = prepare_build_root(&py_ctx, &out_dir).expect_err("expected error");
        assert!(
            err.to_string().contains("expected a Python module"),
            "unexpected error: {err}"
        );
        assert!(
            !project_root.join("src").exists(),
            "prepare_build_root must not create src/ in the project"
        );
    }
}
