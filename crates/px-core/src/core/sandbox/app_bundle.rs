use std::collections::BTreeSet;
use std::fs;
use std::io::{Cursor, Read, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::json;
use tar::{Archive, Builder, Header};

use super::pack::sha256_hex;
use super::{sandbox_error, PXAPP_VERSION, SBX_VERSION};
use crate::core::system_deps::SystemDeps;
use crate::{InstallUserError, PX_VERSION};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PxAppMetadata {
    pub(crate) format_version: u32,
    pub(crate) sbx_version: u32,
    pub(crate) sbx_id: String,
    pub(crate) base_os_oid: String,
    pub(crate) profile_oid: String,
    pub(crate) capabilities: BTreeSet<String>,
    #[serde(default)]
    pub(crate) system_deps: SystemDeps,
    pub(crate) entrypoint: Vec<String>,
    pub(crate) workdir: String,
    pub(crate) manifest_digest: String,
    pub(crate) config_digest: String,
    pub(crate) layer_digests: Vec<String>,
    pub(crate) created_at: String,
    pub(crate) px_version: String,
}

#[derive(Clone, Debug)]
pub(crate) struct PxAppLayer {
    pub(crate) digest: String,
    pub(crate) bytes: Vec<u8>,
}

#[derive(Clone, Debug)]
pub(crate) struct PxAppBundle {
    pub(crate) metadata: PxAppMetadata,
    pub(crate) manifest_bytes: Vec<u8>,
    pub(crate) config_bytes: Vec<u8>,
    pub(crate) layers: Vec<PxAppLayer>,
}

pub(crate) fn bundle_identity(bundle: &PxAppBundle) -> String {
    let mut parts = Vec::new();
    parts.push(bundle.metadata.sbx_id.clone());
    parts.push(bundle.metadata.manifest_digest.clone());
    parts.push(bundle.metadata.config_digest.clone());
    let mut layer_ids = bundle.metadata.layer_digests.clone();
    layer_ids.sort();
    parts.extend(layer_ids);
    sha256_hex(parts.join("|").as_bytes())
}

pub(crate) fn write_pxapp_bundle(
    out: &Path,
    mut metadata: PxAppMetadata,
    manifest_bytes: &[u8],
    config_bytes: &[u8],
    layers: &[super::pack::LayerTar],
) -> Result<(), InstallUserError> {
    if metadata.format_version != PXAPP_VERSION {
        metadata.format_version = PXAPP_VERSION;
    }
    metadata.sbx_version = SBX_VERSION;
    metadata.px_version = PX_VERSION.to_string();
    metadata.manifest_digest = sha256_hex(manifest_bytes);
    metadata.config_digest = sha256_hex(config_bytes);
    metadata.layer_digests = layers.iter().map(|layer| layer.digest.clone()).collect();
    let encoded_meta = serde_json::to_vec_pretty(&metadata).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to encode pxapp metadata",
            json!({ "error": err.to_string() }),
        )
    })?;
    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            sandbox_error(
                "PX903",
                "failed to prepare pxapp output directory",
                json!({ "path": parent.display().to_string(), "error": err.to_string() }),
            )
        })?;
    }
    let file = fs::File::create(out).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to write pxapp bundle",
            json!({ "path": out.display().to_string(), "error": err.to_string() }),
        )
    })?;
    let mut builder = Builder::new(file);
    append_bytes(&mut builder, Path::new("metadata.json"), &encoded_meta)?;
    append_bytes(&mut builder, Path::new("manifest.json"), manifest_bytes)?;
    append_bytes(&mut builder, Path::new("config.json"), config_bytes)?;
    for layer in layers {
        let name = format!("layers/{}.tar", layer.digest);
        append_file(&mut builder, Path::new(&name), &layer.path, layer.size)?;
    }
    builder.into_inner().map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to finalize pxapp bundle",
            json!({ "error": err.to_string() }),
        )
    })?;
    Ok(())
}

pub(crate) fn read_pxapp_bundle(path: &Path) -> Result<PxAppBundle, InstallUserError> {
    let data = fs::read(path).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to read pxapp bundle",
            json!({ "path": path.display().to_string(), "error": err.to_string() }),
        )
    })?;
    let cursor = Cursor::new(data);
    let mut archive = Archive::new(cursor);
    let mut metadata: Option<PxAppMetadata> = None;
    let mut manifest_bytes: Option<Vec<u8>> = None;
    let mut config_bytes: Option<Vec<u8>> = None;
    let mut layers = Vec::new();
    for entry in archive.entries().map_err(|err| {
        sandbox_error(
            "PX903",
            "pxapp bundle is invalid or corrupted",
            json!({ "error": err.to_string(), "path": path.display().to_string() }),
        )
    })? {
        let mut entry = entry.map_err(|err| {
            sandbox_error(
                "PX903",
                "pxapp bundle is invalid or corrupted",
                json!({ "error": err.to_string(), "path": path.display().to_string() }),
            )
        })?;
        let mut contents = Vec::new();
        entry.read_to_end(&mut contents).map_err(|err| {
            sandbox_error(
                "PX903",
                "pxapp bundle is invalid or corrupted",
                json!({ "error": err.to_string(), "path": path.display().to_string() }),
            )
        })?;
        let path_name = entry
            .path()
            .ok()
            .and_then(|p| p.into_owned().to_str().map(str::to_string))
            .unwrap_or_default();
        match path_name.as_str() {
            "metadata.json" => {
                metadata = Some(serde_json::from_slice(&contents).map_err(|err| {
                    sandbox_error(
                        "PX903",
                        "pxapp metadata is invalid",
                        json!({ "error": err.to_string(), "path": path.display().to_string() }),
                    )
                })?);
            }
            "manifest.json" => manifest_bytes = Some(contents),
            "config.json" => config_bytes = Some(contents),
            name if name.starts_with("layers/") && name.ends_with(".tar") => {
                let digest = name
                    .trim_start_matches("layers/")
                    .trim_end_matches(".tar")
                    .to_string();
                let computed = sha256_hex(&contents);
                if computed != digest {
                    return Err(sandbox_error(
                        "PX904",
                        "pxapp layer digest mismatch",
                        json!({
                            "expected": digest,
                            "computed": computed,
                            "path": path.display().to_string(),
                        }),
                    ));
                }
                layers.push(PxAppLayer {
                    digest,
                    bytes: contents,
                });
            }
            _ => {}
        }
    }
    let metadata = metadata.ok_or_else(|| {
        sandbox_error(
            "PX903",
            "pxapp bundle is missing metadata.json",
            json!({ "path": path.display().to_string() }),
        )
    })?;
    if metadata.format_version != PXAPP_VERSION {
        return Err(sandbox_error(
            "PX904",
            "pxapp bundle format is incompatible with this px version",
            json!({
                "expected": PXAPP_VERSION,
                "found": metadata.format_version,
                "path": path.display().to_string(),
            }),
        ));
    }
    if metadata.sbx_version != SBX_VERSION {
        return Err(sandbox_error(
            "PX904",
            "pxapp sandbox format version is incompatible with this px version",
            json!({
                "expected": SBX_VERSION,
                "found": metadata.sbx_version,
                "path": path.display().to_string(),
            }),
        ));
    }
    let manifest_bytes = manifest_bytes.ok_or_else(|| {
        sandbox_error(
            "PX903",
            "pxapp bundle is missing manifest.json",
            json!({ "path": path.display().to_string() }),
        )
    })?;
    let config_bytes = config_bytes.ok_or_else(|| {
        sandbox_error(
            "PX903",
            "pxapp bundle is missing config.json",
            json!({ "path": path.display().to_string() }),
        )
    })?;
    let manifest_digest = sha256_hex(&manifest_bytes);
    if metadata.manifest_digest != manifest_digest {
        return Err(sandbox_error(
            "PX904",
            "pxapp manifest digest mismatch",
            json!({
                "expected": metadata.manifest_digest,
                "computed": manifest_digest,
                "path": path.display().to_string(),
            }),
        ));
    }
    let config_digest = sha256_hex(&config_bytes);
    if metadata.config_digest != config_digest {
        return Err(sandbox_error(
            "PX904",
            "pxapp config digest mismatch",
            json!({
                "expected": metadata.config_digest,
                "computed": config_digest,
                "path": path.display().to_string(),
            }),
        ));
    }
    let manifest_value: serde_json::Value =
        serde_json::from_slice(&manifest_bytes).map_err(|err| {
            sandbox_error(
                "PX904",
                "pxapp manifest is invalid",
                json!({ "error": err.to_string(), "path": path.display().to_string() }),
            )
        })?;
    let manifest_layers = manifest_value
        .get("layers")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut expected_layers = Vec::new();
    for layer in manifest_layers {
        if let Some(digest) = layer
            .get("digest")
            .and_then(|v| v.as_str())
            .map(|s| s.trim_start_matches("sha256:").to_string())
        {
            expected_layers.push(digest);
        }
    }
    expected_layers.sort();
    let mut provided_digests: Vec<_> = layers.iter().map(|layer| layer.digest.clone()).collect();
    provided_digests.sort();
    if expected_layers != provided_digests {
        return Err(sandbox_error(
            "PX904",
            "pxapp bundle layers do not match manifest",
            json!({
                "expected": expected_layers,
                "found": provided_digests,
                "path": path.display().to_string(),
            }),
        ));
    }
    let meta_layers = {
        let mut ids = metadata.layer_digests.clone();
        ids.sort();
        ids
    };
    if meta_layers != provided_digests {
        return Err(sandbox_error(
            "PX904",
            "pxapp metadata does not match bundle layers",
            json!({
                "expected": meta_layers,
                "found": provided_digests,
                "path": path.display().to_string(),
            }),
        ));
    }
    Ok(PxAppBundle {
        metadata,
        manifest_bytes,
        config_bytes,
        layers,
    })
}

fn append_bytes(
    builder: &mut Builder<impl Write>,
    path: &Path,
    data: &[u8],
) -> Result<(), InstallUserError> {
    let mut header = Header::new_gnu();
    header.set_path(path).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to stage pxapp entry",
            json!({ "path": path.display().to_string(), "error": err.to_string() }),
        )
    })?;
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(0);
    header.set_uid(0);
    header.set_gid(0);
    header.set_cksum();
    builder.append(&header, data).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to write pxapp entry",
            json!({ "path": path.display().to_string(), "error": err.to_string() }),
        )
    })
}

fn append_file(
    builder: &mut Builder<impl Write>,
    archive_path: &Path,
    path: &Path,
    size: u64,
) -> Result<(), InstallUserError> {
    let mut header = Header::new_gnu();
    header.set_path(archive_path).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to stage pxapp entry",
            json!({ "path": archive_path.display().to_string(), "error": err.to_string() }),
        )
    })?;
    header.set_size(size);
    header.set_mode(0o644);
    header.set_mtime(0);
    header.set_uid(0);
    header.set_gid(0);
    header.set_cksum();
    let mut file = fs::File::open(path).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to read pxapp layer",
            json!({ "path": path.display().to_string(), "error": err.to_string() }),
        )
    })?;
    builder.append(&header, &mut file).map_err(|err| {
        sandbox_error(
            "PX903",
            "failed to write pxapp entry",
            json!({ "path": archive_path.display().to_string(), "error": err.to_string() }),
        )
    })
}
