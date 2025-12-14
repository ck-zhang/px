use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::core::system_deps::SystemDeps;
use crate::python_sys::detect_interpreter;

pub mod cas;
pub mod pypi;

mod cache;
mod prefetch;
mod sdist;
mod wheel;

use sdist::BuildMethod;

#[allow(unused_imports)]
pub use cache::{
    collect_cache_walk, compute_cache_usage, ensure_pyc_cache_prefix, prune_cache_entries,
    pyc_cache_prefix, resolve_cache_store_path, CacheEntry, CacheLocation, CachePruneError,
    CachePruneResult, CacheUsage, CacheWalk,
};
pub use prefetch::prefetch_artifacts;
pub use sdist::ensure_sdist_build;
pub(crate) use sdist::wheel_build_options_hash;
#[allow(unused_imports)]
pub use wheel::{
    cache_wheel, compute_sha256, download_with_retry, ensure_wheel_dist, http_client,
    marker_matches, parse_wheel_tags, unpack_wheel, validate_existing, wheel_path,
};

const USER_AGENT: &str = concat!("px-store/", env!("CARGO_PKG_VERSION"));
pub(crate) const DOWNLOAD_ATTEMPTS: usize = 3;
pub(crate) const HTTP_TIMEOUT: Duration = Duration::from_secs(60);
pub(crate) const WHEEL_MARKER_NAME: &str = ".px-wheel.json";

pub struct ArtifactRequest<'a> {
    pub name: &'a str,
    pub version: &'a str,
    pub filename: &'a str,
    pub url: &'a str,
    pub sha256: &'a str,
}

#[derive(Debug, Clone)]
pub struct CachedArtifact {
    pub wheel_path: PathBuf,
    pub dist_path: PathBuf,
    pub size: u64,
}

#[derive(Debug, Clone)]
pub struct CachedWheelFile {
    pub path: PathBuf,
    pub size: u64,
}

#[derive(Debug, Clone)]
pub struct PrefetchSpec<'a> {
    pub name: &'a str,
    pub version: &'a str,
    pub filename: &'a str,
    pub url: &'a str,
    pub sha256: &'a str,
}

#[derive(Debug, Clone, Copy)]
pub struct PrefetchOptions {
    pub dry_run: bool,
    pub parallel: usize,
}

impl Default for PrefetchOptions {
    fn default() -> Self {
        Self {
            dry_run: false,
            parallel: 4,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PrefetchSummary {
    pub requested: usize,
    pub hit: usize,
    pub fetched: usize,
    pub failed: usize,
    pub bytes_fetched: u64,
    pub errors: Vec<String>,
}

pub struct SdistRequest<'a> {
    pub normalized_name: &'a str,
    pub version: &'a str,
    pub filename: &'a str,
    pub url: &'a str,
    pub sha256: Option<&'a str>,
    pub python_path: &'a str,
    pub builder_id: &'a str,
    pub builder_root: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct BuiltWheel {
    pub filename: String,
    pub url: String,
    pub sha256: String,
    pub source_sha256: String,
    pub size: u64,
    pub cached_path: PathBuf,
    pub dist_path: PathBuf,
    pub python_tag: String,
    pub abi_tag: String,
    pub platform_tag: String,
    pub build_options_hash: String,
    pub system_deps: SystemDeps,
    pub(crate) build_method: BuildMethod,
    pub(crate) builder_id: String,
}

#[derive(Serialize, Deserialize)]
struct BuiltWheelMetadata {
    filename: String,
    url: String,
    sha256: String,
    #[serde(default)]
    source_sha256: String,
    size: u64,
    cached_path: String,
    #[serde(default)]
    dist_path: Option<String>,
    python_tag: String,
    abi_tag: String,
    platform_tag: String,
    #[serde(default)]
    build_options_hash: String,
    #[serde(default)]
    system_deps: SystemDeps,
    #[serde(default)]
    build_method: BuildMethod,
    #[serde(default)]
    builder_id: String,
}

#[derive(Serialize, Deserialize)]
pub struct WheelUnpackMetadata {
    pub sha256: String,
}

fn load_cached_build(meta_path: &Path) -> Result<Option<BuiltWheel>> {
    let contents = match fs::read_to_string(meta_path) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    let meta: BuiltWheelMetadata = serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse {}", meta_path.display()))?;
    let dist_path = meta.dist_path.as_ref().map(PathBuf::from);
    Ok(Some(BuiltWheel {
        filename: meta.filename,
        url: meta.url,
        sha256: meta.sha256,
        source_sha256: meta.source_sha256,
        size: meta.size,
        cached_path: PathBuf::from(meta.cached_path),
        dist_path: dist_path.unwrap_or_default(),
        python_tag: meta.python_tag,
        abi_tag: meta.abi_tag,
        platform_tag: meta.platform_tag,
        build_options_hash: meta.build_options_hash,
        system_deps: meta.system_deps,
        build_method: meta.build_method,
        builder_id: meta.builder_id,
    }))
}

fn persist_metadata(meta_path: &Path, built: &BuiltWheel) -> Result<()> {
    let meta = BuiltWheelMetadata {
        filename: built.filename.clone(),
        url: built.url.clone(),
        sha256: built.sha256.clone(),
        source_sha256: built.source_sha256.clone(),
        size: built.size,
        cached_path: built.cached_path.display().to_string(),
        dist_path: Some(built.dist_path.display().to_string()),
        python_tag: built.python_tag.clone(),
        abi_tag: built.abi_tag.clone(),
        platform_tag: built.platform_tag.clone(),
        build_options_hash: built.build_options_hash.clone(),
        system_deps: built.system_deps.clone(),
        build_method: built.build_method,
        builder_id: built.builder_id.clone(),
    };
    if let Some(parent) = meta_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(meta_path, serde_json::to_string_pretty(&meta)?)?;
    Ok(())
}

#[allow(dead_code)]
fn apply_python_env(cmd: &mut Command) {
    if let Ok(interpreter) = detect_interpreter() {
        cmd.env("PYTHON", interpreter);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn download_packaging_smoke() -> Result<()> {
        if env::var("PX_ONLINE").ok().as_deref() != Some("1") {
            eprintln!("skipping download_packaging_smoke (PX_ONLINE!=1)");
            return Ok(());
        }

        let temp = tempfile::tempdir()?;
        let request = ArtifactRequest {
            name: "packaging",
            version: "24.1",
            filename: "packaging-24.1-py3-none-any.whl",
            url: "https://files.pythonhosted.org/packages/08/aa/cc0199a5f0ad350994d660967a8efb233fe0416e4639146c089643407ce6/packaging-24.1-py3-none-any.whl",
            sha256: "5b8f2217dbdbd2f7f384c41c628544e6d52f2d0f53c6d0c3ea61aa5d1d7ff124",
        };

        let artifact = cache_wheel(temp.path(), &request)?;
        assert!(artifact.wheel_path.exists());
        assert!(artifact.dist_path.exists());
        assert_eq!(artifact.size, fs::metadata(&artifact.wheel_path)?.len());
        Ok(())
    }

    #[test]
    fn resolves_cache_path_override() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let override_path = temp.path().join("cache-root");
        fs::create_dir_all(&override_path)?;
        let previous: Option<std::ffi::OsString> = env::var_os("PX_CACHE_PATH");
        env::set_var("PX_CACHE_PATH", &override_path);
        let location = resolve_cache_store_path()?;
        match previous {
            Some(value) => env::set_var("PX_CACHE_PATH", value),
            None => env::remove_var("PX_CACHE_PATH"),
        }

        assert_eq!(location.source, "PX_CACHE_PATH");
        assert_eq!(location.path.canonicalize()?, override_path.canonicalize()?);
        Ok(())
    }

    #[test]
    fn prefetch_hits_cached_artifacts() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let cache_root = temp.path();
        let wheel_path = cache_root.join("wheels/demo/1.0.0/demo-1.0.0.whl");
        fs::create_dir_all(wheel_path.parent().unwrap())?;
        wheel::write_dummy_wheel(&wheel_path, b"print('demo')")?;

        let sha = compute_sha256(&wheel_path)?;
        let name = "demo".to_string();
        let version = "1.0.0".to_string();
        let filename = "demo-1.0.0.whl".to_string();
        let url = "https://example.invalid/demo.whl".to_string();
        let specs = vec![PrefetchSpec {
            name: name.as_str(),
            version: version.as_str(),
            filename: filename.as_str(),
            url: url.as_str(),
            sha256: sha.as_str(),
        }];

        let summary = prefetch_artifacts(cache_root, &specs, PrefetchOptions::default());
        assert_eq!(summary.requested, 1);
        assert_eq!(summary.hit, 1);
        assert_eq!(summary.fetched, 0);
        assert_eq!(summary.failed, 0);
        assert!(wheel_path.with_extension("dist").exists());
        Ok(())
    }

    #[test]
    fn prune_cache_entries_deletes_and_reports_errors() -> Result<()> {
        let temp = tempdir()?;
        let present = temp.path().join("wheel/demo-1.0.0.whl");
        fs::create_dir_all(present.parent().unwrap())?;
        fs::write(&present, b"demo")?;
        let missing = temp.path().join("wheel/missing-1.0.0.whl");
        let walk = CacheWalk {
            exists: true,
            files: vec![
                CacheEntry {
                    path: present.clone(),
                    size: 4,
                },
                CacheEntry {
                    path: missing.clone(),
                    size: 9,
                },
            ],
            dirs: Vec::new(),
            total_bytes: 13,
        };

        let result = prune_cache_entries(&walk);

        assert_eq!(result.candidate_entries, 2);
        assert_eq!(result.candidate_size_bytes, 13);
        assert_eq!(result.deleted_entries, 1);
        assert_eq!(result.deleted_size_bytes, 4);
        assert_eq!(result.errors.len(), 1);
        assert!(
            result.errors[0].path.ends_with(missing),
            "expected error to refer to missing path"
        );
        assert!(
            !present.exists(),
            "present file should be removed after prune"
        );
        Ok(())
    }

    #[test]
    fn marker_matches_rejects_mismatched_checksum() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let marker = temp.path().join("wheel/.px-wheel.json");
        wheel::write_marker(&marker, "deadbeef")?;
        assert!(
            !marker_matches(&marker, "cafebabe"),
            "marker should reject differing checksum"
        );
        Ok(())
    }

    #[test]
    fn collect_cache_walk_sorts_entries() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();
        let alpha = root.join("a/alpha.whl");
        let beta = root.join("b/beta.whl");
        fs::create_dir_all(alpha.parent().unwrap())?;
        fs::create_dir_all(beta.parent().unwrap())?;
        fs::write(&alpha, b"a")?;
        fs::write(&beta, b"beta-bits")?;

        let walk = collect_cache_walk(root)?;

        assert!(walk.exists);
        assert_eq!(walk.total_bytes, 1 + 9);
        let files: Vec<&Path> = walk
            .files
            .iter()
            .map(|entry| entry.path.as_path())
            .collect();
        assert_eq!(files, vec![alpha.as_path(), beta.as_path()]);
        assert_eq!(
            walk.dirs.len(),
            2,
            "expected two child directories recorded"
        );
        Ok(())
    }

    #[test]
    fn ensure_wheel_dist_rebuilds_on_marker_mismatch() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let wheel = temp.path().join("demo-1.0.0.whl");
        wheel::write_dummy_wheel(&wheel, b"print('ok')")?;
        let sha = compute_sha256(&wheel)?;

        // First extraction writes a matching marker.
        let dist = ensure_wheel_dist(&wheel, &sha)?;
        let marker = dist.join(WHEEL_MARKER_NAME);
        assert!(marker.exists(), "expected marker to be written");

        // Corrupt the marker so a subsequent call must rebuild.
        wheel::write_marker(&marker, "cafebabe")?;
        let rebuilt = ensure_wheel_dist(&wheel, &sha)?;
        assert_eq!(rebuilt, dist, "rebuilt dist should reuse same path");
        let updated = fs::read_to_string(&marker)?;
        assert!(
            updated.contains(&sha),
            "marker should be updated to correct checksum"
        );
        Ok(())
    }

    #[test]
    fn apply_python_env_sets_python() -> Result<()> {
        let mut cmd = Command::new("echo");
        apply_python_env(&mut cmd);
        let envs: Vec<(String, String)> = cmd
            .get_envs()
            .filter_map(|(k, v)| {
                v.map(|value| {
                    (
                        k.to_string_lossy().into_owned(),
                        value.to_string_lossy().into_owned(),
                    )
                })
            })
            .collect();
        assert!(
            envs.iter().any(|(k, _)| k == "PYTHON"),
            "PYTHON should be set by apply_python_env"
        );
        Ok(())
    }
}
