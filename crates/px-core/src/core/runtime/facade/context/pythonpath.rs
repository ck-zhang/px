use std::env;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use toml_edit::{DocumentMut, Item};

use crate::core::runtime as effects;

use super::super::env_materialize::resolve_project_site;

pub(crate) struct PythonPathInfo {
    pub(crate) pythonpath: String,
    pub(crate) allowed_paths: Vec<PathBuf>,
    pub(crate) site_bin: Option<PathBuf>,
    pub(crate) pep582_bin: Vec<PathBuf>,
}

pub(in super::super) fn detect_local_site_packages(
    fs: &dyn effects::FileSystem,
    site_dir: &Path,
) -> Option<PathBuf> {
    let lib_dir = site_dir.join("lib");
    if let Ok(entries) = fs.read_dir(&lib_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if let Some(name) = path.file_name().and_then(|value| value.to_str()) {
                if !name.starts_with("python") {
                    continue;
                }
            }
            let candidate = path.join("site-packages");
            if fs.metadata(&candidate).is_ok() {
                return Some(candidate);
            }
        }
    }
    let fallback = site_dir.join("site-packages");
    fs.metadata(&fallback).ok().map(|_| fallback)
}

fn discover_code_generator_paths(
    fs: &dyn effects::FileSystem,
    project_root: &Path,
    max_depth: usize,
) -> Vec<PathBuf> {
    let _timing = crate::tooling::timings::TimingGuard::new("discover_code_generators");

    fn should_skip_dir(name: &str) -> bool {
        matches!(
            name,
            ".git"
                | ".px"
                | "__pycache__"
                | ".pytest_cache"
                | ".mypy_cache"
                | ".ruff_cache"
                | "tests"
                | "test"
                | ".cache"
                | ".venv"
                | ".tox"
                | "target"
                | "dist"
                | "build"
                | "artifacts"
                | "node_modules"
                | ".idea"
                | ".vscode"
        )
    }

    let mut extras = Vec::new();
    let mut stack = vec![(project_root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = fs.read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = entry.file_name();
            if name
                .to_str()
                .is_some_and(|value| value == "code_generators")
            {
                extras.push(path.clone());
                continue;
            }
            if name.to_str().is_some_and(should_skip_dir) {
                continue;
            }
            if depth < max_depth {
                stack.push((path, depth + 1));
            }
        }
    }
    extras
}

fn contains_native_sources(
    fs: &dyn effects::FileSystem,
    project_root: &Path,
    max_depth: usize,
) -> bool {
    fn should_skip_dir(name: &str) -> bool {
        matches!(
            name,
            ".git"
                | ".px"
                | "__pycache__"
                | ".pytest_cache"
                | ".mypy_cache"
                | ".ruff_cache"
                | "tests"
                | "test"
                | ".cache"
                | ".venv"
                | ".tox"
                | "target"
                | "dist"
                | "build"
                | "artifacts"
                | "node_modules"
                | ".idea"
                | ".vscode"
        )
    }

    let mut stack = vec![(project_root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = fs.read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if depth < max_depth {
                    if let Some(name) = entry.file_name().to_str() {
                        if should_skip_dir(name) {
                            continue;
                        }
                    }
                    stack.push((path, depth + 1));
                }
                continue;
            }
            let Some(ext) = path.extension().and_then(|value| value.to_str()) else {
                continue;
            };
            if matches!(
                ext,
                "c" | "cc" | "cpp" | "cxx" | "h" | "hpp" | "hh" | "hxx" | "pyx" | "pxd"
            ) {
                return true;
            }
        }
    }
    false
}

fn prefers_installed_project_wheel(fs: &dyn effects::FileSystem, project_root: &Path) -> bool {
    let manifest = project_root.join("pyproject.toml");
    let Ok(contents) = fs.read_to_string(&manifest) else {
        return false;
    };
    let Ok(doc) = contents.parse::<DocumentMut>() else {
        return false;
    };
    let build_system = doc.get("build-system").and_then(Item::as_table);
    let build_backend = build_system
        .and_then(|table| table.get("build-backend"))
        .and_then(Item::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if build_backend == "mesonpy" || build_backend == "maturin" {
        return true;
    }
    let requires = build_system
        .and_then(|table| table.get("requires"))
        .and_then(Item::as_array)
        .map(|array| {
            array
                .iter()
                .filter_map(|value| value.as_str())
                .map(|value| value.to_ascii_lowercase())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let is_setuptools = build_backend.starts_with("setuptools")
        || (build_backend.is_empty() && requires.iter().any(|req| req.contains("setuptools")));
    if is_setuptools
        && requires.iter().any(|req| {
            req.contains("cython")
                || req.contains("scikit-build-core")
                || req.contains("setuptools-rust")
        })
    {
        return true;
    }
    if contains_native_sources(fs, project_root, 4) {
        return true;
    }
    requires
        .iter()
        .any(|req| req.contains("meson-python") || req.contains("maturin"))
}

pub(crate) fn build_pythonpath(
    fs: &dyn effects::FileSystem,
    project_root: &Path,
    site_override: Option<PathBuf>,
) -> Result<PythonPathInfo> {
    let _timing = crate::tooling::timings::TimingGuard::new("build_pythonpath");

    let site_dir = match site_override {
        Some(dir) => dir,
        None => resolve_project_site(fs, project_root)?,
    };

    let mut site_paths = Vec::new();
    let mut site_packages_used = None;
    let code_paths = discover_code_generator_paths(fs, project_root, 3);

    let canonical = fs.canonicalize(&site_dir).unwrap_or(site_dir.clone());
    let site_dir_used = Some(canonical.clone());
    site_paths.push(canonical.clone());
    if let Some(site_packages) = detect_local_site_packages(fs, &canonical) {
        site_packages_used = Some(site_packages.clone());
        site_paths.push(site_packages.clone());
        if let Ok(canon) = fs.canonicalize(&site_packages) {
            if canon != site_packages {
                site_paths.push(canon);
            }
        }
    }
    #[cfg(windows)]
    {
        let legacy_site_packages = canonical.join("Lib").join("site-packages");
        if fs.metadata(&legacy_site_packages).is_ok() {
            let should_add = site_packages_used
                .as_ref()
                .map(|existing| existing != &legacy_site_packages)
                .unwrap_or(true);
            if should_add {
                site_paths.push(legacy_site_packages.clone());
                if let Ok(canon) = fs.canonicalize(&legacy_site_packages) {
                    if canon != legacy_site_packages {
                        site_paths.push(canon);
                    }
                }
            }
        }
    }

    let mut project_paths = Vec::new();
    let src = project_root.join("src");
    if src.exists() {
        project_paths.push(src);
    }
    let lib = project_root.join("lib");
    if lib.exists() && looks_like_python_source_root(fs, &lib) {
        project_paths.push(lib);
    }
    let python_dir = project_root.join("python");
    if python_dir.exists() {
        project_paths.push(python_dir);
    }
    let mut child_projects = Vec::new();
    if let Ok(entries) = fs.read_dir(project_root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let manifest = path.join("pyproject.toml");
            if fs.metadata(&manifest).is_ok() {
                child_projects.push(path);
            }
        }
    }
    child_projects.sort();
    for path in child_projects {
        if path != project_root {
            project_paths.push(path);
        }
    }
    project_paths.push(project_root.to_path_buf());

    let prefer_wheel = prefers_installed_project_wheel(fs, project_root);

    let mut pep582_libs = Vec::new();
    let mut pep582_bins = Vec::new();
    let pep582_root = project_root.join("__pypackages__");
    if pep582_root.exists() {
        if let Ok(entries) = fs.read_dir(&pep582_root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let lib = path.join("lib");
                if lib.exists() {
                    pep582_libs.push(lib);
                } else {
                    pep582_libs.push(path.clone());
                }
                let bin = path.join("bin");
                if bin.exists() {
                    pep582_bins.push(bin);
                }
            }
        }
    }

    let mut paths = Vec::new();
    if let Some(dir) = site_dir_used.as_ref() {
        paths.push(dir.clone());
    }
    if prefer_wheel {
        if let Some(pkgs) = site_packages_used.as_ref() {
            paths.push(pkgs.clone());
        }
        for path in &site_paths {
            if Some(path) == site_dir_used.as_ref() {
                continue;
            }
            if site_packages_used.as_ref().is_some_and(|pkgs| pkgs == path) {
                continue;
            }
            paths.push(path.clone());
        }
        paths.extend(code_paths.clone());
        paths.extend(project_paths.clone());
    } else {
        paths.extend(code_paths.clone());
        paths.extend(project_paths.clone());
        if let Some(pkgs) = site_packages_used.as_ref() {
            paths.push(pkgs.clone());
        }
        for path in &site_paths {
            if Some(path) == site_dir_used.as_ref() {
                continue;
            }
            if site_packages_used.as_ref().is_some_and(|pkgs| pkgs == path) {
                continue;
            }
            if project_paths.iter().any(|pkg| pkg == path) {
                continue;
            }
            if code_paths.iter().any(|extra| extra == path) {
                continue;
            }
            paths.push(path.clone());
        }
    }
    paths.extend(pep582_libs);
    paths.retain(|p| p.exists());
    if paths.is_empty() {
        paths.push(project_root.to_path_buf());
    }

    let joined = env::join_paths(&paths).context("failed to build PYTHONPATH")?;
    let pythonpath = joined
        .into_string()
        .map_err(|_| anyhow!("pythonpath contains non-UTF paths"))?;
    let site_bin = site_dir_used
        .map(|dir| dir.join("bin"))
        .filter(|bin| bin.exists());
    Ok(PythonPathInfo {
        pythonpath,
        allowed_paths: paths,
        site_bin,
        pep582_bin: pep582_bins,
    })
}

fn looks_like_python_source_root(fs: &dyn effects::FileSystem, root: &Path) -> bool {
    let Ok(entries) = fs.read_dir(root) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if fs.metadata(&path.join("__init__.py")).is_ok() {
                return true;
            }
            continue;
        }
        if path
            .extension()
            .is_some_and(|ext| matches!(ext.to_str(), Some("py" | "pyi")))
        {
            return true;
        }
    }
    false
}
