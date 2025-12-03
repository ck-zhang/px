use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::PythonContext;

#[derive(Debug, Clone)]
pub(crate) struct PassthroughTarget {
    pub(crate) program: String,
    pub(crate) display: String,
    pub(crate) reason: PassthroughReason,
    pub(crate) resolved: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) enum PassthroughReason {
    PythonAlias,
    ExecutablePath,
    PythonScript {
        script_arg: String,
        script_path: String,
    },
}

#[derive(Debug, Clone)]
pub(crate) enum RunTargetPlan {
    Script(PathBuf),
    Passthrough(PassthroughTarget),
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

    if let Some(target) = detect_passthrough_target(target, py_ctx) {
        return Ok(RunTargetPlan::Passthrough(target));
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

pub(crate) fn python_script_target(entry: &str, root: &Path) -> Option<(String, String)> {
    if !looks_like_python_script(entry) {
        return None;
    }
    let script_arg = entry.to_string();
    let script_path = resolve_script_path(entry, root);
    Some((script_arg, script_path))
}

fn detect_passthrough_target(entry: &str, ctx: &PythonContext) -> Option<PassthroughTarget> {
    if looks_like_python_alias(entry) {
        return Some(PassthroughTarget {
            program: ctx.python.clone(),
            display: entry.to_string(),
            reason: PassthroughReason::PythonAlias,
            resolved: Some(ctx.python.clone()),
        });
    }

    if let Some((script_arg, script_path)) = python_script_target(entry, &ctx.project_root) {
        return Some(PassthroughTarget {
            program: ctx.python.clone(),
            display: entry.to_string(),
            reason: PassthroughReason::PythonScript {
                script_arg,
                script_path,
            },
            resolved: Some(ctx.python.clone()),
        });
    }

    if looks_like_path_target(entry) {
        let (program, resolved) = resolve_executable_path(entry, &ctx.project_root);
        return Some(PassthroughTarget {
            program,
            display: entry.to_string(),
            reason: PassthroughReason::ExecutablePath,
            resolved,
        });
    }

    None
}

fn looks_like_python_alias(entry: &str) -> bool {
    let lower = entry.to_lowercase();
    lower == "python"
        || lower == "python3"
        || lower.starts_with("python3.")
        || lower == "py"
        || lower == "py3"
}

fn looks_like_path_target(entry: &str) -> bool {
    let path = Path::new(entry);
    path.components().count() > 1 || entry.contains('/') || entry.contains('\\')
}

fn looks_like_python_script(entry: &str) -> bool {
    Path::new(entry)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("py") || ext.eq_ignore_ascii_case("pyw"))
}

fn resolve_script_path(entry: &str, root: &Path) -> String {
    let path = Path::new(entry);
    if path.is_absolute() {
        entry.to_string()
    } else {
        root.join(path).display().to_string()
    }
}

fn detect_console_script(entry: &str, ctx: &PythonContext) -> Option<String> {
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

fn resolve_executable_path(entry: &str, root: &Path) -> (String, Option<String>) {
    let path = Path::new(entry);
    if path.is_absolute() {
        let display = path.display().to_string();
        (display.clone(), Some(display))
    } else {
        let resolved = root.join(path);
        let display = resolved.display().to_string();
        (display.clone(), Some(display))
    }
}
