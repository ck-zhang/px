use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DependencyGroupSource {
    IncludeConfig,
    LegacyConfig,
    DeclaredDefault,
    None,
}

impl DependencyGroupSource {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            DependencyGroupSource::IncludeConfig => "include-groups",
            DependencyGroupSource::LegacyConfig => "legacy",
            DependencyGroupSource::DeclaredDefault => "declared",
            DependencyGroupSource::None => "none",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DependencyGroupSelection {
    pub active: Vec<String>,
    pub declared: Vec<String>,
    pub source: DependencyGroupSource,
}

pub(super) fn normalize_dependency_group_name(name: &str) -> String {
    dependency_name(name.trim())
}

fn normalize_group_list(groups: Vec<String>) -> Vec<String> {
    let mut normalized: Vec<String> = groups
        .into_iter()
        .map(|name| normalize_dependency_group_name(&name))
        .filter(|name| !name.is_empty())
        .collect();
    normalized.sort();
    normalized.dedup();
    normalized
}

fn parse_env_groups(raw: &str) -> Vec<String> {
    raw.split(|ch: char| ch == ',' || ch.is_whitespace() || ch == ';')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(std::string::ToString::to_string)
        .collect()
}

fn declared_dependency_groups(doc: &DocumentMut) -> Vec<String> {
    let mut groups = Vec::new();
    if let Some(table) = doc.get("dependency-groups").and_then(Item::as_table) {
        for (name, _item) in table.iter() {
            groups.push(name.to_string());
        }
    }
    if let Some(table) = doc
        .get("project")
        .and_then(Item::as_table)
        .and_then(|project| project.get("optional-dependencies"))
        .and_then(Item::as_table)
    {
        for (name, entry) in table.iter() {
            let Some(array) = entry.as_array() else {
                continue;
            };
            let has_dev_tools = array
                .iter()
                .filter_map(|val| val.as_str())
                .any(is_dev_tool_spec);
            let include = if is_common_dev_group(name) {
                true
            } else if name.eq_ignore_ascii_case("all") && has_dev_tools {
                array
                    .iter()
                    .filter_map(|val| val.as_str())
                    .all(is_dev_tool_spec)
            } else {
                has_dev_tools
            };
            if include {
                groups.push(name.to_string());
            }
        }
    }
    if let Some(table) = doc
        .get("tool")
        .and_then(Item::as_table)
        .and_then(|tool| tool.get("poetry"))
        .and_then(Item::as_table)
        .and_then(|poetry| poetry.get("group"))
        .and_then(Item::as_table)
    {
        for (name, entry) in table.iter() {
            let dependencies = entry
                .as_table()
                .and_then(|group| group.get("dependencies"))
                .and_then(Item::as_table);
            if dependencies.is_some() {
                groups.push(name.to_string());
            }
        }
    }
    normalize_group_list(groups)
}

fn include_group_config(doc: &DocumentMut) -> Option<Vec<String>> {
    doc.get("tool")
        .and_then(Item::as_table)
        .and_then(|tool| tool.get("px"))
        .and_then(Item::as_table)
        .and_then(|px| px.get("dependencies"))
        .and_then(Item::as_table)
        .and_then(|deps| deps.get("include-groups"))
        .and_then(Item::as_array)
        .map(|array| {
            array
                .iter()
                .filter_map(|value| value.as_str().map(std::string::ToString::to_string))
                .collect()
        })
}

fn legacy_dependency_group_config(doc: &DocumentMut) -> Vec<String> {
    doc.get("tool")
        .and_then(Item::as_table)
        .and_then(|tool| tool.get("px"))
        .and_then(Item::as_table)
        .and_then(|px| px.get("dependency-groups"))
        .and_then(Item::as_array)
        .map(|array| {
            array
                .iter()
                .filter_map(|value| value.as_str().map(std::string::ToString::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn configured_dependency_groups(doc: &DocumentMut) -> (Vec<String>, DependencyGroupSource) {
    if let Some(include) = include_group_config(doc) {
        return (
            normalize_group_list(include),
            DependencyGroupSource::IncludeConfig,
        );
    }
    let legacy = legacy_dependency_group_config(doc);
    if !legacy.is_empty() {
        return (
            normalize_group_list(legacy),
            DependencyGroupSource::LegacyConfig,
        );
    }
    (Vec::new(), DependencyGroupSource::None)
}

pub(crate) fn select_dependency_groups(doc: &DocumentMut) -> DependencyGroupSelection {
    let declared = declared_dependency_groups(doc);
    let (mut active, mut source) = configured_dependency_groups(doc);
    if active.is_empty() && !declared.is_empty() {
        active = declared.clone();
        source = DependencyGroupSource::DeclaredDefault;
    }
    if let Ok(raw) = env::var("PX_GROUPS") {
        active.extend(parse_env_groups(&raw));
    }
    DependencyGroupSelection {
        active: normalize_group_list(active),
        declared,
        source,
    }
}

fn manifest_has_dependency(doc: &DocumentMut, needle: &str) -> bool {
    let target = dependency_name(needle);
    for spec in read_dependencies_from_doc(doc) {
        if dependency_name(&spec) == target {
            return true;
        }
    }
    if let Some(table) = doc
        .get("project")
        .and_then(Item::as_table)
        .and_then(|project| project.get("optional-dependencies"))
        .and_then(Item::as_table)
    {
        for (_, entry) in table.iter() {
            if let Some(array) = entry.as_array() {
                for val in array.iter().filter_map(|v| v.as_str()) {
                    if dependency_name(val) == target {
                        return true;
                    }
                }
            }
        }
    }
    if let Some(table) = doc.get("dependency-groups").and_then(Item::as_table) {
        for (_, entry) in table.iter() {
            if let Some(array) = entry.as_array() {
                for val in array.iter().filter_map(|v| v.as_str()) {
                    if dependency_name(val) == target {
                        return true;
                    }
                }
            }
        }
    }
    false
}

fn uses_hatch(doc: &DocumentMut) -> bool {
    if doc
        .get("tool")
        .and_then(Item::as_table)
        .and_then(|tool| tool.get("hatch"))
        .and_then(Item::as_table)
        .is_some()
    {
        return true;
    }
    doc.get("build-system")
        .and_then(Item::as_table)
        .and_then(|table| table.get("requires"))
        .and_then(Item::as_array)
        .map(|requires| {
            requires.iter().any(|entry| {
                entry
                    .as_str()
                    .map(|value| dependency_name(value) == "hatchling")
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

pub(crate) fn ensure_tooling_requirements(doc: &mut DocumentMut) -> bool {
    if !uses_hatch(doc) {
        return false;
    }
    if manifest_has_dependency(doc, "tomli-w") {
        return false;
    }
    merge_dev_dependency_specs(doc, &[TOMLI_W_REQUIREMENT.to_string()])
}

/// Resolves dependency specifications for the selected groups, supporting `include-group` entries.
pub(crate) fn resolve_dependency_groups(
    doc: &DocumentMut,
    groups: &[String],
) -> Result<Vec<String>> {
    let mut collected = Vec::new();
    let mut visiting = HashSet::new();
    for group in groups {
        collect_group_dependencies(doc, group, &mut visiting, &mut collected)?;
    }
    sort_and_dedupe(&mut collected);
    Ok(collected)
}

fn collect_group_dependencies(
    doc: &DocumentMut,
    group: &str,
    visiting: &mut HashSet<String>,
    collected: &mut Vec<String>,
) -> Result<()> {
    let normalized = normalize_dependency_group_name(group);
    if !visiting.insert(normalized.clone()) {
        return Err(anyhow!("dependency group cycle detected at `{group}`"));
    }

    let mut handled = collect_dependency_group_entries(doc, &normalized, visiting, collected)?;
    if let Some(array) = find_optional_dependency_group(doc, &normalized) {
        handled = true;
        for value in array.iter() {
            if let Some(spec) = value.as_str() {
                collected.push(spec.to_string());
            }
        }
    }

    if !handled {
        tracing::debug!("requested dependency group `{group}` not found in manifest");
    }

    visiting.remove(&normalized);
    Ok(())
}

fn collect_dependency_group_entries(
    doc: &DocumentMut,
    normalized_group: &str,
    visiting: &mut HashSet<String>,
    collected: &mut Vec<String>,
) -> Result<bool> {
    if let Some(table) = doc.get("dependency-groups").and_then(Item::as_table) {
        for (name, item) in table.iter() {
            if normalize_dependency_group_name(name) != normalized_group {
                continue;
            }
            if let Some(array) = item.as_array() {
                for value in array.iter() {
                    if let Some(inline) = value.as_inline_table() {
                        if let Some(target) = inline
                            .get("include-group")
                            .and_then(TomlValue::as_str)
                            .map(str::to_string)
                        {
                            collect_group_dependencies(doc, &target, visiting, collected)?;
                            continue;
                        }
                    }
                    if let Some(spec) = value.as_str() {
                        collected.push(spec.to_string());
                    }
                }
            }
            return Ok(true);
        }
    }
    if let Some(table) = doc
        .get("tool")
        .and_then(Item::as_table)
        .and_then(|tool| tool.get("poetry"))
        .and_then(Item::as_table)
        .and_then(|poetry| poetry.get("group"))
        .and_then(Item::as_table)
        .and_then(|groups| groups.get(normalized_group))
        .and_then(Item::as_table)
        .and_then(|group| group.get("dependencies"))
        .and_then(Item::as_table)
    {
        for (name, entry) in table.iter() {
            if let Some(spec) = poetry_dependency_spec(name, entry) {
                collected.push(spec);
            }
        }
        return Ok(true);
    }
    Ok(false)
}

fn normalize_poetry_version_spec(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Some(spec) = trimmed.strip_prefix('^').and_then(caret_version_bounds) {
        return spec;
    }
    trimmed.to_string()
}

fn caret_version_bounds(raw: &str) -> Option<String> {
    let mut parts: Vec<u64> = raw
        .split('.')
        .map(|piece| piece.parse::<u64>().unwrap_or(0))
        .collect();
    while parts.len() < 3 {
        parts.push(0);
    }
    let major = parts.first().copied().unwrap_or(0);
    let minor = parts.get(1).copied().unwrap_or(0);
    let patch = parts.get(2).copied().unwrap_or(0);

    if major > 0 {
        Some(format!(">={raw},<{}.0.0", major + 1))
    } else if minor > 0 {
        Some(format!(">={raw},<0.{}.0", minor + 1))
    } else {
        Some(format!(">={raw},<0.0.{}", patch + 1))
    }
}

fn poetry_python_marker_expr(raw: &str) -> Option<String> {
    let mut clauses = Vec::new();
    for part in raw.split(',') {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }
        let (op, value) = match [">=", "<=", "==", "!=", "~=", ">", "<"]
            .iter()
            .find_map(|op| {
                trimmed
                    .strip_prefix(op)
                    .map(|rest| (*op, rest.trim_start()))
            }) {
            Some(pair) => pair,
            None => continue,
        };
        if value.is_empty() {
            continue;
        }
        clauses.push(format!(r#"python_version {op} "{value}""#));
    }
    if clauses.is_empty() {
        None
    } else {
        Some(clauses.join(" and "))
    }
}

fn poetry_dependency_spec(name: &str, item: &Item) -> Option<String> {
    if let Some(version) = item.as_str() {
        let trimmed = version.trim();
        return Some(if trimmed.is_empty() || trimmed == "*" {
            name.to_string()
        } else {
            format!("{name} {}", normalize_poetry_version_spec(trimmed))
        });
    }
    let (extras, version, python, marker_expr) = if let Some(table) = item.as_table() {
        (
            table.get("extras").and_then(Item::as_array),
            table.get("version").and_then(Item::as_str),
            table.get("python").and_then(Item::as_str),
            table.get("markers").and_then(Item::as_str),
        )
    } else if let Some(inline) = item.as_value().and_then(TomlValue::as_inline_table) {
        (
            inline.get("extras").and_then(TomlValue::as_array),
            inline.get("version").and_then(TomlValue::as_str),
            inline.get("python").and_then(TomlValue::as_str),
            inline.get("markers").and_then(TomlValue::as_str),
        )
    } else {
        return None;
    };
    let mut spec = name.to_string();
    if let Some(extras) = extras {
        let extras: Vec<String> = extras
            .iter()
            .filter_map(|val| val.as_str().map(std::string::ToString::to_string))
            .collect();
        if !extras.is_empty() {
            spec = format!("{spec}[{}]", extras.join(","));
        }
    }
    if let Some(version) = version {
        let trimmed = version.trim();
        if !trimmed.is_empty() && trimmed != "*" {
            spec = format!("{spec} {}", normalize_poetry_version_spec(trimmed));
        }
    }
    let mut markers = Vec::new();
    if let Some(python) = python {
        if let Some(expr) = poetry_python_marker_expr(python) {
            markers.push(expr);
        }
    }
    if let Some(marker) = marker_expr {
        let trimmed = marker.trim();
        if !trimmed.is_empty() {
            markers.push(trimmed.to_string());
        }
    }
    if !markers.is_empty() {
        spec = format!("{spec}; {}", markers.join(" and "));
    }
    Some(spec)
}

fn find_optional_dependency_group<'a>(
    doc: &'a DocumentMut,
    normalized_group: &str,
) -> Option<&'a Array> {
    doc.get("project")
        .and_then(Item::as_table)
        .and_then(|project| project.get("optional-dependencies"))
        .and_then(Item::as_table)
        .and_then(|table| {
            table.iter().find_map(|(name, entry)| {
                if normalize_dependency_group_name(name) == normalized_group {
                    entry.as_array()
                } else {
                    None
                }
            })
        })
}

pub(crate) fn read_optional_dependency_group(doc: &DocumentMut, group: &str) -> Vec<String> {
    let normalized = normalize_dependency_group_name(group);
    find_optional_dependency_group(doc, &normalized)
        .map(|array| {
            array
                .iter()
                .filter_map(|val| val.as_str().map(std::string::ToString::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn is_common_dev_group(name: &str) -> bool {
    let lowered = name.to_ascii_lowercase();
    matches!(
        lowered.as_str(),
        "dev"
            | "test"
            | "tests"
            | "doc"
            | "docs"
            | "lint"
            | "format"
            | "fmt"
            | "typing"
            | "mypy"
            | "px-dev"
    )
}

fn is_dev_tool_spec(spec: &str) -> bool {
    let name = requirement_display_name(spec).to_ascii_lowercase();
    matches!(
        name.as_str(),
        "pytest"
            | "pytest-cov"
            | "pytest-xdist"
            | "hypothesis"
            | "ruff"
            | "flake8"
            | "mypy"
            | "coverage"
            | "tox"
            | "nox"
            | "black"
            | "isort"
            | "sphinx"
            | "pylint"
            | "bandit"
            | "pre-commit"
    )
}

pub(crate) fn ensure_dependency_group_config(doc: &mut DocumentMut) -> bool {
    if include_group_config(doc).is_some() {
        return false;
    }
    let declared = declared_dependency_groups(doc);
    if declared.is_empty() {
        return false;
    }
    let deps_entry = doc
        .entry("tool")
        .or_insert(Item::Table(Table::default()))
        .as_table_mut()
        .expect("tool table")
        .entry("px")
        .or_insert(Item::Table(Table::default()))
        .as_table_mut()
        .expect("px table")
        .entry("dependencies")
        .or_insert(Item::Table(Table::default()));
    if !deps_entry.is_table() {
        *deps_entry = Item::Table(Table::default());
    }
    let deps_table = deps_entry
        .as_table_mut()
        .expect("[tool.px.dependencies] must be a table");
    let mut array = Array::new();
    for group in declared {
        array.push(group.as_str());
    }
    deps_table.insert("include-groups", Item::Value(TomlValue::Array(array)));
    true
}
