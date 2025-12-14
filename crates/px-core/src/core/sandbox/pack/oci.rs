use std::env;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::Result;
use oci_distribution::client::{Client, ClientConfig, Config as OciConfig, ImageLayer};
use oci_distribution::manifest::OciImageManifest;
use oci_distribution::secrets::RegistryAuth;
use oci_distribution::Reference;
use serde_json::json;
use sha2::{Digest, Sha256};
use tar::Builder;
use tokio::runtime::Runtime;
use walkdir::WalkDir;

use super::super::{sandbox_error, sandbox_timestamp_string, SandboxArtifacts};
use crate::InstallUserError;

#[derive(Clone, Debug)]
pub(crate) struct LayerTar {
    pub(crate) digest: String,
    pub(crate) size: u64,
    pub(crate) path: PathBuf,
}

pub(crate) struct BuiltImage {
    pub(crate) manifest_digest: String,
    pub(crate) manifest_bytes: Vec<u8>,
    pub(crate) config_bytes: Vec<u8>,
    pub(crate) layers: Vec<LayerTar>,
}

pub(crate) fn build_oci_image(
    artifacts: &SandboxArtifacts,
    oci_root: &Path,
    layers: Vec<LayerTar>,
    tag: Option<&str>,
    working_dir: &Path,
    pythonpath: Option<&str>,
) -> Result<BuiltImage, InstallUserError> {
    fs::create_dir_all(oci_root).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to prepare sandbox image directory",
            json!({ "path": oci_root.display().to_string(), "error": err.to_string() }),
        )
    })?;
    let blobs = oci_root.join("blobs").join("sha256");
    fs::create_dir_all(&blobs).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to prepare OCI blob directory",
            json!({ "path": blobs.display().to_string(), "error": err.to_string() }),
        )
    })?;
    let mut image_layers = Vec::with_capacity(layers.len());
    for layer in layers {
        let dest = blobs.join(&layer.digest);
        if !dest.exists() {
            link_or_copy(&layer.path, &dest).map_err(|err| {
                sandbox_error(
                    "PX903",
                    "failed to write sandbox layer",
                    json!({
                        "path": dest.display().to_string(),
                        "error": err.to_string(),
                    }),
                )
            })?;
        }
        image_layers.push(LayerTar {
            digest: layer.digest,
            size: layer.size,
            path: dest,
        });
    }
    let diff_ids: Vec<String> = image_layers
        .iter()
        .map(|layer| format!("sha256:{}", layer.digest))
        .collect();
    let mut env_vars = Vec::new();
    env_vars.push(
        "PATH=/px/env/bin:/px/runtime/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/bin"
            .into(),
    );
    if let Some(py_path) = pythonpath {
        env_vars.push(format!("PYTHONPATH={py_path}"));
    }
    env_vars.push(format!("PX_SANDBOX_ID={}", artifacts.definition.sbx_id()));
    let config = json!({
        "created": sandbox_timestamp_string(),
        "architecture": "amd64",
        "os": "linux",
        "rootfs": {
            "type": "layers",
            "diff_ids": diff_ids,
        },
        "config": {
            "WorkingDir": working_dir.display().to_string(),
            "Env": env_vars,
        },
        "px": {
            "sbx_id": artifacts.definition.sbx_id(),
            "base": artifacts.base.name,
            "capabilities": artifacts.definition.capabilities,
            "profile_oid": artifacts.definition.profile_oid,
            "system_deps": artifacts.definition.system_deps.clone(),
        },
    });
    let config_bytes = serde_json::to_vec_pretty(&config).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to encode sandbox config",
            json!({ "error": err.to_string() }),
        )
    })?;
    let config_digest = sha256_hex(&config_bytes);
    let config_path = blobs.join(&config_digest);
    fs::write(&config_path, &config_bytes).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to write sandbox config",
            json!({ "path": config_path.display().to_string(), "error": err.to_string() }),
        )
    })?;
    let manifest_layers: Vec<_> = image_layers
        .iter()
        .map(|layer| {
            json!({
                "mediaType": "application/vnd.oci.image.layer.v1.tar",
                "digest": format!("sha256:{}", layer.digest),
                "size": layer.size,
            })
        })
        .collect();
    let manifest = json!({
        "schemaVersion": 2,
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": format!("sha256:{config_digest}"),
            "size": config_bytes.len(),
        },
        "layers": manifest_layers,
    });
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to encode OCI manifest",
            json!({ "error": err.to_string() }),
        )
    })?;
    let manifest_digest = sha256_hex(&manifest_bytes);
    let manifest_path = blobs.join(&manifest_digest);
    fs::write(&manifest_path, &manifest_bytes).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to write OCI manifest",
            json!({ "path": manifest_path.display().to_string(), "error": err.to_string() }),
        )
    })?;
    let mut annotations = json!({
        "px.sbx_id": artifacts.definition.sbx_id(),
        "px.base": artifacts.base.name,
    });
    if let Some(tag) = tag {
        annotations["org.opencontainers.image.ref.name"] = json!(tag);
    }
    let index = json!({
        "schemaVersion": 2,
        "manifests": [
            {
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "digest": format!("sha256:{manifest_digest}"),
                "size": manifest_bytes.len(),
                "annotations": annotations,
            }
        ],
    });
    fs::write(
        oci_root.join("index.json"),
        serde_json::to_vec_pretty(&index).map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to encode OCI index",
                json!({ "error": err.to_string() }),
            )
        })?,
    )
    .map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to write OCI index",
            json!({ "path": oci_root.join("index.json").display().to_string(), "error": err.to_string() }),
        )
    })?;
    fs::write(
        oci_root.join("oci-layout"),
        b"{\"imageLayoutVersion\":\"1.0.0\"}",
    )
    .map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to write OCI layout file",
            json!({ "path": oci_root.join("oci-layout").display().to_string(), "error": err.to_string() }),
        )
    })?;
    Ok(BuiltImage {
        manifest_digest,
        manifest_bytes,
        config_bytes,
        layers: image_layers,
    })
}

pub(crate) fn load_layer_from_blobs(
    blobs: &Path,
    digest: &str,
) -> Result<LayerTar, InstallUserError> {
    let path = blobs.join(digest);
    let mut file = File::open(&path).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to read sandbox layer",
            json!({ "path": path.display().to_string(), "error": err.to_string() }),
        )
    })?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to read sandbox layer",
                json!({ "path": path.display().to_string(), "error": err.to_string() }),
            )
        })?;
        if read == 0 {
            break;
        }
        size = size.saturating_add(read as u64);
        hasher.update(&buffer[..read]);
    }
    let computed = format!("{:x}", hasher.finalize());
    if computed != digest {
        return Err(sandbox_error(
            "PX904",
            "sandbox layer digest mismatch",
            json!({
                "expected": digest,
                "computed": computed,
                "path": path.display().to_string(),
            }),
        ));
    }
    Ok(LayerTar {
        digest: computed,
        size,
        path,
    })
}

pub(crate) fn export_output(
    oci_root: &Path,
    out: &Path,
    cwd: &Path,
) -> Result<(), InstallUserError> {
    if out.extension().map(|ext| ext == "tar").unwrap_or(false) {
        let target = if out.is_absolute() {
            out.to_path_buf()
        } else {
            cwd.join(out)
        };
        let file = File::create(&target).map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to create OCI tarball",
                json!({ "path": target.display().to_string(), "error": err.to_string() }),
            )
        })?;
        let mut builder = Builder::new(file);
        builder.append_dir_all(".", oci_root).map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to write OCI tarball",
                json!({ "path": target.display().to_string(), "error": err.to_string() }),
            )
        })?;
        builder.into_inner().map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to finalize OCI tarball",
                json!({ "path": target.display().to_string(), "error": err.to_string() }),
            )
        })?;
        return Ok(());
    }

    let target_dir = if out.is_absolute() {
        out.to_path_buf()
    } else {
        cwd.join(out)
    };
    if target_dir.exists() {
        let _ = fs::remove_dir_all(&target_dir);
    }
    copy_dir(oci_root, &target_dir)?;
    Ok(())
}

pub(super) fn push_oci_image(tag: &str, built: &BuiltImage) -> Result<(), InstallUserError> {
    let reference = Reference::try_from(tag).map_err(|err| {
        sandbox_error(
            "PX903",
            "invalid image reference for push",
            json!({ "reference": tag, "error": err.to_string() }),
        )
    })?;
    let manifest: OciImageManifest =
        serde_json::from_slice(&built.manifest_bytes).map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to encode OCI manifest for push",
                json!({ "error": err.to_string() }),
            )
        })?;
    let mut client = Client::new(ClientConfig::default());
    let auth = registry_auth_from_env()?;
    let layers = built
        .layers
        .iter()
        .map(|layer| {
            let bytes = fs::read(&layer.path).map_err(|err| {
                sandbox_error(
                    "PX903",
                    "failed to read sandbox layer for push",
                    json!({ "path": layer.path.display().to_string(), "error": err.to_string() }),
                )
            })?;
            Ok(ImageLayer::new(
                bytes,
                "application/vnd.oci.image.layer.v1.tar".to_string(),
                None,
            ))
        })
        .collect::<Result<Vec<_>, InstallUserError>>()?;
    let config = OciConfig::new(
        built.config_bytes.clone(),
        "application/vnd.oci.image.config.v1+json".to_string(),
        None,
    );
    let rt = Runtime::new().map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to initialize registry client",
            json!({ "error": err.to_string() }),
        )
    })?;
    rt.block_on(async {
        client
            .push(&reference, &layers, config, &auth, Some(manifest))
            .await
    })
    .map_err(|err| {
        let message = err.to_string();
        let mut details = json!({ "reference": tag, "error": message });
        if message.contains("401") || message.contains("Unauthorized") {
            details["reason"] = json!("unauthorized");
            details["hint"] =
                json!("check PX_REGISTRY_USERNAME/PX_REGISTRY_PASSWORD or registry permissions");
        }
        sandbox_error("PX903", "failed to push sandbox image", details)
    })?;
    Ok(())
}

pub(super) fn registry_auth_from_env() -> Result<RegistryAuth, InstallUserError> {
    let username = env::var("PX_REGISTRY_USERNAME").unwrap_or_default();
    let password = env::var("PX_REGISTRY_PASSWORD").unwrap_or_default();
    if username.is_empty() && password.is_empty() {
        return Ok(RegistryAuth::Anonymous);
    }
    if username.is_empty() || password.is_empty() {
        return Err(sandbox_error(
            "PX903",
            "registry credentials incomplete",
            json!({
                "code": "PX903",
                "reason": "registry_auth_missing",
                "hint": "set PX_REGISTRY_USERNAME and PX_REGISTRY_PASSWORD",
            }),
        ));
    }
    Ok(RegistryAuth::Basic(username, password))
}

fn copy_dir(from: &Path, to: &Path) -> Result<(), InstallUserError> {
    fs::create_dir_all(to).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to prepare output directory",
            json!({ "path": to.display().to_string(), "error": err.to_string() }),
        )
    })?;
    for entry in WalkDir::new(from) {
        let entry = entry.map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to walk OCI layout",
                json!({ "error": err.to_string() }),
            )
        })?;
        let path = entry.path();
        let rel = path.strip_prefix(from).unwrap_or(path);
        let dest = to.join(rel);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&dest).map_err(|err| {
                sandbox_error(
                    "PX903",
                    "failed to create output directory",
                    json!({ "path": dest.display().to_string(), "error": err.to_string() }),
                )
            })?;
        } else {
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent).map_err(|err| {
                    sandbox_error(
                        "PX903",
                        "failed to create output directory",
                        json!({ "path": parent.display().to_string(), "error": err.to_string() }),
                    )
                })?;
            }
            fs::copy(path, &dest).map_err(|err| {
                sandbox_error(
                    "PX903",
                    "failed to copy OCI file",
                    json!({ "path": dest.display().to_string(), "error": err.to_string() }),
                )
            })?;
        }
    }
    Ok(())
}

fn link_or_copy(from: &Path, to: &Path) -> std::io::Result<()> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::hard_link(from, to) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(_) => {
            fs::copy(from, to)?;
            Ok(())
        }
    }
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}
