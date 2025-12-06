use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::Path;

use clap::builder::StyledStr;
use clap_complete::engine::{CompletionCandidate, PathCompleter, ValueCompleter};
use px_core::{run_target_completions, RunTargetKind};

pub fn run_target_completer(current: &OsStr) -> Vec<CompletionCandidate> {
    let current_str = current.to_string_lossy();
    let prefix = if current_str.is_empty() {
        None
    } else {
        Some(current_str.as_ref())
    };
    let completions = run_target_completions(prefix);

    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    for (idx, suggestion) in completions.suggestions.into_iter().enumerate() {
        let value = suggestion.value.clone();
        if !seen.insert(value.clone()) {
            continue;
        }
        let mut candidate = CompletionCandidate::new(value)
            .display_order(Some(idx))
            .tag(Some(StyledStr::from("target")));
        if let Some(help) = suggestion
            .detail
            .or_else(|| Some(kind_label(&suggestion.kind).to_string()))
        {
            candidate = candidate.help(Some(help.into()));
        }
        candidates.push(candidate);
    }

    if candidates.is_empty() || looks_like_path(&current_str) {
        let mut cwd = None;
        let root = completions.project_root.as_deref().or_else(|| {
            cwd = std::env::current_dir().ok();
            cwd.as_deref()
        });
        if let Some(root) = root {
            let path_candidates = PathCompleter::any()
                .filter(path_filter)
                .current_dir(root)
                .complete(current);
            for candidate in path_candidates {
                let value = candidate.get_value().to_string_lossy().to_string();
                if seen.insert(value) {
                    candidates.push(candidate.display_order(Some(usize::MAX)));
                }
            }
        }
    }

    candidates
}

fn kind_label(kind: &RunTargetKind) -> &'static str {
    match kind {
        RunTargetKind::EntryPoint => "pyproject entry point",
        RunTargetKind::EnvExecutable => "environment executable",
        RunTargetKind::ScriptFile => "project file",
    }
}

fn looks_like_path(input: &str) -> bool {
    input.starts_with('.') || input.starts_with('/') || input.contains('/')
}

fn path_filter(path: &Path) -> bool {
    path.is_dir() || is_pythonish(path)
}

fn is_pythonish(path: &Path) -> bool {
    if path.is_dir() {
        return true;
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if name.eq_ignore_ascii_case("__main__.py") {
        return true;
    }
    matches!(
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase()),
        Some(ext) if ext == "py" || ext == "pyw"
    )
}
