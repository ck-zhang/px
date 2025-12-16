use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use serde_json;

use crate::core::sandbox::{
    base_apt_opts, detect_container_backend, internal_apt_mirror_env_overrides,
    internal_apt_mirror_setup_snippet, internal_keep_proxies, internal_proxy_env_overrides,
};
use crate::core::system_deps::{
    base_apt_packages, capability_apt_map, package_capability_rules, SystemDeps,
};

use super::super::SdistRequest;

const BUILDER_IMAGE: &str =
    "docker.io/mambaorg/micromamba@sha256:008e06cd8432eb558faa4738a092f30b38dd8db3137a5dd3fca57374a790825b";

#[derive(Debug, Clone)]
pub(super) struct BuilderArtifacts {
    pub(super) python_path: PathBuf,
    pub(super) env_root: PathBuf,
}

pub(super) fn load_builder_system_deps(build_root: &Path) -> SystemDeps {
    let path = build_root.join("system-deps.json");
    let Ok(contents) = fs::read_to_string(&path) else {
        return SystemDeps::default();
    };
    serde_json::from_str::<SystemDeps>(&contents).unwrap_or_default()
}

pub(super) fn builder_container_mounts(builder_mount: &Path, build_mount: &Path) -> Vec<String> {
    vec![
        format!("{}:/work:rw,Z", build_mount.display()),
        format!("{}:/builder:rw,Z", builder_mount.display()),
    ]
}

pub(super) fn build_with_container_builder(
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
    let pkg_key =
        super::sanitize_builder_id(&format!("{}-{}", request.normalized_name, request.version));
    let builder_home = builder_root
        .join("builders")
        .join(super::sanitize_builder_id(request.builder_id))
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
    python==__PY_VERSION__ pip wheel setuptools pkg-config numpy >/dev/null
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
  export ALL_APT
  __APT_MIRROR_SETUP__
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
micromamba run -p "$ENV_ROOT" env CC=/usr/bin/gcc CXX=/usr/bin/g++ "$PY_BIN" -m pip wheel --no-deps --wheel-dir "$DIST_DIR" "$SDIST"
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
        .replace("__APT_OPTS__", &apt_opts)
        .replace("__APT_MIRROR_SETUP__", internal_apt_mirror_setup_snippet());

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
    for mirror in internal_apt_mirror_env_overrides() {
        cmd.arg("--env").arg(mirror);
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

pub(super) fn python_version(python: &str) -> Result<String> {
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
