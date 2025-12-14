use std::env;
use std::path::{Path, PathBuf};

pub(super) fn invocation_workdir(project_root: &Path) -> PathBuf {
    map_workdir(Some(project_root), project_root)
}

pub(super) fn map_workdir(invocation_root: Option<&Path>, context_root: &Path) -> PathBuf {
    let cwd = env::current_dir().unwrap_or_else(|_| context_root.to_path_buf());
    if let Some(root) = invocation_root {
        if let Ok(rel) = cwd.strip_prefix(root) {
            return context_root.join(rel);
        }
    }
    if cwd.starts_with(context_root) {
        cwd
    } else {
        context_root.to_path_buf()
    }
}
