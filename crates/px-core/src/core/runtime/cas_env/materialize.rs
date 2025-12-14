use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Context, Result};
use flate2::read::GzDecoder;
use serde_json::json;
use tar::Archive;

use crate::core::runtime::facade::{site_packages_dir, RuntimeMetadata, SITE_CUSTOMIZE};
use crate::store::cas::{
    global_store, make_read_only_recursive, LoadedObject, ProfileHeader, RuntimeHeader,
    MATERIALIZED_PKG_BUILDS_DIR, MATERIALIZED_RUNTIMES_DIR,
};
use crate::ManifestSnapshot;

use super::fs_tree::make_writable_recursive;
use super::scripts::{rewrite_python_entrypoint, should_rewrite_python_entrypoint};
use super::{default_envs_root, write_python_shim};

pub(super) fn materialize_profile_env(
    _snapshot: &ManifestSnapshot,
    runtime: &RuntimeMetadata,
    manifest: &ProfileHeader,
    profile_oid: &str,
    runtime_exe: &Path,
) -> Result<PathBuf> {
    let envs_root = default_envs_root()?;
    fs::create_dir_all(&envs_root)?;
    let env_root = envs_root.join(profile_oid);

    let store = global_store();
    let _lock = store.acquire_lock(profile_oid)?;

    let temp_root = env_root.with_extension("partial");
    if temp_root.exists() {
        let _ = fs::remove_dir_all(&temp_root);
    }
    fs::create_dir_all(&temp_root)?;
    let site_packages = site_packages_dir(&temp_root, &runtime.version);
    fs::create_dir_all(&site_packages)?;
    let bin_dir = temp_root.join("bin");
    fs::create_dir_all(&bin_dir)?;

    let mut site_entries: HashMap<String, PathBuf> = HashMap::new();
    for pkg in &manifest.packages {
        let loaded = store.load(&pkg.pkg_build_oid)?;
        let LoadedObject::PkgBuild { archive, .. } = loaded else {
            return Err(anyhow!(
                "CAS object {} is not a pkg-build archive",
                pkg.pkg_build_oid
            ));
        };
        let materialized = materialize_pkg_archive(&pkg.pkg_build_oid, &archive)?;
        let pkg_site = materialized.join("site-packages");
        if pkg_site.exists() {
            site_entries.insert(pkg.pkg_build_oid.clone(), pkg_site);
        } else {
            site_entries.insert(pkg.pkg_build_oid.clone(), materialized.clone());
        }
        let pkg_bin = materialized.join("bin");
        if pkg_bin.exists() {
            for entry in fs::read_dir(&pkg_bin)? {
                let entry = entry?;
                if entry.file_type()?.is_file() {
                    let src = entry.path();
                    let dest = bin_dir.join(entry.file_name());
                    let env_python = env_root.join("bin").join("python");
                    link_bin_entry(&src, &dest, Some(&env_python))?;
                }
            }
        }
    }

    let sys_path_order = if manifest.sys_path_order.is_empty() {
        manifest
            .packages
            .iter()
            .map(|pkg| pkg.pkg_build_oid.clone())
            .collect()
    } else {
        manifest.sys_path_order.clone()
    };
    let mut seen = HashSet::new();
    let resolved_sys_path: Vec<String> = sys_path_order
        .into_iter()
        .filter(|oid| seen.insert(oid.clone()))
        .collect();
    let mut pth_body = String::new();
    for oid in &resolved_sys_path {
        if let Some(entry) = site_entries.get(oid) {
            pth_body.push_str(&entry.display().to_string());
            pth_body.push('\n');
        }
    }
    for (oid, entry) in site_entries {
        if seen.insert(oid) {
            pth_body.push_str(&entry.display().to_string());
            pth_body.push('\n');
        }
    }
    fs::write(site_packages.join("px.pth"), pth_body)?;
    write_sitecustomize(&temp_root, Some(&site_packages))?;

    let manifest_path = temp_root.join("manifest.json");
    fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&json!({
            "profile_oid": profile_oid,
            "runtime_oid": manifest.runtime_oid,
            "packages": manifest.packages,
            "sys_path_order": resolved_sys_path,
            "env_vars": manifest.env_vars,
        }))?,
    )?;
    let backup_root = env_root.with_extension("backup");
    if backup_root.exists() {
        let _ = fs::remove_dir_all(&backup_root);
    }
    if env_root.exists() {
        fs::rename(&env_root, &backup_root).with_context(|| {
            format!(
                "failed to move existing environment {} to backup",
                env_root.display()
            )
        })?;
    }
    if let Err(err) = fs::rename(&temp_root, &env_root) {
        let _ = fs::remove_dir_all(&temp_root);
        if backup_root.exists() {
            let _ = fs::rename(&backup_root, &env_root);
        }
        return Err(err).with_context(|| {
            format!(
                "failed to finalize environment materialization at {}",
                env_root.display()
            )
        });
    }
    let _ = fs::remove_dir_all(&backup_root);

    let final_site = site_packages_dir(&env_root, &runtime.version);
    write_python_shim(
        &env_root.join("bin"),
        runtime_exe,
        &final_site,
        &manifest.env_vars,
    )?;
    install_python_links(&env_root.join("bin"), runtime_exe)?;
    Ok(env_root)
}

fn link_bin_entry(src: &Path, dest: &Path, env_python: Option<&Path>) -> Result<()> {
    if let Some(python) = env_python {
        if should_rewrite_python_entrypoint(src)? {
            rewrite_python_entrypoint(src, dest, python)?;
            return Ok(());
        }
    }

    if dest.exists() {
        let _ = fs::remove_file(dest);
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        if let Err(sym_err) = symlink(src, dest) {
            fs::hard_link(src, dest).with_context(|| {
                format!(
                    "failed to link CAS bin entry {} -> {} (symlink error: {})",
                    dest.display(),
                    src.display(),
                    sym_err
                )
            })?;
        }
    }

    #[cfg(not(unix))]
    {
        if let Err(hard_err) = fs::hard_link(src, dest) {
            #[cfg(windows)]
            {
                use std::os::windows::fs::symlink_file;
                symlink_file(src, dest).with_context(|| {
                    format!(
                        "failed to link CAS bin entry {} -> {} (hard link error: {})",
                        dest.display(),
                        src.display(),
                        hard_err
                    )
                })?;
            }
            #[cfg(not(windows))]
            {
                return Err(hard_err).with_context(|| {
                    format!(
                        "failed to link CAS bin entry {} -> {}",
                        dest.display(),
                        src.display()
                    )
                });
            }
        }
    }
    Ok(())
}

pub(crate) fn materialize_pkg_archive(oid: &str, archive: &[u8]) -> Result<PathBuf> {
    let store = global_store();
    let root = store.root().join(MATERIALIZED_PKG_BUILDS_DIR).join(oid);
    if root.exists() {
        return Ok(root);
    }
    let _lock = store.acquire_lock(oid)?;
    if root.exists() {
        return Ok(root);
    }
    if let Some(parent) = root.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = root.with_extension("partial");
    if tmp.exists() {
        let _ = fs::remove_dir_all(&tmp);
    }
    fs::create_dir_all(&tmp)?;
    let decoder = GzDecoder::new(archive);
    let mut tar = Archive::new(decoder);
    tar.unpack(&tmp)?;
    fs::rename(&tmp, &root)?;
    make_read_only_recursive(&root)?;
    Ok(root)
}

pub(super) fn materialize_runtime_archive(
    oid: &str,
    header: &RuntimeHeader,
    archive: &[u8],
) -> Result<PathBuf> {
    let exe_rel = Path::new(&header.exe_path);
    if exe_rel.is_absolute() {
        return Err(anyhow!(
            "runtime executable path must be relative (got {})",
            header.exe_path
        ));
    }
    let store = global_store();
    let root = store.root().join(MATERIALIZED_RUNTIMES_DIR).join(oid);
    let _lock = store.acquire_lock(oid)?;
    let exe_path = root.join(exe_rel);
    if root.exists() && exe_path.exists() {
        let _ = store.write_runtime_manifest(oid, header);
        return Ok(exe_path);
    }
    if root.exists() {
        let _ = fs::remove_dir_all(&root);
    }
    if let Some(parent) = root.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = root.with_extension("partial");
    if tmp.exists() {
        let _ = fs::remove_dir_all(&tmp);
    }
    fs::create_dir_all(&tmp)?;
    let decoder = GzDecoder::new(archive);
    let mut tar = Archive::new(decoder);
    tar.unpack(&tmp)?;
    if let Err(err) = fs::rename(&tmp, &root) {
        make_writable_recursive(&root);
        let _ = fs::remove_dir_all(&root);
        fs::rename(&tmp, &root).map_err(|retry| anyhow!(retry).context(err))?;
    }
    let exe_path = root.join(exe_rel);
    if !exe_path.exists() {
        return Err(anyhow!(
            "runtime executable missing after materialization: {}",
            exe_path.display()
        ));
    }
    store.write_runtime_manifest(oid, header)?;
    make_read_only_recursive(&root)?;
    Ok(exe_path)
}

fn write_sitecustomize(env_root: &Path, site_packages: Option<&Path>) -> Result<()> {
    let path = env_root.join("sitecustomize.py");
    fs::write(&path, SITE_CUSTOMIZE.as_bytes())?;
    if let Some(extra) = site_packages {
        fs::create_dir_all(extra)?;
        fs::write(extra.join("sitecustomize.py"), SITE_CUSTOMIZE.as_bytes())?;
    }
    Ok(())
}

fn install_python_links(bin_dir: &Path, runtime: &Path) -> Result<()> {
    let python_path = PathBuf::from(runtime);
    for name in ["python", "python3"] {
        let dest = bin_dir.join(name);
        if dest.exists() {
            continue;
        }
        let _ = fs::remove_file(&dest);
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink(&python_path, &dest).or_else(|_| fs::copy(&python_path, &dest).map(|_| ()))?;
        }
        #[cfg(not(unix))]
        {
            fs::copy(&python_path, &dest).map(|_| ())?;
        }
    }
    Ok(())
}
