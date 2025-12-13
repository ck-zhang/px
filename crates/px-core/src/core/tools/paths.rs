use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use dirs_next::home_dir;

pub(crate) const TOOLS_DIR_ENV: &str = "PX_TOOLS_DIR";
pub(crate) const TOOL_STORE_ENV: &str = "PX_TOOL_STORE";

pub(crate) fn tools_root() -> Result<PathBuf> {
    if let Some(dir) = env::var_os(TOOLS_DIR_ENV) {
        let path = PathBuf::from(dir);
        fs::create_dir_all(&path)?;
        return Ok(path);
    }
    let home = home_dir().ok_or_else(|| anyhow!("home directory not found"))?;
    let path = home.join(".px").join("tools");
    fs::create_dir_all(&path)?;
    Ok(path)
}

pub(crate) fn tool_root_dir(name: &str) -> Result<PathBuf> {
    Ok(tools_root()?.join(name))
}

pub(crate) fn tools_env_store_root() -> Result<PathBuf> {
    let base = if let Some(dir) = env::var_os(TOOL_STORE_ENV) {
        PathBuf::from(dir)
    } else {
        tools_root()?.join("store")
    };
    fs::create_dir_all(&base)?;
    let envs = base.join("envs");
    fs::create_dir_all(&envs)?;
    Ok(envs)
}

pub(crate) fn copy_dir_contents(src: &Path, dst: &Path) -> Result<()> {
    if !src.exists() {
        return Ok(());
    }
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let target = dst.join(entry.file_name());
        if path.is_dir() {
            copy_dir_contents(&path, &target)?;
        } else {
            fs::copy(&path, &target)
                .with_context(|| format!("copying {} to {}", path.display(), target.display()))?;
        }
    }
    Ok(())
}

pub(crate) fn find_env_site(root: &Path) -> Option<PathBuf> {
    let envs_root = root.join(".px").join("envs");
    let entries = fs::read_dir(envs_root).ok()?;
    for entry in entries.flatten() {
        let site = entry.path().join("site");
        if site.exists() {
            return Some(site);
        }
    }
    None
}

pub(crate) fn normalize_tool_name(raw: &str) -> String {
    raw.chars()
        .filter(|ch| ch.is_alphanumeric() || *ch == '_' || *ch == '-')
        .collect::<String>()
        .to_lowercase()
}
