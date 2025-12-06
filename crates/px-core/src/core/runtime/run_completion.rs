use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use toml_edit::DocumentMut;

use crate::core::runtime::facade::StoredEnvironment;
use crate::core::workspace::{discover_workspace_scope, WorkspaceScope};
use px_domain::discover_project_root;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunTargetKind {
    EntryPoint,
    EnvExecutable,
    ScriptFile,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunTargetSuggestion {
    pub value: String,
    pub kind: RunTargetKind,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunTargetCompletions {
    pub project_root: Option<PathBuf>,
    pub suggestions: Vec<RunTargetSuggestion>,
}

const MAIN_LIKE: &[&str] = &[
    "main.py",
    "app.py",
    "cli.py",
    "manage.py",
    "server.py",
    "__main__.py",
    "run.py",
];

pub fn run_target_completions(prefix: Option<&str>) -> RunTargetCompletions {
    let Some(context) = detect_context() else {
        return RunTargetCompletions::default();
    };

    let manifest = load_manifest(&context.project_root);
    let project_name = manifest
        .as_ref()
        .and_then(|doc| {
            doc.get("project")
                .and_then(|item| item.get("name"))
                .and_then(|name| name.as_str())
        })
        .map(|name| name.to_string());

    let mut candidates: HashMap<String, RunTargetSuggestion> = HashMap::new();

    if let Some(doc) = manifest.as_ref() {
        for suggestion in manifest_entrypoints(doc) {
            insert_candidate(&mut candidates, suggestion);
        }
    }

    if let Some(bin) = discover_env_bin(&context) {
        for suggestion in env_binaries(&bin) {
            insert_candidate(&mut candidates, suggestion);
        }
    }

    for suggestion in script_files(&context.project_root, project_name.as_deref()) {
        insert_candidate(&mut candidates, suggestion);
    }

    let mut suggestions: Vec<_> = candidates.into_values().collect();
    if let Some(prefix) = prefix {
        let trimmed = prefix.trim();
        if !trimmed.is_empty() {
            suggestions.retain(|item| matches_prefix(trimmed, &item.value));
        }
    }
    suggestions.sort_by_key(suggestion_sort_key);

    RunTargetCompletions {
        project_root: Some(context.project_root),
        suggestions,
    }
}

fn detect_context() -> Option<CompletionContext> {
    if let Ok(Some(scope)) = discover_workspace_scope() {
        return match scope {
            WorkspaceScope::Member {
                workspace,
                member_root,
            } => Some(CompletionContext {
                project_root: member_root,
                workspace_root: Some(workspace.config.root),
            }),
            WorkspaceScope::Root(workspace) => Some(CompletionContext {
                project_root: workspace.config.root.clone(),
                workspace_root: Some(workspace.config.root),
            }),
        };
    }

    if let Ok(Some(root)) = discover_project_root() {
        return Some(CompletionContext {
            project_root: root,
            workspace_root: None,
        });
    }

    None
}

struct CompletionContext {
    project_root: PathBuf,
    workspace_root: Option<PathBuf>,
}

fn load_manifest(root: &Path) -> Option<DocumentMut> {
    let manifest = root.join("pyproject.toml");
    let contents = fs::read_to_string(manifest).ok()?;
    contents.parse::<DocumentMut>().ok()
}

fn manifest_entrypoints(doc: &DocumentMut) -> Vec<RunTargetSuggestion> {
    let mut names: Vec<String> = Vec::new();
    let project = match doc.get("project") {
        Some(project) => project,
        None => return Vec::new(),
    };

    if let Some(table) = project.get("scripts").and_then(|item| item.as_table_like()) {
        for (key, _value) in table.iter() {
            names.push(key.to_string());
        }
    }

    if let Some(table) = project
        .get("gui-scripts")
        .and_then(|item| item.as_table_like())
    {
        for (key, _value) in table.iter() {
            names.push(key.to_string());
        }
    }

    if let Some(entry_points) = project
        .get("entry-points")
        .and_then(|item| item.as_table_like())
    {
        for group in ["console_scripts", "gui_scripts"] {
            if let Some(table) = entry_points
                .get(group)
                .and_then(|item| item.as_table_like())
            {
                for (key, _value) in table.iter() {
                    names.push(key.to_string());
                }
            }
        }
    }

    names
        .into_iter()
        .filter(|name| !name.trim().is_empty())
        .map(|name| RunTargetSuggestion {
            value: name,
            kind: RunTargetKind::EntryPoint,
            detail: Some("pyproject entry point".to_string()),
        })
        .collect()
}

fn env_binaries(bin: &Path) -> Vec<RunTargetSuggestion> {
    let mut items = Vec::new();
    let Ok(entries) = fs::read_dir(bin) else {
        return items;
    };

    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !(meta.is_file() || meta.is_symlink()) {
            continue;
        }
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        let trimmed = name.trim();
        if trimmed.is_empty() {
            continue;
        }
        items.push(RunTargetSuggestion {
            value: trimmed.to_string(),
            kind: RunTargetKind::EnvExecutable,
            detail: Some(format!("env bin ({})", bin.display())),
        });
    }

    items
}

fn script_files(root: &Path, project_name: Option<&str>) -> Vec<RunTargetSuggestion> {
    let mut items = Vec::new();

    for name in MAIN_LIKE {
        let path = root.join(name);
        if is_pythonish(&path) {
            items.push(path);
        }
    }

    for dir in ["scripts", "bin", "tools"] {
        let path = root.join(dir);
        collect_python_files(&path, &mut items);
    }

    if let Some(name) = project_name {
        for candidate in ["__main__.py", "cli.py"] {
            let main_in_src = root.join("src").join(name).join(candidate);
            if is_pythonish(&main_in_src) {
                items.push(main_in_src);
            }
            let main_in_root = root.join(name).join(candidate);
            if is_pythonish(&main_in_root) {
                items.push(main_in_root);
            }
        }
    }

    items
        .into_iter()
        .filter_map(|path| path.strip_prefix(root).ok().map(Path::to_path_buf))
        .filter_map(|path| path.to_str().map(|value| value.replace('\\', "/")))
        .map(|value| RunTargetSuggestion {
            value,
            kind: RunTargetKind::ScriptFile,
            detail: Some("project file".to_string()),
        })
        .collect()
}

fn collect_python_files(dir: &Path, acc: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if is_pythonish(&path) {
            acc.push(path);
        }
    }
}

fn is_pythonish(path: &Path) -> bool {
    if !path.is_file() {
        return false;
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

fn insert_candidate(
    targets: &mut HashMap<String, RunTargetSuggestion>,
    candidate: RunTargetSuggestion,
) {
    match targets.get(&candidate.value) {
        Some(existing) if kind_priority(&existing.kind) <= kind_priority(&candidate.kind) => {}
        _ => {
            targets.insert(candidate.value.clone(), candidate);
        }
    }
}

fn kind_priority(kind: &RunTargetKind) -> u8 {
    match kind {
        RunTargetKind::EntryPoint => 0,
        RunTargetKind::EnvExecutable => 1,
        RunTargetKind::ScriptFile => 2,
    }
}

fn matches_prefix(prefix: &str, value: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }
    let value_lower = value.to_ascii_lowercase();
    for candidate in [prefix, prefix.trim_start_matches("./")] {
        if candidate.is_empty() {
            continue;
        }
        if value_lower.starts_with(&candidate.to_ascii_lowercase()) {
            return true;
        }
    }
    false
}

fn discover_env_bin(context: &CompletionContext) -> Option<PathBuf> {
    if let Some(root) = &context.workspace_root {
        if let Some(env) = load_state_env(&root.join(".px").join("workspace-state.json")) {
            if let Some(bin) = env_bin_from_env(&env) {
                return Some(bin);
            }
        }
    }

    if let Some(env) = load_state_env(&context.project_root.join(".px").join("state.json")) {
        if let Some(bin) = env_bin_from_env(&env) {
            return Some(bin);
        }
    }

    None
}

fn env_bin_from_env(env: &StoredEnvironment) -> Option<PathBuf> {
    let mut roots = Vec::new();
    if let Some(path) = env.env_path.as_ref() {
        roots.push(PathBuf::from(path));
    }
    let site = PathBuf::from(&env.site_packages);
    if site.components().count() >= 3 {
        if let Some(root) = site
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
        {
            roots.push(root.to_path_buf());
        }
    }
    for root in roots {
        let bin = root.join("bin");
        if bin.is_dir() {
            return Some(bin);
        }
    }
    None
}

fn load_state_env(path: &Path) -> Option<StoredEnvironment> {
    let contents = fs::read_to_string(path).ok()?;
    let state: CompletionState = serde_json::from_str(&contents).ok()?;
    state.current_env
}

#[derive(Debug, Deserialize)]
struct CompletionState {
    #[serde(default)]
    current_env: Option<StoredEnvironment>,
}

fn suggestion_sort_key(s: &RunTargetSuggestion) -> (u8, u8, usize, usize, String, String) {
    let (kind_rank, secondary) = match s.kind {
        RunTargetKind::ScriptFile if is_main_like_name(&s.value) => (0, 0),
        RunTargetKind::EntryPoint => (1, 0),
        RunTargetKind::EnvExecutable => (2, 0),
        RunTargetKind::ScriptFile => (3, 1),
    };
    let depth = path_depth(&s.value);
    let length = s.value.len();
    let lower = s.value.to_ascii_lowercase();
    (kind_rank, secondary, depth, length, lower, s.value.clone())
}

fn is_main_like_name(value: &str) -> bool {
    let file = Path::new(value)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    MAIN_LIKE
        .iter()
        .any(|candidate| file == candidate.to_ascii_lowercase())
}

fn path_depth(value: &str) -> usize {
    value
        .split('/')
        .filter(|segment| !segment.is_empty())
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::env;
    use tempfile::tempdir;

    #[test]
    #[serial]
    fn collects_entrypoints_and_scripts() -> anyhow::Result<()> {
        let temp = tempdir()?;
        let cwd = env::current_dir()?;
        env::set_current_dir(temp.path())?;
        fs::write(
            temp.path().join("pyproject.toml"),
            r#"[project]
name = "demo"
version = "0.1.0"
requires-python = ">=3.11"
scripts = { demo = "demo.cli:main" }
gui-scripts = { demogui = "demo.gui:main" }
[project.entry-points.console_scripts]
extra-demo = "demo.cli:extra"
[tool.px]
"#,
        )?;

        fs::create_dir_all(temp.path().join("scripts"))?;
        fs::write(temp.path().join("scripts").join("app.py"), "print('hi')")?;
        fs::write(temp.path().join("main.py"), "print('hi')")?;

        let env_root = temp.path().join(".px").join("env");
        let bin_dir = env_root.join("bin");
        fs::create_dir_all(&bin_dir)?;
        fs::write(bin_dir.join("ruff"), "echo ruff")?;
        let state_path = temp.path().join(".px").join("state.json");
        fs::create_dir_all(state_path.parent().unwrap())?;
        fs::write(
            &state_path,
            serde_json::json!({
                "current_env": {
                    "id": "demo",
                    "lock_id": "lock",
                    "platform": "linux",
                    "site_packages": env_root.join("lib").join("python3.11").join("site-packages"),
                    "env_path": env_root,
                    "python": { "path": "/usr/bin/python", "version": "3.11.0" }
                }
            })
            .to_string(),
        )?;

        let completions = run_target_completions(None);
        let values: Vec<String> = completions
            .suggestions
            .into_iter()
            .map(|s| s.value)
            .collect();
        assert_eq!(
            values,
            vec![
                "main.py",
                "scripts/app.py",
                "demo",
                "demogui",
                "extra-demo",
                "ruff"
            ]
        );
        env::set_current_dir(cwd)?;
        Ok(())
    }

    #[test]
    #[serial]
    fn filters_by_prefix_case_insensitive() -> anyhow::Result<()> {
        let temp = tempdir()?;
        let cwd = env::current_dir()?;
        env::set_current_dir(temp.path())?;
        fs::write(
            temp.path().join("pyproject.toml"),
            r#"[project]
name = "demo"
version = "0.1.0"
requires-python = ">=3.11"
scripts = { demo = "demo.cli:main" }
[tool.px]
"#,
        )?;
        fs::write(temp.path().join("main.py"), "print('hi')")?;

        let completions = run_target_completions(Some("MA"));
        let values: Vec<String> = completions
            .suggestions
            .into_iter()
            .map(|s| s.value)
            .collect();
        assert_eq!(values, vec!["main.py".to_string()]);
        env::set_current_dir(cwd)?;
        Ok(())
    }
}
