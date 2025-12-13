mod app_bundle;
mod pack;
mod pxapp;
mod runner;

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tempfile::tempdir;
use time::OffsetDateTime;
use walkdir::WalkDir;

use crate::core::system_deps::{
    capability_apt_map, package_capability_rules, read_sys_deps_metadata, resolve_system_deps,
    SystemDeps,
};
use crate::{InstallUserError, PX_VERSION};
use px_domain::{LockSnapshot, SandboxConfig, WorkspaceLock};

const DEFAULT_BASE: &str = "debian-12";
pub(crate) const SBX_VERSION: u32 = 5;
pub(crate) const PXAPP_VERSION: u32 = 1;
pub(crate) const SYSTEM_DEPS_IMAGE: &str =
    "docker.io/library/debian:12-slim@sha256:ef5c368548841bdd8199a8606f6307402f7f2a2f8edc4acbc9c1c70c340bc023";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SystemDepsMode {
    Strict,
    Offline,
}

pub(crate) fn system_deps_mode() -> SystemDepsMode {
    if let Ok(raw) = env::var("PX_SYSTEM_DEPS_MODE") {
        let value = raw.trim().to_ascii_lowercase();
        if matches!(value.as_str(), "offline" | "skip" | "disabled") {
            return SystemDepsMode::Offline;
        }
    }
    if let Ok(raw) = env::var("PX_SYSTEM_DEPS_OFFLINE") {
        let value = raw.trim().to_ascii_lowercase();
        if matches!(value.as_str(), "1" | "true" | "yes") {
            return SystemDepsMode::Offline;
        }
    }
    SystemDepsMode::Strict
}

pub(crate) fn internal_keep_proxies() -> bool {
    if should_disable_apt_proxy() {
        return false;
    }
    const PROXY_KEYS: &[&str] = &[
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "NO_PROXY",
        "no_proxy",
    ];
    PROXY_KEYS.iter().any(|key| {
        env::var(key)
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
    })
}

pub(crate) fn internal_apt_mirror_env_overrides() -> Vec<String> {
    fn normalize_url(raw: &str) -> Option<String> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }
        if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            return Some(trimmed.trim_end_matches('/').to_string());
        }
        None
    }

    let Some(raw_mirror) = env::var("PX_APT_MIRROR").ok() else {
        return Vec::new();
    };
    let raw_mirror = raw_mirror.trim();
    if raw_mirror.is_empty() {
        return Vec::new();
    }

    let mut security_mirror = env::var("PX_APT_SECURITY_MIRROR").ok().and_then(|raw| {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            None
        } else {
            normalize_url(trimmed)
        }
    });

    let debian_mirror =
        if raw_mirror.eq_ignore_ascii_case("tsinghua") || raw_mirror.eq_ignore_ascii_case("tuna") {
            if security_mirror.is_none() {
                security_mirror =
                    Some("http://mirrors.tuna.tsinghua.edu.cn/debian-security".to_string());
            }
            Some("http://mirrors.tuna.tsinghua.edu.cn/debian".to_string())
        } else {
            normalize_url(raw_mirror)
        };
    if security_mirror.is_none() {
        if let Some(mirror) = debian_mirror.as_deref() {
            if mirror.ends_with("/debian") {
                security_mirror = Some(format!("{}-security", mirror));
            }
        }
    }

    let Some(debian_mirror) = debian_mirror else {
        return Vec::new();
    };

    let mut envs = vec![format!("PX_APT_MIRROR={debian_mirror}")];
    if let Some(security) = security_mirror {
        envs.push(format!("PX_APT_SECURITY_MIRROR={security}"));
    }
    envs
}

pub(crate) fn internal_apt_mirror_setup_snippet() -> &'static str {
    r#"
if [ -n "${PX_APT_MIRROR:-}" ]; then
  mirror="$PX_APT_MIRROR"
  security="${PX_APT_SECURITY_MIRROR:-}"
  codename=""
  if [ -f /etc/os-release ]; then
    codename="$(. /etc/os-release && echo "${VERSION_CODENAME:-}")"
  fi
  if [ -z "$codename" ]; then
    codename="bookworm"
  fi
  if [ -d /etc/apt/sources.list.d ]; then
    rm -f /etc/apt/sources.list.d/*.list /etc/apt/sources.list.d/*.sources
  fi
  printf "deb %s %s main contrib non-free non-free-firmware\n" "$mirror" "$codename" > /etc/apt/sources.list
  printf "deb %s %s-updates main contrib non-free non-free-firmware\n" "$mirror" "$codename" >> /etc/apt/sources.list
  if [ -n "$security" ]; then
    printf "deb %s %s-security main contrib non-free non-free-firmware\n" "$security" "$codename" >> /etc/apt/sources.list
  fi
fi
"#
}

pub(crate) fn internal_proxy_env_overrides(_backend: &ContainerBackend) -> Vec<String> {
    const PROXY_KEYS: &[&str] = &[
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "NO_PROXY",
        "no_proxy",
    ];
    if !internal_keep_proxies() {
        return PROXY_KEYS
            .iter()
            .map(|key| format!("{key}="))
            .collect::<Vec<_>>();
    }
    let mut envs = Vec::new();
    for key in PROXY_KEYS {
        if let Ok(value) = env::var(key) {
            if value.trim().is_empty() {
                continue;
            }
            envs.push(format!("{key}={value}"));
        }
    }
    envs
}

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
    let mut resolution =
        resolve_sandbox_definition(config, lock, workspace_lock, profile_oid, site_packages)?;
    pin_missing_apt_versions(&mut resolution.definition)?;
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

fn system_deps_container_args(
    workdir: &Path,
    keep_proxies: bool,
    proxy_envs: &[String],
) -> Vec<String> {
    let mut args = vec![
        "run".to_string(),
        "--rm".to_string(),
        "--user".to_string(),
        "0:0".to_string(),
        "--workdir".to_string(),
        "/work".to_string(),
        "--volume".to_string(),
        format!("{}:/work:rw,Z", workdir.display()),
    ];
    if keep_proxies {
        args.push("--network".to_string());
        args.push("host".to_string());
    }
    for proxy in proxy_envs {
        args.push("--env".to_string());
        args.push(proxy.clone());
    }
    args
}

pub(crate) fn pin_system_deps(deps: &mut SystemDeps) -> Result<(), InstallUserError> {
    let mode = system_deps_mode();
    let missing: Vec<String> = deps
        .apt_packages
        .iter()
        .filter(|pkg| !deps.apt_versions.contains_key(*pkg))
        .cloned()
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    if matches!(mode, SystemDepsMode::Offline) {
        for pkg in &missing {
            deps.apt_versions
                .entry(pkg.clone())
                .or_insert_with(|| "unpinned".to_string());
        }
        return Ok(());
    }
    let backend = detect_container_backend()?;
    if matches!(backend.kind, BackendKind::Custom) {
        for pkg in &missing {
            deps.apt_versions
                .entry(pkg.clone())
                .or_insert_with(|| "unpinned".to_string());
        }
        return Ok(());
    }
    let temp = tempdir().map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to prepare sandbox work directory",
            json!({ "error": err.to_string() }),
        )
    })?;
    let workdir = temp.path();
    let proxies_allowed = internal_keep_proxies();
    let mut apt_opts = base_apt_opts();
    if !proxies_allowed || should_disable_apt_proxy() {
        apt_opts.push_str(" -o Acquire::http::Proxy=false -o Acquire::https::Proxy=false");
    }
    let mut script = r#"set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
PACKAGES="__PACKAGES__"
APT_OPTS="__APT_OPTS__"
__APT_MIRROR_SETUP__
apt-get $APT_OPTS update -y >/dev/null || true
rm -f /work/apt-versions.txt
for pkg in $PACKAGES; do
  ver=$(apt-cache policy "$pkg" 2>/dev/null | awk '/Candidate:/ {print $2; exit}')
  echo "${pkg}=${ver}" >> /work/apt-versions.txt
done
"#
    .to_string();
    script = script
        .replace("__PACKAGES__", &missing.join(" "))
        .replace("__APT_OPTS__", &apt_opts)
        .replace("__APT_MIRROR_SETUP__", internal_apt_mirror_setup_snippet());
    let mut proxy_envs = internal_proxy_env_overrides(&backend);
    proxy_envs.extend(internal_apt_mirror_env_overrides());
    let mut cmd = Command::new(&backend.program);
    for arg in system_deps_container_args(workdir, proxies_allowed, &proxy_envs) {
        cmd.arg(arg);
    }
    cmd.arg(SYSTEM_DEPS_IMAGE).arg("bash").arg("-c").arg(script);
    let output = cmd.output().map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to pin sandbox system dependencies",
            json!({ "error": err.to_string() }),
        )
    })?;
    if !output.status.success() {
        return Err(sandbox_error(
            "PX903",
            "failed to pin sandbox system dependencies",
            json!({
                "code": output.status.code(),
                "stdout": String::from_utf8_lossy(&output.stdout).to_string(),
                "stderr": String::from_utf8_lossy(&output.stderr).to_string(),
            }),
        ));
    }
    let versions_path = workdir.join("apt-versions.txt");
    let data = fs::read_to_string(&versions_path).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to read pinned apt versions",
            json!({ "path": versions_path.display().to_string(), "error": err.to_string() }),
        )
    })?;
    for line in data.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let (pkg, ver) = match trimmed.split_once('=') {
            Some((name, ver)) => (name.trim(), ver.trim()),
            None => continue,
        };
        if pkg.is_empty() {
            continue;
        }
        if !ver.is_empty() {
            deps.apt_versions
                .entry(pkg.to_string())
                .or_insert_with(|| ver.to_string());
        }
    }
    Ok(())
}

fn pin_missing_apt_versions(definition: &mut SandboxDefinition) -> Result<(), InstallUserError> {
    pin_system_deps(&mut definition.system_deps)
}

pub(crate) fn ensure_system_deps_rootfs(
    deps: &SystemDeps,
) -> Result<Option<PathBuf>, InstallUserError> {
    if deps.apt_packages.is_empty() || matches!(system_deps_mode(), SystemDepsMode::Offline) {
        return Ok(None);
    }
    let Some(fingerprint) = deps.fingerprint() else {
        return Ok(None);
    };
    let store_root = default_store_root().map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to resolve sandbox store",
            json!({ "error": err.to_string() }),
        )
    })?;
    let target = store_root
        .join("system-deps")
        .join(&fingerprint)
        .join("rootfs");
    if target.exists() {
        return Ok(Some(target));
    }

    let backend = detect_container_backend()?;
    if matches!(backend.kind, BackendKind::Custom) {
        return Ok(None);
    }
    let mut deps = deps.clone();
    pin_system_deps(&mut deps)?;
    let mut packages: Vec<String> = deps
        .apt_packages
        .iter()
        .map(|pkg| {
            deps.apt_versions
                .get(pkg)
                .map(|ver| format!("{pkg}={ver}"))
                .unwrap_or_else(|| pkg.clone())
        })
        .collect();
    packages.sort();
    packages.dedup();
    if packages.is_empty() {
        return Ok(None);
    }
    let proxies_allowed = internal_keep_proxies();
    let mut apt_opts = base_apt_opts();
    if !proxies_allowed || should_disable_apt_proxy() {
        apt_opts.push_str(" -o Acquire::http::Proxy=false -o Acquire::https::Proxy=false");
    }
    let temp = tempdir().map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to prepare sandbox work directory",
            json!({ "error": err.to_string() }),
        )
    })?;
    let workdir = temp.path();
    let mut script = r#"set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
PACKAGES="__PACKAGES__"
APT_OPTS="__APT_OPTS__"
if [ -n "$PACKAGES" ]; then
  __APT_MIRROR_SETUP__
  apt-get $APT_OPTS update -y >/dev/null || true
  rm -rf /work/rootfs
  mkdir -p /work/rootfs/usr
  mkdir -p /work/rootfs/usr/bin /work/rootfs/usr/sbin /work/rootfs/usr/lib /work/rootfs/usr/lib64
  ln -s usr/bin /work/rootfs/bin
  ln -s usr/sbin /work/rootfs/sbin
  ln -s usr/lib /work/rootfs/lib
  ln -s usr/lib64 /work/rootfs/lib64
  apt-get $APT_OPTS install -y --no-install-recommends --download-only $PACKAGES >/dev/null
  for deb in /var/cache/apt/archives/*.deb; do
    [ -f "$deb" ] || continue
    dpkg -x "$deb" /work/rootfs
  done
  # Preserve Debian's merged-/usr layout so this layer does not clobber base symlinks.
  for d in bin sbin lib lib64; do
    if [ -d "/work/rootfs/$d" ] && [ ! -L "/work/rootfs/$d" ]; then
      mkdir -p "/work/rootfs/usr/$d"
      cp -a "/work/rootfs/$d/." "/work/rootfs/usr/$d/" || true
      rm -rf "/work/rootfs/$d"
    fi
  done
  ln -sfn usr/bin /work/rootfs/bin
  ln -sfn usr/sbin /work/rootfs/sbin
  ln -sfn usr/lib /work/rootfs/lib
  ln -sfn usr/lib64 /work/rootfs/lib64
  # Some Debian libs (e.g. BLAS/LAPACK) are installed under subdirs and rely on
  # update-alternatives; when extracting .debs we need stable SONAME symlinks.
  multiarch="$(dpkg-architecture -qDEB_HOST_MULTIARCH 2>/dev/null || echo x86_64-linux-gnu)"
  libdir="/work/rootfs/usr/lib/$multiarch"
  if [ -d "$libdir/blas" ] && [ ! -e "$libdir/libblas.so.3" ]; then
    if [ -e "$libdir/blas/libblas.so.3" ]; then
      ln -s "blas/libblas.so.3" "$libdir/libblas.so.3"
    else
      candidate="$(ls "$libdir/blas"/libblas.so.3.* 2>/dev/null | head -n 1 || true)"
      if [ -n "$candidate" ]; then
        ln -s "blas/$(basename "$candidate")" "$libdir/libblas.so.3"
      fi
    fi
  fi
  if [ -d "$libdir/lapack" ] && [ ! -e "$libdir/liblapack.so.3" ]; then
    if [ -e "$libdir/lapack/liblapack.so.3" ]; then
      ln -s "lapack/liblapack.so.3" "$libdir/liblapack.so.3"
    else
      candidate="$(ls "$libdir/lapack"/liblapack.so.3.* 2>/dev/null | head -n 1 || true)"
      if [ -n "$candidate" ]; then
        ln -s "lapack/$(basename "$candidate")" "$libdir/liblapack.so.3"
      fi
    fi
  fi
fi
"#
    .to_string();
    script = script
        .replace("__PACKAGES__", &packages.join(" "))
        .replace("__APT_OPTS__", &apt_opts)
        .replace("__APT_MIRROR_SETUP__", internal_apt_mirror_setup_snippet());
    let mut proxy_envs = internal_proxy_env_overrides(&backend);
    proxy_envs.extend(internal_apt_mirror_env_overrides());
    let mut cmd = Command::new(&backend.program);
    for arg in system_deps_container_args(workdir, proxies_allowed, &proxy_envs) {
        cmd.arg(arg);
    }
    cmd.arg(SYSTEM_DEPS_IMAGE).arg("bash").arg("-c").arg(script);
    let output = cmd.output().map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to prepare system dependencies",
            json!({ "error": err.to_string() }),
        )
    })?;
    if !output.status.success() {
        return Err(sandbox_error(
            "PX903",
            "failed to prepare system dependencies",
            json!({
                "code": output.status.code(),
                "stdout": String::from_utf8_lossy(&output.stdout).to_string(),
                "stderr": String::from_utf8_lossy(&output.stderr).to_string(),
            }),
        ));
    }
    let source = workdir.join("rootfs");
    if !source.exists() {
        return Ok(None);
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to create system dependency cache directory",
                json!({ "path": parent.display().to_string(), "error": err.to_string() }),
            )
        })?;
    }
    if fs::rename(&source, &target).is_err() {
        copy_dir(&source, &target)?;
    }
    Ok(Some(target))
}

fn copy_dir(src: &Path, dest: &Path) -> Result<(), InstallUserError> {
    for entry in WalkDir::new(src) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                return Err(sandbox_error(
                    "PX903",
                    "failed to copy system dependency tree",
                    json!({ "error": err.to_string() }),
                ))
            }
        };
        let path = entry.path();
        let rel = match path.strip_prefix(src) {
            Ok(rel) => rel,
            Err(_) => continue,
        };
        let dest_path = dest.join(rel);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&dest_path).map_err(|err| {
                sandbox_error(
                    "PX903",
                    "failed to prepare system dependency directory",
                    json!({ "path": dest_path.display().to_string(), "error": err.to_string() }),
                )
            })?;
        } else if entry.file_type().is_symlink() {
            let target = fs::read_link(path).map_err(|err| {
                sandbox_error(
                    "PX903",
                    "failed to read system dependency symlink",
                    json!({ "path": path.display().to_string(), "error": err.to_string() }),
                )
            })?;
            #[allow(clippy::redundant_closure_for_method_calls)]
            if dest_path.exists() {
                fs::remove_file(&dest_path).ok();
            }
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(&target, &dest_path).map_err(|err| {
                    sandbox_error(
                        "PX903",
                        "failed to copy system dependency symlink",
                        json!({
                            "path": dest_path.display().to_string(),
                            "error": err.to_string()
                        }),
                    )
                })?;
            }
            #[cfg(not(unix))]
            {
                fs::copy(path, &dest_path).map_err(|err| {
                    sandbox_error(
                        "PX903",
                        "failed to copy system dependency link",
                        json!({
                            "path": dest_path.display().to_string(),
                            "error": err.to_string()
                        }),
                    )
                })?;
            }
        } else {
            if let Some(parent) = dest_path.parent() {
                fs::create_dir_all(parent).map_err(|err| {
                    sandbox_error(
                        "PX903",
                        "failed to prepare system dependency file",
                        json!({
                            "path": parent.display().to_string(),
                            "error": err.to_string()
                        }),
                    )
                })?;
            }
            fs::copy(path, &dest_path).map_err(|err| {
                sandbox_error(
                    "PX903",
                    "failed to copy system dependency file",
                    json!({
                        "path": path.display().to_string(),
                        "error": err.to_string()
                    }),
                )
            })?;
        }
    }
    Ok(())
}

pub(crate) fn base_apt_opts() -> String {
    "-o Acquire::Retries=3 -o Acquire::http::Timeout=30 -o Acquire::https::Timeout=30".to_string()
}

pub(crate) fn should_disable_apt_proxy() -> bool {
    fn is_socks_proxy(value: &str) -> bool {
        value.trim().to_ascii_lowercase().starts_with("socks")
    }

    let mut has_http_proxy = false;
    for key in ["HTTP_PROXY", "http_proxy", "HTTPS_PROXY", "https_proxy"] {
        if let Ok(val) = std::env::var(key) {
            if val.trim().is_empty() {
                continue;
            }
            has_http_proxy = true;
            if is_socks_proxy(&val) {
                return true;
            }
        }
    }
    if has_http_proxy {
        return false;
    }
    for key in ["ALL_PROXY", "all_proxy"] {
        if let Ok(val) = std::env::var(key) {
            if val.trim().is_empty() {
                continue;
            }
            if is_socks_proxy(&val) {
                return true;
            }
        }
    }
    false
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
    let system_deps = resolve_system_deps(&capabilities, site_packages);
    let definition = SandboxDefinition {
        base_os_oid: base.base_os_oid.clone(),
        capabilities,
        system_deps,
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
    let mut caps: BTreeSet<String> = capability_apt_map()
        .iter()
        .map(|(cap, _)| (*cap).to_string())
        .collect();
    for (cap, _) in package_capability_rules() {
        caps.insert((*cap).to_string());
    }
    caps
}

fn infer_capabilities_from_lock(lock: &LockSnapshot) -> BTreeSet<String> {
    let mut caps = BTreeSet::new();
    for spec in &lock.dependencies {
        let name = crate::dependency_name(spec);
        caps.extend(
            crate::core::system_deps::capabilities_from_names([name.as_str()])
                .into_iter()
                .collect::<BTreeSet<_>>(),
        );
    }
    for dep in &lock.resolved {
        let name = dep.name.trim();
        caps.extend(
            crate::core::system_deps::capabilities_from_names([name])
                .into_iter()
                .collect::<BTreeSet<_>>(),
        );
    }
    caps
}

fn infer_capabilities_from_workspace(lock: &WorkspaceLock) -> BTreeSet<String> {
    let mut caps = BTreeSet::new();
    for member in &lock.members {
        for dep in &member.dependencies {
            let name = crate::dependency_name(dep);
            caps.extend(
                crate::core::system_deps::capabilities_from_names([name.as_str()])
                    .into_iter()
                    .collect::<BTreeSet<_>>(),
            );
        }
    }
    caps
}

fn infer_capabilities_from_site(site: &Path) -> BTreeSet<String> {
    let mut caps = BTreeSet::new();
    if !site.exists() {
        return caps;
    }
    let meta = read_sys_deps_metadata(site);
    caps.extend(meta.capabilities);
    for entry in WalkDir::new(site) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        caps.extend(
            crate::core::system_deps::capabilities_from_libraries([name])
                .into_iter()
                .collect::<BTreeSet<_>>(),
        );
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

pub(crate) fn sandbox_timestamp() -> OffsetDateTime {
    if let Ok(raw) = env::var("SOURCE_DATE_EPOCH") {
        if let Ok(epoch) = raw.trim().parse::<i64>() {
            if let Ok(ts) = OffsetDateTime::from_unix_timestamp(epoch) {
                return ts;
            }
        }
    }
    OffsetDateTime::UNIX_EPOCH
}

pub(crate) fn sandbox_timestamp_string() -> String {
    sandbox_timestamp()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
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

pub use pack::{pack_app, pack_image, PackRequest, PackTarget};
pub(crate) use pxapp::run_pxapp_bundle;
use runner::BackendKind;
pub(crate) use runner::{
    detect_container_backend, ensure_image_layout, run_container, ContainerBackend,
    ContainerRunArgs, Mount, RunMode, SandboxImageLayout,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::system_deps::{write_sys_deps_metadata, SystemDeps};
    use px_domain::LockSnapshot;
    use std::collections::BTreeSet;
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::{Mutex, OnceLock};
    use tempfile::tempdir;

    static PROXY_ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

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
    fn system_deps_container_avoids_host_apt_cache_mounts() {
        let workdir = PathBuf::from("/tmp/work");
        let proxies = vec!["HTTP_PROXY=".to_string()];
        let args = system_deps_container_args(&workdir, true, &proxies);
        let volume = format!("{}:/work:rw,Z", workdir.display());
        assert!(
            args.windows(2)
                .any(|pair| pair[0] == "--volume" && pair[1] == volume),
            "work directory should be mounted into container"
        );
        assert!(
            args.iter().all(|arg| {
                !arg.contains("apt-cache")
                    && !arg.contains("/var/cache/apt")
                    && !arg.contains("/var/lib/apt/lists")
            }),
            "system deps container should not mount host apt cache directories"
        );
        assert!(
            args.iter().any(|arg| arg == "--network"),
            "host networking should be enabled when proxies are kept"
        );
    }

    #[test]
    fn internal_containers_forward_proxy_env_when_set() {
        let _guard = PROXY_ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap();
        let keys = [
            "HTTP_PROXY",
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
            "NO_PROXY",
            "no_proxy",
            "FTP_PROXY",
        ];
        let originals: Vec<(String, Option<String>)> = keys
            .iter()
            .map(|key| (key.to_string(), env::var(key).ok()))
            .collect();

        for key in [
            "HTTP_PROXY",
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
            "NO_PROXY",
            "no_proxy",
        ] {
            env::remove_var(key);
        }
        env::set_var("HTTP_PROXY", "http://proxy.example:3128");
        env::set_var("http_proxy", "http://proxy-lower.example:3128");
        env::set_var("ALL_PROXY", "http://proxy.example:3128");
        env::set_var("NO_PROXY", "localhost,127.0.0.1");
        env::set_var("FTP_PROXY", "http://should-not-pass");

        let backend = ContainerBackend {
            program: PathBuf::from("docker"),
            kind: runner::BackendKind::Docker,
        };
        let proxy_envs = internal_proxy_env_overrides(&backend);
        assert!(
            proxy_envs.contains(&"HTTP_PROXY=http://proxy.example:3128".to_string()),
            "HTTP_PROXY should be forwarded"
        );
        assert!(
            proxy_envs.contains(&"http_proxy=http://proxy-lower.example:3128".to_string()),
            "http_proxy should be forwarded"
        );
        assert!(
            proxy_envs.contains(&"ALL_PROXY=http://proxy.example:3128".to_string()),
            "ALL_PROXY should be forwarded"
        );
        assert!(
            proxy_envs.contains(&"NO_PROXY=localhost,127.0.0.1".to_string()),
            "NO_PROXY should be forwarded"
        );
        assert!(
            proxy_envs.iter().all(|env| !env.starts_with("FTP_PROXY=")),
            "only allowlisted proxy env vars should be forwarded"
        );

        let workdir = PathBuf::from("/tmp/work");
        let args = system_deps_container_args(&workdir, internal_keep_proxies(), &proxy_envs);
        assert!(
            args.windows(2).any(|pair| {
                pair[0] == "--env" && pair[1] == "HTTP_PROXY=http://proxy.example:3128"
            }),
            "system deps container args should include forwarded proxy"
        );

        for (key, original) in originals {
            match original {
                Some(value) => env::set_var(&key, value),
                None => env::remove_var(&key),
            }
        }
    }

    #[test]
    fn internal_apt_mirror_env_overrides_empty_when_unset() {
        let _guard = PROXY_ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap();
        let keys = ["PX_APT_MIRROR", "PX_APT_SECURITY_MIRROR"];
        let originals: Vec<(String, Option<String>)> = keys
            .iter()
            .map(|key| (key.to_string(), env::var(key).ok()))
            .collect();
        for key in keys {
            env::remove_var(key);
        }

        assert!(
            internal_apt_mirror_env_overrides().is_empty(),
            "apt mirror env list should be empty when unset"
        );

        for (key, original) in originals {
            match original {
                Some(value) => env::set_var(&key, value),
                None => env::remove_var(&key),
            }
        }
    }

    #[test]
    fn internal_apt_mirror_env_overrides_supports_tsinghua_preset() {
        let _guard = PROXY_ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap();
        let keys = ["PX_APT_MIRROR", "PX_APT_SECURITY_MIRROR"];
        let originals: Vec<(String, Option<String>)> = keys
            .iter()
            .map(|key| (key.to_string(), env::var(key).ok()))
            .collect();
        for key in keys {
            env::remove_var(key);
        }
        env::set_var("PX_APT_MIRROR", "tsinghua");

        let envs = internal_apt_mirror_env_overrides();
        assert!(
            envs.contains(&"PX_APT_MIRROR=http://mirrors.tuna.tsinghua.edu.cn/debian".to_string()),
            "tsinghua preset should populate debian mirror"
        );
        assert!(
            envs.contains(
                &"PX_APT_SECURITY_MIRROR=http://mirrors.tuna.tsinghua.edu.cn/debian-security"
                    .to_string()
            ),
            "tsinghua preset should populate security mirror"
        );

        for (key, original) in originals {
            match original {
                Some(value) => env::set_var(&key, value),
                None => env::remove_var(&key),
            }
        }
    }

    #[test]
    fn internal_apt_mirror_env_overrides_derives_security_from_debian_suffix() {
        let _guard = PROXY_ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap();
        let keys = ["PX_APT_MIRROR", "PX_APT_SECURITY_MIRROR"];
        let originals: Vec<(String, Option<String>)> = keys
            .iter()
            .map(|key| (key.to_string(), env::var(key).ok()))
            .collect();
        for key in keys {
            env::remove_var(key);
        }
        env::set_var("PX_APT_MIRROR", "https://example.invalid/debian");

        let envs = internal_apt_mirror_env_overrides();
        assert!(
            envs.contains(&"PX_APT_MIRROR=https://example.invalid/debian".to_string()),
            "explicit mirror url should be forwarded"
        );
        assert!(
            envs.contains(&"PX_APT_SECURITY_MIRROR=https://example.invalid/debian-security".to_string()),
            "security mirror should be derived when mirror ends with /debian"
        );

        for (key, original) in originals {
            match original {
                Some(value) => env::set_var(&key, value),
                None => env::remove_var(&key),
            }
        }
    }

    #[test]
    fn apt_mirror_setup_snippet_writes_sources_list() {
        let snippet = internal_apt_mirror_setup_snippet();
        assert!(
            snippet.contains("/etc/apt/sources.list"),
            "apt mirror snippet should write sources.list"
        );
        assert!(
            snippet.contains("PX_APT_MIRROR"),
            "apt mirror snippet should be gated on PX_APT_MIRROR"
        );
    }

    #[test]
    fn should_not_disable_apt_proxy_when_all_proxy_is_socks_but_http_proxy_present() {
        let _guard = PROXY_ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap();
        let keys = [
            "HTTP_PROXY",
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
        ];
        let originals: Vec<(String, Option<String>)> = keys
            .iter()
            .map(|key| (key.to_string(), env::var(key).ok()))
            .collect();
        for key in keys {
            env::remove_var(key);
        }

        env::set_var("HTTP_PROXY", "http://127.0.0.1:3128");
        env::set_var("HTTPS_PROXY", "http://127.0.0.1:3128");
        env::set_var("ALL_PROXY", "socks5h://127.0.0.1:12334");
        assert!(
            !should_disable_apt_proxy(),
            "ALL_PROXY socks should not disable apt when HTTP(S)_PROXY is set"
        );
        assert!(
            internal_keep_proxies(),
            "internal containers should keep proxies when HTTP(S)_PROXY is set"
        );

        for (key, original) in originals {
            match original {
                Some(value) => env::set_var(&key, value),
                None => env::remove_var(&key),
            }
        }
    }

    #[test]
    fn internal_containers_do_not_forward_proxy_env_when_unset() {
        let _guard = PROXY_ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap();
        let keys = [
            "HTTP_PROXY",
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
            "NO_PROXY",
            "no_proxy",
        ];
        let originals: Vec<(String, Option<String>)> = keys
            .iter()
            .map(|key| (key.to_string(), env::var(key).ok()))
            .collect();
        for key in keys {
            env::remove_var(key);
        }

        let backend = ContainerBackend {
            program: PathBuf::from("docker"),
            kind: runner::BackendKind::Docker,
        };
        let proxy_envs = internal_proxy_env_overrides(&backend);
        for key in keys {
            assert!(
                proxy_envs.contains(&format!("{key}=")),
                "{key} should be cleared when unset"
            );
        }
        assert!(
            !internal_keep_proxies(),
            "internal_keep_proxies should be false without proxies"
        );

        for (key, original) in originals {
            match original {
                Some(value) => env::set_var(&key, value),
                None => env::remove_var(&key),
            }
        }
    }

    #[test]
    fn sandbox_id_is_stable_when_capabilities_unsorted() {
        let mut caps_a = BTreeSet::new();
        caps_a.insert("postgres".to_string());
        caps_a.insert("imagecodecs".to_string());
        let sys_deps_a = resolve_system_deps(&caps_a, None);
        let def_a = SandboxDefinition {
            base_os_oid: "base".to_string(),
            capabilities: caps_a,
            system_deps: sys_deps_a,
            profile_oid: "profile".to_string(),
            sbx_version: SBX_VERSION,
        };
        let mut caps_b = BTreeSet::new();
        caps_b.insert("imagecodecs".to_string());
        caps_b.insert("postgres".to_string());
        let sys_deps_b = resolve_system_deps(&caps_b, None);
        let def_b = SandboxDefinition {
            base_os_oid: "base".to_string(),
            capabilities: caps_b,
            system_deps: sys_deps_b,
            profile_oid: "profile".to_string(),
            sbx_version: SBX_VERSION,
        };
        assert_eq!(def_a.sbx_id(), def_b.sbx_id());
    }

    #[test]
    fn sandbox_id_reflects_system_dep_versions() {
        let mut caps = BTreeSet::new();
        caps.insert("postgres".to_string());
        let mut deps_a = resolve_system_deps(&caps, None);
        deps_a.apt_packages.insert("libpq-dev".into());
        deps_a.apt_versions.insert("libpq-dev".into(), "1".into());
        let def_a = SandboxDefinition {
            base_os_oid: "base".to_string(),
            capabilities: caps.clone(),
            system_deps: deps_a,
            profile_oid: "profile".to_string(),
            sbx_version: SBX_VERSION,
        };
        let mut deps_b = resolve_system_deps(&caps, None);
        deps_b.apt_packages.insert("libpq-dev".into());
        deps_b.apt_versions.insert("libpq-dev".into(), "2".into());
        let def_b = SandboxDefinition {
            base_os_oid: "base".to_string(),
            capabilities: caps,
            system_deps: deps_b,
            profile_oid: "profile".to_string(),
            sbx_version: SBX_VERSION,
        };
        assert_ne!(def_a.sbx_id(), def_b.sbx_id());
    }

    #[test]
    fn ensure_image_writes_manifest_and_reuses_store() {
        let temp = tempdir().expect("tempdir");
        let previous_mode = env::var("PX_SYSTEM_DEPS_MODE").ok();
        env::set_var("PX_SYSTEM_DEPS_MODE", "offline");
        let mut config = SandboxConfig {
            auto: false,
            ..Default::default()
        };
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
        match previous_mode {
            Some(value) => env::set_var("PX_SYSTEM_DEPS_MODE", value),
            None => env::remove_var("PX_SYSTEM_DEPS_MODE"),
        }
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
    fn lock_inference_detects_gdal_stack() {
        let config = SandboxConfig::default();
        let lock = lock_with_deps(vec!["gdal==3.8.0".into()]);
        let resolved = resolve_sandbox_definition(&config, Some(&lock), None, "profile", None)
            .expect("resolution");
        assert!(
            resolved.definition.capabilities.contains("gdal"),
            "gdal package should infer gdal capability"
        );
    }

    #[test]
    fn site_inference_detects_gdal_library() {
        let temp = tempdir().expect("tempdir");
        let site = temp.path().join("site");
        fs::create_dir_all(&site).expect("create site dir");
        fs::write(site.join("libgdal.so.34"), b"").expect("write sentinel");
        let config = SandboxConfig::default();
        let lock = lock_with_deps(vec![]);
        let resolved =
            resolve_sandbox_definition(&config, Some(&lock), None, "profile", Some(site.as_path()))
                .expect("resolution");
        assert!(
            resolved.definition.capabilities.contains("gdal"),
            "libgdal should infer gdal capability"
        );
    }

    #[test]
    fn site_inference_reads_builder_metadata() {
        let temp = tempdir().expect("tempdir");
        let site = temp.path().join("site");
        fs::create_dir_all(&site).expect("create site dir");
        let deps = SystemDeps {
            capabilities: ["postgres".into()].into_iter().collect(),
            apt_packages: ["libpq-dev".into()].into_iter().collect(),
            apt_versions: [("libpq-dev".into(), "1.0".into())].into_iter().collect(),
        };
        write_sys_deps_metadata(&site, "demo", &deps).expect("write metadata");
        let config = SandboxConfig::default();
        let lock = lock_with_deps(vec![]);
        let resolved =
            resolve_sandbox_definition(&config, Some(&lock), None, "profile", Some(site.as_path()))
                .expect("resolution");
        assert!(
            resolved.definition.capabilities.contains("postgres"),
            "metadata should propagate capabilities"
        );
        assert!(
            resolved
                .definition
                .system_deps
                .apt_packages
                .contains("libpq-dev"),
            "apt packages from metadata should propagate"
        );
        assert_eq!(
            resolved
                .definition
                .system_deps
                .apt_versions
                .get("libpq-dev"),
            Some(&"1.0".to_string())
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
