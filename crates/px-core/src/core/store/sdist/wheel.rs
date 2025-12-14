use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};

pub(super) fn find_wheel(dist_dir: &Path) -> Result<PathBuf> {
    let mut found = None;
    for entry in fs::read_dir(dist_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_file()
            && entry
                .path()
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("whl"))
        {
            found = Some(entry.path());
            break;
        }
    }
    found.ok_or_else(|| anyhow!("wheel not found in {}", dist_dir.display()))
}
