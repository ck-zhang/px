use anyhow::{Context, Result};
use serde_json::json;

use crate::{CommandContext, ExecutionOutcome};

#[derive(Clone, Debug, Default)]
pub struct CacheStatsRequest;

#[derive(Clone, Debug, Default)]
pub struct CachePathRequest;

#[derive(Clone, Debug)]
pub struct CachePruneRequest {
    pub all: bool,
    pub dry_run: bool,
}

pub fn cache_stats(ctx: &CommandContext, _request: CacheStatsRequest) -> Result<ExecutionOutcome> {
    cache_stats_outcome(ctx)
}

pub fn cache_path(ctx: &CommandContext, _request: CachePathRequest) -> Result<ExecutionOutcome> {
    cache_path_outcome(ctx)
}

pub fn cache_prune(ctx: &CommandContext, request: CachePruneRequest) -> Result<ExecutionOutcome> {
    cache_prune_outcome(ctx, request.all, request.dry_run)
}

fn cache_path_outcome(ctx: &CommandContext) -> Result<ExecutionOutcome> {
    let cache = ctx.cache();
    ctx.fs()
        .create_dir_all(&cache.path)
        .context("unable to create cache directory")?;
    let canonical = ctx
        .fs()
        .canonicalize(&cache.path)
        .unwrap_or(cache.path.clone());
    let path_str = canonical.display().to_string();
    Ok(ExecutionOutcome::success(
        format!("cache directory: {path_str}"),
        json!({
            "status": "path",
            "cache_path": path_str,
            "path": path_str,
            "source": cache.source,
        }),
    ))
}

fn cache_stats_outcome(ctx: &CommandContext) -> Result<ExecutionOutcome> {
    let cache = ctx.cache();
    let usage = ctx.cache_store().compute_usage(&cache.path)?;
    let message = if usage.exists {
        format!(
            "stats: {} files, {} bytes",
            usage.total_entries, usage.total_size_bytes
        )
    } else {
        format!("cache path {} not found", cache.path.display())
    };
    Ok(ExecutionOutcome::success(
        message,
        json!({
            "status": "stats",
            "cache_path": cache.path.display().to_string(),
            "cache_exists": usage.exists,
            "total_entries": usage.total_entries,
            "total_size_bytes": usage.total_size_bytes,
        }),
    ))
}

fn cache_prune_outcome(ctx: &CommandContext, all: bool, dry_run: bool) -> Result<ExecutionOutcome> {
    let cache = ctx.cache();
    if !all {
        return Ok(ExecutionOutcome::user_error(
            "px cache prune currently requires --all",
            json!({
                "cache_path": cache.path.display().to_string(),
                "dry_run": dry_run,
                "hint": "rerun with --all to prune every cached artifact",
            }),
        ));
    }

    if !cache.path.exists() {
        return Ok(ExecutionOutcome::success(
            format!("cache path {} not found", cache.path.display()),
            json!({
                "cache_path": cache.path.display().to_string(),
                "cache_exists": false,
                "dry_run": dry_run,
                "candidate_entries": 0,
                "candidate_size_bytes": 0,
                "deleted_entries": 0,
                "deleted_size_bytes": 0,
                "errors": [],
                "status": "no-cache",
            }),
        ));
    }

    let walk = ctx.cache_store().collect_walk(&cache.path)?;
    let candidate_entries = walk.files.len() as u64;
    let candidate_size_bytes = walk.total_bytes;

    if candidate_entries == 0 {
        return Ok(ExecutionOutcome::success(
            format!("nothing to remove under {}", cache.path.display()),
            json!({
                "cache_path": cache.path.display().to_string(),
                "cache_exists": true,
                "dry_run": dry_run,
                "candidate_entries": 0,
                "candidate_size_bytes": 0,
                "deleted_entries": 0,
                "deleted_size_bytes": 0,
                "errors": [],
                "status": if dry_run { "dry-run" } else { "success" },
            }),
        ));
    }

    if dry_run {
        return Ok(ExecutionOutcome::success(
            format!(
                "would remove {} files ({candidate_size_bytes} bytes)",
                candidate_entries
            ),
            json!({
                "cache_path": cache.path.display().to_string(),
                "cache_exists": true,
                "dry_run": true,
                "candidate_entries": candidate_entries,
                "candidate_size_bytes": candidate_size_bytes,
                "deleted_entries": 0,
                "deleted_size_bytes": 0,
                "errors": [],
                "status": "dry-run",
            }),
        ));
    }

    let prune = ctx.cache_store().prune(&walk);
    let error_count = prune.errors.len();
    let errors_json: Vec<_> = prune
        .errors
        .iter()
        .map(|err| {
            json!({
                "path": err.path.display().to_string(),
                "error": err.error,
            })
        })
        .collect();
    let details = json!({
        "cache_path": cache.path.display().to_string(),
        "cache_exists": true,
        "dry_run": false,
        "candidate_entries": prune.candidate_entries,
        "candidate_size_bytes": prune.candidate_size_bytes,
        "deleted_entries": prune.deleted_entries,
        "deleted_size_bytes": prune.deleted_size_bytes,
        "errors": errors_json,
        "status": if error_count == 0 { "success" } else { "partial" },
    });

    if error_count == 0 {
        Ok(ExecutionOutcome::success(
            format!(
                "removed {} files ({} bytes)",
                prune.deleted_entries, prune.deleted_size_bytes
            ),
            details,
        ))
    } else {
        Ok(ExecutionOutcome::failure(
            format!(
                "removed {} files but {} errors occurred",
                prune.deleted_entries, error_count
            ),
            details,
        ))
    }
}
