use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::process::{
    run_command, run_command_passthrough, run_command_streaming, run_command_with_stdin, RunOutput,
};
use crate::python_sys::detect_interpreter;
use crate::store::{
    cache_wheel, collect_cache_walk, compute_cache_usage, ensure_sdist_build, prefetch_artifacts,
    prune_cache_entries, resolve_cache_store_path, ArtifactRequest, BuiltWheel, CacheLocation,
    CachePruneResult, CacheUsage, CacheWalk, CachedArtifact,
    PrefetchOptions as StorePrefetchOptions, PrefetchSpec, PrefetchSummary as StorePrefetchSummary,
    SdistRequest,
};
use anyhow::{Context, Result};

use crate::pypi::PypiReleaseResponse;

pub trait PythonRuntime: Send + Sync {
    fn detect_interpreter(&self) -> Result<String>;
    fn run_command(
        &self,
        python: &str,
        args: &[String],
        env: &[(String, String)],
        cwd: &Path,
    ) -> Result<RunOutput>;
    fn run_command_streaming(
        &self,
        python: &str,
        args: &[String],
        env: &[(String, String)],
        cwd: &Path,
    ) -> Result<RunOutput>;
    fn run_command_with_stdin(
        &self,
        python: &str,
        args: &[String],
        env: &[(String, String)],
        cwd: &Path,
        inherit_stdin: bool,
    ) -> Result<RunOutput>;
    fn run_command_passthrough(
        &self,
        python: &str,
        args: &[String],
        env: &[(String, String)],
        cwd: &Path,
    ) -> Result<RunOutput>;
}

pub trait GitClient: Send + Sync {
    fn worktree_changes(&self, root: &Path) -> Result<Option<Vec<String>>>;
}

pub trait FileSystem: Send + Sync {
    fn read_to_string(&self, path: &Path) -> Result<String>;
    fn write(&self, path: &Path, contents: &[u8]) -> Result<()>;
    fn create_dir_all(&self, path: &Path) -> Result<()>;
    fn remove_file(&self, path: &Path) -> Result<()>;
    fn remove_dir_all(&self, path: &Path) -> Result<()>;
    fn copy(&self, src: &Path, dest: &Path) -> Result<()>;
    fn metadata(&self, path: &Path) -> Result<std::fs::Metadata>;
    fn read_dir(&self, path: &Path) -> Result<std::fs::ReadDir>;
    fn canonicalize(&self, path: &Path) -> Result<PathBuf>;
}

pub trait CacheStore: Send + Sync {
    fn resolve_store_path(&self) -> Result<CacheLocation>;
    fn compute_usage(&self, path: &Path) -> Result<CacheUsage>;
    fn collect_walk(&self, path: &Path) -> Result<CacheWalk>;
    fn prune(&self, walk: &CacheWalk) -> CachePruneResult;
    fn prefetch(
        &self,
        cache: &Path,
        specs: &[PrefetchSpec<'_>],
        options: StorePrefetchOptions,
    ) -> Result<StorePrefetchSummary>;
    fn cache_wheel(&self, cache: &Path, request: &ArtifactRequest) -> Result<CachedArtifact>;
    fn ensure_sdist_build(&self, cache: &Path, request: &SdistRequest) -> Result<BuiltWheel>;
}

pub trait PypiClient: Send + Sync {
    fn fetch_release(
        &self,
        normalized: &str,
        version: &str,
        specifier: &str,
    ) -> Result<PypiReleaseResponse>;
}

pub trait Effects: Send + Sync {
    fn python(&self) -> &dyn PythonRuntime;
    fn git(&self) -> &dyn GitClient;
    fn fs(&self) -> &dyn FileSystem;
    fn cache(&self) -> &dyn CacheStore;
    fn pypi(&self) -> &dyn PypiClient;
}

pub struct SystemEffects {
    python: Arc<SystemPythonRuntime>,
    git: Arc<SystemGit>,
    fs: Arc<SystemFileSystem>,
    cache: Arc<SystemCacheStore>,
    pypi: Arc<SystemPypiClient>,
}

impl SystemEffects {
    #[must_use]
    pub fn new() -> Self {
        Self {
            python: Arc::new(SystemPythonRuntime),
            git: Arc::new(SystemGit),
            fs: Arc::new(SystemFileSystem),
            cache: Arc::new(SystemCacheStore),
            pypi: Arc::new(SystemPypiClient),
        }
    }
}

impl Default for SystemEffects {
    fn default() -> Self {
        Self::new()
    }
}

impl Effects for SystemEffects {
    fn python(&self) -> &dyn PythonRuntime {
        self.python.as_ref()
    }

    fn git(&self) -> &dyn GitClient {
        self.git.as_ref()
    }

    fn fs(&self) -> &dyn FileSystem {
        self.fs.as_ref()
    }

    fn cache(&self) -> &dyn CacheStore {
        self.cache.as_ref()
    }

    fn pypi(&self) -> &dyn PypiClient {
        self.pypi.as_ref()
    }
}

struct SystemPythonRuntime;

impl PythonRuntime for SystemPythonRuntime {
    fn detect_interpreter(&self) -> Result<String> {
        detect_interpreter()
    }

    fn run_command(
        &self,
        python: &str,
        args: &[String],
        env: &[(String, String)],
        cwd: &Path,
    ) -> Result<RunOutput> {
        run_command(python, args, env, cwd)
    }

    fn run_command_streaming(
        &self,
        python: &str,
        args: &[String],
        env: &[(String, String)],
        cwd: &Path,
    ) -> Result<RunOutput> {
        run_command_streaming(python, args, env, cwd)
    }

    fn run_command_with_stdin(
        &self,
        python: &str,
        args: &[String],
        env: &[(String, String)],
        cwd: &Path,
        inherit_stdin: bool,
    ) -> Result<RunOutput> {
        run_command_with_stdin(python, args, env, cwd, inherit_stdin)
    }

    fn run_command_passthrough(
        &self,
        python: &str,
        args: &[String],
        env: &[(String, String)],
        cwd: &Path,
    ) -> Result<RunOutput> {
        run_command_passthrough(python, args, env, cwd)
    }
}

struct SystemGit;

impl GitClient for SystemGit {
    fn worktree_changes(&self, root: &Path) -> Result<Option<Vec<String>>> {
        let output = std::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(root)
            .output();
        match output {
            Ok(out) if out.status.success() => {
                let lines = String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .map(ToString::to_string)
                    .collect::<Vec<_>>();
                Ok(Some(lines))
            }
            Ok(_) | Err(_) => Ok(None),
        }
    }
}

struct SystemFileSystem;

impl FileSystem for SystemFileSystem {
    fn read_to_string(&self, path: &Path) -> Result<String> {
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
    }

    fn write(&self, path: &Path, contents: &[u8]) -> Result<()> {
        std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))
    }

    fn create_dir_all(&self, path: &Path) -> Result<()> {
        std::fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))
    }

    fn remove_file(&self, path: &Path) -> Result<()> {
        std::fs::remove_file(path).with_context(|| format!("removing file {}", path.display()))
    }

    fn remove_dir_all(&self, path: &Path) -> Result<()> {
        std::fs::remove_dir_all(path).with_context(|| format!("removing dir {}", path.display()))
    }

    fn copy(&self, src: &Path, dest: &Path) -> Result<()> {
        std::fs::copy(src, dest)
            .map(|_| ())
            .with_context(|| format!("copying {} to {}", src.display(), dest.display()))
    }

    fn metadata(&self, path: &Path) -> Result<std::fs::Metadata> {
        std::fs::metadata(path).with_context(|| format!("metadata for {}", path.display()))
    }

    fn read_dir(&self, path: &Path) -> Result<std::fs::ReadDir> {
        std::fs::read_dir(path).with_context(|| format!("reading dir {}", path.display()))
    }

    fn canonicalize(&self, path: &Path) -> Result<PathBuf> {
        std::fs::canonicalize(path).with_context(|| format!("canonicalizing {}", path.display()))
    }
}

struct SystemCacheStore;

impl CacheStore for SystemCacheStore {
    fn resolve_store_path(&self) -> Result<CacheLocation> {
        resolve_cache_store_path()
    }

    fn compute_usage(&self, path: &Path) -> Result<CacheUsage> {
        compute_cache_usage(path)
    }

    fn collect_walk(&self, path: &Path) -> Result<CacheWalk> {
        collect_cache_walk(path)
    }

    fn prune(&self, walk: &CacheWalk) -> CachePruneResult {
        prune_cache_entries(walk)
    }

    fn prefetch(
        &self,
        cache: &Path,
        specs: &[PrefetchSpec<'_>],
        options: StorePrefetchOptions,
    ) -> Result<StorePrefetchSummary> {
        Ok(prefetch_artifacts(cache, specs, options))
    }

    fn cache_wheel(&self, cache: &Path, request: &ArtifactRequest) -> Result<CachedArtifact> {
        cache_wheel(cache, request)
    }

    fn ensure_sdist_build(&self, cache: &Path, request: &SdistRequest) -> Result<BuiltWheel> {
        ensure_sdist_build(cache, request)
    }
}

struct SystemPypiClient;

impl PypiClient for SystemPypiClient {
    fn fetch_release(
        &self,
        normalized: &str,
        version: &str,
        specifier: &str,
    ) -> Result<PypiReleaseResponse> {
        let client = crate::build_http_client()?;
        crate::fetch_release(&client, normalized, version, specifier)
    }
}

pub type SharedEffects = Arc<dyn Effects>;
