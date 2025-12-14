use super::*;

use super::dependency_groups::normalize_dependency_group_name;

fn project_identity(doc: &DocumentMut) -> Result<(String, String)> {
    let project = project_table(doc)?;
    let name = project
        .get("name")
        .and_then(Item::as_str)
        .ok_or_else(|| anyhow!("pyproject missing [project].name"))?
        .to_string();
    let python_requirement = project
        .get("requires-python")
        .and_then(Item::as_str)
        .map_or_else(|| ">=3.11".to_string(), std::string::ToString::to_string);
    Ok((name, python_requirement))
}

pub fn manifest_fingerprint(
    doc: &DocumentMut,
    requirements: &[String],
    groups: &[String],
    options: &PxOptions,
) -> Result<String> {
    let (name, python_requirement) = project_identity(doc)?;
    let mut deps = requirements.to_vec();
    sort_and_dedupe(&mut deps);
    let mut hasher = Sha256::new();
    hasher.update(name.trim().to_lowercase().as_bytes());
    hasher.update(python_requirement.trim().as_bytes());
    for dep in deps {
        hasher.update(dep.trim().as_bytes());
        hasher.update(b"\n");
    }
    let mut group_names = groups
        .iter()
        .map(|name| normalize_dependency_group_name(name))
        .collect::<Vec<_>>();
    group_names.sort();
    group_names.dedup();
    for group in group_names {
        hasher.update(b"group:");
        hasher.update(group.trim().as_bytes());
        hasher.update(b"\n");
    }
    if let Some(tool_python) = doc
        .get("tool")
        .and_then(Item::as_table)
        .and_then(|tool| tool.get("px"))
        .and_then(Item::as_table)
        .and_then(|px| px.get("python"))
        .and_then(Item::as_str)
    {
        hasher.update(tool_python.trim().as_bytes());
    }
    if let Some(manage) = options.manage_command.as_ref() {
        let trimmed = manage.trim();
        if !trimmed.is_empty() {
            hasher.update(b"manage:");
            hasher.update(trimmed.as_bytes());
        }
    }
    if !options.plugin_imports.is_empty() {
        let mut imports = options.plugin_imports.clone();
        imports.sort();
        imports.dedup();
        for name in imports {
            let trimmed = name.trim();
            if trimmed.is_empty() {
                continue;
            }
            hasher.update(b"plugin:");
            hasher.update(trimmed.as_bytes());
            hasher.update(b"\n");
        }
    }
    if !options.env_vars.is_empty() {
        let mut vars = options.env_vars.clone().into_iter().collect::<Vec<_>>();
        vars.sort_by(|a, b| a.0.cmp(&b.0));
        for (key, value) in vars {
            hasher.update(b"env:");
            hasher.update(key.trim().as_bytes());
            hasher.update(b"=");
            hasher.update(value.trim().as_bytes());
            hasher.update(b"\n");
        }
    }
    Ok(format!("{:x}", hasher.finalize()))
}
