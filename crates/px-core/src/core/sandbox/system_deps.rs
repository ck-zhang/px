use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::Result;
use serde_json::json;
use tempfile::tempdir;
use walkdir::WalkDir;

use super::errors::sandbox_error;
use super::paths::default_store_root;
use super::runner::{detect_container_backend, BackendKind, ContainerBackend};
use super::types::SandboxDefinition;
use crate::core::system_deps::SystemDeps;
use crate::InstallUserError;

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
    PROXY_KEYS
        .iter()
        .any(|key| env::var(key).map(|v| !v.trim().is_empty()).unwrap_or(false))
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

pub(super) fn pin_missing_apt_versions(
    definition: &mut SandboxDefinition,
) -> Result<(), InstallUserError> {
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
            #[allow(clippy::redundant_closure_for_method_calls)]
            if dest_path.exists() {
                fs::remove_file(&dest_path).ok();
            }
            #[cfg(unix)]
            {
                let target = fs::read_link(path).map_err(|err| {
                    sandbox_error(
                        "PX903",
                        "failed to read system dependency symlink",
                        json!({ "path": path.display().to_string(), "error": err.to_string() }),
                    )
                })?;
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
    "-o Acquire::Retries=3 -o Acquire::By-Hash=yes -o Acquire::http::Timeout=30 -o Acquire::https::Timeout=30"
        .to_string()
}

pub(crate) fn should_disable_apt_proxy() -> bool {
    fn is_socks_proxy(value: &str) -> bool {
        value.trim().to_ascii_lowercase().starts_with("socks")
    }

    let mut has_http_proxy = false;
    for key in ["HTTP_PROXY", "http_proxy", "HTTPS_PROXY", "https_proxy"] {
        if let Ok(val) = env::var(key) {
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
        if let Ok(val) = env::var(key) {
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

pub(super) fn system_deps_container_args(
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
