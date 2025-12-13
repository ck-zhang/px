use std::path::PathBuf;

use anyhow::{anyhow, Result};
use serde_json::json;

use crate::{relative_path_str, CommandContext, ExecutionOutcome, InstallUserError, PythonContext};

use super::artifacts::{ArtifactSummary, BuildTargets};
use super::uv::{discover_publish_artifacts, PublishArtifact};

#[derive(Clone, Debug)]
pub struct BuildRequest {
    pub include_sdist: bool,
    pub include_wheel: bool,
    pub out: Option<PathBuf>,
    pub dry_run: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct BuildPlan {
    pub(crate) targets: BuildTargets,
    pub(crate) out_dir: PathBuf,
}

pub(crate) fn plan_build(py_ctx: &PythonContext, request: &BuildRequest) -> BuildPlan {
    BuildPlan {
        targets: build_targets_from_request(request),
        out_dir: resolve_output_dir_from_request(py_ctx, request.out.as_ref()),
    }
}

pub(crate) fn build_targets_from_request(request: &BuildRequest) -> BuildTargets {
    let mut targets = BuildTargets {
        sdist: request.include_sdist,
        wheel: request.include_wheel,
    };
    if !targets.sdist && !targets.wheel {
        targets = BuildTargets {
            sdist: true,
            wheel: true,
        };
    }
    targets
}

pub(crate) fn resolve_output_dir_from_request(
    ctx: &PythonContext,
    out: Option<&PathBuf>,
) -> PathBuf {
    match out {
        Some(path) if path.is_absolute() => path.clone(),
        Some(path) => ctx.project_root.join(path),
        None => ctx.project_root.join("dist"),
    }
}

#[derive(Clone, Debug)]
pub struct PublishRequest {
    pub registry: Option<String>,
    pub token_env: Option<String>,
    pub dry_run: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct PublishRegistry {
    pub(crate) label: String,
    pub(crate) url: String,
}

#[derive(Clone, Debug)]
pub(crate) struct PublishPlan {
    pub(crate) registry: PublishRegistry,
    pub(crate) token_env: String,
    pub(crate) token: Option<String>,
    pub(crate) artifacts: Vec<PublishArtifact>,
    pub(crate) dry_run: bool,
}

impl PublishPlan {
    pub(crate) fn summaries(&self) -> Vec<ArtifactSummary> {
        self.artifacts
            .iter()
            .map(|artifact| artifact.summary().clone())
            .collect()
    }
}

pub(crate) enum PublishPlanning {
    Plan(PublishPlan),
    Outcome(ExecutionOutcome),
}

pub(crate) fn plan_publish(
    ctx: &CommandContext,
    py_ctx: &PythonContext,
    request: &PublishRequest,
) -> Result<PublishPlanning> {
    let registry = resolve_publish_registry(request.registry.as_deref());
    let token_env = request
        .token_env
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| ctx.config().publish.default_token_env.to_string());
    let dist_dir = py_ctx.project_root.join("dist");
    let artifacts = discover_publish_artifacts(&py_ctx.project_root, &dist_dir).map_err(|err| {
        InstallUserError::new(
            format!("px publish: {err}"),
            json!({
                "reason": "publish_artifacts",
                "error": err.to_string(),
            }),
        )
    })?;
    if artifacts.is_empty() {
        return Ok(PublishPlanning::Outcome(ExecutionOutcome::user_error(
            "px publish: no artifacts found (run `px build` first)",
            json!({ "dist_dir": relative_path_str(&dist_dir, &py_ctx.project_root) }),
        )));
    }

    if request.dry_run {
        return Ok(PublishPlanning::Plan(PublishPlan {
            registry,
            token_env,
            token: None,
            artifacts,
            dry_run: true,
        }));
    }

    let explicit_online = std::env::var("PX_ONLINE").ok().as_deref() == Some("1");
    if !explicit_online {
        return Ok(PublishPlanning::Outcome(ExecutionOutcome::user_error(
            "px publish: PX_ONLINE=1 required for uploads",
            json!({
                "registry": registry.label,
                "token_env": token_env,
                "hint": format!("export PX_ONLINE=1 && {token_env}=<token> before publishing"),
            }),
        )));
    }

    if !ctx.env_contains(&token_env) {
        return Ok(PublishPlanning::Outcome(ExecutionOutcome::user_error(
            format!("px publish: {token_env} must be set"),
            json!({
                "registry": registry.label,
                "token_env": token_env,
                "hint": format!("export {token_env}=<token> before publishing"),
            }),
        )));
    }

    let token_value = std::env::var(&token_env)
        .map_err(|err| anyhow!("failed to read {token_env} from environment: {err}"))?;
    if token_value.trim().is_empty() {
        return Ok(PublishPlanning::Outcome(ExecutionOutcome::user_error(
            format!("px publish: {token_env} is empty"),
            json!({
                "registry": registry.label,
                "token_env": token_env,
                "hint": format!("export {token_env}=<token> before publishing"),
            }),
        )));
    }

    Ok(PublishPlanning::Plan(PublishPlan {
        registry,
        token_env,
        token: Some(token_value),
        artifacts,
        dry_run: false,
    }))
}

const PYPI_UPLOAD_URL: &str = "https://upload.pypi.org/legacy/";
const TEST_PYPI_UPLOAD_URL: &str = "https://test.pypi.org/legacy/";

fn resolve_publish_registry(selection: Option<&str>) -> PublishRegistry {
    let trimmed = selection.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    });
    match trimmed {
        None => PublishRegistry {
            label: "pypi".to_string(),
            url: PYPI_UPLOAD_URL.to_string(),
        },
        Some(value) if value.starts_with("http://") || value.starts_with("https://") => {
            PublishRegistry {
                label: value.to_string(),
                url: value.to_string(),
            }
        }
        Some(value) => match value.to_ascii_lowercase().as_str() {
            "pypi" => PublishRegistry {
                label: "pypi".to_string(),
                url: PYPI_UPLOAD_URL.to_string(),
            },
            "testpypi" | "test-pypi" => PublishRegistry {
                label: value.to_string(),
                url: TEST_PYPI_UPLOAD_URL.to_string(),
            },
            _ => PublishRegistry {
                label: value.to_string(),
                url: format!("https://{value}/legacy/"),
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn resolve_output_dir_handles_relative_and_absolute() -> anyhow::Result<()> {
        let root = tempdir()?;
        let ctx = PythonContext {
            project_root: root.path().to_path_buf(),
            project_name: "demo".to_string(),
            python: "/usr/bin/python".to_string(),
            pythonpath: String::new(),
            allowed_paths: Vec::new(),
            site_bin: None,
            pep582_bin: Vec::new(),
            pyc_cache_prefix: None,
            px_options: px_domain::PxOptions::default(),
        };

        let rel = PathBuf::from("custom/dist");
        let resolved_rel = resolve_output_dir_from_request(&ctx, Some(&rel));
        assert_eq!(resolved_rel, root.path().join("custom/dist"));

        let abs = root.path().join("abs/dist");
        let resolved_abs = resolve_output_dir_from_request(&ctx, Some(&abs));
        assert_eq!(resolved_abs, abs);
        Ok(())
    }

    #[test]
    fn build_targets_default_to_both_when_not_selected() {
        let request = BuildRequest {
            include_sdist: false,
            include_wheel: false,
            out: None,
            dry_run: false,
        };

        let targets = build_targets_from_request(&request);
        assert!(targets.sdist, "sdist should be selected by default");
        assert!(targets.wheel, "wheel should be selected by default");
    }

    #[test]
    fn resolve_publish_registry_handles_aliases_and_urls() {
        let default = resolve_publish_registry(None);
        assert_eq!(default.label, "pypi");
        assert_eq!(default.url, PYPI_UPLOAD_URL);

        let testpypi = resolve_publish_registry(Some("test-pypi"));
        assert_eq!(testpypi.label, "test-pypi");
        assert_eq!(testpypi.url, TEST_PYPI_UPLOAD_URL);

        let host = resolve_publish_registry(Some("packages.example.com"));
        assert_eq!(host.label, "packages.example.com");
        assert_eq!(host.url, "https://packages.example.com/legacy/");

        let url = resolve_publish_registry(Some("https://upload.example.invalid/simple/"));
        assert_eq!(url.label, "https://upload.example.invalid/simple/");
        assert_eq!(url.url, "https://upload.example.invalid/simple/");
    }
}
