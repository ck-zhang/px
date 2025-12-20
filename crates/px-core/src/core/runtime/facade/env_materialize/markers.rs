// Environment marker shim generation for materialized Python environments.
use super::*;

pub(crate) fn write_python_environment_markers(
    site_dir: &Path,
    runtime: &RuntimeMetadata,
    runtime_path: &Path,
    fs: &dyn effects::FileSystem,
) -> Result<PathBuf> {
    let bin_dir = site_dir.join("bin");
    fs.create_dir_all(&bin_dir)?;

    let canonical_runtime = fs
        .canonicalize(runtime_path)
        .unwrap_or_else(|_| runtime_path.to_path_buf());

    let home = crate::core::fs::python_install_root(&canonical_runtime).unwrap_or_else(|| {
        canonical_runtime
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .to_path_buf()
    });
    let pyvenv_cfg = format!(
        "home = {}\ninclude-system-site-packages = false\nversion = {}\n",
        home.display(),
        runtime.version
    );
    fs.write(&site_dir.join("pyvenv.cfg"), pyvenv_cfg.as_bytes())?;

    #[cfg(windows)]
    {
        // Windows needs a native launcher for `bin/python`; prefer using the real runtime
        // executable path and rely on px to set environment variables when spawning.
        return Ok(canonical_runtime);
    }

    #[cfg(not(windows))]
    {
        let site_packages = site_packages_dir(site_dir, &runtime.version);
        let manifest_env_vars = {
            let mut env_vars = BTreeMap::new();
            let manifest_path = site_dir.join("manifest.json");
            if let Ok(contents) = fs.read_to_string(&manifest_path) {
                if let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&contents) {
                    if let Some(vars) = manifest.get("env_vars").and_then(|v| v.as_object()) {
                        env_vars = vars.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                    }
                }
            }
            env_vars
        };

        write_python_shim(
            &bin_dir,
            &canonical_runtime,
            &site_packages,
            &manifest_env_vars,
        )?;

        let primary = bin_dir.join("python");
        let mut names = vec!["python3".to_string()];
        if let Some((major, minor)) = parse_python_version(&runtime.version) {
            names.push(format!("python{major}"));
            names.push(format!("python{major}.{minor}"));
        }
        for name in names {
            let dest = bin_dir.join(&name);
            install_python_link(&primary, &dest)?;
        }
        Ok(primary)
    }
}
