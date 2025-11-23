use std::path::Path;

use super::{
    wheel::{cache_wheel, ensure_wheel_dist, validate_existing, wheel_path},
    ArtifactRequest, PrefetchOptions, PrefetchSpec, PrefetchSummary,
};

/// Fetch every artifact described by `specs` into the cache and summarize the results.
pub fn prefetch_artifacts(
    cache_root: &Path,
    specs: &[PrefetchSpec<'_>],
    options: PrefetchOptions,
) -> PrefetchSummary {
    let mut summary = PrefetchSummary {
        requested: specs.len(),
        ..PrefetchSummary::default()
    };

    if specs.is_empty() {
        return summary;
    }

    let batch_size = options.parallel.max(1);

    for chunk in specs.chunks(batch_size) {
        for spec in chunk {
            let dest = wheel_path(cache_root, spec.name, spec.version, spec.filename);
            let existing = match validate_existing(&dest, spec.sha256) {
                Ok(value) => value,
                Err(err) => {
                    summary.failed += 1;
                    summary.errors.push(err.to_string());
                    continue;
                }
            };

            if options.dry_run {
                if existing.is_some() {
                    summary.hit += 1;
                }
                continue;
            }

            if let Some(file) = existing {
                match ensure_wheel_dist(&file.path, spec.sha256) {
                    Ok(_) => summary.hit += 1,
                    Err(err) => {
                        summary.failed += 1;
                        summary.errors.push(err.to_string());
                    }
                }
                continue;
            }

            let request = ArtifactRequest {
                name: spec.name,
                version: spec.version,
                filename: spec.filename,
                url: spec.url,
                sha256: spec.sha256,
            };

            match cache_wheel(cache_root, &request) {
                Ok(artifact) => {
                    summary.fetched += 1;
                    summary.bytes_fetched += artifact.size;
                }
                Err(err) => {
                    summary.failed += 1;
                    summary.errors.push(err.to_string());
                }
            }
        }
    }

    summary
}
