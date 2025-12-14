// Test-only helpers for materializing a project site.
use super::*;

#[cfg(test)]
pub fn materialize_project_site(
    site_dir: &Path,
    site_packages: &Path,
    lock: &LockSnapshot,
    python: Option<&Path>,
    fs: &dyn effects::FileSystem,
) -> Result<()> {
    fs.create_dir_all(site_dir)?;
    fs.create_dir_all(site_packages)?;
    let pth_path = site_dir.join("px.pth");
    let pth_copy_path = site_packages.join("px.pth");
    let bin_dir = site_dir.join("bin");
    fs.create_dir_all(&bin_dir)?;
    let mut entries = Vec::new();
    for dep in &lock.resolved {
        let Some(artifact) = &dep.artifact else {
            continue;
        };
        if artifact.cached_path.is_empty() {
            continue;
        }
        let wheel_path = PathBuf::from(&artifact.cached_path);
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
