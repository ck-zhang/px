use anyhow::{anyhow, Context, Result};
use serde_json::json;
use tokio::runtime::Builder;

use crate::{python_context, CommandContext, ExecutionOutcome, InstallUserError};

use super::plan::{plan_publish, PublishPlanning, PublishRegistry, PublishRequest};
use super::uv::{PublishUploadReport, UvPublishSession};

/// Publishes the built artifacts to the selected Python package registry.
///
/// # Errors
/// Returns an error when metadata cannot be loaded or an upload request fails.
pub fn publish_project(ctx: &CommandContext, request: &PublishRequest) -> Result<ExecutionOutcome> {
    publish_project_outcome(ctx, request)
}

fn publish_project_outcome(
    ctx: &CommandContext,
    request: &PublishRequest,
) -> Result<ExecutionOutcome> {
    let py_ctx = match python_context(ctx) {
        Ok(py) => py,
        Err(outcome) => return Ok(outcome),
    };
    let plan = match plan_publish(ctx, &py_ctx, request)? {
        PublishPlanning::Plan(plan) => plan,
        PublishPlanning::Outcome(outcome) => return Ok(outcome),
    };

    if plan.dry_run {
        let details = json!({
            "registry": plan.registry.label,
            "token_env": plan.token_env,
            "dry_run": true,
            "artifacts": plan.summaries(),
        });
        let message = format!(
            "px publish: dry-run to {} ({} artifacts)",
            plan.registry.label,
            plan.artifacts.len()
        );
        return Ok(ExecutionOutcome::success(message, details));
    }

    let token_value = plan
        .token
        .as_ref()
        .ok_or_else(|| anyhow!("publish plan missing token"))?;
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .context("creating publish runtime")?;
    let session = UvPublishSession::new(&plan.registry.url, token_value, &ctx.cache().path)
        .map_err(|err| publish_user_error(err, &plan.registry, Some(plan.token_env.clone())))?;
    let report = runtime
        .block_on(session.publish(&plan.artifacts))
        .map_err(|err| publish_user_error(err, &plan.registry, Some(plan.token_env.clone())))?;

    let mut details = json!({
        "registry": plan.registry.label,
        "token_env": plan.token_env,
        "dry_run": false,
        "artifacts": plan.summaries(),
        "uploaded": report.uploaded,
    });
    if report.skipped_existing > 0 {
        details["skipped_existing"] = json!(report.skipped_existing);
    }

    let message = publish_success_message(&plan.registry, &report);
    Ok(ExecutionOutcome::success(message, details))
}

fn publish_success_message(registry: &PublishRegistry, report: &PublishUploadReport) -> String {
    if report.skipped_existing > 0 {
        format!(
            "px publish: uploaded {} artifacts to {} ({} skipped existing)",
            report.uploaded, registry.label, report.skipped_existing
        )
    } else {
        format!(
            "px publish: uploaded {} artifacts to {}",
            report.uploaded, registry.label
        )
    }
}

fn publish_user_error<E: std::fmt::Display>(
    err: E,
    registry: &PublishRegistry,
    token_env: Option<String>,
) -> InstallUserError {
    InstallUserError::new(
        format!("px publish: {err}"),
        json!({
            "registry": registry.label,
            "token_env": token_env,
            "reason": "publish_failed",
            "error": err.to_string(),
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::super::uv::discover_publish_artifacts;
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn discover_publish_artifacts_uses_uv_filenames() -> Result<()> {
        let tmp = tempdir()?;
        let project_root = tmp.path();
        let dist_dir = project_root.join("dist");
        std::fs::create_dir_all(&dist_dir)?;

        let valid_wheel = dist_dir.join("demo_pkg-0.1.0-py3-none-any.whl");
        let invalid = dist_dir.join("README.md");
        std::fs::write(&valid_wheel, b"wheel-bytes")?;
        std::fs::write(&invalid, b"ignore-me")?;

        let artifacts = discover_publish_artifacts(project_root, &dist_dir)?;
        assert_eq!(artifacts.len(), 1, "only valid distributions are selected");
        let summary = artifacts[0].summary();
        assert_eq!(summary.path, "dist/demo_pkg-0.1.0-py3-none-any.whl");
        Ok(())
    }

    #[test]
    fn publish_user_error_populates_details() {
        let registry = PublishRegistry {
            label: "pypi".into(),
            url: "https://upload.pypi.org/legacy/".into(),
        };
        let user_err = publish_user_error("permission denied", &registry, Some("PX_TOKEN".into()));
        assert!(
            user_err.message().contains("permission denied"),
            "message should include source error"
        );
        assert_eq!(user_err.details()["registry"], "pypi");
        assert_eq!(user_err.details()["token_env"], "PX_TOKEN");
    }

    #[test]
    fn publish_success_message_reports_skipped() {
        let registry = PublishRegistry {
            label: "pypi".into(),
            url: "https://upload.pypi.org/legacy/".into(),
        };
        let report = PublishUploadReport {
            uploaded: 1,
            skipped_existing: 2,
        };
        let msg = publish_success_message(&registry, &report);
        assert!(
            msg.contains("2 skipped existing"),
            "success message should mention skipped uploads: {msg}"
        );
    }
}
