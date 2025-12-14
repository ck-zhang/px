use std::collections::{BTreeSet, HashSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;
use ignore::WalkBuilder;
use serde_json::json;
use sha2::{Digest, Sha256};
use tar::{Builder, EntryType, Header, HeaderMode};
use tempfile::NamedTempFile;
use walkdir::WalkDir;

use super::super::runner::{BackendKind, ContainerBackend};
use super::super::{sandbox_error, system_deps_mode, SystemDepsMode, SYSTEM_DEPS_IMAGE};
use super::LayerTar;
use crate::core::system_deps::SystemDeps;
use crate::InstallUserError;

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
                let archive_path = Path::new("px").join("store").join(rel);
                if seen.insert(archive_path.clone()) {
                    append_path(&mut builder, &archive_path, path)?;
                }
            }
        }
    }
    if let Some(runtime_root) = runtime_root {
        let walker = WalkDir::new(&runtime_root)
            .sort_by(|a, b| a.path().cmp(b.path()))
            .into_iter()
            .filter_map(Result::ok);
        for entry in walker {
            let path = entry.path();
            if path == runtime_root {
                continue;
            }
            let rel = match path.strip_prefix(&runtime_root) {
                Ok(rel) => rel,
                Err(_) => continue,
            };
            let is_python_related = rel.components().any(|comp| {
                comp.as_os_str()
                    .to_str()
                    .map(|name| name.starts_with("python") || name.starts_with("libpython"))
                    .unwrap_or(false)
            });
            if !is_python_related {
                continue;
            }
            let archive_path = Path::new("px").join("runtime").join(rel);
            if seen.insert(archive_path.clone()) {
                append_path(&mut builder, &archive_path, path)?;
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

pub(crate) fn write_system_deps_layer(
    backend: &ContainerBackend,
    deps: &SystemDeps,
    blobs: &Path,
) -> Result<Option<LayerTar>, InstallUserError> {
    if deps.apt_packages.is_empty() || matches!(system_deps_mode(), SystemDepsMode::Offline) {
        return Ok(None);
    }
    if matches!(backend.kind, BackendKind::Custom) {
        return Ok(None);
    }
    let rootfs = match crate::core::sandbox::ensure_system_deps_rootfs(deps)? {
        Some(path) => path,
        None => return Ok(None),
    };
    let layer = write_rootfs_layer(&rootfs, blobs)?;
    Ok(Some(layer))
}

pub(crate) fn write_base_os_layer(
    backend: &ContainerBackend,
    blobs: &Path,
) -> Result<LayerTar, InstallUserError> {
    fs::create_dir_all(blobs).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to prepare OCI blob directory",
            json!({ "path": blobs.display().to_string(), "error": err.to_string() }),
        )
    })?;

    let create = Command::new(&backend.program)
        .arg("create")
        .arg(SYSTEM_DEPS_IMAGE)
        .output()
        .map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to create base sandbox container",
                json!({ "error": err.to_string(), "image": SYSTEM_DEPS_IMAGE }),
            )
        })?;
    if !create.status.success() {
        return Err(sandbox_error(
            "PX903",
            "failed to create base sandbox container",
            json!({
                "image": SYSTEM_DEPS_IMAGE,
                "code": create.status.code(),
                "stdout": String::from_utf8_lossy(&create.stdout).to_string(),
                "stderr": String::from_utf8_lossy(&create.stderr).to_string(),
            }),
        ));
    }
    let id = String::from_utf8_lossy(&create.stdout).trim().to_string();
    if id.is_empty() {
        return Err(sandbox_error(
            "PX903",
            "failed to create base sandbox container",
            json!({
                "image": SYSTEM_DEPS_IMAGE,
                "reason": "missing_container_id",
                "stdout": String::from_utf8_lossy(&create.stdout).to_string(),
            }),
        ));
    }

    let temp = NamedTempFile::new_in(blobs).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to create base sandbox layer",
            json!({ "path": blobs.display().to_string(), "error": err.to_string() }),
        )
    })?;
    let out_file = temp.reopen().map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to prepare base sandbox layer",
            json!({ "error": err.to_string() }),
        )
    })?;
    let export = Command::new(&backend.program)
        .arg("export")
        .arg(&id)
        .stdout(out_file)
        .stderr(std::process::Stdio::piped())
        .output();

    let _ = Command::new(&backend.program)
        .arg("rm")
        .arg("-f")
        .arg(&id)
        .output();

    let export = export.map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to export base sandbox filesystem",
            json!({ "error": err.to_string(), "image": SYSTEM_DEPS_IMAGE }),
        )
    })?;
    if !export.status.success() {
        return Err(sandbox_error(
            "PX903",
            "failed to export base sandbox filesystem",
            json!({
                "image": SYSTEM_DEPS_IMAGE,
                "code": export.status.code(),
                "stdout": String::from_utf8_lossy(&export.stdout).to_string(),
                "stderr": String::from_utf8_lossy(&export.stderr).to_string(),
            }),
        ));
    }

    let mut file = File::open(temp.path()).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to read base sandbox layer",
            json!({ "path": temp.path().display().to_string(), "error": err.to_string() }),
        )
    })?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to read base sandbox layer",
                json!({ "path": temp.path().display().to_string(), "error": err.to_string() }),
            )
        })?;
        if read == 0 {
            break;
        }
        size = size.saturating_add(read as u64);
        hasher.update(&buffer[..read]);
    }
    let digest = format!("{:x}", hasher.finalize());
    let layer_path = blobs.join(&digest);
    if !layer_path.exists() {
        match temp.persist_noclobber(&layer_path) {
            Ok(_) => {}
            Err(err) => {
                if err.error.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(sandbox_error(
                        "PX903",
                        "failed to write base sandbox layer",
                        json!({
                            "path": layer_path.display().to_string(),
                            "error": err.error.to_string(),
                        }),
                    ));
                }
            }
        }
    }

    Ok(LayerTar {
        digest,
        size,
        path: layer_path,
    })
}

fn write_rootfs_layer(rootfs: &Path, blobs: &Path) -> Result<LayerTar, InstallUserError> {
    let mut builder = layer_tar_builder(blobs)?;
    let walker = WalkDir::new(rootfs).sort_by(|a, b| a.path().cmp(b.path()));
    for entry in walker {
        let entry = entry.map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to walk system dependency tree",
                json!({ "error": err.to_string() }),
            )
        })?;
        let path = entry.path();
        if path == rootfs {
            continue;
        }
        let rel = match path.strip_prefix(rootfs) {
            Ok(rel) => rel,
            Err(_) => continue,
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        let archive_path = Path::new("").join(rel);
        append_path(&mut builder, &archive_path, path)?;
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

pub(super) fn write_app_layer_tar(
    source_root: &Path,
    blobs: &Path,
) -> Result<LayerTar, InstallUserError> {
    let mut builder = layer_tar_builder(blobs)?;
    let mut walker = WalkBuilder::new(source_root);
    walker
        .git_ignore(true)
        .git_exclude(true)
        .parents(true)
        .hidden(false)
        .ignore(true)
        .sort_by_file_name(|a, b| a.cmp(b));
    for entry in walker.build() {
        let entry = entry.map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to walk source tree for sandbox pack",
                json!({ "error": err.to_string() }),
            )
        })?;
        let path = entry.path();
        if path == source_root || should_skip(path, source_root) {
            continue;
        }
        let rel = match path.strip_prefix(source_root) {
            Ok(rel) => rel,
            Err(_) => continue,
        };
        let archive_path = Path::new("app").join(rel);
        append_path(&mut builder, &archive_path, path)?;
    }
    finalize_layer(builder, blobs)
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

fn append_path<W: Write>(
    builder: &mut Builder<W>,
    archive_path: &Path,
    path: &Path,
) -> Result<(), InstallUserError> {
    let metadata = fs::symlink_metadata(path).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to read source metadata for sandbox layer",
            json!({ "path": path.display().to_string(), "error": err.to_string() }),
        )
    })?;
    if metadata.is_dir() {
        builder.append_dir(archive_path, path).map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to stage directory for sandbox layer",
                json!({ "path": path.display().to_string(), "error": err.to_string() }),
            )
        })?;
    } else if metadata.file_type().is_symlink() {
        let target = fs::read_link(path).map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to read symlink target for sandbox layer",
                json!({ "path": path.display().to_string(), "error": err.to_string() }),
            )
        })?;
        let mut header = Header::new_gnu();
        header.set_metadata_in_mode(&metadata, HeaderMode::Deterministic);
        header.set_entry_type(EntryType::Symlink);
        header.set_size(0);
        builder
            .append_link(&mut header, archive_path, &target)
            .map_err(|err| {
                sandbox_error(
                    "PX903",
                    "failed to add symlink to sandbox layer",
                    json!({ "path": path.display().to_string(), "error": err.to_string() }),
                )
            })?;
    } else {
        let mut file = File::open(path).map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to read source file for sandbox layer",
                json!({ "path": path.display().to_string(), "error": err.to_string() }),
            )
        })?;
        builder
            .append_file(archive_path, &mut file)
            .map_err(|err| {
                sandbox_error(
                    "PX903",
                    "failed to add file to sandbox layer",
                    json!({ "path": path.display().to_string(), "error": err.to_string() }),
                )
            })?;
    }
    Ok(())
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

struct HashingWriter<W> {
    inner: W,
    hasher: Sha256,
    bytes_written: u64,
}

impl<W> HashingWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            bytes_written: 0,
        }
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.hasher.update(&buf[..written]);
        self.bytes_written = self
            .bytes_written
            .saturating_add(written.try_into().unwrap_or(u64::MAX));
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn layer_tar_builder(
    blobs: &Path,
) -> Result<Builder<HashingWriter<NamedTempFile>>, InstallUserError> {
    fs::create_dir_all(blobs).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to prepare layer directory",
            json!({ "path": blobs.display().to_string(), "error": err.to_string() }),
        )
    })?;
    let file = NamedTempFile::new_in(blobs).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to create sandbox layer",
            json!({ "path": blobs.display().to_string(), "error": err.to_string() }),
        )
    })?;
    Ok(Builder::new(HashingWriter::new(file)))
}

fn finalize_layer(
    builder: Builder<HashingWriter<NamedTempFile>>,
    blobs: &Path,
) -> Result<LayerTar, InstallUserError> {
    let writer = builder.into_inner().map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to finalize sandbox layer",
            json!({ "error": err.to_string() }),
        )
    })?;
    let HashingWriter {
        inner: temp,
        hasher,
        bytes_written: size,
    } = writer;
    let digest = format!("{:x}", hasher.finalize());
    let layer_path = blobs.join(&digest);
    if !layer_path.exists() {
        match temp.persist_noclobber(&layer_path) {
            Ok(_) => {}
            Err(err) => {
                if err.error.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(sandbox_error(
                        "PX903",
                        "failed to write sandbox layer",
                        json!({
                            "path": layer_path.display().to_string(),
                            "error": err.error.to_string(),
                        }),
                    ));
                }
            }
        }
    }
    Ok(LayerTar {
        digest,
        size,
        path: layer_path,
    })
}

fn should_skip(path: &Path, root: &Path) -> bool {
    if path == root {
        return false;
    }
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    matches!(
        name,
        ".git"
            | ".px"
            | "__pycache__"
            | "target"
            | "dist"
            | ".mypy_cache"
            | ".pytest_cache"
            | ".nox"
            | ".tox"
            | ".venv"
            | "venv"
            | ".ruff_cache"
    ) || name.ends_with(".pyc")
}
