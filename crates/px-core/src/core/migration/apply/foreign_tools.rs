use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use toml_edit::DocumentMut;

pub(super) fn detect_foreign_tool_sections(path: &PathBuf) -> Result<Vec<String>> {
    let contents = fs::read_to_string(path)?;
    let doc: DocumentMut = contents.parse()?;
    let tool_table = doc
        .get("tool")
        .and_then(toml_edit::Item::as_table)
        .map(toml_edit::Table::iter)
        .into_iter()
        .flatten();

    let known = ["poetry", "pdm", "hatch", "flit", "rye"];
    let mut found = Vec::new();
    for (key, _) in tool_table {
        if known.contains(&key) {
            found.push(key.to_string());
        }
    }
    found.sort();
    found.dedup();
    Ok(found)
}

fn item_has_dependencies(item: &toml_edit::Item) -> bool {
    if let Some(array) = item.as_array() {
        return !array.is_empty();
    }
    if let Some(table) = item.as_table() {
        return !table.is_empty();
    }
    false
}

fn table_declares_dependencies(table: &toml_edit::Table) -> bool {
    for key in ["dependencies", "dev-dependencies"] {
        if let Some(entry) = table.get(key) {
            if item_has_dependencies(entry) {
                return true;
            }
        }
    }

    if let Some(group_table) = table.get("group").and_then(toml_edit::Item::as_table) {
        for (_, group_item) in group_table.iter() {
            if let Some(group_entry) = group_item.as_table() {
                if table_declares_dependencies(group_entry) {
                    return true;
                }
            }
        }
    }
    false
}

pub(super) fn detect_foreign_tool_conflicts(path: &PathBuf) -> Result<Vec<String>> {
    let contents = fs::read_to_string(path)?;
    let doc: DocumentMut = contents.parse()?;
    let Some(tool_table) = doc.get("tool").and_then(toml_edit::Item::as_table) else {
        return Ok(Vec::new());
    };

    let known = ["poetry", "pdm", "hatch", "flit", "rye"];
    let mut owners = Vec::new();
    for (key, value) in tool_table.iter() {
        if !known.contains(&key) {
            continue;
        }
        if let Some(table) = value.as_table() {
            if table_declares_dependencies(table) {
                owners.push(key.to_string());
            }
        }
    }
    owners.sort();
    owners.dedup();
    Ok(owners)
}
