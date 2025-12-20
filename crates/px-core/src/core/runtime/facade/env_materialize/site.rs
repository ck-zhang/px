// Site layout helpers (site-packages path, python selection).
use super::*;

pub(crate) fn site_packages_dir(site_dir: &Path, runtime_version: &str) -> PathBuf {
    if let Some((major, minor)) = parse_python_version(runtime_version) {
        site_dir
            .join("lib")
            .join(format!("python{major}.{minor}"))
            .join("site-packages")
    } else {
        site_dir.join("site-packages")
    }
}

#[cfg(not(windows))]
pub(super) fn install_python_link(source: &Path, dest: &Path) -> Result<()> {
    if dest.symlink_metadata().is_ok() {
        let _ = fs::remove_file(dest);
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        if symlink(source, dest).is_ok() {
            return Ok(());
        }
    }
    fs::copy(source, dest).with_context(|| {
        format!(
            "failed to link python from {} to {}",
            source.display(),
            dest.display()
        )
    })?;
    set_exec_permissions(dest);
    Ok(())
}

pub(crate) fn select_python_from_site(
    site_bin: &Option<PathBuf>,
    runtime_path: &str,
    runtime_version: &str,
) -> String {
    if let Some(bin) = site_bin {
        let mut candidates = vec![bin.join("python"), bin.join("python3")];
        if let Some((major, minor)) = parse_python_version(runtime_version) {
            candidates.push(bin.join(format!("python{major}")));
            candidates.push(bin.join(format!("python{major}.{minor}")));
        }
        if let Some(found) = candidates.into_iter().find(|path| path.exists()) {
            return found.display().to_string();
        }
    }
    runtime_path.to_string()
}
