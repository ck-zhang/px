use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;
use serde_json::json;
use tar::{Builder, Header};
use walkdir::WalkDir;

use crate::core::sandbox::sandbox_error;
use crate::InstallUserError;

use super::super::LayerTar;
use super::tar::{append_path, finalize_layer, layer_tar_builder};

pub(crate) fn write_env_layer_tar(
    env_root: &Path,
    runtime_root: Option<&Path>,
    blobs: &Path,
) -> Result<LayerTar, InstallUserError> {
    let mut builder = layer_tar_builder(blobs)?;
    let runtime_root = runtime_root.and_then(|path| path.canonicalize().ok());
    let runtime_root_str = runtime_root
        .as_ref()
        .and_then(|p| p.to_str())
        .map(|s| s.to_string());
    let env_root_canon = env_root
        .canonicalize()
        .unwrap_or_else(|_| env_root.to_path_buf());
    let store_mapping = discover_store_mapping(&env_root_canon)?;
    let mut extra_paths = Vec::new();
    if let Some(runtime_root) = runtime_root.as_ref() {
        let runtime_python = runtime_root.join("bin").join("python");
        for lib in shared_libs(&runtime_python) {
            if lib.starts_with(runtime_root) || lib.starts_with(&env_root_canon) {
                extra_paths.push(lib);
            }
        }
        extra_paths.push(runtime_python);
        extra_paths.sort();
        extra_paths.dedup();
    }
    let mut seen = HashSet::new();
    let walker = WalkDir::new(env_root).sort_by(|a, b| a.path().cmp(b.path()));
    for entry in walker {
        let entry = entry.map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to walk environment tree for sandbox image",
                json!({ "error": err.to_string() }),
            )
        })?;
        let path = entry.path();
        if path == env_root {
            continue;
        }
        let rel = match path.strip_prefix(env_root) {
            Ok(rel) => rel,
            Err(_) => continue,
        };
        let archive_path = Path::new("px").join("env").join(rel);
        if seen.insert(archive_path.clone()) {
            let is_python_shim = runtime_root.is_some()
                && (entry.file_type().is_file() || entry.file_type().is_symlink())
                && archive_path.starts_with(Path::new("px").join("env").join("bin"))
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|name| name.starts_with("python"))
                    .unwrap_or(false);
            if is_python_shim {
                append_rewritten_python(
                    &mut builder,
                    &archive_path,
                    path,
                    env_root,
                    runtime_root_str.as_deref(),
                    store_mapping
                        .as_ref()
                        .map(|mapping| mapping.host_root_str.as_str()),
                )?;
            } else if entry.file_type().is_file()
                && path.file_name().and_then(|n| n.to_str()) == Some("pyvenv.cfg")
            {
                append_rewritten_pyvenv(&mut builder, &archive_path, path)?;
            } else if entry.file_type().is_file()
                && path.extension().and_then(|ext| ext.to_str()) == Some("pth")
            {
                append_rewritten_pth(
                    &mut builder,
                    &archive_path,
                    path,
                    store_mapping
                        .as_ref()
                        .map(|mapping| mapping.host_root_str.as_str()),
                )?;
            } else {
                append_path(&mut builder, &archive_path, path)?;
            }
        }
    }
    if let Some(mapping) = store_mapping {
        for pkg_root in mapping.pkg_build_roots {
            let walker = WalkDir::new(&pkg_root).sort_by(|a, b| a.path().cmp(b.path()));
            for entry in walker {
                let entry = entry.map_err(|err| {
                    sandbox_error(
                        "PX903",
                        "failed to walk package build tree for sandbox image",
                        json!({ "error": err.to_string() }),
                    )
                })?;
                let path = entry.path();
                if path == pkg_root {
                    continue;
                }
                let rel = match path.strip_prefix(&mapping.host_root) {
                    Ok(rel) => rel,
                    Err(_) => continue,
                };
                if rel.as_os_str().is_empty() {
                    continue;
                }
                let archive_path = Path::new("px").join("store").join(rel);
                if seen.insert(archive_path.clone()) {
                    append_path(&mut builder, &archive_path, path)?;
                }
            }
        }
    }
    for host_path in extra_paths {
        if !host_path.exists() {
            continue;
        }
        let rel = host_path
            .strip_prefix("/")
            .unwrap_or(&host_path)
            .to_path_buf();
        if rel.as_os_str().is_empty() {
            continue;
        }
        if rel.components().count() == 0 {
            continue;
        }
        let archive_path = Path::new("").join(rel);
        if seen.insert(archive_path.clone()) {
            append_path(&mut builder, &archive_path, &host_path)?;
        }
    }
    finalize_layer(builder, blobs)
}

fn shared_libs(binary: &Path) -> Vec<PathBuf> {
    let mut libs = Vec::new();
    if !binary.exists() {
        return libs;
    }
    let Ok(output) = Command::new("ldd").arg(binary).output() else {
        return libs;
    };
    if !output.status.success() {
        return libs;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("linux-vdso") {
            continue;
        }
        let parts: Vec<_> = trimmed.split_whitespace().collect();
        let path = if parts.len() >= 3 && parts[1] == "=>" {
            parts[2]
        } else {
            parts.first().copied().unwrap_or_default()
        };
        if path.starts_with('/') && Path::new(path).exists() {
            libs.push(PathBuf::from(path));
        }
    }
    libs
}

#[derive(Clone, Debug)]
struct StoreMapping {
    host_root: PathBuf,
    host_root_str: String,
    pkg_build_roots: Vec<PathBuf>,
}

fn discover_store_mapping(env_root: &Path) -> Result<Option<StoreMapping>, InstallUserError> {
    let mut store_roots = BTreeSet::<String>::new();
    let mut pkg_build_roots = Vec::<PathBuf>::new();

    let walker = WalkDir::new(env_root).sort_by(|a, b| a.path().cmp(b.path()));
    for entry in walker {
        let entry = entry.map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to inspect environment for sandbox packaging",
                json!({ "error": err.to_string() }),
            )
        })?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("pth") {
            continue;
        }
        let contents = fs::read_to_string(path).map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to read environment path file for sandbox packaging",
                json!({ "path": path.display().to_string(), "error": err.to_string() }),
            )
        })?;
        for line in contents.lines() {
            let trimmed = line.trim();
            if !trimmed.starts_with('/') {
                continue;
            }
            let Some(index) = trimmed.find("/store/pkg-builds/") else {
                continue;
            };
            let Some(root) = trimmed.get(..index + "/store".len()) else {
                continue;
            };
            store_roots.insert(root.to_string());
            pkg_build_roots.push(PathBuf::from(trimmed));
        }
    }

    if pkg_build_roots.is_empty() {
        return Ok(None);
    }
    if store_roots.len() != 1 {
        return Err(sandbox_error(
            "PX904",
            "sandbox environment references multiple package stores",
            json!({
                "reason": "multiple_store_roots",
                "stores": store_roots.into_iter().collect::<Vec<_>>(),
            }),
        ));
    }
    let root = store_roots
        .into_iter()
        .next()
        .unwrap_or_else(|| "/".to_string());
    let host_root = PathBuf::from(&root)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(&root));
    let host_root_str = host_root.to_string_lossy().to_string();

    let mut canonical_roots = Vec::new();
    for raw in pkg_build_roots {
        let canonical = raw.canonicalize().unwrap_or(raw);
        if !canonical.exists() {
            return Err(sandbox_error(
                "PX903",
                "sandbox environment references a missing package build",
                json!({
                    "reason": "missing_pkg_build",
                    "path": canonical.display().to_string(),
                }),
            ));
        }
        if !canonical.starts_with(&host_root) {
            return Err(sandbox_error(
                "PX903",
                "sandbox environment references a package build outside the store root",
                json!({
                    "reason": "pkg_build_outside_store",
                    "path": canonical.display().to_string(),
                    "store_root": host_root_str,
                }),
            ));
        }
        canonical_roots.push(canonical);
    }
    canonical_roots.sort();
    canonical_roots.dedup();

    Ok(Some(StoreMapping {
        host_root,
        host_root_str,
        pkg_build_roots: canonical_roots,
    }))
}

fn append_rewritten_python(
    builder: &mut Builder<impl Write>,
    archive_path: &Path,
    path: &Path,
    env_root: &Path,
    runtime_root: Option<&str>,
    store_root: Option<&str>,
) -> Result<(), InstallUserError> {
    let runtime_root = runtime_root.unwrap_or("/px/runtime");
    let env_root = env_root
        .canonicalize()
        .unwrap_or_else(|_| env_root.to_path_buf());
    let env_root_str = env_root.to_string_lossy().to_string();
    let target_name = archive_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("python");
    let target = format!("/px/runtime/bin/{target_name}");
    if path.is_symlink() {
        let shim = "#!/bin/bash\nexec \"/px/env/bin/python\" \"$@\"\n";
        return append_bytes_deterministic(builder, archive_path, shim.as_bytes(), Some(0o755));
    }
    let contents = fs::read_to_string(path).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to read python shim for sandbox layer",
            json!({ "path": path.display().to_string(), "error": err.to_string() }),
        )
    })?;
    let mut rewritten_lines = Vec::new();
    rewritten_lines.push("#!/bin/bash".to_string());
    for line in contents.lines() {
        if line.trim_start().starts_with("#!") {
            continue;
        }
        let mut line = line.replace(&env_root_str, "/px/env");
        line = line.replace(runtime_root, "/px/runtime");
        if let Some(store_root) = store_root {
            line = line.replace(store_root, "/px/store");
        }
        if line.trim_start().starts_with("export LD_LIBRARY_PATH=") {
            if let Some(rewritten) = rewrite_ld_library_path(&line) {
                line = rewritten;
            }
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with("export PX_PYTHON") {
            line = format!(r#"export PX_PYTHON="{target}""#);
        } else if trimmed.starts_with("exec ") {
            line = format!(r#"exec "{target}" "$@""#);
        }
        rewritten_lines.push(line);
    }
    let rewritten = rewritten_lines.join("\n") + "\n";
    append_bytes_deterministic(builder, archive_path, rewritten.as_bytes(), Some(0o755))
}

fn rewrite_ld_library_path(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let rhs = trimmed.strip_prefix("export LD_LIBRARY_PATH=")?;
    let rhs = rhs.trim();
    let (quote, value) = match rhs.chars().next()? {
        '\'' => ('\'', rhs.trim_matches('\'')),
        '"' => ('"', rhs.trim_matches('"')),
        _ => ('\0', rhs),
    };
    let filtered: Vec<&str> = value
        .split(':')
        .map(|part| part.trim())
        .filter(|part| !part.is_empty())
        .filter(|part| !part.contains("/sys-libs"))
        .collect();
    let joined = filtered.join(":");
    if quote == '\0' {
        Some(format!("export LD_LIBRARY_PATH={joined}"))
    } else {
        Some(format!("export LD_LIBRARY_PATH={quote}{joined}{quote}"))
    }
}

fn append_rewritten_pth(
    builder: &mut Builder<impl Write>,
    archive_path: &Path,
    path: &Path,
    store_root: Option<&str>,
) -> Result<(), InstallUserError> {
    let contents = fs::read_to_string(path).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to read .pth file for sandbox layer",
            json!({ "path": path.display().to_string(), "error": err.to_string() }),
        )
    })?;
    let rewritten = if let Some(store_root) = store_root {
        contents.replace(store_root, "/px/store")
    } else {
        contents
    };
    let rewritten = if rewritten.ends_with('\n') {
        rewritten
    } else {
        format!("{rewritten}\n")
    };
    append_bytes_deterministic(builder, archive_path, rewritten.as_bytes(), Some(0o644))
}

fn append_rewritten_pyvenv(
    builder: &mut Builder<impl Write>,
    archive_path: &Path,
    path: &Path,
) -> Result<(), InstallUserError> {
    let contents = fs::read_to_string(path).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to read pyvenv.cfg for sandbox layer",
            json!({ "path": path.display().to_string(), "error": err.to_string() }),
        )
    })?;
    let mut lines = Vec::new();
    for line in contents.lines() {
        if line.trim_start().starts_with("home") {
            lines.push("home = /px/runtime".to_string());
        } else {
            lines.push(line.to_string());
        }
    }
    let rewritten = lines.join("\n") + "\n";
    append_bytes_deterministic(builder, archive_path, rewritten.as_bytes(), Some(0o644))
}

fn append_bytes_deterministic(
    builder: &mut Builder<impl Write>,
    archive_path: &Path,
    data: &[u8],
    mode: Option<u32>,
) -> Result<(), InstallUserError> {
    let mut header = Header::new_gnu();
    header.set_path(archive_path).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to stage sandbox entry",
            json!({ "path": archive_path.display().to_string(), "error": err.to_string() }),
        )
    })?;
    header.set_size(data.len() as u64);
    header.set_mode(mode.unwrap_or(0o644));
    header.set_mtime(0);
    header.set_uid(0);
    header.set_gid(0);
    header.set_cksum();
    builder.append(&header, data).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to write sandbox entry",
            json!({ "path": archive_path.display().to_string(), "error": err.to_string() }),
        )
    })
}
