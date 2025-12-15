//! Permission hardening for materialized CAS roots.

use super::super::super::*;

impl ContentAddressableStore {
    pub(super) fn ensure_store_permissions(&self) {
        if self.health.permissions_checked.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Err(err) = self.harden_store_permissions() {
            warn!(
                root = %self.root.display(),
                %err,
                "failed to harden CAS store permissions; write protections may be incomplete"
            );
        }
    }

    fn harden_store_permissions(&self) -> Result<()> {
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
