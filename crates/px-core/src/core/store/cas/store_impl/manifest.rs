use super::super::*;

impl ContentAddressableStore {
    fn runtime_manifest_path(&self, oid: &str) -> PathBuf {
        self.root
            .join(MATERIALIZED_RUNTIMES_DIR)
            .join(oid)
            .join("manifest.json")
    }

    pub(crate) fn write_runtime_manifest(&self, oid: &str, header: &RuntimeHeader) -> Result<()> {
        let path = self.runtime_manifest_path(oid);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let manifest = json!({
            "runtime_oid": oid,
            "version": header.version,
            "platform": header.platform,
            "owner_id": format!("runtime:{}:{}", header.version, header.platform),
        });
        fs::write(&path, serde_json::to_string_pretty(&manifest)?)
            .with_context(|| format!("failed to write runtime manifest {}", path.display()))?;
        Ok(())
    }

    /// Remove a materialized environment projection for the given profile oid.
    pub(crate) fn remove_env_materialization(&self, profile_oid: &str) -> Result<()> {
        let env_root = self.envs_root.join(profile_oid);
        for path in [
            env_root.clone(),
            env_root.with_extension("partial"),
            env_root.with_extension("backup"),
        ] {
            if path.exists() {
                fs::remove_dir_all(&path).with_context(|| {
                    format!("failed to remove env materialization {}", path.display())
                })?;
            }
        }
        Ok(())
    }
}
