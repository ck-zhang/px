// Editable metadata stub + entrypoint/script helpers.
use super::*;

#[derive(Clone, Debug)]
pub(in super::super) struct EditableProjectMetadata {
    name: String,
    normalized_name: String,
    pub(in super::super) version: String,
    requires_python: Option<String>,
    requires_dist: Vec<String>,
    optional_requires: BTreeMap<String, Vec<String>>,
    summary: Option<String>,
    entry_points: BTreeMap<String, BTreeMap<String, String>>,
    top_level: Vec<String>,
}

pub(in super::super) fn write_project_metadata_stub(
    snapshot: &ManifestSnapshot,
    site_dir: &Path,
    fs_ops: &dyn effects::FileSystem,
) -> Result<()> {
    let metadata = match load_editable_project_metadata(&snapshot.manifest_path, fs_ops) {
        Ok(meta) => meta,
        Err(err) => {
            warn!(
                error = %err,
                path = %snapshot.manifest_path.display(),
                "skipping editable metadata stub"
            );
            return Ok(());
        }
    };

    cleanup_editable_metadata(site_dir, &metadata.normalized_name, fs_ops)?;
    if let Some(site_packages) = detect_local_site_packages(fs_ops, site_dir) {
        let prefix = format!("{}-", metadata.normalized_name);
        let mut installed = false;
        if let Ok(entries) = fs_ops.read_dir(&site_packages) {
            for entry in entries.flatten() {
                let file_name = entry.file_name();
                let name = file_name.to_str().unwrap_or_default();
                if name.starts_with(&prefix) && name.ends_with(".dist-info") {
                    installed = true;
                    break;
                }
            }
        }
        if installed {
            if let Ok(entries) = fs_ops.read_dir(site_dir) {
                for entry in entries.flatten() {
                    let file_name = entry.file_name();
                    let name = file_name.to_str().unwrap_or_default();
                    if name.starts_with(&prefix) && name.ends_with(".dist-info") {
                        let _ = fs_ops.remove_dir_all(&entry.path());
                    }
                }
            }
            return Ok(());
        }
    }
    let dist_dir = site_dir.join(format!(
        "{}-{}.dist-info",
        metadata.normalized_name, metadata.version
    ));
    fs_ops.create_dir_all(&dist_dir)?;

    let mut record_paths = Vec::new();

    let metadata_body = render_editable_metadata(&metadata);
    fs_ops.write(&dist_dir.join("METADATA"), metadata_body.as_bytes())?;
    record_paths.push(dist_dir.join("METADATA"));
    if let Some(entry_points) = render_editable_entry_points(&metadata) {
        fs_ops.write(&dist_dir.join("entry_points.txt"), entry_points.as_bytes())?;
        record_paths.push(dist_dir.join("entry_points.txt"));
    }
    let bin_dir = site_dir.join("bin");
    let python_path = bin_dir.join("python");
    let python = Some(python_path.as_path());
    let install_entrypoints = |entries: &BTreeMap<String, String>,
                               record_paths: &mut Vec<PathBuf>| {
        for (name, target) in entries {
            let _ = fs::remove_file(bin_dir.join(name));
            let target_value = target.split_whitespace().next().unwrap_or(target).trim();
            if let Some((module, callable)) = target_value.split_once(':') {
                if let Ok(script_path) =
                    write_entrypoint_script(&bin_dir, name, module.trim(), callable.trim(), python)
                {
                    record_paths.push(script_path);
                }
            }
        }
    };
    if let Some(entries) = metadata.entry_points.get("console_scripts") {
        install_entrypoints(entries, &mut record_paths);
    }
    if let Some(entries) = metadata.entry_points.get("gui_scripts") {
        install_entrypoints(entries, &mut record_paths);
    }

    let project_root = fs_ops
        .canonicalize(&snapshot.root)
        .unwrap_or_else(|_| snapshot.root.clone());
    let direct_url = Url::from_file_path(&project_root)
        .ok()
        .map(|url| url.to_string())
        .unwrap_or_else(|| format!("file://{}", project_root.display()));
    let direct_url = serde_json::to_string_pretty(&json!({
        "dir_info": { "editable": true },
        "url": direct_url,
    }))?;
    fs_ops.write(&dist_dir.join("direct_url.json"), direct_url.as_bytes())?;
    record_paths.push(dist_dir.join("direct_url.json"));
    fs_ops.write(&dist_dir.join("INSTALLER"), b"px\n")?;
    record_paths.push(dist_dir.join("INSTALLER"));
    fs_ops.write(&dist_dir.join("PX-EDITABLE"), b"px\n")?;
    record_paths.push(dist_dir.join("PX-EDITABLE"));

    if !metadata.top_level.is_empty() {
        let mut body = metadata.top_level.join("\n");
        body.push('\n');
        fs_ops.write(&dist_dir.join("top_level.txt"), body.as_bytes())?;
        record_paths.push(dist_dir.join("top_level.txt"));
    }
    write_record_file(site_dir, &dist_dir, record_paths, fs_ops)?;
    Ok(())
}

pub(in super::super) fn uses_maturin_backend(manifest_path: &Path) -> Result<bool> {
    let contents = fs::read_to_string(manifest_path)?;
    let doc: DocumentMut = contents.parse()?;
    let mut uses_maturin = doc
        .get("build-system")
        .and_then(Item::as_table)
        .and_then(|table| table.get("requires"))
        .and_then(Item::as_array)
        .map(|requires| {
            requires
                .iter()
                .filter_map(|value| value.as_str())
                .any(|entry| entry.to_ascii_lowercase().contains("maturin"))
        })
        .unwrap_or(false);

    if !uses_maturin {
        uses_maturin = doc
            .get("tool")
            .and_then(Item::as_table)
            .and_then(|tool| tool.get("maturin"))
            .and_then(Item::as_table)
            .map(|table| !table.is_empty())
            .unwrap_or(false);
    }

    Ok(uses_maturin)
}

fn cleanup_editable_metadata(
    site_dir: &Path,
    normalized_name: &str,
    fs_ops: &dyn effects::FileSystem,
) -> Result<()> {
    if let Ok(entries) = fs_ops.read_dir(site_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
                continue;
            };
            if !name.starts_with(&format!("{normalized_name}-")) || !name.ends_with(".dist-info") {
                continue;
            }
            let marker = path.join("PX-EDITABLE");
            if marker.exists() {
                let _ = fs_ops.remove_dir_all(&path);
            }
        }
    }
    Ok(())
}

fn write_record_file(
    site_dir: &Path,
    dist_dir: &Path,
    mut record_paths: Vec<PathBuf>,
    fs_ops: &dyn effects::FileSystem,
) -> Result<()> {
    let record_path = dist_dir.join("RECORD");
    record_paths.push(record_path.clone());
    let mut seen = HashSet::new();
    let mut lines = Vec::new();
    for path in record_paths {
        if !path.exists() {
            continue;
        }
        let rel = path.strip_prefix(site_dir).unwrap_or(path.as_path());
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if !seen.insert(rel_str.clone()) {
            continue;
        }
        lines.push(format!("{rel_str},,"));
    }
    lines.sort();
    fs_ops.write(&record_path, lines.join("\n").as_bytes())?;
    Ok(())
}

pub(in super::super) fn load_editable_project_metadata(
    manifest_path: &Path,
    fs_ops: &dyn effects::FileSystem,
) -> Result<EditableProjectMetadata> {
    let contents = fs_ops.read_to_string(manifest_path)?;
    let doc: DocumentMut = contents.parse()?;
    let project = project_table(&doc)?;
    let name = project
        .get("name")
        .and_then(Item::as_str)
        .ok_or_else(|| anyhow!("pyproject missing [project].name"))?
        .to_string();
    let normalized_name = normalize_project_name(&name);
    let root = manifest_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let version = project
        .get("version")
        .and_then(Item::as_str)
        .map(std::string::ToString::to_string)
        .or_else(|| infer_version_from_version_file(&root, &doc, fs_ops))
        .or_else(|| infer_version_from_versioneer(&root, &normalized_name, fs_ops))
        .or_else(|| infer_version_from_hatch_vcs(&root, &doc))
        .or_else(|| infer_version_from_sources(&root, &normalized_name, fs_ops))
        .unwrap_or_else(|| "0.0.0+unknown".to_string());
    let requires_python = project
        .get("requires-python")
        .and_then(Item::as_str)
        .map(std::string::ToString::to_string);
    let requires_dist = project
        .get("dependencies")
        .and_then(Item::as_array)
        .map(|array| {
            array
                .iter()
                .filter_map(|value| value.as_str().map(std::string::ToString::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let optional_requires = collect_optional_dependencies(project);
    let summary = project
        .get("description")
        .and_then(Item::as_str)
        .map(std::string::ToString::to_string);
    let entry_points = collect_entry_points(project);
    let top_level = discover_top_level_modules(&root, &normalized_name, fs_ops);

    Ok(EditableProjectMetadata {
        name,
        normalized_name,
        version,
        requires_python,
        requires_dist,
        optional_requires,
        summary,
        entry_points,
        top_level,
    })
}

fn infer_version_from_version_file(
    manifest_root: &Path,
    doc: &DocumentMut,
    fs_ops: &dyn effects::FileSystem,
) -> Option<String> {
    let candidates = [
        hatch_version_file(doc),
        setuptools_scm_version_file(doc),
        pdm_version_file(doc),
    ];
    for relative in candidates.into_iter().flatten() {
        let path = manifest_root.join(relative);
        if fs_ops.metadata(&path).is_err() {
            continue;
        }
        if let Ok(contents) = fs_ops.read_to_string(&path) {
            let trimmed = contents.trim();
            if !trimmed.is_empty()
                && !trimmed.contains('=')
                && !trimmed.contains("__version__")
                && !trimmed.contains('\n')
            {
                return Some(trimmed.to_string());
            }
            for line in contents.lines() {
                let trimmed = line.trim_start();
                if let Some(raw) = trimmed.strip_prefix("version =") {
                    let value = raw.trim().trim_matches('"');
                    if !value.is_empty() {
                        return Some(value.to_string());
                    }
                }
                if trimmed.starts_with("__version__") {
                    if let Some((_, raw_value)) = trimmed.split_once('=') {
                        let value = raw_value.trim();
                        if value.starts_with('"') || value.starts_with('\'') {
                            let clean = value
                                .trim_matches(|ch| matches!(ch, '"' | '\''))
                                .to_string();
                            if !clean.is_empty() {
                                return Some(clean);
                            }
                        }
                    }
                }
            }
        }
    }
    None
}

fn infer_version_from_versioneer(
    project_root: &Path,
    normalized_name: &str,
    fs_ops: &dyn effects::FileSystem,
) -> Option<String> {
    let module = normalized_name.replace(['-', '.'], "_").to_lowercase();
    let candidates = [
        project_root.join("src").join(&module).join("_version.py"),
        project_root.join(&module).join("_version.py"),
    ];
    let path = candidates
        .iter()
        .find(|path| fs_ops.metadata(path).is_ok())?;

    let python = detect_interpreter().ok()?;
    let script = format!(
        r#"import importlib.util, json, pathlib
path = pathlib.Path({path:?})
spec = importlib.util.spec_from_file_location("px_versioneer", path)
if spec is None or spec.loader is None:
    print(json.dumps({{}}))
    raise SystemExit(0)
mod = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mod)
getter = getattr(mod, "get_versions", None) or getattr(mod, "get_version", None)
version = None
if callable(getter):
    value = getter()
    if isinstance(value, dict):
        version = value.get("version") or value.get("closest-tag") or value.get("closest_tag")
    else:
        version = value
print(json.dumps({{"version": version}}))
"#
    );
    let output = Command::new(python)
        .args(["-c", &script])
        .current_dir(project_root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let payload: Value = serde_json::from_slice(&output.stdout).ok()?;
    payload
        .get("version")
        .and_then(Value::as_str)
        .map(std::string::ToString::to_string)
        .filter(|value| !value.is_empty())
}

fn infer_version_from_hatch_vcs(manifest_root: &Path, doc: &DocumentMut) -> Option<String> {
    if !uses_hatch_vcs(doc) {
        return None;
    }
    let describe = hatch_git_describe_command(doc);
    let simplify = hatch_prefers_simplified_semver(doc);
    let drop_local = hatch_drops_local_version(doc);
    derive_vcs_version(
        manifest_root,
        &VersionDeriveOptions {
            git_describe_command: describe.as_deref(),
            simplified_semver: simplify,
            drop_local,
        },
    )
    .ok()
}

fn infer_version_from_sources(
    project_root: &Path,
    normalized_name: &str,
    fs_ops: &dyn effects::FileSystem,
) -> Option<String> {
    let module_name = normalized_name.replace(['-', '.'], "_").to_lowercase();
    let candidates = [
        project_root
            .join("src")
            .join(&module_name)
            .join("__init__.py"),
        project_root.join(&module_name).join("__init__.py"),
    ];
    for candidate in candidates {
        if fs_ops.metadata(&candidate).is_err() {
            continue;
        }
        if let Ok(contents) = fs_ops.read_to_string(&candidate) {
            for line in contents.lines() {
                let trimmed = line.trim_start();
                if !trimmed.starts_with("__version__") {
                    continue;
                }
                if let Some((_, raw_value)) = trimmed.split_once('=') {
                    let value = raw_value.trim();
                    let quoted = (value.starts_with('"') && value.ends_with('"'))
                        || (value.starts_with('\'') && value.ends_with('\''));
                    if !quoted {
                        continue;
                    }
                    let cleaned = value.trim_matches(|ch| ch == '"' || ch == '\'');
                    if !cleaned.is_empty() {
                        return Some(cleaned.to_string());
                    }
                }
            }
        }
    }
    None
}

fn discover_top_level_modules(
    project_root: &Path,
    normalized_name: &str,
    fs_ops: &dyn effects::FileSystem,
) -> Vec<String> {
    let mut names = Vec::new();
    let mut push_name = |value: &str| {
        if !value.is_empty() && !value.starts_with('.') && value != "__pycache__" {
            names.push(value.to_string());
        }
    };
    for base in [project_root.join("src"), project_root.to_path_buf()] {
        if let Ok(entries) = fs_ops.read_dir(&base) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if fs_ops.metadata(&path.join("__init__.py")).is_ok() {
                        if let Some(value) = path.file_name().and_then(|name| name.to_str()) {
                            push_name(value);
                        }
                    }
                } else if path.extension().is_some_and(|ext| ext == "py") {
                    if let Some(stem) = path.file_stem().and_then(|name| name.to_str()) {
                        push_name(stem);
                    }
                }
            }
        }
    }
    if names.is_empty() {
        names.push(normalized_name.replace(['-', '.'], "_").to_lowercase());
    }
    names.sort();
    names.dedup();
    names
}

fn collect_optional_dependencies(project: &Table) -> BTreeMap<String, Vec<String>> {
    let mut extras = BTreeMap::new();
    if let Some(optional) = project
        .get("optional-dependencies")
        .and_then(toml_edit::Item::as_table)
    {
        for (name, array) in optional.iter() {
            if let Some(values) = array.as_array() {
                let mut deps = Vec::new();
                for value in values {
                    if let Some(spec) = value.as_str() {
                        deps.push(spec.to_string());
                    }
                }
                if !deps.is_empty() {
                    extras.insert(name.to_string(), deps);
                }
            }
        }
    }
    extras
}

fn collect_entry_points(project: &Table) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut groups = BTreeMap::new();
    collect_entry_point_group(project, "scripts", "console_scripts", &mut groups);
    collect_entry_point_group(project, "gui-scripts", "gui_scripts", &mut groups);
    if let Some(ep_table) = project
        .get("entry-points")
        .and_then(toml_edit::Item::as_table)
    {
        for (group, table) in ep_table.iter() {
            if let Some(entries) = table.as_table() {
                let mut mapped = BTreeMap::new();
                for (name, value) in entries.iter() {
                    if let Some(target) = value.as_str() {
                        mapped.insert(name.to_string(), target.to_string());
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

fn collect_entry_point_group(
    project: &Table,
    project_key: &str,
    entry_point_group: &str,
    groups: &mut BTreeMap<String, BTreeMap<String, String>>,
) {
    if let Some(scripts) = project.get(project_key).and_then(toml_edit::Item::as_table) {
        let mut mapped = BTreeMap::new();
        for (name, value) in scripts.iter() {
            if let Some(target) = value.as_str() {
                mapped.insert(name.to_string(), target.to_string());
            }
        }
        if !mapped.is_empty() {
            groups.insert(entry_point_group.to_string(), mapped);
        }
    }
}

fn render_editable_entry_points(metadata: &EditableProjectMetadata) -> Option<String> {
    if metadata.entry_points.is_empty() {
        return None;
    }
    let mut sections = Vec::new();
    for (group, entries) in &metadata.entry_points {
        sections.push(format!("[{group}]"));
        for (name, target) in entries {
            sections.push(format!("{name} = {target}"));
        }
        sections.push(String::new());
    }
    Some(sections.join("\n"))
}

fn render_editable_metadata(metadata: &EditableProjectMetadata) -> String {
    let mut lines = Vec::new();
    lines.push("Metadata-Version: 2.1".to_string());
    lines.push(format!("Name: {}", metadata.name));
    lines.push(format!("Version: {}", metadata.version));
    if let Some(summary) = &metadata.summary {
        lines.push(format!("Summary: {summary}"));
    }
    if let Some(rp) = &metadata.requires_python {
        lines.push(format!("Requires-Python: {rp}"));
    }
    for extra in metadata.optional_requires.keys() {
        lines.push(format!("Provides-Extra: {extra}"));
    }
    for req in &metadata.requires_dist {
        lines.push(format!("Requires-Dist: {req}"));
    }
    for (extra, reqs) in &metadata.optional_requires {
        for req in reqs {
            lines.push(format!(r#"Requires-Dist: {req} ; extra == "{extra}""#));
        }
    }
    lines.push(String::new());
    lines.join("\n")
}

pub(in super::super) fn normalize_project_name(name: &str) -> String {
    let mut result = String::new();
    for ch in name.chars() {
        if matches!(ch, '-' | '.' | ' ') {
            result.push('_');
        } else {
            result.push(ch);
        }
    }
    result
}

fn write_entrypoint_script(
    bin_dir: &Path,
    name: &str,
    module: &str,
    callable: &str,
    python: Option<&Path>,
) -> Result<PathBuf> {
    fs::create_dir_all(bin_dir)?;
    let python_shebang = python
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "/usr/bin/env python3".to_string());
    let parts: Vec<String> = callable
        .split('.')
        .filter(|part| !part.is_empty())
        .map(ToString::to_string)
        .collect();
    let parts_repr = format!("{parts:?}");
    let contents = format!(
        "#!{python_shebang}\nimport importlib\nimport sys\n\ndef _load():\n    module = importlib.import_module({module:?})\n    target = module\n    for attr in {parts_repr}:\n        target = getattr(target, attr)\n    return target\n\nif __name__ == '__main__':\n    sys.exit(_load()())\n"
    );
    let script_path = bin_dir.join(name);
    fs::write(&script_path, contents)?;
    set_exec_permissions(&script_path);
    Ok(script_path)
}

pub(in super::super) fn materialize_wheel_scripts(
    artifact_path: &Path,
    bin_dir: &Path,
    python: Option<&Path>,
) -> Result<()> {
    fs::create_dir_all(bin_dir)?;
    if artifact_path.extension().is_some_and(|ext| ext == "dist") && artifact_path.is_dir() {
        let entry_points = fs::read_dir(artifact_path)?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .find(|path| path.extension().is_some_and(|ext| ext == "dist-info"))
            .and_then(|dist_info| {
                let ep = dist_info.join("entry_points.txt");
                ep.exists().then_some(ep)
            });
        if let Some(ep_path) = entry_points {
            if let Ok(contents) = fs::read_to_string(&ep_path) {
                let mut section = String::new();
                for line in contents.lines() {
                    let trimmed = line.trim();
                    if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
                        continue;
                    }
                    if trimmed.starts_with('[') && trimmed.ends_with(']') {
                        section = trimmed
                            .trim_start_matches('[')
                            .trim_end_matches(']')
                            .to_string();
                        continue;
                    }
                    if section != "console_scripts" && section != "gui_scripts" {
                        continue;
                    }
                    if let Some((name, target)) = trimmed.split_once('=') {
                        let entry_name = name.trim();
                        let raw_target = target.trim();
                        let target_value = raw_target
                            .split_whitespace()
                            .next()
                            .unwrap_or(raw_target)
                            .trim();
                        if let Some((module, callable)) = target_value.split_once(':') {
                            let _ = write_entrypoint_script(
                                bin_dir,
                                entry_name,
                                module.trim(),
                                callable.trim(),
                                python,
                            );
                        }
                    }
                }
            }
        }

        let script_dirs: Vec<PathBuf> = fs::read_dir(artifact_path)?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .map(|name| name.ends_with(".data"))
                    .unwrap_or(false)
            })
            .map(|data_dir| data_dir.join("scripts"))
            .filter(|path| path.exists())
            .collect();
        for dir in script_dirs {
            for entry in fs::read_dir(&dir)? {
                let entry = entry?;
                if entry.file_type()?.is_file() {
                    let dest = bin_dir.join(entry.file_name());
                    fs::copy(entry.path(), &dest)?;
                    set_exec_permissions(&dest);
                }
            }
        }
        return Ok(());
    }

    if artifact_path.extension().is_some_and(|ext| ext == "whl") && artifact_path.is_file() {
        let file = File::open(artifact_path)?;
        let mut archive = zip::ZipArchive::new(file)?;

        if let Some(idx) = (0..archive.len()).find(|i| {
            archive
                .by_index(*i)
                .ok()
                .map(|file| file.name().ends_with("entry_points.txt"))
                .unwrap_or(false)
        }) {
            if let Ok(mut ep_file) = archive.by_index(idx) {
                let mut contents = String::new();
                ep_file.read_to_string(&mut contents)?;
                let mut section = String::new();
                for line in contents.lines() {
                    let trimmed = line.trim();
                    if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
                        continue;
                    }
                    if trimmed.starts_with('[') && trimmed.ends_with(']') {
                        section = trimmed
                            .trim_start_matches('[')
                            .trim_end_matches(']')
                            .to_string();
                        continue;
                    }
                    if section != "console_scripts" && section != "gui_scripts" {
                        continue;
                    }
                    if let Some((name, target)) = trimmed.split_once('=') {
                        let entry_name = name.trim();
                        let raw_target = target.trim();
                        let target_value = raw_target
                            .split_whitespace()
                            .next()
                            .unwrap_or(raw_target)
                            .trim();
                        if let Some((module, callable)) = target_value.split_once(':') {
                            let _ = write_entrypoint_script(
                                bin_dir,
                                entry_name,
                                module.trim(),
                                callable.trim(),
                                python,
                            );
                        }
                    }
                }
            }
        }

        for i in 0..archive.len() {
            let mut file = archive.by_index(i)?;
            let name = file.name().to_string();
            if !name.contains(".data/scripts/") || name.ends_with('/') {
                continue;
            }
            if let Some((_, script_name)) = name.rsplit_once(".data/scripts/") {
                let dest = bin_dir.join(script_name);
                let mut contents = Vec::new();
                file.read_to_end(&mut contents)?;
                fs::write(&dest, contents)?;
                set_exec_permissions(&dest);
            }
        }
    }

    Ok(())
}

#[cfg(test)]
pub(in super::super) fn write_sitecustomize(
    site_dir: &Path,
    extra_dir: Option<&Path>,
    fs: &dyn effects::FileSystem,
) -> Result<()> {
    let path = site_dir.join("sitecustomize.py");
    fs.write(&path, SITE_CUSTOMIZE.as_bytes())?;
    if let Some(extra) = extra_dir {
        fs.create_dir_all(extra)?;
        fs.write(&extra.join("sitecustomize.py"), SITE_CUSTOMIZE.as_bytes())?;
    }
    Ok(())
}
