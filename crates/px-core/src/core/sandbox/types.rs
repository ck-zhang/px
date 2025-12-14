use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::core::system_deps::SystemDeps;

pub(crate) const SBX_VERSION: u32 = 5;
pub(crate) const PXAPP_VERSION: u32 = 1;

#[derive(Clone, Debug)]
pub(crate) struct SandboxBase {
    pub(crate) name: String,
    pub(crate) base_os_oid: String,
    pub(crate) supported_capabilities: BTreeSet<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct SandboxDefinition {
    pub(crate) base_os_oid: String,
    pub(crate) capabilities: BTreeSet<String>,
    #[serde(default)]
    pub(crate) system_deps: SystemDeps,
    pub(crate) profile_oid: String,
    pub(crate) sbx_version: u32,
}

impl SandboxDefinition {
    #[must_use]
    pub(crate) fn sbx_id(&self) -> String {
        let mut map = BTreeMap::new();
        map.insert(
            "base_os_oid".to_string(),
            serde_json::Value::String(self.base_os_oid.clone()),
        );
        map.insert(
            "capabilities".to_string(),
            serde_json::Value::Array(
                self.capabilities
                    .iter()
                    .cloned()
                    .map(serde_json::Value::String)
                    .collect(),
            ),
        );
        map.insert(
            "profile_oid".to_string(),
            serde_json::Value::String(self.profile_oid.clone()),
        );
        map.insert(
            "sbx_version".to_string(),
            serde_json::Value::Number(self.sbx_version.into()),
        );
        if let Some(fingerprint) = self.system_deps.fingerprint() {
            map.insert(
                "system_deps".to_string(),
                serde_json::Value::String(fingerprint),
            );
        }
        let wrapper = json!({ "sandbox": map });
        let encoded = serde_json::to_vec(&wrapper).unwrap_or_default();
        let mut hasher = Sha256::new();
        hasher.update(encoded);
        format!("{:x}", hasher.finalize())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct SandboxImageManifest {
    pub(crate) sbx_id: String,
    pub(crate) base_os_oid: String,
    pub(crate) profile_oid: String,
    pub(crate) capabilities: BTreeSet<String>,
    #[serde(default)]
    pub(crate) system_deps: SystemDeps,
    pub(crate) image_digest: String,
    #[serde(default)]
    pub(crate) base_layer_digest: Option<String>,
    #[serde(default)]
    pub(crate) env_layer_digest: Option<String>,
    #[serde(default)]
    pub(crate) system_layer_digest: Option<String>,
    pub(crate) created_at: String,
    pub(crate) px_version: String,
    pub(crate) sbx_version: u32,
}

#[derive(Clone, Debug)]
pub(crate) struct SandboxResolution {
    pub(crate) base: SandboxBase,
    pub(crate) definition: SandboxDefinition,
}

#[derive(Clone, Debug)]
pub(crate) struct SandboxArtifacts {
    pub(crate) base: SandboxBase,
    pub(crate) definition: SandboxDefinition,
    pub(crate) manifest: SandboxImageManifest,
    pub(crate) env_root: PathBuf,
}
