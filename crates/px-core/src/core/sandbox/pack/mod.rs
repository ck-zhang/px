use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::SandboxArtifacts;
use crate::workspace::prepare_workspace_run_context;
use crate::{
    is_missing_project_error, manifest_snapshot, missing_project_outcome, CommandContext,
    ExecutionOutcome,
};

// Split from the former mega-module `pack.rs`:
// - `base_image.rs`: base image build + manifest persistence
// - `layers.rs`: layer tar creation (env/system/base/app)
// - `oci.rs`: OCI layout build/export/push
// - `project.rs`: single-project `px pack` flow
// - `workspace_member.rs`: workspace-member `px pack` flow
// - `defaults.rs`: tag/path defaults
// - `entrypoint.rs`: entrypoint resolution
// - `tests.rs`: module tests
mod base_image;
mod defaults;
mod entrypoint;
mod layers;
mod oci;
mod project;
mod workspace_member;

pub(crate) use base_image::runtime_home_from_env;
pub(crate) use layers::{write_base_os_layer, write_env_layer_tar, write_system_deps_layer};
pub(crate) use oci::ensure_docker_archive_layout;
pub(crate) use oci::{build_oci_image, export_output, load_layer_from_blobs, sha256_hex, LayerTar};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PackTarget {
    Image,
    App,
}

#[derive(Clone, Debug)]
pub struct PackRequest {
    pub target: PackTarget,
    pub tag: Option<String>,
    pub out: Option<PathBuf>,
    pub push: bool,
    pub allow_dirty: bool,
    pub entrypoint: Option<Vec<String>>,
    pub workdir: Option<PathBuf>,
}

pub fn pack_image(ctx: &CommandContext, request: &PackRequest) -> Result<ExecutionOutcome> {
    let mut request = request.clone();
    request.target = PackTarget::Image;
    pack(ctx, &request)
}

pub fn pack_app(ctx: &CommandContext, request: &PackRequest) -> Result<ExecutionOutcome> {
    let mut request = request.clone();
    request.target = PackTarget::App;
    pack(ctx, &request)
}

fn pack(ctx: &CommandContext, request: &PackRequest) -> Result<ExecutionOutcome> {
    if matches!(request.target, PackTarget::App) && request.push {
        return Ok(ExecutionOutcome::user_error(
            "px pack app does not support --push",
            json!({
                "code": "PX903",
                "reason": "push_not_supported",
            }),
        ));
    }
    if let Some(ws_ctx) = match prepare_workspace_run_context(ctx, true, "pack", false) {
        Ok(value) => value,
        Err(outcome) => return Ok(outcome),
    } {
        return workspace_member::pack_workspace_member(ctx, request, ws_ctx);
    }

    let snapshot = match manifest_snapshot() {
        Ok(snapshot) => snapshot,
        Err(err) => {
            if is_missing_project_error(&err) {
                return Ok(missing_project_outcome());
            }
            let msg = err.to_string();
            if msg.contains("pyproject.toml not found") {
                let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                return Ok(crate::tooling::missing_pyproject_outcome("pack", &root));
            }
            return Err(err);
        }
    };
    project::pack_project(ctx, request, &snapshot)
}

fn format_capabilities(artifacts: &SandboxArtifacts) -> String {
    if artifacts.definition.capabilities.is_empty() {
        "none".into()
    } else {
        artifacts
            .definition
            .capabilities
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(",")
    }
}

#[cfg(test)]
mod tests;
