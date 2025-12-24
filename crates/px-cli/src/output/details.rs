use serde_json::Value;

use crate::style::Style;
use crate::traceback;

pub(super) fn hint_from_details(details: &Value) -> Option<&str> {
    details
        .as_object()
        .and_then(|map| map.get("hint"))
        .and_then(Value::as_str)
}

pub(super) fn output_from_details<'a>(details: &'a Value, key: &str) -> Option<&'a str> {
    details
        .as_object()
        .and_then(|map| map.get(key))
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
}

pub(super) fn traceback_from_details(
    style: &Style,
    details: &Value,
) -> Option<traceback::TracebackDisplay> {
    let map = details.as_object()?;
    let traceback_value = map.get("traceback")?;
    traceback::format_traceback(style, traceback_value)
}

pub(super) fn autosync_note_from_details(details: &Value) -> Option<&str> {
    details
        .as_object()
        .and_then(|map| map.get("autosync"))
        .and_then(Value::as_object)
        .and_then(|map| map.get("note"))
        .and_then(Value::as_str)
}

pub(super) fn gitignore_note_from_details(details: &Value) -> Option<&str> {
    details
        .as_object()
        .and_then(|map| map.get("gitignore"))
        .and_then(Value::as_object)
        .and_then(|map| map.get("note"))
        .and_then(Value::as_str)
}

pub(super) fn manifest_change_lines_from_details(details: &Value) -> Vec<String> {
    let Some(entries) = details
        .as_object()
        .and_then(|map| map.get("manifest_changes"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    let mut lines = Vec::new();
    for entry in entries {
        let Some(obj) = entry.as_object() else {
            continue;
        };
        let Some(before) = obj.get("before").and_then(Value::as_str) else { continue };
        let Some(after) = obj.get("after").and_then(Value::as_str) else {
            continue;
        };
        if before.trim().is_empty() || after.trim().is_empty() {
            continue;
        }
        lines.push(format!("pyproject.toml: ~ {before} -> {after}"));
    }
    lines
}

pub(super) fn manifest_add_lines_from_details(details: &Value) -> Vec<String> {
    let Some(specs) = details
        .as_object()
        .and_then(|map| map.get("manifest_added"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };

    let mut lines = Vec::new();
    for spec in specs.iter().filter_map(Value::as_str) {
        let trimmed = spec.trim();
        if trimmed.is_empty() {
            continue;
        }
        lines.push(format!("pyproject.toml: + {trimmed}"));
    }
    lines
}

pub(super) fn lock_change_summary_line_from_details(details: &Value) -> Option<String> {
    let lock = details.get("lock_changes").and_then(Value::as_object)?;
    let changed = lock
        .get("changed")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !changed {
        return Some("px.lock: no changes".to_string());
    }
    let packages = lock.get("packages").and_then(Value::as_object)?;
    let before = packages.get("before").and_then(Value::as_u64)?;
    let after = packages.get("after").and_then(Value::as_u64)?;
    let added = packages.get("added").and_then(Value::as_u64)?;
    let removed = packages.get("removed").and_then(Value::as_u64)?;
    let updated = packages.get("updated").and_then(Value::as_u64)?;
    Some(format!(
        "px.lock: changed (packages: {before} -> {after}; +{added} -{removed} ~{updated})"
    ))
}

pub(super) fn lock_direct_change_lines_from_details(details: &Value, verbose: u8) -> Vec<String> {
    lock_highlight_lines(details, "direct", verbose)
}

pub(super) fn lock_updated_version_lines_from_details(
    details: &Value,
    verbose: u8,
) -> Vec<String> {
    lock_highlight_lines(details, "updated_versions", verbose)
}

fn lock_highlight_lines(details: &Value, key: &str, verbose: u8) -> Vec<String> {
    let Some(entries) = details
        .get("lock_changes")
        .and_then(Value::as_object)
        .and_then(|map| map.get(key))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };

    let mut lines = Vec::new();
    for entry in entries {
        let Some(obj) = entry.as_object() else {
            continue;
        };
        let Some(name) = obj.get("name").and_then(Value::as_str) else {
            continue;
        };
        let from = obj.get("from").and_then(Value::as_str);
        let to = obj.get("to").and_then(Value::as_str);
        let line = match (from, to) {
            (Some(from), Some(to)) => format!("px.lock: ~ {name} {from} -> {to}"),
            (None, Some(to)) => format!("px.lock: + {name}=={to}"),
            (Some(from), None) => format!("px.lock: - {name}=={from}"),
            (None, None) => continue,
        };
        lines.push(line);
    }

    if verbose > 0 {
        return lines;
    }

    let max = match key {
        "updated_versions" => 10usize,
        _ => 3usize,
    };
    if lines.len() > max {
        let remaining = lines.len() - max;
        lines.truncate(max);
        lines.push(format!("px.lock: … (+{remaining} more; use -v)"));
    }
    lines
}

pub(super) fn is_passthrough(details: &Value) -> bool {
    details
        .as_object()
        .and_then(|map| map.get("passthrough"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

pub(super) fn dry_run_preview_lines_from_details(details: &Value, verbose: u8) -> Vec<String> {
    let Some(preview) = details.get("preview").and_then(Value::as_object) else {
        return Vec::new();
    };

    let mut lines = Vec::new();

    if let Some(pyproject) = preview.get("pyproject").and_then(Value::as_object) {
        if let Some(changes) = pyproject.get("changes").and_then(Value::as_array) {
            for entry in changes {
                let Some(obj) = entry.as_object() else {
                    continue;
                };
                let group = obj
                    .get("group")
                    .and_then(Value::as_str)
                    .unwrap_or("dependencies");
                let label = if group == "dependencies" {
                    "pyproject.toml".to_string()
                } else {
                    format!("pyproject.toml ({group})")
                };
                if let Some(added) = obj.get("added").and_then(Value::as_array) {
                    for spec in added.iter().filter_map(Value::as_str) {
                        lines.push(format!("{label}: + {spec}"));
                    }
                }
                if let Some(removed) = obj.get("removed").and_then(Value::as_array) {
                    for spec in removed.iter().filter_map(Value::as_str) {
                        lines.push(format!("{label}: - {spec}"));
                    }
                }
                if let Some(updated) = obj.get("updated").and_then(Value::as_array) {
                    for change in updated {
                        let Some(change) = change.as_object() else {
                            continue;
                        };
                        let Some(before) = change.get("before").and_then(Value::as_str) else {
                            continue;
                        };
                        let Some(after) = change.get("after").and_then(Value::as_str) else {
                            continue;
                        };
                        lines.push(format!("{label}: ~ {before} -> {after}"));
                    }
                }
            }
        }
    }

    if let Some(lock) = preview.get("lock").and_then(Value::as_object) {
        let would_change = lock
            .get("would_change")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let packages = lock.get("packages").and_then(Value::as_object);
        let before = packages
            .and_then(|pkg| pkg.get("before"))
            .and_then(Value::as_u64);
        let after = packages
            .and_then(|pkg| pkg.get("after"))
            .and_then(Value::as_u64);
        let added = packages
            .and_then(|pkg| pkg.get("added"))
            .and_then(Value::as_u64);
        let removed = packages
            .and_then(|pkg| pkg.get("removed"))
            .and_then(Value::as_u64);
        let updated = packages
            .and_then(|pkg| pkg.get("updated"))
            .and_then(Value::as_u64);

        let summary = if !would_change {
            "px.lock: no changes".to_string()
        } else if let (Some(before), Some(after), Some(added), Some(removed), Some(updated)) =
            (before, after, added, removed, updated)
        {
            format!("px.lock: would change (packages: {before} -> {after}; +{added} -{removed} ~{updated})")
        } else if let (Some(before), Some(after)) = (before, after) {
            format!("px.lock: would change (packages: {before} -> {after})")
        } else if let Some(before) = before {
            format!("px.lock: would change (packages: {before} -> ?)")
        } else {
            "px.lock: would change".to_string()
        };
        lines.push(summary);

        if would_change {
            let mut highlight_lines = Vec::new();
            if let Some(highlights) = lock.get("highlights").and_then(Value::as_array) {
                for entry in highlights {
                    let Some(obj) = entry.as_object() else {
                        continue;
                    };
                    let Some(name) = obj.get("name").and_then(Value::as_str) else {
                        continue;
                    };
                    let from = obj.get("from").and_then(Value::as_str);
                    let to = obj.get("to").and_then(Value::as_str);
                    let line = match (from, to) {
                        (Some(from), Some(to)) => format!("px.lock: ~ {name} {from} -> {to}"),
                        (None, Some(to)) => format!("px.lock: + {name}=={to}"),
                        (Some(from), None) => format!("px.lock: - {name}=={from}"),
                        (None, None) => continue,
                    };
                    highlight_lines.push(line);
                }
            }
            if verbose > 0 {
                lines.extend(highlight_lines);
            } else {
                let max = 3usize;
                if highlight_lines.len() > max {
                    let remaining = highlight_lines.len() - max;
                    lines.extend(highlight_lines.into_iter().take(max));
                    lines.push(format!("px.lock: … (+{remaining} more; use -v)"));
                } else {
                    lines.extend(highlight_lines);
                }
            }
        }

        if let Some(note) = lock.get("note").and_then(Value::as_str) {
            if !note.trim().is_empty() {
                lines.push(format!("px.lock: note: {note}"));
            }
        }
    }

    if let Some(env) = preview.get("env").and_then(Value::as_object) {
        if let Some(rebuild) = env.get("would_rebuild").and_then(Value::as_bool) {
            lines.push(format!(
                "env: would rebuild ({})",
                if rebuild { "yes" } else { "no" }
            ));
        }
    }

    if let Some(tools) = preview.get("tools").and_then(Value::as_object) {
        if let Some(rebuild) = tools.get("would_rebuild").and_then(Value::as_bool) {
            lines.push(format!(
                "tools: would rebuild ({})",
                if rebuild { "yes" } else { "no" }
            ));
        }
    }

    if verbose == 0 {
        let max = 12usize;
        if lines.len() > max {
            let remaining = lines.len() - max;
            lines.truncate(max);
            lines.push(format!("… (+{remaining} more; use -v)"));
        }
    }

    lines
}
