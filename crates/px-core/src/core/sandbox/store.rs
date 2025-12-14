use std::{fs, path::PathBuf};

use anyhow::Result;
use serde_json::json;

use super::errors::sandbox_error;
use super::time::sandbox_timestamp_string;
use super::types::{SandboxBase, SandboxDefinition, SandboxImageManifest, SBX_VERSION};
use crate::{InstallUserError, PX_VERSION};

#[derive(Clone, Debug)]
pub(crate) struct SandboxStore {
    root: PathBuf,
}

impl SandboxStore {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn bases_dir(&self) -> PathBuf {
        self.root.join("bases")
    }

    fn images_dir(&self) -> PathBuf {
        self.root.join("images")
    }

    fn base_manifest_path(&self, base: &SandboxBase) -> PathBuf {
        self.bases_dir()
            .join(&base.base_os_oid)
            .join("manifest.json")
    }

    pub(super) fn image_manifest_path(&self, sbx_id: &str) -> PathBuf {
        self.images_dir().join(sbx_id).join("manifest.json")
    }

    pub(crate) fn oci_dir(&self, sbx_id: &str) -> PathBuf {
        self.images_dir().join(sbx_id).join("oci")
    }

    pub(crate) fn pack_dir(&self, sbx_id: &str) -> PathBuf {
        self.images_dir().join(sbx_id).join("pack")
    }

    pub(crate) fn bundle_dir(&self, sbx_id: &str, bundle_id: &str) -> PathBuf {
        self.images_dir()
            .join(sbx_id)
            .join("bundles")
            .join(bundle_id)
    }

    pub(crate) fn ensure_base_manifest(&self, base: &SandboxBase) -> Result<()> {
        let manifest_path = self.base_manifest_path(base);
        if manifest_path.exists() {
            return Ok(());
        }
        let payload = json!({
            "name": base.name,
            "base_os_oid": base.base_os_oid,
            "capabilities": base.supported_capabilities,
            "sbx_version": SBX_VERSION,
        });
        if let Some(parent) = manifest_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&manifest_path, serde_json::to_vec_pretty(&payload)?)?;
        Ok(())
    }

    pub(crate) fn ensure_image_manifest(
        &self,
        definition: &SandboxDefinition,
        _base: &SandboxBase,
    ) -> Result<SandboxImageManifest, InstallUserError> {
        let manifest_path = self.image_manifest_path(&definition.sbx_id());
        if manifest_path.exists() {
            let contents = fs::read_to_string(&manifest_path).map_err(|err| {
                sandbox_error(
                    "PX903",
                    "failed to read sandbox image metadata",
                    json!({
                        "path": manifest_path.display().to_string(),
                        "error": err.to_string(),
                    }),
                )
            })?;
            let manifest: SandboxImageManifest =
                serde_json::from_str(&contents).map_err(|err| {
                    sandbox_error(
                        "PX904",
                        "sandbox image metadata is incompatible with this px version",
                        json!({
                            "path": manifest_path.display().to_string(),
                            "error": err.to_string(),
                        }),
                    )
                })?;
            if manifest.sbx_version != SBX_VERSION {
                return Err(sandbox_error(
                    "PX904",
                    "sandbox image metadata is incompatible with this px version",
                    json!({
                        "expected": SBX_VERSION,
                        "found": manifest.sbx_version,
                        "path": manifest_path.display().to_string(),
                    }),
                ));
            }
            if manifest.base_os_oid != definition.base_os_oid
                || manifest.profile_oid != definition.profile_oid
            {
                return Err(sandbox_error(
                    "PX904",
                    "sandbox image metadata does not match the requested sandbox definition",
                    json!({
                        "expected_base_os_oid": definition.base_os_oid,
                        "found_base_os_oid": manifest.base_os_oid,
                        "expected_profile_oid": definition.profile_oid,
                        "found_profile_oid": manifest.profile_oid,
                        "path": manifest_path.display().to_string(),
                    }),
                ));
            }
            if manifest.capabilities != definition.capabilities {
                return Err(sandbox_error(
                    "PX904",
                    "sandbox image metadata does not match the requested sandbox definition",
                    json!({
                        "expected_capabilities": definition.capabilities,
                        "found_capabilities": manifest.capabilities,
                        "path": manifest_path.display().to_string(),
                    }),
                ));
            }
            if manifest.system_deps != definition.system_deps {
                return Err(sandbox_error(
                    "PX904",
                    "sandbox image metadata does not match the requested sandbox definition",
                    json!({
                        "expected_system_deps": definition.system_deps,
                        "found_system_deps": manifest.system_deps,
                        "path": manifest_path.display().to_string(),
                    }),
                ));
            }
            if manifest.system_layer_digest.is_some()
                && manifest.system_deps.apt_packages.is_empty()
            {
                return Err(sandbox_error(
                    "PX904",
                    "sandbox image metadata includes unexpected system layer",
                    json!({
                        "path": manifest_path.display().to_string(),
                    }),
                ));
            }
            return Ok(manifest);
        }

        if let Some(parent) = manifest_path.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                sandbox_error(
                    "PX903",
                    "failed to create sandbox image directory",
                    json!({
                        "path": parent.display().to_string(),
                        "error": err.to_string(),
                    }),
                )
            })?;
        }
        let created_at = sandbox_timestamp_string();
        let manifest = SandboxImageManifest {
            sbx_id: definition.sbx_id(),
            base_os_oid: definition.base_os_oid.clone(),
            profile_oid: definition.profile_oid.clone(),
            capabilities: definition.capabilities.clone(),
            system_deps: definition.system_deps.clone(),
            image_digest: format!("sha256:{}", definition.sbx_id()),
            base_layer_digest: None,
            env_layer_digest: None,
            system_layer_digest: None,
            created_at,
            px_version: PX_VERSION.to_string(),
            sbx_version: SBX_VERSION,
        };
        let encoded = serde_json::to_vec_pretty(&manifest).map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to encode sandbox image metadata",
                json!({ "error": err.to_string() }),
            )
        })?;
        fs::write(&manifest_path, encoded).map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to write sandbox image metadata",
                json!({
                    "path": manifest_path.display().to_string(),
                    "error": err.to_string(),
                }),
            )
        })?;
        Ok(manifest)
    }
}
