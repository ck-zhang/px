use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use toml_edit::{DocumentMut, Item};

use crate::PythonContext;

#[derive(Debug, Clone)]
pub(crate) struct ResolvedEntry {
    pub(crate) entry: String,
    pub(crate) call: Option<String>,
    pub(crate) source: EntrySource,
}

#[derive(Debug, Clone)]
pub(crate) enum EntrySource {
    PxScript { script: String },
}

impl EntrySource {
    pub(crate) fn label(&self) -> &'static str {
        match self {
            EntrySource::PxScript { .. } => "px-scripts",
        }
    }

    pub(crate) fn script_name(&self) -> Option<&str> {
        match self {
            EntrySource::PxScript { script } => Some(script.as_str()),
        }
    }
}

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
    PxScript(ResolvedEntry),
    Script(PathBuf),
    Passthrough(PassthroughTarget),
    Executable(String),
}

pub(crate) fn plan_run_target(
    py_ctx: &PythonContext,
    manifest: &Path,
    target: &str,
) -> Result<RunTargetPlan> {
    if let Some(resolved) = resolve_px_script(manifest, target)? {
        return Ok(RunTargetPlan::PxScript(resolved));
    }

    if let Some(script_path) = script_under_project_root(&py_ctx.project_root, target) {
        return Ok(RunTargetPlan::Script(script_path));
    }

    if let Some(target) = detect_passthrough_target(target, py_ctx) {
        return Ok(RunTargetPlan::Passthrough(target));
    }

    Ok(RunTargetPlan::Executable(target.to_string()))
}

fn resolve_px_script(manifest: &Path, target: &str) -> Result<Option<ResolvedEntry>> {
    if !manifest.exists() {
        return Ok(None);
    }
    let contents = fs::read_to_string(manifest)?;
    let doc: DocumentMut = contents.parse()?;
    let scripts = doc
        .get("tool")
        .and_then(Item::as_table)
        .and_then(|tool| tool.get("px"))
        .and_then(Item::as_table)
        .and_then(|px| px.get("scripts"))
        .and_then(Item::as_table);
    let Some(table) = scripts else {
        return Ok(None);
    };
    let value = match table.get(target) {
        Some(item) => item,
        None => return Ok(None),
    };
    let Some(raw) = value.as_str() else {
        return Ok(None);
    };
    let (entry, call) = parse_entry_value(raw)
        .ok_or_else(|| anyhow!("invalid [tool.px.scripts] entry for `{target}`"))?;
    Ok(Some(ResolvedEntry {
        entry,
        call,
        source: EntrySource::PxScript {
            script: target.to_string(),
        },
    }))
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

fn resolve_executable_path(entry: &str, root: &Path) -> (String, Option<String>) {
    let path = Path::new(entry);
    if path.is_absolute() {
        (entry.to_string(), Some(entry.to_string()))
    } else {
        let resolved = root.join(path);
        let display = resolved.display().to_string();
        (display.clone(), Some(display))
    }
}

fn parse_entry_value(value: &str) -> Option<(String, Option<String>)> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let token = trimmed.split_whitespace().next().unwrap_or("").trim();
    if token.is_empty() {
        return None;
    }
    let mut parts = token.splitn(2, ':');
    let module = parts.next().unwrap_or("").trim();
    if module.is_empty() {
        return None;
    }
    let call = parts
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Some((module.to_string(), call))
}
