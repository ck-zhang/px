mod pack;
mod runner;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use walkdir::WalkDir;

use crate::{InstallUserError, PX_VERSION};
use px_domain::{LockSnapshot, SandboxConfig, WorkspaceLock};

const DEFAULT_BASE: &str = "debian-12";
pub(crate) const SBX_VERSION: u32 = 1;

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
    pub(crate) image_digest: String,
    #[serde(default)]
    pub(crate) env_layer_digest: Option<String>,
    pub(crate) created_at: String,
    pub(crate) px_version: String,
    pub(crate) sbx_version: u32,
}

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

    fn image_manifest_path(&self, sbx_id: &str) -> PathBuf {
        self.images_dir().join(sbx_id).join("manifest.json")
    }

    pub(crate) fn oci_dir(&self, sbx_id: &str) -> PathBuf {
        self.images_dir().join(sbx_id).join("oci")
    }

    pub(crate) fn pack_dir(&self, sbx_id: &str) -> PathBuf {
        self.images_dir().join(sbx_id).join("pack")
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
                        "expected_base": definition.base_os_oid,
                        "found_base": manifest.base_os_oid,
                        "expected_profile": definition.profile_oid,
                        "found_profile": manifest.profile_oid,
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
        let created_at = OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();
        let manifest = SandboxImageManifest {
            sbx_id: definition.sbx_id(),
            base_os_oid: definition.base_os_oid.clone(),
            profile_oid: definition.profile_oid.clone(),
            capabilities: definition.capabilities.clone(),
            image_digest: format!("sha256:{}", definition.sbx_id()),
            env_layer_digest: None,
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

pub(crate) fn ensure_sandbox_image(
    store: &SandboxStore,
    config: &SandboxConfig,
    lock: Option<&LockSnapshot>,
    workspace_lock: Option<&WorkspaceLock>,
    profile_oid: &str,
    env_root: &Path,
    site_packages: Option<&Path>,
) -> Result<SandboxArtifacts, InstallUserError> {
    let resolution =
        resolve_sandbox_definition(config, lock, workspace_lock, profile_oid, site_packages)?;
    let env_root = validate_env_root(env_root)?;
    store
        .ensure_base_manifest(&resolution.base)
        .map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to prepare sandbox base",
                json!({
                    "base": resolution.base.name,
                    "error": err.to_string(),
                }),
            )
        })?;
    let manifest = store.ensure_image_manifest(&resolution.definition, &resolution.base)?;
    Ok(SandboxArtifacts {
        base: resolution.base,
        definition: resolution.definition,
        manifest,
        env_root,
    })
}

pub(crate) fn resolve_sandbox_definition(
    config: &SandboxConfig,
    lock: Option<&LockSnapshot>,
    workspace_lock: Option<&WorkspaceLock>,
    profile_oid: &str,
    site_packages: Option<&Path>,
) -> Result<SandboxResolution, InstallUserError> {
    let base = resolve_base(config.base.as_deref())?;
    let inferred = if config.auto {
        let mut inferred = BTreeSet::new();
        if let Some(lock) = lock {
            inferred.extend(infer_capabilities_from_lock(lock));
        }
        if let Some(workspace) = workspace_lock {
            inferred.extend(infer_capabilities_from_workspace(workspace));
        }
        if let Some(site) = site_packages {
            inferred.extend(infer_capabilities_from_site(site));
        }
        inferred
    } else {
        BTreeSet::new()
    };
    let capabilities = effective_capabilities(config, &inferred)?;
    validate_capabilities(&base, &capabilities)?;
    let definition = SandboxDefinition {
        base_os_oid: base.base_os_oid.clone(),
        capabilities,
        profile_oid: profile_oid.to_string(),
        sbx_version: SBX_VERSION,
    };
    Ok(SandboxResolution { base, definition })
}

fn resolve_base(name: Option<&str>) -> Result<SandboxBase, InstallUserError> {
    let target = name.unwrap_or(DEFAULT_BASE).trim();
    let Some(base) = known_bases()
        .iter()
        .find(|candidate| candidate.name == target)
        .cloned()
    else {
        return Err(sandbox_error(
            "PX900",
            "requested sandbox base is unavailable",
            json!({ "base": target }),
        ));
    };
    Ok(base)
}

fn known_bases() -> &'static [SandboxBase] {
    use std::sync::OnceLock;
    static BASES: OnceLock<Vec<SandboxBase>> = OnceLock::new();
    BASES.get_or_init(|| {
        let capabilities = known_capabilities();
        vec![
            SandboxBase {
                name: DEFAULT_BASE.to_string(),
                base_os_oid: base_os_identity(DEFAULT_BASE),
                supported_capabilities: capabilities.clone(),
            },
            SandboxBase {
                name: "alpine-3.20".to_string(),
                base_os_oid: base_os_identity("alpine-3.20"),
                supported_capabilities: capabilities,
            },
        ]
    })
}

fn base_os_identity(name: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!("base:{name}:v{SBX_VERSION}"));
    format!("{:x}", hasher.finalize())
}

fn known_capabilities() -> BTreeSet<String> {
    [
        "postgres",
        "mysql",
        "imagecodecs",
        "xml",
        "ldap",
        "ffi",
        "curl",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

fn capability_package_map() -> &'static [(&'static str, &'static [&'static str])] {
    &[
        ("psycopg2", &["postgres"]),
        ("psycopg2-binary", &["postgres"]),
        ("asyncpg", &["postgres"]),
        ("pg8000", &["postgres"]),
        ("mysqlclient", &["mysql"]),
        ("pymysql", &["mysql"]),
        ("aiomysql", &["mysql"]),
        ("pillow", &["imagecodecs"]),
        ("Pillow", &["imagecodecs"]),
        ("lxml", &["xml"]),
        ("pyldap", &["ldap"]),
        ("python-ldap", &["ldap"]),
        ("cffi", &["ffi"]),
        ("pycparser", &["ffi"]),
        ("httpx", &["curl"]),
        ("requests", &["curl"]),
    ]
}

fn library_capability_map() -> &'static [(&'static str, &'static str)] {
    &[
        ("libpq", "postgres"),
        ("libmysqlclient", "mysql"),
        ("libmysql", "mysql"),
        ("libjpeg", "imagecodecs"),
        ("libpng", "imagecodecs"),
        ("libz", "imagecodecs"),
        ("libxml2", "xml"),
        ("libldap", "ldap"),
        ("libffi", "ffi"),
        ("libcurl", "curl"),
        ("libssl", "curl"),
    ]
}

fn infer_capabilities_from_lock(lock: &LockSnapshot) -> BTreeSet<String> {
    let mut caps = BTreeSet::new();
    let map = capability_package_map();
    for spec in &lock.dependencies {
        let name = crate::dependency_name(spec);
        for (pkg, needed) in map {
            if name.eq_ignore_ascii_case(pkg) {
                caps.extend(needed.iter().map(|c| c.to_string()));
            }
        }
    }
    for dep in &lock.resolved {
        let name = dep.name.trim();
        for (pkg, needed) in map {
            if name.eq_ignore_ascii_case(pkg) {
                caps.extend(needed.iter().map(|c| c.to_string()));
            }
        }
    }
    caps
}

fn infer_capabilities_from_workspace(lock: &WorkspaceLock) -> BTreeSet<String> {
    let mut caps = BTreeSet::new();
    let map = capability_package_map();
    for member in &lock.members {
        for dep in &member.dependencies {
            let name = crate::dependency_name(dep);
            for (pkg, needed) in map {
                if name.eq_ignore_ascii_case(pkg) {
                    caps.extend(needed.iter().map(|c| c.to_string()));
                }
            }
        }
    }
    caps
}

fn infer_capabilities_from_site(site: &Path) -> BTreeSet<String> {
    let mut caps = BTreeSet::new();
    if !site.exists() {
        return caps;
    }
    for entry in WalkDir::new(site) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        for (pattern, capability) in library_capability_map() {
            if name.contains(pattern) {
                caps.insert(capability.to_string());
            }
        }
    }
    caps
}

fn effective_capabilities(
    config: &SandboxConfig,
    inferred: &BTreeSet<String>,
) -> Result<BTreeSet<String>, InstallUserError> {
    let known = known_capabilities();
    let mut capabilities = BTreeSet::new();
    for (cap, enabled) in &config.capabilities {
        if !known.contains(cap) {
            return Err(sandbox_error(
                "PX901",
                "sandbox capability is not recognized",
                json!({ "capability": cap }),
            ));
        }
        if *enabled {
            capabilities.insert(cap.to_string());
        }
    }
    if config.auto {
        for cap in inferred {
            if *config.capabilities.get(cap).unwrap_or(&true) {
                capabilities.insert(cap.clone());
            }
        }
    }
    Ok(capabilities)
}

pub(crate) fn env_root_from_site_packages(site: &Path) -> Option<PathBuf> {
    site.parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .map(PathBuf::from)
}

pub(crate) fn discover_site_packages(env_root: &Path) -> Option<PathBuf> {
    let lib_dir = env_root.join("lib");
    if let Ok(entries) = fs::read_dir(&lib_dir) {
        let mut candidates: Vec<PathBuf> = entries
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                if !path.is_dir() {
                    return None;
                }
                let name = path.file_name()?.to_str()?;
                if !name.starts_with("python") {
                    return None;
                }
                let site = path.join("site-packages");
                if site.exists() {
                    Some(site)
                } else {
                    None
                }
            })
            .collect();
        candidates.sort();
        if let Some(site) = candidates.into_iter().next() {
            return Some(site);
        }
    }
    let fallback = env_root.join("site-packages");
    if fallback.exists() {
        return Some(fallback);
    }
    None
}

fn validate_env_root(env_root: &Path) -> Result<PathBuf, InstallUserError> {
    if env_root.as_os_str().is_empty() {
        return Err(sandbox_error(
            "PX902",
            "sandbox requires an environment",
            json!({ "reason": "missing_env" }),
        ));
    }
    let path = env_root
        .canonicalize()
        .unwrap_or_else(|_| env_root.to_path_buf());
    if !path.exists() {
        return Err(sandbox_error(
            "PX902",
            "sandbox environment path is missing",
            json!({
                "reason": "missing_env",
                "env_root": env_root.display().to_string(),
            }),
        ));
    }
    Ok(path)
}

fn validate_capabilities(
    base: &SandboxBase,
    capabilities: &BTreeSet<String>,
) -> Result<(), InstallUserError> {
    for cap in capabilities {
        if !base.supported_capabilities.contains(cap) {
            return Err(sandbox_error(
                "PX901",
                "requested sandbox capability is not available for this base",
                json!({
                    "capability": cap,
                    "base": base.name,
                }),
            ));
        }
    }
    Ok(())
}

pub(crate) fn default_store_root() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("PX_SANDBOX_STORE") {
        return Ok(PathBuf::from(path));
    }
    dirs_next::home_dir()
        .map(|home| home.join(".px").join("sandbox"))
        .ok_or_else(|| anyhow!("unable to determine sandbox store location"))
}

pub(crate) fn sandbox_image_tag(sbx_id: &str) -> String {
    format!("px.sbx.local/{sbx_id}:latest")
}

pub(crate) fn sandbox_error(code: &str, message: &str, details: Value) -> InstallUserError {
    let mut merged = details;
    match merged {
        Value::Object(ref mut map) => {
            map.insert("code".into(), Value::String(code.to_string()));
        }
        _ => {
            merged = json!({
                "code": code,
                "details": merged,
            });
        }
    }
    InstallUserError::new(message, merged)
}

pub use pack::{pack_image, PackRequest};
pub(crate) use runner::{
    detect_container_backend, ensure_image_layout, run_container, ContainerBackend, ContainerRunArgs,
    Mount, RunMode, SandboxImageLayout,
};

#[cfg(test)]
mod tests {
    use super::*;
    use px_domain::LockSnapshot;
    use std::collections::BTreeSet;
    use std::fs;
    use tempfile::tempdir;

    fn lock_with_deps(dependencies: Vec<String>) -> LockSnapshot {
        LockSnapshot {
            version: 1,
            project_name: Some("demo".into()),
            python_requirement: Some(">=3.11".into()),
            manifest_fingerprint: Some("fp".into()),
            lock_id: Some("lid".into()),
            dependencies,
            mode: None,
            resolved: vec![],
            graph: None,
            workspace: None,
        }
    }

    #[test]
    fn sandbox_id_is_stable_when_capabilities_unsorted() {
        let mut caps_a = BTreeSet::new();
        caps_a.insert("postgres".to_string());
        caps_a.insert("imagecodecs".to_string());
        let def_a = SandboxDefinition {
            base_os_oid: "base".to_string(),
            capabilities: caps_a,
            profile_oid: "profile".to_string(),
            sbx_version: SBX_VERSION,
        };
        let mut caps_b = BTreeSet::new();
        caps_b.insert("imagecodecs".to_string());
        caps_b.insert("postgres".to_string());
        let def_b = SandboxDefinition {
            base_os_oid: "base".to_string(),
            capabilities: caps_b,
            profile_oid: "profile".to_string(),
            sbx_version: SBX_VERSION,
        };
        assert_eq!(def_a.sbx_id(), def_b.sbx_id());
    }

    #[test]
    fn ensure_image_writes_manifest_and_reuses_store() {
        let temp = tempdir().expect("tempdir");
        let mut config = SandboxConfig::default();
        config.auto = false;
        config.capabilities.insert("postgres".to_string(), true);
        let lock = lock_with_deps(vec![]);
        let store = SandboxStore::new(temp.path().to_path_buf());
        let env_root = temp.path().join("env");
        fs::create_dir_all(&env_root).expect("env root");
        let artifacts = ensure_sandbox_image(
            &store,
            &config,
            Some(&lock),
            None,
            "profile-1",
            &env_root,
            None,
        )
        .expect("sandbox artifacts");
        assert_eq!(artifacts.definition.sbx_id(), artifacts.manifest.sbx_id);
        let again = ensure_sandbox_image(
            &store,
            &config,
            Some(&lock),
            None,
            "profile-1",
            &env_root,
            None,
        )
        .expect("reuse sandbox image");
        assert_eq!(artifacts.manifest.sbx_id, again.manifest.sbx_id);
    }

    #[test]
    fn site_inference_detects_postgres_library() {
        let temp = tempdir().expect("tempdir");
        let site = temp.path().join("site");
        fs::create_dir_all(&site).expect("create site dir");
        fs::write(site.join("libpq.so.5"), b"").expect("write sentinel");
        let config = SandboxConfig::default();
        let lock = lock_with_deps(vec![]);
        let resolved =
            resolve_sandbox_definition(&config, Some(&lock), None, "profile", Some(site.as_path()))
                .expect("resolution");
        assert!(
            resolved.definition.capabilities.contains("postgres"),
            "libpq should infer postgres capability"
        );
    }

    #[test]
    fn explicit_false_overrides_inference() {
        let mut config = SandboxConfig::default();
        config.capabilities.insert("postgres".into(), false);
        let lock = lock_with_deps(vec!["psycopg2".into()]);
        let resolved = resolve_sandbox_definition(&config, Some(&lock), None, "profile", None)
            .expect("resolution");
        assert!(
            !resolved.definition.capabilities.contains("postgres"),
            "explicit false should disable inferred capability"
        );
    }
}
