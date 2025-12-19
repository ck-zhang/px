use super::super::*;
use super::EphemeralInput;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::json;
use toml_edit::DocumentMut;

fn collect_entry_points(project: &toml_edit::Table) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut groups: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();

    for (project_key, group_key) in [
        ("scripts", "console_scripts"),
        ("gui-scripts", "gui_scripts"),
    ] {
        if let Some(table) = project.get(project_key).and_then(toml_edit::Item::as_table) {
            let mut mapped = BTreeMap::new();
            for (name, value) in table.iter() {
                if let Some(target) = value.as_str() {
                    let trimmed_name = name.trim();
                    let trimmed_target = target.trim();
                    if trimmed_name.is_empty() || trimmed_target.is_empty() {
                        continue;
                    }
                    mapped.insert(trimmed_name.to_string(), trimmed_target.to_string());
                }
            }
            if !mapped.is_empty() {
                groups.insert(group_key.to_string(), mapped);
            }
        }
    }

    if let Some(ep_table) = project
        .get("entry-points")
        .and_then(toml_edit::Item::as_table)
    {
        for (group, table) in ep_table.iter() {
            if let Some(entries) = table.as_table() {
                let mut mapped = BTreeMap::new();
                for (name, value) in entries.iter() {
                    if let Some(target) = value.as_str() {
                        let trimmed_name = name.trim();
                        let trimmed_target = target.trim();
                        if trimmed_name.is_empty() || trimmed_target.is_empty() {
                            continue;
                        }
                        mapped.insert(trimmed_name.to_string(), trimmed_target.to_string());
                    }
                }
                if !mapped.is_empty() {
                    groups.insert(group.to_string(), mapped);
                }
            }
        }
    }

    groups
}

pub(in super::super) fn detect_ephemeral_input(
    invocation_root: &Path,
    run_target: Option<&str>,
) -> Result<EphemeralInput, ExecutionOutcome> {
    if let Some(target) = run_target {
        if let Some(inline) = crate::core::runtime::script::detect_inline_script_at(
            invocation_root,
            target,
        )? {
            let mut deps = inline.dependencies().to_vec();
            deps.sort();
            deps.dedup();
            return Ok(EphemeralInput::InlineScript {
                requires_python: inline.requires_python().to_string(),
                deps,
            });
        }
    }

    let pyproject_path = invocation_root.join("pyproject.toml");
    if pyproject_path.exists() {
        let contents = fs::read_to_string(&pyproject_path).map_err(|err| {
            ExecutionOutcome::failure(
                "failed to read pyproject.toml for ephemeral run",
                json!({
                    "error": err.to_string(),
                    "pyproject": pyproject_path.display().to_string(),
                }),
            )
        })?;
        let doc: DocumentMut = contents.parse::<DocumentMut>().map_err(|err| {
            ExecutionOutcome::failure(
                "failed to parse pyproject.toml for ephemeral run",
                json!({
                    "error": err.to_string(),
                    "pyproject": pyproject_path.display().to_string(),
                }),
            )
        })?;
        let entry_points = doc
            .get("project")
            .and_then(toml_edit::Item::as_table)
            .map(collect_entry_points)
            .unwrap_or_default();
        let snapshot =
            px_domain::api::ProjectSnapshot::from_document(invocation_root, &pyproject_path, doc)
                .map_err(|err| {
                    ExecutionOutcome::failure(
                        "failed to parse pyproject.toml for ephemeral run",
                        json!({
                            "error": err.to_string(),
                            "pyproject": pyproject_path.display().to_string(),
                        }),
                    )
                })?;
        return Ok(EphemeralInput::Pyproject {
            requires_python: snapshot.python_requirement,
            deps: snapshot.requirements,
            entry_points,
        });
    }

    let requirements_path = invocation_root.join("requirements.txt");
    if requirements_path.exists() {
        let deps = read_requirements_for_ephemeral(&requirements_path)?;
        return Ok(EphemeralInput::Requirements { deps });
    }

    Ok(EphemeralInput::Empty)
}

pub(in super::super) fn enforce_pinned_inputs(
    command: &str,
    invocation_root: &Path,
    input: &EphemeralInput,
    frozen: bool,
) -> Result<(), ExecutionOutcome> {
    let empty: &[String] = &[];
    let (requires_python, deps) = match input {
        EphemeralInput::InlineScript {
            requires_python,
            deps,
        } => (Some(requires_python.as_str()), deps.as_slice()),
        EphemeralInput::Pyproject {
            requires_python,
            deps,
            ..
        } => (Some(requires_python.as_str()), deps.as_slice()),
        EphemeralInput::Requirements { deps } => (None, deps.as_slice()),
        EphemeralInput::Empty => (None, empty),
    };
    let mut unpinned = Vec::new();
    for spec in deps {
        if px_domain::api::spec_requires_pin(spec) {
            unpinned.push(spec.clone());
        }
    }
    if unpinned.is_empty() {
        return Ok(());
    }
    let why = if frozen {
        "ephemeral runs in --frozen mode require fully pinned dependencies"
    } else {
        "ephemeral runs in CI require fully pinned dependencies"
    };
    let mut details = json!({
        "reason": "ephemeral_unpinned_inputs",
        "command": command,
        "unpinned": unpinned,
        "hint": "Pin dependencies as `name==version` (or adopt with `px migrate --apply` to generate px.lock).",
        "cwd": invocation_root.display().to_string(),
    });
    if let Some(req) = requires_python {
        if let Some(map) = details.as_object_mut() {
            map.insert("requires_python".to_string(), json!(req));
        }
    }
    Err(ExecutionOutcome::user_error(why, details))
}

fn read_requirements_for_ephemeral(path: &Path) -> Result<Vec<String>, ExecutionOutcome> {
    let mut visited = std::collections::HashSet::new();
    let mut deps = Vec::new();
    collect_requirements_recursive(path, &mut visited, &mut deps)?;
    deps.sort();
    deps.dedup();
    Ok(deps)
}

fn collect_requirements_recursive(
    path: &Path,
    visited: &mut std::collections::HashSet<PathBuf>,
    deps: &mut Vec<String>,
) -> Result<(), ExecutionOutcome> {
    let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(canonical.clone()) {
        return Ok(());
    }

    let contents = fs::read_to_string(&canonical).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to read requirements file for ephemeral run",
            json!({
                "error": err.to_string(),
                "requirements": canonical.display().to_string(),
            }),
        )
    })?;
    let base_dir = canonical.parent().unwrap_or_else(|| Path::new("."));

    let mut pending = String::new();
    let mut pending_start_line: usize = 0;

    for (idx, raw_line) in contents.lines().enumerate() {
        let mut line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if let Some(hash_idx) = line.find('#') {
            if line[..hash_idx]
                .chars()
                .last()
                .is_some_and(|ch| ch.is_whitespace())
            {
                line = line[..hash_idx].trim_end();
            }
        }

        let continues = line.ends_with('\\');
        if continues {
            line = line
                .strip_suffix('\\')
                .unwrap_or(line)
                .trim_end_matches([' ', '\t']);
        }

        if pending.is_empty() {
            pending_start_line = idx + 1;
        } else {
            pending.push(' ');
        }
        pending.push_str(line);

        if continues {
            continue;
        }

        if !pending.trim().is_empty() {
            parse_ephemeral_requirement_line(
                pending.trim(),
                pending_start_line,
                &canonical,
                base_dir,
                visited,
                deps,
            )?;
        }
        pending.clear();
        pending_start_line = 0;
    }

    if !pending.trim().is_empty() {
        parse_ephemeral_requirement_line(
            pending.trim(),
            pending_start_line,
            &canonical,
            base_dir,
            visited,
            deps,
        )?;
    }

    Ok(())
}

fn strip_pip_hash_tokens(line: &str) -> String {
    let mut out = String::new();
    let mut tokens = line.split_whitespace().peekable();

    while let Some(token) = tokens.next() {
        let lower = token.to_ascii_lowercase();
        if lower == "--hash" {
            let _ = tokens.next();
            continue;
        }
        if lower.starts_with("--hash=") {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(token);
    }

    out
}

fn looks_like_windows_abs_path(line: &str) -> bool {
    let bytes = line.as_bytes();
    if bytes.len() < 3 {
        return false;
    }
    bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && matches!(bytes[2], b'/' | b'\\')
}

fn parse_ephemeral_requirement_line(
    line: &str,
    line_no: usize,
    requirements: &Path,
    base_dir: &Path,
    visited: &mut std::collections::HashSet<PathBuf>,
    deps: &mut Vec<String>,
) -> Result<(), ExecutionOutcome> {
    let stripped = strip_pip_hash_tokens(line);
    let line = stripped.trim();
    if line.is_empty() {
        return Ok(());
    }

    if let Some(rest) = line
        .strip_prefix("-r")
        .or_else(|| line.strip_prefix("--requirement"))
    {
        let target = rest.trim_start_matches([' ', '=']).trim();
        if target.is_empty() {
            return Ok(());
        }
        let include = if Path::new(target).is_absolute() {
            PathBuf::from(target)
        } else {
            base_dir.join(target)
        };
        collect_requirements_recursive(&include, visited, deps)?;
        return Ok(());
    }

    if line.starts_with("-e") || line.starts_with("--editable") {
        return Err(ExecutionOutcome::user_error(
            "ephemeral requirements.txt does not support editable installs",
            json!({
                "reason": "ephemeral_requirements_editable_unsupported",
                "requirements": requirements.display().to_string(),
                "line": line_no,
                "hint": "Use pinned, non-editable requirements (no -e) or adopt the project with `px migrate --apply`.",
            }),
        ));
    }

    if line.starts_with('-') {
        return Err(ExecutionOutcome::user_error(
            "ephemeral requirements.txt does not support pip options",
            json!({
                "reason": "ephemeral_requirements_option_unsupported",
                "requirements": requirements.display().to_string(),
                "line": line_no,
                "hint": "Move pip options (like --index-url/--find-links/--constraint) to your environment or adopt the project with `px migrate --apply`.",
            }),
        ));
    }

    if line == "."
        || line.starts_with("./")
        || line.starts_with("../")
        || line.starts_with('/')
        || line.starts_with("\\\\")
        || looks_like_windows_abs_path(line)
        || line.to_ascii_lowercase().starts_with("file:")
    {
        return Err(ExecutionOutcome::user_error(
            "ephemeral requirements.txt does not support local path dependencies",
            json!({
                "reason": "ephemeral_requirements_local_path_unsupported",
                "requirements": requirements.display().to_string(),
                "line": line_no,
                "hint": "Use published, pinned requirements, or adopt the project with `px migrate --apply`.",
            }),
        ));
    }

    deps.push(line.to_string());
    Ok(())
}
