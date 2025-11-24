use std::env;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use toml_edit::{DocumentMut, Item};

use crate::{
    install_snapshot, manifest_snapshot_at, refresh_project_site, relative_path_str,
    CommandContext, ExecutionOutcome, InstallUserError,
};
use px_domain::{
    discover_project_root, infer_package_name, project_name_from_pyproject, ProjectInitializer,
};

#[derive(Clone, Debug)]
pub struct ProjectInitRequest {
    pub package: Option<String>,
    pub python: Option<String>,
    pub dry_run: bool,
    pub force: bool,
}

/// Initializes a px project in the current directory.
///
/// # Errors
/// Returns an error if filesystem access or dependency installation fails.
pub fn project_init(
    ctx: &CommandContext,
    request: &ProjectInitRequest,
) -> Result<ExecutionOutcome> {
    let cwd = env::current_dir().context("unable to determine current directory")?;
    if let Some(existing_root) = discover_project_root()? {
        return existing_pyproject_response(&existing_root.join("pyproject.toml"));
    }
    let root = cwd;
    let pyproject_path = root.join("pyproject.toml");
    let pyproject_preexisting = pyproject_path.exists();

    if pyproject_preexisting {
        if let Some(conflict) = detect_init_conflict(&pyproject_path)? {
            return Ok(conflict.into_outcome(&pyproject_path));
        }
    }

    if !request.force {
        if let Some(changes) = ctx.git().worktree_changes(&root)? {
            if !changes.is_empty() {
                return Ok(dirty_worktree_response(&changes));
            }
        }
    }

    let package_arg = request
        .package
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let (package, inferred) = infer_package_name(package_arg, &root)?;
    let python_req = resolve_python_requirement_arg(request.python.as_deref());

    let mut files = ProjectInitializer::scaffold(&root, &package, &python_req, request.dry_run)?;
    if request.dry_run {
        let package_name = package.clone();
        let mut details = json!({
            "package": package_name.clone(),
            "python": python_req,
            "files_created": files,
            "project_root": root.display().to_string(),
            "lockfile": root.join("px.lock").display().to_string(),
            "dry_run": true,
        });
        if inferred && !pyproject_preexisting {
            details["inferred_package"] = Value::Bool(true);
            details["hint"] = Value::String(
                "Pass --package <name> to override the inferred module name.".to_string(),
            );
        }
        return Ok(ExecutionOutcome::success(
            format!("initialized project {package_name} (dry-run)"),
            details,
        ));
    }
    let snapshot = manifest_snapshot_at(&root)?;
    let actual_name = snapshot.name.clone();
    let lock_existed = snapshot.lock_path.exists();
    match install_snapshot(ctx, &snapshot, false, None) {
        Ok(_) => {}
        Err(err) => match err.downcast::<InstallUserError>() {
            Ok(user) => return Ok(ExecutionOutcome::user_error(user.message, user.details)),
            Err(err) => return Err(err),
        },
    }
    refresh_project_site(&snapshot, ctx)?;
    if !lock_existed {
        files.push(relative_path_str(&snapshot.lock_path, &snapshot.root));
    }

    let mut details = json!({
        "package": actual_name,
        "python": python_req,
        "files_created": files,
        "project_root": root.display().to_string(),
        "lockfile": snapshot.lock_path.display().to_string(),
    });
    if inferred && !pyproject_preexisting {
        details["inferred_package"] = Value::Bool(true);
        details["hint"] = Value::String(
            "Pass --package <name> to override the inferred module name.".to_string(),
        );
    }

    Ok(ExecutionOutcome::success(
        format!("initialized project {actual_name}"),
        details,
    ))
}

fn resolve_python_requirement_arg(raw: Option<&str>) -> String {
    raw.map(str::trim).filter(|s| !s.is_empty()).map_or_else(
        || ">=3.11".to_string(),
        |s| {
            if s.chars()
                .next()
                .is_some_and(|ch| matches!(ch, '>' | '<' | '=' | '~' | '!'))
            {
                s.to_string()
            } else {
                format!(">={s}")
            }
        },
    )
}

#[derive(Debug)]
enum InitConflict {
    OtherTool(String),
    ExistingDependencies,
}

impl InitConflict {
    fn into_outcome(self, pyproject_path: &Path) -> ExecutionOutcome {
        match self {
            InitConflict::OtherTool(tool) => ExecutionOutcome::user_error(
                format!("pyproject managed by {tool}; run `px migrate --apply` to adopt px"),
                json!({
                    "pyproject": pyproject_path.display().to_string(),
                    "tool": tool,
                    "hint": "Run `px migrate --apply` to convert this project to px.",
                }),
            ),
            InitConflict::ExistingDependencies => ExecutionOutcome::user_error(
                "pyproject already declares dependencies",
                json!({
                    "pyproject": pyproject_path.display().to_string(),
                    "hint": "Run `px migrate --apply` to import existing dependencies into px.",
                }),
            ),
        }
    }
}

fn detect_init_conflict(pyproject_path: &Path) -> Result<Option<InitConflict>> {
    let contents = std::fs::read_to_string(pyproject_path)?;
    let doc: DocumentMut = contents.parse()?;
    if let Some(tool) = detect_foreign_tool(&doc) {
        return Ok(Some(InitConflict::OtherTool(tool)));
    }
    if project_dependencies_declared(&doc) {
        return Ok(Some(InitConflict::ExistingDependencies));
    }
    Ok(None)
}

fn detect_foreign_tool(doc: &DocumentMut) -> Option<String> {
    let tools = doc
        .get("tool")
        .and_then(Item::as_table)
        .map(|table| {
            table
                .iter()
                .filter_map(|(key, _)| {
                    let name = key.to_string();
                    match name.as_str() {
                        "poetry" | "pdm" | "hatch" | "flit" | "rye" => Some(name),
                        _ => None,
                    }
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    tools.into_iter().next()
}

fn project_dependencies_declared(doc: &DocumentMut) -> bool {
    doc.get("project")
        .and_then(Item::as_table)
        .and_then(|table| table.get("dependencies"))
        .and_then(Item::as_array)
        .is_some_and(|array| !array.is_empty())
}

fn existing_pyproject_response(pyproject_path: &Path) -> Result<ExecutionOutcome> {
    let mut details = json!({
        "pyproject": pyproject_path.display().to_string(),
    });
    if let Some(name) = project_name_from_pyproject(pyproject_path)? {
        details["package"] = Value::String(name);
    }
    details["hint"] = Value::String(
        "pyproject.toml already exists; run `px add` or start in an empty directory.".to_string(),
    );
    Ok(ExecutionOutcome::user_error(
        "project already initialized (pyproject.toml present)",
        details,
    ))
}

fn dirty_worktree_response(changes: &[String]) -> ExecutionOutcome {
    ExecutionOutcome::user_error(
        "worktree dirty; stash, commit, or rerun with --force",
        json!({
            "changes": changes,
            "hint": "Stash or commit changes, or add --force to bypass this guard.",
        }),
    )
}
