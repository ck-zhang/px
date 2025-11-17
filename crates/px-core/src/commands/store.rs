use anyhow::Result;
use px_lockfile::{load_lockfile_optional, lock_prefetch_specs};
use px_store::PrefetchOptions as StorePrefetchOptions;
use serde_json::json;

use crate::{manifest_snapshot, store_prefetch_specs, workspace, CommandContext, ExecutionOutcome};

#[derive(Clone, Debug)]
pub struct StorePrefetchRequest {
    pub workspace: bool,
    pub dry_run: bool,
}

pub fn store_prefetch(
    ctx: &CommandContext,
    request: StorePrefetchRequest,
) -> Result<ExecutionOutcome> {
    store_prefetch_outcome(ctx, &request)
}

fn store_prefetch_outcome(
    ctx: &CommandContext,
    request: &StorePrefetchRequest,
) -> Result<ExecutionOutcome> {
    if !request.dry_run && !ctx.is_online() {
        return Ok(ExecutionOutcome::user_error(
            "PX_ONLINE=1 required for downloads",
            json!({
                "status": "gated-offline",
                "dry_run": request.dry_run,
                "hint": "export PX_ONLINE=1 or add --dry-run to inspect work without downloading",
            }),
        ));
    }

    if request.workspace {
        workspace::prefetch(ctx, request.dry_run)
    } else {
        handle_project_prefetch(ctx, request.dry_run)
    }
}

fn handle_project_prefetch(ctx: &CommandContext, dry_run: bool) -> Result<ExecutionOutcome> {
    let snapshot = manifest_snapshot()?;
    let lock = match load_lockfile_optional(&snapshot.lock_path)? {
        Some(lock) => lock,
        None => {
            return Ok(ExecutionOutcome::user_error(
                "px.lock not found (run `px sync`)",
                json!({
                    "lockfile": snapshot.lock_path.display().to_string(),
                    "hint": "run `px sync` to regenerate the lockfile",
                }),
            ))
        }
    };

    let lock_specs = match lock_prefetch_specs(&lock) {
        Ok(specs) => specs,
        Err(err) => {
            return Ok(ExecutionOutcome::user_error(
                err.to_string(),
                json!({ "lockfile": snapshot.lock_path.display().to_string() }),
            ))
        }
    };

    if lock_specs.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            "px.lock does not contain artifact metadata",
            json!({ "lockfile": snapshot.lock_path.display().to_string() }),
        ));
    }

    let cache = ctx.cache();
    let store_specs = store_prefetch_specs(&lock_specs);
    let summary = ctx.cache_store().prefetch(
        &cache.path,
        &store_specs,
        StorePrefetchOptions {
            dry_run,
            parallel: 4,
        },
    )?;

    let mut details = json!({
        "lockfile": snapshot.lock_path.display().to_string(),
        "cache": {
            "path": cache.path.display().to_string(),
            "source": cache.source,
        },
        "dry_run": dry_run,
        "summary": summary,
    });
    details["status"] =
        serde_json::Value::String(if dry_run { "dry-run" } else { "prefetched" }.to_string());

    if summary.failed > 0 {
        return Ok(ExecutionOutcome::user_error(
            "prefetch encountered errors",
            details,
        ));
    }

    let message = if dry_run {
        format!(
            "dry-run {} artifacts ({} cached)",
            summary.requested, summary.hit
        )
    } else {
        format!(
            "hydrated {} artifacts ({} cached, {} fetched)",
            summary.requested, summary.hit, summary.fetched
        )
    };

    Ok(ExecutionOutcome::success(message, details))
}
