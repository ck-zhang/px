//! Permission hardening for materialized CAS roots.

use super::super::super::*;

const PERMISSIONS_MARKER: &str = ".px-store-permissions";

impl ContentAddressableStore {
    pub(super) fn ensure_store_permissions(&self) {
        if self.health.permissions_checked.swap(true, Ordering::SeqCst) {
            return;
        }
        if self.root_is_default && self.permissions_marker_matches_px_version() {
            return;
        }
        if let Err(err) = self.harden_store_permissions() {
            warn!(
                root = %self.root.display(),
                %err,
                "failed to harden CAS store permissions; write protections may be incomplete"
            );
        } else {
            let _ = self.write_permissions_marker();
        }
    }

    fn permissions_marker_path(&self) -> PathBuf {
        self.root.join(PERMISSIONS_MARKER)
    }

    fn permissions_marker_matches_px_version(&self) -> bool {
        let path = self.permissions_marker_path();
        fs::read_to_string(path)
            .ok()
            .is_some_and(|contents| contents.trim() == PX_VERSION)
    }

    fn write_permissions_marker(&self) -> Result<()> {
        let path = self.permissions_marker_path();
        fs::write(path, format!("{PX_VERSION}\n"))?;
        Ok(())
    }

    fn harden_store_permissions(&self) -> Result<()> {
        let _timing =
            crate::tooling::timings::TimingGuard::new("harden_store_permissions");
        let objects_root = self.root.join(OBJECTS_DIR);
        if objects_root.exists() {
            for entry in walkdir::WalkDir::new(&objects_root)
                .min_depth(2)
                .max_depth(2)
            {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(err) => {
                        warn!(%err, "failed to walk CAS objects during permission hardening");
                        continue;
                    }
                };
                if !entry.file_type().is_file() {
                    continue;
                }
                if let Err(err) = make_read_only_recursive(entry.path()) {
                    warn!(
                        path = %entry.path().display(),
                        %err,
                        "failed to harden CAS object permissions"
                    );
                }
            }
        }

        for dir in [
            MATERIALIZED_PKG_BUILDS_DIR,
            MATERIALIZED_RUNTIMES_DIR,
            MATERIALIZED_REPO_SNAPSHOTS_DIR,
        ] {
            let root = self.root.join(dir);
            if !root.exists() {
                continue;
            }
            for entry in fs::read_dir(&root)? {
                let Ok(entry) = entry else { continue };
                if let Err(err) = make_read_only_recursive(&entry.path()) {
                    warn!(
                        path = %entry.path().display(),
                        %err,
                        "failed to harden materialized CAS directory permissions"
                    );
                }
            }
        }
        Ok(())
    }
}
