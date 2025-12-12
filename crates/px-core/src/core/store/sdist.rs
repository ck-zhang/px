use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use hex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use tracing::debug;
use walkdir::WalkDir;

use crate::core::runtime::builder::BUILDER_VERSION;
use crate::core::sandbox::{
    base_apt_opts, detect_container_backend, internal_keep_proxies,
    internal_proxy_env_overrides,
};
use crate::core::system_deps::{
    base_apt_packages, capability_apt_map, package_capability_rules, system_deps_from_names,
    write_sys_deps_metadata, SystemDeps,
};

use super::{
    cas::{
        global_store, pkg_build_lookup_key, source_lookup_key, ObjectKind, ObjectPayload,
        PkgBuildHeader, SourceHeader,
    },
    load_cached_build, persist_metadata,
    wheel::{compute_sha256, ensure_wheel_dist, parse_wheel_tags, wheel_path},
    BuiltWheel, SdistRequest,
};
use std::borrow::Cow;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum BuildMethod {
    PipWheel,
    PythonBuild,
    BuilderWheel,
}

const BUILDER_IMAGE: &str =
    "docker.io/mambaorg/micromamba@sha256:008e06cd8432eb558faa4738a092f30b38dd8db3137a5dd3fca57374a790825b";

fn copy_native_libs(env_root: &Path, dist_path: &Path) -> Result<()> {
    let mut env_lib_index: HashMap<String, PathBuf> = HashMap::new();
    for dir in ["lib", "lib64"] {
        let lib_root = env_root.join(dir);
        if !lib_root.exists() {
            continue;
        }
        for entry in WalkDir::new(&lib_root) {
            let entry = entry?;
            if entry.file_type().is_file() && is_shared_lib(entry.path()) {
                if let Some(name) = entry
                    .path()
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(str::to_string)
                {
                    env_lib_index
                        .entry(name)
                        .or_insert(entry.path().to_path_buf());
                }
            }
        }
    }
    debug!(
        env_root = %env_root.display(),
        lib_count = env_lib_index.len(),
        "collecting native libs from builder environment"
    );

    let mut queue = VecDeque::new();
    for entry in WalkDir::new(dist_path) {
        let entry = entry?;
        if entry.file_type().is_file() && is_shared_lib(entry.path()) {
            queue.push_back(entry.path().to_path_buf());
        }
    }
    debug!(
        dist = %dist_path.display(),
        seeds = queue.len(),
        "scanning built wheel for native dependencies"
    );

    let mut seen = HashSet::new();
    let mut deps_to_copy: HashSet<PathBuf> = HashSet::new();

    while let Some(target) = queue.pop_front() {
        if !seen.insert(target.clone()) {
            continue;
        }
        let deps = ldd_dependencies(&target, env_root)?;
        for dep in deps {
            let resolved = if dep.starts_with(env_root) {
                Some(dep)
            } else {
                dep.file_name()
                    .and_then(|name| name.to_str())
                    .and_then(|name| env_lib_index.get(name).cloned())
                    .or_else(|| Some(dep.clone()))
            };
            let Some(dep_path) = resolved else {
                continue;
            };
            if should_skip_native_dep(&dep_path) {
                continue;
            }
            if deps_to_copy.insert(dep_path.clone()) {
                queue.push_back(dep_path);
            }
        }
    }

    debug!(
        deps = deps_to_copy.len(),
        "copying resolved native libraries into wheel dist"
    );
    let sys_libs_root = dist_path.join("sys-libs");
    for dep in deps_to_copy {
        if should_skip_native_dep(&dep) {
            continue;
        }
        let rel = if dep.starts_with(env_root) {
            dep.strip_prefix(env_root)
                .unwrap_or(dep.as_path())
                .to_path_buf()
        } else {
            sys_libs_root.join(
                dep.file_name()
                    .unwrap_or_else(|| std::ffi::OsStr::new("unknown")),
            )
        };
        let dest = dist_path.join(rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&dep, &dest).with_context(|| {
            format!(
                "failed to copy native lib {} to {}",
                dep.display(),
                dest.display()
            )
        })?;
    }
    Ok(())
}

fn load_builder_system_deps(build_root: &Path) -> SystemDeps {
    let path = build_root.join("system-deps.json");
    let Ok(contents) = fs::read_to_string(&path) else {
        return SystemDeps::default();
    };
    serde_json::from_str::<SystemDeps>(&contents).unwrap_or_default()
}

fn is_shared_lib(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| {
            let lower = name.to_ascii_lowercase();
            lower.contains(".so") || lower.ends_with(".dylib") || lower.ends_with(".dll")
        })
        .unwrap_or(false)
}

fn should_skip_native_dep(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| {
            let lower = name.to_ascii_lowercase();
            lower.starts_with("ld-linux") || lower.starts_with("libc.")
        })
        .unwrap_or(false)
}

fn ldd_dependencies(target: &Path, env_root: &Path) -> Result<Vec<PathBuf>> {
    let mut cmd = Command::new("ldd");
    cmd.arg(target);
    let mut search_paths = Vec::new();
    for dir in ["lib", "lib64"] {
        let candidate = env_root.join(dir);
        if candidate.exists() {
            search_paths.push(candidate);
        }
    }
    if !search_paths.is_empty() {
        let joined = search_paths
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(":");
        cmd.env("LD_LIBRARY_PATH", joined);
    }
    let output = cmd
        .output()
        .with_context(|| format!("failed to run ldd on {}", target.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("ldd failed for {}: {stderr}", target.display());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut deps = Vec::new();
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut path_candidate: Option<PathBuf> = None;
        if let Some((_, rest)) = trimmed.split_once("=>") {
            let value = rest.trim();
            if value.starts_with("not found") || value == "not found" {
                continue;
            }
            let path_part = value.split_whitespace().next().unwrap_or("");
            if path_part.is_empty() || path_part == "not" {
                continue;
            }
            if path_part.starts_with('/') {
                path_candidate = Some(PathBuf::from(path_part));
            }
        } else if let Some(first) = trimmed.split_whitespace().next() {
            if first.starts_with('/') {
                path_candidate = Some(PathBuf::from(first));
            }
        }
        if let Some(path) = path_candidate {
            deps.push(path);
        }
    }
    Ok(deps)
}

impl Default for BuildMethod {
    fn default() -> Self {
        Self::PipWheel
    }
}

/// Build or retrieve an sdist-derived wheel from the cache.
///
/// # Errors
///
/// Returns an error when the archive cannot be unpacked, hashed, or copied into
/// the cache directory.
pub fn ensure_sdist_build(cache_root: &Path, request: &SdistRequest<'_>) -> Result<BuiltWheel> {
    let cas = global_store();

    let precomputed_id = request.sha256.map(|sha| build_identifier(request, sha));
    if let Some(id) = &precomputed_id {
        let meta_path = cache_root.join("sdist-build").join(id).join("meta.json");
        if let Some(built) = load_cached_build(&meta_path)? {
            if cache_hit_matches(request, &built)? && built.cached_path.exists() {
                return Ok(built);
            }
        }
    }

    let (temp_file, download_sha) = download_sdist(request)?;
    if let Some(expected) = request.sha256 {
        if download_sha != expected {
            bail!(
                "sdist checksum mismatch for {} (expected {}, got {})",
                request.filename,
                expected,
                download_sha
            );
        }
    }
    let sdist_bytes = fs::read(temp_file.path())?;
    let source_header = SourceHeader {
        name: request.normalized_name.to_string(),
        version: request.version.to_string(),
        filename: request.filename.to_string(),
        index_url: request.url.to_string(),
        sha256: download_sha.clone(),
    };
    let source_key = source_lookup_key(&source_header);
    let source_oid = match cas.lookup_key(ObjectKind::Source, &source_key)? {
        Some(oid) => oid,
        None => {
            let payload = ObjectPayload::Source {
                header: source_header.clone(),
                bytes: Cow::Owned(sdist_bytes.clone()),
            };
            let stored = cas.store(&payload)?;
            cas.record_key(ObjectKind::Source, &source_key, &stored.oid)?;
            stored.oid
        }
    };
    let build_id = precomputed_id.unwrap_or_else(|| build_identifier(request, &download_sha));
    let build_root = cache_root.join("sdist-build").join(&build_id);
    let meta_path = build_root.join("meta.json");

    if let Some(built) = load_cached_build(&meta_path)? {
        if cache_hit_matches(request, &built)? && built.cached_path.exists() {
            return Ok(built);
        }
        if built.builder_id != request.builder_id && build_root.exists() {
            let _ = fs::remove_dir_all(&build_root);
        }
    }

    fs::create_dir_all(&build_root)?;
    let sdist_path = build_root.join(request.filename);
    persist_named_tempfile(temp_file, &sdist_path)
        .map_err(|err| anyhow!("unable to persist sdist download: {err}"))?;

    let dist_dir = build_root.join("dist");
    if dist_dir.exists() {
        fs::remove_dir_all(&dist_dir)?;
    }
    fs::create_dir_all(&dist_dir)?;

    let builder = build_with_container_builder(request, &sdist_path, &dist_dir, &build_root)?;
    let build_method = BuildMethod::BuilderWheel;
    let build_python_path = builder.python_path.clone();
    let builder_env_root = Some(builder.env_root.clone());
    let mut system_deps = load_builder_system_deps(&build_root);
    if system_deps.is_empty() {
        system_deps = system_deps_from_names([request.normalized_name]);
    }

    let built_wheel_path = find_wheel(&dist_dir)?;
    let filename = built_wheel_path
        .file_name()
        .ok_or_else(|| anyhow!("wheel missing filename"))?
        .to_string_lossy()
        .to_string();
    let (python_tag, abi_tag, platform_tag) = parse_wheel_tags(&filename)
        .ok_or_else(|| anyhow!("unable to parse wheel tags from {filename}"))?;

    let dest = wheel_path(
        cache_root,
        request.normalized_name,
        request.version,
        &filename,
    );
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    persist_or_copy(&built_wheel_path, &dest)?;

    let sha256 = compute_sha256(&dest)?;
    let size = fs::metadata(&dest)?.len();
    let dist_path = ensure_wheel_dist(&dest, &sha256)?;
    if let Some(env_root) = &builder_env_root {
        copy_native_libs(env_root, &dist_path)?;
    }
    if !system_deps.capabilities.is_empty() || !system_deps.apt_packages.is_empty() {
        write_sys_deps_metadata(&dist_path, request.normalized_name, &system_deps)?;
    }
    let runtime_abi = format!("{python_tag}-{abi_tag}-{platform_tag}");
    let mut build_options_hash = compute_build_options_hash(
        build_python_path.to_str().unwrap_or(request.python_path),
        build_method,
    )?;
    if builder_env_root.is_some() {
        build_options_hash = format!("{build_options_hash}-native-libs");
    }
    if let Some(fingerprint) = system_deps.fingerprint() {
        build_options_hash = format!("{build_options_hash}-sys{fingerprint}");
    }
    let archive = super::cas::archive_dir_canonical(&dist_path)?;
    let pkg_header = PkgBuildHeader {
        source_oid,
        runtime_abi,
        builder_id: request.builder_id.to_string(),
        build_options_hash: build_options_hash.clone(),
    };
    let pkg_key = pkg_build_lookup_key(&pkg_header);
    let pkg_payload = ObjectPayload::PkgBuild {
        header: pkg_header.clone(),
        archive: Cow::Owned(archive),
    };
    let stored_pkg = cas.store(&pkg_payload)?;
    cas.record_key(ObjectKind::PkgBuild, &pkg_key, &stored_pkg.oid)?;

    let built = BuiltWheel {
        filename,
        url: request.url.to_string(),
        sha256,
        source_sha256: download_sha,
        size,
        cached_path: dest.clone(),
        dist_path: dist_path.clone(),
        python_tag,
        abi_tag,
        platform_tag,
        build_options_hash,
        system_deps: system_deps.clone(),
        build_method,
        builder_id: request.builder_id.to_string(),
    };
    persist_metadata(&meta_path, &built)?;

    let _ = fs::remove_dir_all(&dist_dir);

    Ok(built)
}

fn build_identifier(request: &SdistRequest<'_>, sha256: &str) -> String {
    let short_len = sha256.len().min(16);
    let builder = sanitize_builder_id(request.builder_id);
    format!(
        "{}-{}-{}-{}",
        request.normalized_name,
        request.version,
        &sha256[..short_len],
        builder
    )
}

fn sanitize_builder_id(builder_id: &str) -> String {
    builder_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn cache_hit_matches(request: &SdistRequest<'_>, built: &BuiltWheel) -> Result<bool> {
    if !built.builder_id.is_empty() && built.builder_id != request.builder_id {
        return Ok(false);
    }
    if built.build_method == BuildMethod::BuilderWheel {
        let mut expected = compute_build_options_hash(request.python_path, built.build_method)?;
        expected = format!("{expected}-native-libs");
        if let Some(fingerprint) = built.system_deps.fingerprint() {
            expected = format!("{expected}-sys{fingerprint}");
        }
        return Ok(built.build_options_hash == expected);
    }
    if built.build_options_hash.is_empty() {
        return Ok(false);
    }
    let expected = compute_build_options_hash(request.python_path, built.build_method)?;
    Ok(built.build_options_hash == expected)
}

#[derive(Debug, Clone)]
struct BuilderArtifacts {
    python_path: PathBuf,
    env_root: PathBuf,
}

fn builder_container_mounts(builder_mount: &Path, build_mount: &Path) -> Vec<String> {
    vec![
        format!("{}:/work:rw,Z", build_mount.display()),
        format!("{}:/builder:rw,Z", builder_mount.display()),
    ]
}

fn build_with_container_builder(
    request: &SdistRequest<'_>,
    sdist_path: &Path,
    dist_dir: &Path,
    build_root: &Path,
) -> Result<BuilderArtifacts> {
    let backend = detect_container_backend().map_err(|err| anyhow!(err.to_string()))?;
    let builder_root = request
        .builder_root
        .clone()
        .unwrap_or_else(std::env::temp_dir);
    let py_version = python_version(request.python_path)?;
    let pkg_key = sanitize_builder_id(&format!("{}-{}", request.normalized_name, request.version));
    let builder_home = builder_root
        .join("builders")
        .join(sanitize_builder_id(request.builder_id))
        .join(pkg_key)
        .join(format!("py{py_version}"));
    let env_root = builder_home.join("env");
    let env_python = env_root.join("bin").join("python");
    fs::create_dir_all(&builder_home)?;
    fs::create_dir_all(dist_dir)?;

    let mut cap_rules: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (cap, patterns) in package_capability_rules() {
        cap_rules.insert(
            (*cap).to_string(),
            patterns.iter().map(|p| (*p).to_string()).collect(),
        );
    }
    let mut apt_rules: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (cap, pkgs) in capability_apt_map() {
        apt_rules.insert(
            (*cap).to_string(),
            pkgs.iter().map(|p| (*p).to_string()).collect(),
        );
    }
    let cap_rules_json = serde_json::to_string(&cap_rules)?;
    let apt_rules_json = serde_json::to_string(&apt_rules)?;
    let base_apt = base_apt_packages().join(" ");
    let keep_proxies = internal_keep_proxies();
    let mut apt_opts = base_apt_opts();
    if !keep_proxies || crate::core::sandbox::should_disable_apt_proxy() {
        apt_opts.push_str(" -o Acquire::http::Proxy=false -o Acquire::https::Proxy=false");
    }
    let builder_mount = fs::canonicalize(&builder_home).unwrap_or(builder_home.clone());
    let build_mount = fs::canonicalize(build_root).unwrap_or_else(|_| build_root.to_path_buf());
    let dist_dir_container = "/work/dist";
    let env_root_container = "/builder/env";
    let sdist_container = format!(
        "/work/{}",
        sdist_path.file_name().unwrap_or_default().to_string_lossy()
    );
    let conda_spec = format!(
        "{}=={}",
        request.normalized_name.replace('_', "-"),
        request.version
    );
    let mut script = r#"set -euo pipefail
umask 022
ENV_ROOT="__ENV_ROOT__"
DIST_DIR="__DIST_DIR__"
SDIST="__SDIST__"
export MAMBA_PKGS_DIRS=/builder/pkgs
export PIP_CACHE_DIR=/builder/pip-cache
export PIP_NO_BUILD_ISOLATION=1
export PROJ_LIB="$ENV_ROOT/share/proj"
export GDAL_DATA="$ENV_ROOT/share/gdal"
export PX_CAP_RULES='__CAP_RULES__'
export PX_APT_RULES='__APT_RULES__'
export PX_BASE_APT="__BASE_APT__"
export CONDA_SPEC="__CONDA_SPEC__"
APT_OPTS="__APT_OPTS__"
PY_BIN="$ENV_ROOT/bin/python"
if [ ! -d "$ENV_ROOT/conda-meta" ]; then
  rm -rf "$ENV_ROOT"
fi
mkdir -p "$MAMBA_PKGS_DIRS" "$DIST_DIR" "$PIP_CACHE_DIR"
rm -rf "$DIST_DIR"
mkdir -p "$DIST_DIR"
if [ ! -x "$PY_BIN" ]; then
  micromamba create -y -p "$ENV_ROOT" --override-channels -c conda-forge \
    python==__PY_VERSION__ pip wheel setuptools pkg-config c-compiler cxx-compiler fortran-compiler numpy >/dev/null
  HTTP_PROXY= HTTPS_PROXY= ALL_PROXY= http_proxy= https_proxy= all_proxy= micromamba run -p "$ENV_ROOT" python -m pip install --upgrade pip build wheel pysocks >/dev/null
fi
micromamba repoquery depends --json --override-channels -c conda-forge "$CONDA_SPEC" > /work/repoquery.json || true
APT_LIST=$(micromamba run -p "$ENV_ROOT" python - <<'PY'
import json, os
from pathlib import Path

repo = Path("/work/repoquery.json")
data = {}
if repo.exists():
    try:
        data = json.loads(repo.read_text())
    except Exception:
        data = {}
rules = json.loads(os.environ.get("PX_CAP_RULES","{}") or "{}")
apt_rules = json.loads(os.environ.get("PX_APT_RULES","{}") or "{}")
spec = os.environ.get("CONDA_SPEC","")
depends = data.get("result", {}).get("depends", []) if isinstance(data, dict) else []
names = set()
for entry in depends:
    if isinstance(entry, str):
        names.add(entry.split()[0].lower())
if spec:
    names.add(spec.split("==")[0].split("=")[0].lower())
caps = set()
for name in names:
    for cap, patterns in rules.items():
        if any(name.startswith(pat) for pat in patterns):
            caps.add(cap)
apt = set()
for cap in caps:
    for pkg in apt_rules.get(cap, []):
        apt.add(pkg)
meta = {"capabilities": sorted(caps), "apt_packages": sorted(apt)}
Path("/work/system-deps.json").write_text(json.dumps(meta, sort_keys=True, indent=2))
print(" ".join(sorted(apt)))
PY
)
ALL_APT="$APT_LIST $PX_BASE_APT"
if [ -n "$ALL_APT" ]; then
  ALL_APT=$(printf "%s\n" "$ALL_APT" | tr ' ' '\n' | sed '/^$/d' | sort -u | xargs)
  apt-get $APT_OPTS update -y >/dev/null
  APT_INSTALL=$(micromamba run -p "$ENV_ROOT" python - <<'PY'
import json, os, subprocess
from pathlib import Path

meta_path = Path("/work/system-deps.json")
data = {}
if meta_path.exists():
    try:
        data = json.loads(meta_path.read_text())
    except Exception:
        data = {}
names = [name for name in os.environ.get("ALL_APT","").split() if name]
packages = sorted(set(names + list(data.get("apt_packages", []))))
versions = {}
install = []
for name in packages:
    version = ""
    try:
        out = subprocess.check_output(["apt-cache", "policy", name], text=True)
        for line in out.splitlines():
            line = line.strip()
            if line.startswith("Candidate:"):
                version = line.split(":", 1)[-1].strip()
                break
    except Exception:
        version = ""
    if version:
        install.append(f"{name}={version}")
        versions[name] = version
    else:
        install.append(name)
data["apt_packages"] = packages
data["apt_versions"] = versions
meta_path.write_text(json.dumps(data, sort_keys=True, indent=2))
print(" ".join(install))
PY
)
  if [ -n "$APT_INSTALL" ]; then
    DEBIAN_FRONTEND=noninteractive apt-get $APT_OPTS install -y --no-install-recommends $APT_INSTALL >/dev/null
  fi
fi
micromamba run -p "$ENV_ROOT" python -m pip wheel --no-deps --wheel-dir "$DIST_DIR" "$SDIST"
"#
    .to_string();
    script = script
        .replace("__ENV_ROOT__", env_root_container)
        .replace("__DIST_DIR__", dist_dir_container)
        .replace("__SDIST__", &sdist_container)
        .replace("__CAP_RULES__", &cap_rules_json)
        .replace("__APT_RULES__", &apt_rules_json)
        .replace("__BASE_APT__", &base_apt)
        .replace("__CONDA_SPEC__", &conda_spec)
        .replace("__PY_VERSION__", &py_version)
        .replace("__APT_OPTS__", &apt_opts);

    let mut cmd = Command::new(&backend.program);
    cmd.arg("run")
        .arg("--rm")
        .arg("--user")
        .arg("0:0")
        .arg("--workdir")
        .arg("/work");
    for mount in builder_container_mounts(&builder_mount, &build_mount) {
        cmd.arg("--volume").arg(mount);
    }
    if keep_proxies {
        cmd.arg("--network").arg("host");
    }
    for proxy in internal_proxy_env_overrides(&backend) {
        cmd.arg("--env").arg(proxy);
    }
    cmd.arg("--env").arg("MAMBA_PKGS_DIRS=/builder/pkgs");
    cmd.arg("--env").arg("PIP_CACHE_DIR=/builder/pip-cache");
    cmd.arg(BUILDER_IMAGE).arg("bash").arg("-c").arg(script);
    let output = cmd
        .output()
        .with_context(|| format!("failed to run builder container {BUILDER_IMAGE}"))?;
    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "builder container failed (code {}):\nstdout:\n{}\nstderr:\n{}",
            output.status,
            stdout,
            stderr
        );
    }

    Ok(BuilderArtifacts {
        python_path: env_python,
        env_root,
    })
}

fn download_sdist(request: &SdistRequest<'_>) -> Result<(NamedTempFile, String)> {
    if request.url.starts_with("file://") || Path::new(request.url).exists() {
        let path = if request.url.starts_with("file://") {
            PathBuf::from(request.url.trim_start_matches("file://"))
        } else {
            PathBuf::from(request.url)
        };
        let mut src = File::open(&path)
            .with_context(|| format!("failed to open local sdist at {}", path.display()))?;
        let mut tmp = NamedTempFile::new()?;
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let read = src.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            tmp.write_all(&buffer[..read])?;
        }
        let sha256 = hex::encode(hasher.finalize());
        return Ok((tmp, sha256));
    }

    let mut last_err = None;
    for _ in 0..super::DOWNLOAD_ATTEMPTS {
        match download_sdist_once(request) {
            Ok(result) => return Ok(result),
            Err(err) => last_err = Some(err),
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow!("failed to download sdist {}", request.url)))
}

fn download_sdist_once(request: &SdistRequest<'_>) -> Result<(NamedTempFile, String)> {
    let client = super::http_client()?;
    let mut response = client
        .get(request.url)
        .send()
        .with_context(|| format!("failed to fetch {}", request.url))?
        .error_for_status()
        .with_context(|| format!("unexpected response for {}", request.url))?;

    let mut tmp = NamedTempFile::new()?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = response
            .read(&mut buffer)
            .with_context(|| format!("stream error for {}", request.filename))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        tmp.write_all(&buffer[..read])?;
    }
    let sha256 = hex::encode(hasher.finalize());
    Ok((tmp, sha256))
}

fn persist_named_tempfile(tmp: NamedTempFile, dest: &Path) -> io::Result<()> {
    match tmp.persist(dest) {
        Ok(_) => Ok(()),
        Err(err) => {
            let file = err.file;
            if is_cross_device(&err.error) {
                let mut reader = file.reopen()?;
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)?;
                }
                let mut writer = File::create(dest)?;
                io::copy(&mut reader, &mut writer)?;
                file.close().ok();
                Ok(())
            } else {
                Err(err.error)
            }
        }
    }
}

fn persist_or_copy(src: &Path, dest: &Path) -> io::Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::rename(src, dest) {
        Ok(_) => Ok(()),
        Err(err) if is_cross_device(&err) => std::fs::copy(src, dest).map(|_| ()),
        Err(err) => Err(err),
    }
}

fn is_cross_device(err: &io::Error) -> bool {
    matches!(err.raw_os_error(), Some(18))
}

fn python_version(python: &str) -> Result<String> {
    let output = Command::new(python)
        .arg("-c")
        .arg("import sys; print(f\"{sys.version_info[0]}.{sys.version_info[1]}\")")
        .output()
        .context("failed to query python version")?;
    if !output.status.success() {
        bail!("failed to query python version");
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
fn discover_project_dir(root: &Path) -> Result<PathBuf> {
    if is_project_dir(root) {
        return Ok(root.to_path_buf());
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let path = entry.path();
            if is_project_dir(&path) {
                return Ok(path);
            }
        }
    }
    Err(anyhow!("unable to find project dir in {}", root.display()))
}

#[cfg(test)]
fn is_project_dir(path: &Path) -> bool {
    path.join("pyproject.toml").exists()
        || path.join("setup.py").exists()
        || path.join("setup.cfg").exists()
}

fn find_wheel(dist_dir: &Path) -> Result<PathBuf> {
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

pub(crate) fn compute_build_options_hash(python_path: &str, method: BuildMethod) -> Result<String> {
    #[derive(Serialize)]
    struct BuildOptionsFingerprint {
        python: String,
        method: BuildMethod,
        env: BTreeMap<String, String>,
    }

    let python = match method {
        BuildMethod::BuilderWheel => {
            let version = python_version(python_path).unwrap_or_else(|_| "unknown".to_string());
            format!("builder-v{BUILDER_VERSION}-py{version}")
        }
        _ => fs::canonicalize(python_path)
            .unwrap_or_else(|_| PathBuf::from(python_path))
            .display()
            .to_string(),
    };
    let fingerprint = BuildOptionsFingerprint {
        python,
        method,
        env: build_env_fingerprint(),
    };
    let bytes = serde_json::to_vec(&fingerprint)?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

/// Compute the build options hash for a wheel-style build/install.
pub(crate) fn wheel_build_options_hash(python_path: &str) -> Result<String> {
    compute_build_options_hash(python_path, BuildMethod::PipWheel)
}

fn build_env_fingerprint() -> BTreeMap<String, String> {
    const BUILD_ENV_VARS: &[&str] = &[
        "ARCHFLAGS",
        "CFLAGS",
        "CPPFLAGS",
        "CXXFLAGS",
        "LDFLAGS",
        "MACOSX_DEPLOYMENT_TARGET",
        "PKG_CONFIG_PATH",
        "PIP_CONFIG_FILE",
        "PIP_DISABLE_PIP_VERSION_CHECK",
        "PIP_EXTRA_INDEX_URL",
        "PIP_FIND_LINKS",
        "PIP_INDEX_URL",
        "PIP_NO_BUILD_ISOLATION",
        "PIP_NO_CACHE_DIR",
        "PIP_PREFER_BINARY",
        "PIP_PROGRESS_BAR",
        "PYTHONDONTWRITEBYTECODE",
        "PYTHONHASHSEED",
        "PYTHONUTF8",
        "PYTHONWARNINGS",
        "SETUPTOOLS_USE_DISTUTILS",
        "SOURCE_DATE_EPOCH",
        "CARGO_BUILD_TARGET",
        "CARGO_HOME",
        "CARGO_TARGET_DIR",
        "MATURIN_BUILD_ARGS",
        "MATURIN_CARGO_FLAGS",
        "MATURIN_CARGO_PROFILE",
        "MATURIN_FEATURES",
        "MATURIN_PEP517_ARGS",
        "MATURIN_PEP517_FEATURES",
        "PYO3_CONFIG_FILE",
        "RUSTFLAGS",
        "RUSTUP_TOOLCHAIN",
    ];
    let mut env = BTreeMap::new();
    for key in BUILD_ENV_VARS {
        if let Ok(value) = std::env::var(key) {
            env.insert((*key).to_string(), value);
        }
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn restore_env(key: &str, original: Option<String>) {
        match original {
            Some(value) => env::set_var(key, value),
            None => env::remove_var(key),
        }
    }

    #[test]
    fn build_options_hash_reflects_env_changes() -> Result<()> {
        let key = "CFLAGS";
        let original = env::var(key).ok();
        env::set_var(key, "value-1");
        let temp_python = env::temp_dir().join("python");
        let python = temp_python.display().to_string();

        let first = compute_build_options_hash(&python, BuildMethod::PipWheel)?;
        env::set_var(key, "value-2");
        let second = compute_build_options_hash(&python, BuildMethod::PipWheel)?;

        restore_env(key, original);
        assert_ne!(first, second, "hash should change when build env changes");
        Ok(())
    }

    #[test]
    fn build_options_hash_reflects_rust_env() -> Result<()> {
        let key = "RUSTFLAGS";
        let original = env::var(key).ok();
        env::set_var(key, "value-1");
        let temp_python = env::temp_dir().join("python");
        let python = temp_python.display().to_string();

        let first = compute_build_options_hash(&python, BuildMethod::PipWheel)?;
        env::set_var(key, "value-2");
        let second = compute_build_options_hash(&python, BuildMethod::PipWheel)?;

        restore_env(key, original);
        assert_ne!(
            first, second,
            "hash should change when rust build env changes"
        );
        Ok(())
    }

    #[test]
    fn build_options_hash_varies_by_method() -> Result<()> {
        let temp_python = env::temp_dir().join("python");
        let python = temp_python.display().to_string();
        let pip = compute_build_options_hash(&python, BuildMethod::PipWheel)?;
        let build = compute_build_options_hash(&python, BuildMethod::PythonBuild)?;
        assert_ne!(pip, build, "build method should influence options hash");
        Ok(())
    }

    #[test]
    fn builder_container_mounts_do_not_include_apt_cache() {
        let builder_mount = PathBuf::from("/tmp/builder");
        let build_mount = PathBuf::from("/tmp/build");
        let mounts = builder_container_mounts(&builder_mount, &build_mount);
        assert_eq!(
            mounts,
            vec![
                format!("{}:/work:rw,Z", build_mount.display()),
                format!("{}:/builder:rw,Z", builder_mount.display())
            ]
        );
        assert!(
            mounts.iter().all(|mount| {
                !mount.contains("apt-cache")
                    && !mount.contains("/var/cache/apt")
                    && !mount.contains("/var/lib/apt")
            }),
            "builder container should not mount host apt caches"
        );
    }

    #[test]
    fn builder_container_required_when_backend_missing() {
        let temp = tempdir().unwrap();
        let sdist = temp.path().join("demo-0.1.0.tar.gz");
        fs::write(&sdist, b"demo").expect("write sdist");
        let dist_dir = temp.path().join("dist");
        fs::create_dir_all(&dist_dir).expect("dist dir");
        let request = SdistRequest {
            normalized_name: "demo",
            version: "0.1.0",
            filename: "demo-0.1.0.tar.gz",
            url: "https://example.com/demo-0.1.0.tar.gz",
            sha256: None,
            python_path: "python",
            builder_id: "demo-builder",
            builder_root: Some(temp.path().to_path_buf()),
        };
        let previous = env::var("PX_SANDBOX_BACKEND").ok();
        env::set_var("PX_SANDBOX_BACKEND", "/definitely/missing/backend");
        let result = build_with_container_builder(&request, &sdist, &dist_dir, temp.path());
        match previous {
            Some(val) => env::set_var("PX_SANDBOX_BACKEND", val),
            None => env::remove_var("PX_SANDBOX_BACKEND"),
        }
        assert!(result.is_err(), "builder should require container backend");
    }

    #[test]
    fn detects_project_at_root_with_pyproject() -> Result<()> {
        let dir = tempdir()?;
        let pyproject = dir.path().join("pyproject.toml");
        fs::write(&pyproject, b"[project]\nname = \"demo\"")?;

        let detected = discover_project_dir(dir.path())?;

        assert_eq!(detected, dir.path());
        Ok(())
    }

    #[test]
    fn detects_project_in_subdir_with_setup_py() -> Result<()> {
        let dir = tempdir()?;
        let nested = dir.path().join("pkg");
        fs::create_dir_all(&nested)?;
        fs::write(
            nested.join("setup.py"),
            b"from setuptools import setup\nsetup()",
        )?;

        let detected = discover_project_dir(dir.path())?;

        assert_eq!(detected, nested);
        Ok(())
    }

    #[test]
    fn errors_when_project_files_missing() {
        let dir = tempdir().unwrap();
        let result = discover_project_dir(dir.path());
        assert!(result.is_err());
    }
}
