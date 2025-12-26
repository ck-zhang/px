// Test-only helpers for materializing a project site.
use super::*;

#[cfg(test)]
pub fn materialize_project_site(
    cache_root: &Path,
    site_dir: &Path,
    site_packages: &Path,
    lock: &LockSnapshot,
    python: Option<&Path>,
    fs: &dyn effects::FileSystem,
) -> Result<()> {
    fn dependency_versions(lock: &LockSnapshot) -> std::collections::HashMap<String, String> {
        let mut versions = std::collections::HashMap::new();
        for spec in &lock.dependencies {
            let head = spec.split(';').next().unwrap_or(spec).trim();
            if let Some((name_part, ver_part)) = head.split_once("==") {
                let name = crate::core::runtime::artifacts::dependency_name(name_part)
                    .to_ascii_lowercase();
                let version = ver_part.trim().to_string();
                versions.entry(name).or_insert(version);
            }
        }
        if let Some(graph) = &lock.graph {
            for node in &graph.nodes {
                versions
                    .entry(node.name.to_ascii_lowercase())
                    .or_insert(node.version.clone());
            }
        }
        versions
    }

    fn inferred_version_from_filename(filename: &str) -> String {
        let parts: Vec<&str> = filename.trim_end_matches(".whl").split('-').collect();
        if parts.len() >= 2 {
            parts[1].to_string()
        } else {
            "unknown".to_string()
        }
    }

    fs.create_dir_all(site_dir)?;
    fs.create_dir_all(site_packages)?;
    let pth_path = site_dir.join("px.pth");
    let pth_copy_path = site_packages.join("px.pth");
    let bin_dir = site_dir.join("bin");
    fs.create_dir_all(&bin_dir)?;
    let versions = dependency_versions(lock);
    let mut entries = Vec::new();
    for dep in &lock.resolved {
        let Some(artifact) = &dep.artifact else {
            continue;
        };
        let version = versions
            .get(&dep.name.to_ascii_lowercase())
            .cloned()
            .unwrap_or_else(|| inferred_version_from_filename(&artifact.filename));
        let wheel_path =
            crate::store::wheel_path(cache_root, &dep.name, &version, &artifact.filename);
        if !wheel_path.exists() {
            continue;
        }
        let dist_path = wheel_path.with_extension("dist");
        let entry_path = if dist_path.exists() {
            dist_path
        } else {
            wheel_path
        };
        let _ = materialize_wheel_scripts(&entry_path, &bin_dir, python);
        let canonical = entry_path.canonicalize().unwrap_or(entry_path);
        entries.push(canonical);
    }

    entries.sort();
    entries.dedup();

    let mut contents = entries
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    if !contents.is_empty() {
        contents.push('\n');
    }
    fs.write(&pth_path, contents.as_bytes())?;
    fs.write(&pth_copy_path, contents.as_bytes())?;
    write_sitecustomize(site_dir, Some(site_packages), fs)?;
    Ok(())
}
