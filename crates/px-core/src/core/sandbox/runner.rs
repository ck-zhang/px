use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::json;

use super::{
    sandbox_error, sandbox_timestamp_string, system_deps_mode, SandboxArtifacts,
    SandboxImageManifest, SandboxStore, SystemDepsMode,
};
use crate::core::runtime::process::{
    run_command, run_command_passthrough, run_command_streaming, run_command_with_stdin, RunOutput,
};
use crate::core::sandbox::pack::{
    build_oci_image, export_output, load_layer_from_blobs, write_env_layer_tar,
    write_base_os_layer, write_system_deps_layer,
};
use crate::{InstallUserError, PX_VERSION};

#[derive(Clone, Debug)]
pub(crate) struct SandboxImageLayout {
    pub(crate) archive: PathBuf,
    pub(crate) tag: String,
    pub(crate) image_digest: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BackendKind {
    Docker,
    Podman,
    Custom,
}

#[derive(Clone, Debug)]
pub(crate) struct ContainerBackend {
    pub(crate) program: PathBuf,
    pub(crate) kind: BackendKind,
}

#[derive(Clone, Debug)]
pub(crate) struct Mount {
    pub(crate) host: PathBuf,
    pub(crate) guest: PathBuf,
    pub(crate) read_only: bool,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum RunMode {
    Capture,
    Streaming,
    Passthrough,
    WithStdin(bool),
}

#[derive(Clone, Debug)]
pub(crate) struct ContainerRunArgs {
    pub(crate) env: Vec<(String, String)>,
    pub(crate) mounts: Vec<Mount>,
    pub(crate) workdir: PathBuf,
    pub(crate) program: String,
    pub(crate) args: Vec<String>,
}

pub(crate) fn ensure_image_layout(
    artifacts: &mut SandboxArtifacts,
    store: &SandboxStore,
    source_root: &Path,
    allowed_paths: &[PathBuf],
    default_tag: &str,
) -> Result<SandboxImageLayout, InstallUserError> {
    let backend = detect_container_backend()?;
    let needs_system = matches!(system_deps_mode(), SystemDepsMode::Strict)
        && !artifacts.definition.system_deps.apt_packages.is_empty()
        && !matches!(backend.kind, BackendKind::Custom);
    let needs_base_layer = !matches!(backend.kind, BackendKind::Custom);
    let sbx_id = artifacts.definition.sbx_id();
    let oci_dir = store.oci_dir(&sbx_id);
    let index = oci_dir.join("index.json");
    let blobs = oci_dir.join("blobs").join("sha256");
    let mut rebuild = !index.exists();
    let mut wrote_oci = false;
    if !rebuild {
        let digest = artifacts
            .manifest
            .image_digest
            .trim_start_matches("sha256:")
            .to_string();
        let manifest_path = blobs.join(&digest);
        let base_ok = artifacts
            .manifest
            .base_layer_digest
            .as_ref()
            .map(|d| blobs.join(d).exists())
            .unwrap_or(!needs_base_layer);
        let env_ok = artifacts
            .manifest
            .env_layer_digest
            .as_ref()
            .map(|d| blobs.join(d).exists())
            .unwrap_or(false);
        let sys_ok = match &artifacts.manifest.system_layer_digest {
            Some(d) => blobs.join(d).exists(),
            None => !needs_system,
        };
        rebuild = !manifest_path.exists() || !base_ok || !env_ok || !sys_ok;
    }
    if rebuild {
        if let Some(parent) = oci_dir.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                sandbox_error(
                    "PX903",
                    "failed to prepare sandbox image directory",
                    json!({ "path": parent.display().to_string(), "error": err.to_string() }),
                )
            })?;
        }
        fs::create_dir_all(&blobs).map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to prepare sandbox image directory",
                json!({ "path": blobs.display().to_string(), "error": err.to_string() }),
            )
        })?;
        let mut layers = Vec::new();
        if needs_base_layer {
            let base_layer = write_base_os_layer(&backend, &blobs)?;
            artifacts.manifest.base_layer_digest = Some(base_layer.digest.clone());
            layers.push(base_layer);
        } else {
            artifacts.manifest.base_layer_digest = None;
        }
        if let Some(system_layer) =
            write_system_deps_layer(&backend, &artifacts.definition.system_deps, &blobs)?
        {
            artifacts.manifest.system_layer_digest = Some(system_layer.digest.clone());
            layers.push(system_layer);
        } else {
            artifacts.manifest.system_layer_digest = None;
        }
        let runtime_root = crate::core::sandbox::pack::runtime_home_from_env(&artifacts.env_root);
        let env_layer = write_env_layer_tar(&artifacts.env_root, runtime_root.as_deref(), &blobs)?;
        layers.push(env_layer.clone());
        let mapped = map_allowed_paths(allowed_paths, source_root, &artifacts.env_root);
        let pythonpath = join_paths(&mapped)?;
        let built = build_oci_image(
            artifacts,
            &oci_dir,
            layers,
            Some(default_tag),
            Path::new("/app"),
            Some(&pythonpath),
        )?;
        artifacts.manifest.image_digest = format!("sha256:{}", built.manifest_digest);
        artifacts.manifest.env_layer_digest = Some(env_layer.digest);
        artifacts.manifest.created_at = sandbox_timestamp_string();
        artifacts.manifest.px_version = PX_VERSION.to_string();
        write_manifest(store, &artifacts.manifest)?;
        wrote_oci = true;
    }
    let tag = match discover_tag(&oci_dir)? {
        Some(tag) => tag,
        None => {
            let digest = artifacts.manifest.env_layer_digest.clone().ok_or_else(|| {
                sandbox_error(
                    "PX903",
                    "sandbox image metadata is incomplete",
                    json!({
                        "path": oci_dir.display().to_string(),
                        "reason": "missing_env_layer",
                    }),
                )
            })?;
            let mut layers = Vec::new();
            if needs_base_layer {
                let base_digest = artifacts.manifest.base_layer_digest.clone().ok_or_else(|| {
                    sandbox_error(
                        "PX903",
                        "sandbox image metadata is incomplete",
                        json!({
                            "path": oci_dir.display().to_string(),
                            "reason": "missing_base_layer",
                        }),
                    )
                })?;
                let base_layer = load_layer_from_blobs(&blobs, &base_digest)?;
                layers.push(base_layer);
            }
            if let Some(sys_digest) = artifacts.manifest.system_layer_digest.clone() {
                let sys_layer = load_layer_from_blobs(&blobs, &sys_digest)?;
                layers.push(sys_layer);
            }
            let env_layer = load_layer_from_blobs(&blobs, &digest)?;
            layers.push(env_layer);
            let mapped = map_allowed_paths(allowed_paths, source_root, &artifacts.env_root);
            let pythonpath = join_paths(&mapped)?;
            build_oci_image(
                artifacts,
                &oci_dir,
                layers,
                Some(default_tag),
                Path::new("/app"),
                Some(&pythonpath),
            )?;
            wrote_oci = true;
            default_tag.to_string()
        }
    };
    let archive = archive_path(&oci_dir);
    if wrote_oci || !archive.exists() {
        if let Some(parent) = archive.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                sandbox_error(
                    "PX903",
                    "failed to prepare sandbox archive directory",
                    json!({ "path": parent.display().to_string(), "error": err.to_string() }),
                )
            })?;
        }
        export_output(&oci_dir, &archive, Path::new("/"))?;
    }
    Ok(SandboxImageLayout {
        archive,
        tag,
        image_digest: artifacts.manifest.image_digest.clone(),
    })
}

pub(crate) fn detect_container_backend() -> Result<ContainerBackend, InstallUserError> {
    if let Ok(raw) = std::env::var("PX_SANDBOX_BACKEND") {
        let trimmed = raw.trim();
        if trimmed.eq_ignore_ascii_case("podman") {
            return Ok(ContainerBackend {
                program: resolve_program("podman", None)?,
                kind: BackendKind::Podman,
            });
        }
        if trimmed.eq_ignore_ascii_case("docker") {
            return Ok(ContainerBackend {
                program: resolve_program("docker", None)?,
                kind: BackendKind::Docker,
            });
        }
        let path = resolve_program(trimmed, Some(trimmed))?;
        return Ok(ContainerBackend {
            program: path,
            kind: BackendKind::Custom,
        });
    }

    for (name, kind) in [
        ("podman", BackendKind::Podman),
        ("docker", BackendKind::Docker),
    ] {
        if let Ok(path) = resolve_program(name, None) {
            return Ok(ContainerBackend {
                program: path,
                kind,
            });
        }
    }

    Err(sandbox_error(
        "PX903",
        "sandbox container backend unavailable",
        json!({
            "reason": "backend_unavailable",
            "candidates": ["podman", "docker"],
            "hint": "install podman or docker, or set PX_SANDBOX_BACKEND to a compatible binary",
        }),
    ))
}

pub(crate) fn ensure_image_loaded(
    backend: &ContainerBackend,
    layout: &SandboxImageLayout,
) -> Result<(), InstallUserError> {
    let marker = layout
        .archive
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("loaded-{}.txt", backend_name(backend)));
    let already_loaded = fs::read_to_string(&marker)
        .ok()
        .map(|raw| raw.trim().to_string())
        .is_some_and(|value| value == layout.image_digest);
    if already_loaded && image_exists(backend, &layout.tag) {
        return Ok(());
    }
    let args = vec![
        "load".to_string(),
        "--input".to_string(),
        layout.archive.display().to_string(),
    ];
    let output = run_command(
        backend.program.to_string_lossy().as_ref(),
        &args,
        &[],
        layout.archive.parent().unwrap_or_else(|| Path::new(".")),
    )
    .map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to load sandbox image",
            json!({ "error": err.to_string(), "backend": backend_name(backend) }),
        )
    })?;
    if output.code != 0 {
        return Err(sandbox_error(
            "PX903",
            "failed to load sandbox image",
            json!({
                "backend": backend_name(backend),
                "archive": layout.archive.display().to_string(),
                "code": output.code,
                "stdout": output.stdout,
                "stderr": output.stderr,
            }),
        ));
    }
    let _ = fs::write(&marker, format!("{}\n", layout.image_digest));
    Ok(())
}

pub(crate) fn run_container(
    backend: &ContainerBackend,
    layout: &SandboxImageLayout,
    opts: &ContainerRunArgs,
    mode: RunMode,
) -> Result<RunOutput, InstallUserError> {
    ensure_image_loaded(backend, layout)?;
    let args = build_run_args(opts, &layout.tag, needs_stdin(&mode));
    let program = backend.program.to_string_lossy().to_string();
    let cwd = if opts.workdir.exists() {
        canonical_or(opts.workdir.as_path())
    } else {
        PathBuf::from(".")
    };
    match mode {
        RunMode::Capture => run_command(&program, &args, &[], &cwd)
            .map_err(|err| container_error("failed to run sandbox command", backend, err)),
        RunMode::Streaming => run_command_streaming(&program, &args, &[], &cwd)
            .map_err(|err| container_error("failed to run sandbox command", backend, err)),
        RunMode::Passthrough => run_command_passthrough(&program, &args, &[], &cwd)
            .map_err(|err| container_error("failed to run sandbox command", backend, err)),
        RunMode::WithStdin(inherit) => run_command_with_stdin(&program, &args, &[], &cwd, inherit)
            .map_err(|err| container_error("failed to run sandbox command", backend, err)),
    }
}

fn container_error(
    message: &str,
    backend: &ContainerBackend,
    err: anyhow::Error,
) -> InstallUserError {
    sandbox_error(
        "PX903",
        message,
        json!({
            "backend": backend_name(backend),
            "error": err.to_string(),
        }),
    )
}

fn needs_stdin(mode: &RunMode) -> bool {
    matches!(mode, RunMode::Passthrough | RunMode::WithStdin(true))
}

fn build_run_args(opts: &ContainerRunArgs, tag: &str, interactive: bool) -> Vec<String> {
    let mut args = Vec::new();
    args.push("run".to_string());
    args.push("--rm".to_string());
    if interactive {
        args.push("-i".to_string());
    }
    args.push("--workdir".to_string());
    args.push(opts.workdir.display().to_string());

    for mount in unique_mounts(&opts.mounts) {
        let host = canonical_or(&mount.host);
        let guest = mount.guest.clone();
        args.push("--volume".to_string());
        let mode = if mount.read_only { "ro,Z" } else { "rw,Z" };
        args.push(format!("{}:{}:{mode}", host.display(), guest.display()));
    }

    for (key, value) in &opts.env {
        args.push("--env".to_string());
        args.push(format!("{key}={value}"));
    }

    args.push(tag.to_string());
    args.push(opts.program.clone());
    args.extend(opts.args.clone());
    args
}

fn unique_mounts(mounts: &[Mount]) -> Vec<Mount> {
    let mut seen = HashSet::new();
    let mut unique = Vec::new();
    for mount in mounts {
        let host = canonical_or(&mount.host);
        let guest = mount.guest.clone();
        if seen.insert((host.clone(), guest.clone())) {
            unique.push(Mount {
                host,
                guest,
                read_only: mount.read_only,
            });
        }
    }
    unique
}

fn canonical_or(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn archive_path(oci_dir: &Path) -> PathBuf {
    oci_dir.parent().unwrap_or(oci_dir).join("image.tar")
}

fn write_manifest(
    store: &SandboxStore,
    manifest: &SandboxImageManifest,
) -> Result<(), InstallUserError> {
    let path = store.image_manifest_path(&manifest.sbx_id);
    let encoded = serde_json::to_vec_pretty(manifest).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to encode sandbox image metadata",
            json!({ "error": err.to_string() }),
        )
    })?;
    fs::write(&path, encoded).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to write sandbox image metadata",
            json!({ "path": path.display().to_string(), "error": err.to_string() }),
        )
    })
}

fn map_allowed_paths(
    allowed_paths: &[PathBuf],
    project_root: &Path,
    env_root: &Path,
) -> Vec<PathBuf> {
    let container_project = Path::new("/app");
    let container_env = Path::new("/px/env");
    let mut mapped = Vec::new();
    for path in allowed_paths {
        let mapped_path = if path.starts_with(project_root) {
            Some(
                container_project.join(
                    path.strip_prefix(project_root)
                        .unwrap_or_else(|_| Path::new("")),
                ),
            )
        } else if path.starts_with(env_root) {
            Some(
                container_env.join(
                    path.strip_prefix(env_root)
                        .unwrap_or_else(|_| Path::new("")),
                ),
            )
        } else {
            None
        };
        if let Some(mapped_path) = mapped_path {
            if !mapped.iter().any(|p| p == &mapped_path) {
                mapped.push(mapped_path);
            }
        }
    }
    if !mapped.iter().any(|p| p == container_project) {
        mapped.insert(0, container_project.to_path_buf());
    }
    mapped
}

fn join_paths(paths: &[PathBuf]) -> Result<String, InstallUserError> {
    let joined = env::join_paths(paths).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to assemble sandbox PYTHONPATH",
            json!({ "error": err.to_string(), "paths": paths }),
        )
    })?;
    joined.into_string().map_err(|_| {
        sandbox_error(
            "PX903",
            "sandbox python path contains non-utf8 entries",
            json!({ "paths": paths }),
        )
    })
}

fn discover_tag(oci_dir: &Path) -> Result<Option<String>, InstallUserError> {
    let index_path = oci_dir.join("index.json");
    let contents = fs::read_to_string(&index_path).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to read sandbox OCI index",
            json!({ "path": index_path.display().to_string(), "error": err.to_string() }),
        )
    })?;
    let value: serde_json::Value = serde_json::from_str(&contents).map_err(|err| {
        sandbox_error(
            "PX904",
            "sandbox image metadata is incompatible with this px version",
            json!({ "path": index_path.display().to_string(), "error": err.to_string() }),
        )
    })?;
    let tag = value
        .get("manifests")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|item| item.get("annotations"))
        .and_then(|ann| ann.get("org.opencontainers.image.ref.name"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    Ok(tag)
}

fn resolve_program(name: &str, raw: Option<&str>) -> Result<PathBuf, InstallUserError> {
    let candidate = if name.contains(std::path::MAIN_SEPARATOR) || name.contains('/') {
        PathBuf::from(name)
    } else {
        which::which(name).unwrap_or_else(|_| PathBuf::from(name))
    };
    if candidate.exists() {
        return Ok(candidate);
    }
    Err(sandbox_error(
        "PX903",
        "sandbox container backend unavailable",
        json!({
            "reason": "backend_not_found",
            "backend": raw.unwrap_or(name),
        }),
    ))
}

fn image_exists(backend: &ContainerBackend, tag: &str) -> bool {
    let mut command = Command::new(&backend.program);
    command
        .arg("image")
        .arg("inspect")
        .arg(tag)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn backend_name(backend: &ContainerBackend) -> &'static str {
    match backend.kind {
        BackendKind::Docker => "docker",
        BackendKind::Podman => "podman",
        BackendKind::Custom => "custom",
    }
}
