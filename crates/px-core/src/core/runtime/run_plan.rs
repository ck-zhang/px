use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::PythonContext;

#[derive(Debug, Clone)]
pub(crate) enum RunTargetPlan {
    Script(PathBuf),
    Executable(String),
}

pub(crate) fn plan_run_target(
    py_ctx: &PythonContext,
    _manifest: &Path,
    target: &str,
) -> Result<RunTargetPlan> {
    if let Some(script_path) = script_under_project_root(&py_ctx.project_root, target) {
        return Ok(RunTargetPlan::Script(script_path));
    }

    if let Some(resolved) = detect_console_script(target, py_ctx) {
        return Ok(RunTargetPlan::Executable(resolved));
    }

    Ok(RunTargetPlan::Executable(target.to_string()))
}

fn script_under_project_root(root: &Path, target: &str) -> Option<PathBuf> {
    let candidate = if Path::new(target).is_absolute() {
        PathBuf::from(target)
    } else {
        root.join(target)
    };
    let canonical = candidate.canonicalize().ok()?;
    if canonical.starts_with(root) && canonical.is_file() {
        Some(canonical)
    } else {
        None
    }
}

fn detect_console_script(entry: &str, ctx: &PythonContext) -> Option<String> {
    if is_python_alias(entry) {
        return None;
    }
    let site_bin = ctx.site_bin.as_ref()?;
    let mut candidates = vec![site_bin.join(entry)];
    if let Ok(pathext) = std::env::var("PATHEXT") {
        for ext in pathext.split(';').filter(|ext| !ext.is_empty()) {
            candidates.push(site_bin.join(format!("{entry}{ext}")));
        }
    }
    for candidate in candidates {
        if candidate.exists() {
            if let Ok(path) = candidate.canonicalize() {
                return Some(path.display().to_string());
            }
            return Some(candidate.display().to_string());
        }
    }
    None
}

fn is_python_alias(entry: &str) -> bool {
    let lower = entry.to_ascii_lowercase();
    lower == "python"
        || lower == "python3"
        || lower.starts_with("python3.")
        || lower == "py"
        || lower == "py3"
}
