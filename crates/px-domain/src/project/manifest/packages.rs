use super::*;

/// Convert a `pyproject.toml` file into onboarding rows.
///
/// # Errors
///
/// Returns an error if the file cannot be read or parsed.
pub fn collect_pyproject_packages(
    root: &Path,
    path: &Path,
) -> Result<(Value, Vec<OnboardPackagePlan>)> {
    let contents = fs::read_to_string(path)?;
    let doc: DocumentMut = contents.parse()?;
    let deps = read_dependencies_from_doc(&doc);
    let rel = relative_path(root, path);
    let mut rows = Vec::new();
    for dep in deps {
        rows.push(OnboardPackagePlan::new(dep, "prod", rel.clone()));
    }
    Ok((
        json!({ "kind": "pyproject", "path": rel, "count": rows.len() }),
        rows,
    ))
}

/// Convert a requirements file into onboarding rows.
///
/// # Errors
///
/// Returns an error if the file cannot be read or parsed.
pub fn collect_requirement_packages(
    root: &Path,
    path: &Path,
    kind: &str,
    scope: &str,
) -> Result<(Value, Vec<OnboardPackagePlan>)> {
    let parsed = read_requirements_file(path)?;
    let rel = relative_path(root, path);
    let mut rows = Vec::new();
    for spec in parsed.specs {
        rows.push(OnboardPackagePlan::new(spec, scope, rel.clone()));
    }
    if !parsed.extras.is_empty() {
        let pyproject = root.join("pyproject.toml");
        if pyproject.exists() {
            if let Ok(contents) = fs::read_to_string(&pyproject) {
                if let Ok(doc) = contents.parse::<DocumentMut>() {
                    let mut seen = HashSet::new();
                    for extra in parsed.extras {
                        if !seen.insert(extra.clone()) {
                            continue;
                        }
                        let deps = read_optional_dependency_group(&doc, &extra);
                        for dep in deps {
                            rows.push(OnboardPackagePlan::new(
                                dep,
                                scope,
                                format!("{rel} [{extra}]"),
                            ));
                        }
                    }
                }
            }
        }
    }
    Ok((
        json!({ "kind": kind, "path": rel, "count": rows.len() }),
        rows,
    ))
}

/// Convert `setup.cfg` metadata dependencies into onboarding rows.
///
/// # Errors
///
/// Returns an error if the file cannot be read or parsed.
pub fn collect_setup_cfg_packages(
    root: &Path,
    path: &Path,
) -> Result<(Value, Vec<OnboardPackagePlan>)> {
    let specs = read_setup_cfg_requires(path)?;
    let rel = relative_path(root, path);
    let mut rows = Vec::new();
    for spec in specs {
        rows.push(OnboardPackagePlan::new(spec, "prod", rel.clone()));
    }
    Ok((
        json!({ "kind": "setup.cfg", "path": rel, "count": rows.len() }),
        rows,
    ))
}

/// Convert `setup.py` install requirements into onboarding rows.
///
/// # Errors
///
/// Returns an error if the file cannot be read or parsed.
pub fn collect_setup_py_packages(
    root: &Path,
    path: &Path,
) -> Result<(Value, Vec<OnboardPackagePlan>)> {
    let specs = read_setup_py_requires(path)?;
    let rel = relative_path(root, path);
    let mut rows = Vec::new();
    for spec in specs {
        rows.push(OnboardPackagePlan::new(spec, "prod", rel.clone()));
    }
    Ok((
        json!({ "kind": "setup.py", "path": rel, "count": rows.len() }),
        rows,
    ))
}

/// Read every requirement entry from `path`.
///
/// # Errors
///
/// Returns an error when the file cannot be read from disk.
#[derive(Debug, Default)]
pub struct RequirementFile {
    pub specs: Vec<String>,
    pub extras: Vec<String>,
}

pub fn read_requirements_file(path: &Path) -> Result<RequirementFile> {
    let mut visited = HashSet::new();
    read_requirements_file_inner(path, &mut visited)
}

fn read_requirements_file_inner(
    path: &Path,
    visited: &mut HashSet<PathBuf>,
) -> Result<RequirementFile> {
    let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(canonical.clone()) {
        return Ok(RequirementFile::default());
    }
    let contents = fs::read_to_string(&canonical)?;
    let base_dir = canonical.parent().unwrap_or_else(|| Path::new("."));
    let mut specs = Vec::new();
    let mut extras = Vec::new();
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let mut spec = trimmed;
        if let Some(idx) = trimmed.find('#') {
            let before = &trimmed[..idx];
            let is_comment = idx == 0 || before.chars().last().is_some_and(|ch| ch.is_whitespace());
            if is_comment {
                spec = before.trim();
            }
        }
        if let Some(rest) = spec.strip_prefix("-r") {
            let target = rest.trim_start_matches([' ', '=']).trim();
            if !target.is_empty() {
                let include = if Path::new(target).is_absolute() {
                    PathBuf::from(target)
                } else {
                    base_dir.join(target)
                };
                let nested = read_requirements_file_inner(&include, visited)?;
                specs.extend(nested.specs);
                extras.extend(nested.extras);
            }
            continue;
        } else if let Some(rest) = spec.strip_prefix("--requirement") {
            let target = rest.trim_start_matches([' ', '=']).trim();
            if !target.is_empty() {
                let include = if Path::new(target).is_absolute() {
                    PathBuf::from(target)
                } else {
                    base_dir.join(target)
                };
                let nested = read_requirements_file_inner(&include, visited)?;
                specs.extend(nested.specs);
                extras.extend(nested.extras);
            }
            continue;
        }
        if let Some(stripped) = spec.strip_prefix("-e ") {
            spec = stripped.trim();
        } else if let Some(stripped) = spec.strip_prefix("--editable ") {
            spec = stripped.trim();
        }
        if spec.starts_with("git+")
            || spec.starts_with("hg+")
            || spec.starts_with("bzr+")
            || spec.starts_with("svn+")
        {
            if let Some((url, fragment)) = spec.split_once("#egg=") {
                let mut parts = fragment.split('&');
                let egg = parts.next().unwrap_or("").trim();
                if !egg.is_empty() {
                    let rest = parts.collect::<Vec<_>>();
                    let mut clean_url = url.to_string();
                    if !rest.is_empty() {
                        clean_url.push('#');
                        clean_url.push_str(&rest.join("&"));
                    }
                    specs.push(format!("{egg} @ {clean_url}"));
                    continue;
                }
            }
        }
        if let Some(extras_block) = spec.strip_prefix(".[") {
            if let Some(end) = extras_block.find(']') {
                let names = extras_block[..end].split(',');
                for extra in names {
                    let trimmed = extra.trim().to_lowercase();
                    if trimmed == "socks" {
                        specs.push("pysocks".to_string());
                    } else if !trimmed.is_empty() {
                        extras.push(trimmed);
                    }
                }
            }
            continue;
        }
        if spec == "." || spec.starts_with("./") || spec.starts_with(".[") {
            continue;
        }
        if spec.starts_with('-') {
            continue;
        }
        if !spec.is_empty() {
            specs.push(spec.to_string());
        }
    }
    Ok(RequirementFile { specs, extras })
}

/// Read dependency entries from `setup.cfg`.
///
/// # Errors
///
/// Returns an error when the file cannot be read from disk.
pub fn read_setup_cfg_requires(path: &Path) -> Result<Vec<String>> {
    let contents = fs::read_to_string(path)?;
    let mut specs = Vec::new();
    let mut section = String::new();
    let mut collecting = false;

    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            section = trimmed.trim_matches(&['[', ']'][..]).to_ascii_lowercase();
            collecting = false;
            continue;
        }

        if collecting {
            if line.chars().next().is_some_and(char::is_whitespace) {
                if !trimmed.is_empty() && !trimmed.starts_with('#') {
                    specs.push(trimmed.to_string());
                }
                continue;
            }
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            collecting = false;
        }

        if section != "metadata" && section != "options" {
            continue;
        }

        if let Some((raw_key, raw_value)) = line.split_once('=') {
            let key = raw_key.trim().to_ascii_lowercase();
            if key == "requires-dist" || key == "requires_dist" || key == "install_requires" {
                let value = raw_value.trim();
                if !value.is_empty() && !value.starts_with('#') {
                    specs.push(value.to_string());
                }
                collecting = true;
            }
        }
    }

    Ok(specs)
}

/// Read dependency entries from `setup.py` without executing it.
///
/// The parser is intentionally conservative: it looks for a top-level
/// `install_requires = [...]` list containing either string literals or
/// `deps["name"]` lookups. When a `_deps = [...]` list of requirement strings
/// is present, it is used to resolve `deps[...]` keys into full specifiers.
///
/// # Errors
///
/// Returns an error when the file cannot be read from disk.
pub fn read_setup_py_requires(path: &Path) -> Result<Vec<String>> {
    fn setup_py_key(spec: &str) -> String {
        let trimmed = spec.trim();
        let mut key = String::new();
        for ch in trimmed.chars() {
            if ch.is_whitespace() || matches!(ch, '!' | '=' | '<' | '>' | '~' | ';') {
                break;
            }
            key.push(ch);
        }
        key
    }

    fn extract_string_literals(text: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch != '\'' && ch != '"' {
                continue;
            }
            let quote = ch;
            let mut buf = String::new();
            let mut escaped = false;
            for next in chars.by_ref() {
                if escaped {
                    buf.push(next);
                    escaped = false;
                    continue;
                }
                if next == '\\' {
                    escaped = true;
                    continue;
                }
                if next == quote {
                    out.push(buf);
                    break;
                }
                buf.push(next);
            }
        }
        out
    }

    fn first_string_literal(line: &str) -> Option<String> {
        let trimmed = line.trim_start();
        let quote = trimmed.chars().next()?;
        if quote != '"' && quote != '\'' {
            return None;
        }
        let mut buf = String::new();
        let mut escaped = false;
        for ch in trimmed.chars().skip(1) {
            if escaped {
                buf.push(ch);
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if ch == quote {
                return Some(buf);
            }
            buf.push(ch);
        }
        None
    }

    fn collect_list_literal(contents: &str, var: &str) -> Vec<String> {
        let mut items = Vec::new();
        let mut in_block = false;
        for line in contents.lines() {
            let trimmed = line.trim();
            if !in_block {
                let Some(rest) = trimmed.strip_prefix(var) else {
                    continue;
                };
                if !rest.trim_start().starts_with('=') {
                    continue;
                }
                let Some(start) = trimmed.find('[') else {
                    continue;
                };
                let after = &trimmed[start + 1..];
                if let Some(end) = after.rfind(']') {
                    let inside = &after[..end];
                    items.extend(extract_string_literals(inside));
                    break;
                }
                in_block = true;
                continue;
            }
            if trimmed.starts_with(']') {
                break;
            }
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if let Some(value) = first_string_literal(trimmed) {
                items.push(value);
            }
        }
        items
    }

    fn collect_install_requires(contents: &str) -> Vec<String> {
        let mut items = Vec::new();
        let mut in_block = false;
        for line in contents.lines() {
            let trimmed = line.trim();
            if !in_block {
                let Some(rest) = trimmed.strip_prefix("install_requires") else {
                    continue;
                };
                if !rest.trim_start().starts_with('=') {
                    continue;
                }
                if trimmed.contains('[') {
                    in_block = true;
                    continue;
                }
                if trimmed.contains("deps_list(") || trimmed.contains("deps[") {
                    items.extend(extract_string_literals(trimmed));
                } else if let Some(value) =
                    first_string_literal(trimmed.split_once('=').map(|(_, v)| v).unwrap_or(""))
                {
                    items.push(value);
                }
                continue;
            }
            if trimmed.starts_with(']') {
                break;
            }
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if trimmed.starts_with("deps_list(") {
                items.extend(extract_string_literals(trimmed));
                continue;
            }
            if trimmed.starts_with("deps[") {
                items.extend(extract_string_literals(trimmed));
                continue;
            }
            if trimmed.starts_with('"') || trimmed.starts_with('\'') {
                if let Some(value) = first_string_literal(trimmed) {
                    items.push(value);
                }
            }
        }
        items
    }

    fn install_requires_variable(contents: &str) -> Option<String> {
        for line in contents.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let Some(idx) = trimmed.find("install_requires") else {
                continue;
            };
            let after = trimmed[idx + "install_requires".len()..].trim_start();
            if !after.starts_with('=') {
                continue;
            }
            let mut value = after.trim_start_matches('=').trim();
            value = value
                .trim_end_matches(',')
                .trim_end_matches(')')
                .trim_end_matches(',');
            if value
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
            {
                return Some(value.to_string());
            }
        }
        None
    }

    let contents = fs::read_to_string(path)?;

    let mut spec_by_key = BTreeMap::<String, String>::new();
    for spec in collect_list_literal(&contents, "_deps") {
        let key = setup_py_key(&spec);
        if key.is_empty() {
            continue;
        }
        spec_by_key
            .entry(key.clone())
            .or_insert_with(|| spec.clone());
        spec_by_key
            .entry(key.to_ascii_lowercase())
            .or_insert(spec.clone());
    }

    let mut raw_requires = collect_install_requires(&contents);
    if raw_requires.is_empty() {
        if let Some(var) = install_requires_variable(&contents) {
            raw_requires = collect_list_literal(&contents, &var);
        }
    }

    let mut specs = Vec::new();
    for raw in raw_requires {
        let key = raw.trim();
        if key.is_empty() {
            continue;
        }
        if let Some(spec) = spec_by_key
            .get(key)
            .or_else(|| spec_by_key.get(&key.to_ascii_lowercase()))
        {
            specs.push(spec.clone());
        } else {
            specs.push(key.to_string());
        }
    }
    specs.retain(|spec| !spec.trim().is_empty());
    Ok(specs)
}

pub(crate) fn requirement_display_name(spec: &str) -> String {
    PepRequirement::from_str(spec.trim())
        .map_or_else(|_| spec.trim().to_string(), |req| req.name.to_string())
}

pub(crate) fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

pub(crate) fn normalize_onboard_path(root: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}

#[derive(Clone)]
pub struct OnboardPackagePlan {
    pub name: String,
    pub requested: String,
    pub scope: String,
    pub source: String,
}

impl OnboardPackagePlan {
    fn new(requested: String, scope: &str, source: String) -> Self {
        let name = requirement_display_name(&requested);
        Self {
            name,
            requested,
            scope: scope.to_string(),
            source,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn read_setup_py_requires_resolves_install_requires_variable() -> Result<()> {
        let dir = tempdir()?;
        let setup_py = dir.path().join("setup.py");
        fs::write(
            &setup_py,
            r#"import setuptools

dependencies = [
    "requests>=2.22,<3.0",
    "google-auth>=2.0,<3.0",
]

setuptools.setup(
    name="demo",
    version="0.1.0",
    install_requires=dependencies,
)
"#,
        )?;

        let deps = read_setup_py_requires(&setup_py)?;
        assert_eq!(
            deps,
            vec![
                "requests>=2.22,<3.0".to_string(),
                "google-auth>=2.0,<3.0".to_string(),
            ]
        );
        Ok(())
    }

    #[test]
    fn read_requirements_file_converts_editable_vcs_egg_urls() -> Result<()> {
        let dir = tempdir()?;
        let reqs = dir.path().join("requirements.txt");
        fs::write(
            &reqs,
            "-e git+https://github.com/boto/botocore.git@develop#egg=botocore\n",
        )?;
        let parsed = read_requirements_file(&reqs)?;
        assert_eq!(
            parsed.specs,
            vec!["botocore @ git+https://github.com/boto/botocore.git@develop".to_string()]
        );
        Ok(())
    }
}
