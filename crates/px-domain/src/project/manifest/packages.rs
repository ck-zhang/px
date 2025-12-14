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
        let mut spec = if let Some(idx) = trimmed.find('#') {
            trimmed[..idx].trim()
        } else {
            trimmed
        };
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
            if key == "requires-dist" || key == "install_requires" {
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
