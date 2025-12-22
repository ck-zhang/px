use std::cmp::Ordering;
use std::path::Path;
use std::str::FromStr;
use std::sync::{mpsc, Arc, Mutex};
use std::{thread, time::Duration};

use anyhow::{anyhow, Context, Result};
use pep440_rs::{Operator, VersionSpecifiers};
use pep508_rs::{MarkerEnvironment, Requirement as PepRequirement, VersionOrUrl};
use px_domain::api::{
    canonical_extras, canonicalize_package_name, format_specifier, marker_applies,
    normalize_dist_name, LockedArtifact, PinSpec, ResolvedDependency, ResolvedDistSource,
};
use reqwest::{blocking::Client, StatusCode};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::context::CommandContext;
use crate::core::runtime::builder::builder_identity_from_tags;
use crate::progress::{download_concurrency, ProgressReporter};
use crate::pypi::{PypiFile, PypiReleaseResponse};
use crate::python_sys::{detect_interpreter, detect_interpreter_tags, InterpreterTags};
use crate::store::{wheel_build_options_hash, ArtifactRequest, CacheLocation, SdistRequest};
use crate::{InstallUserError, PX_VERSION};

use super::effects;

pub(crate) fn archive_source_dir_for_sdist(source_dir: &Path, prefix: &str) -> Result<Vec<u8>> {
    use std::ffi::OsStr;
    use std::fs::{self, File};
    use std::io::ErrorKind;

    use flate2::Compression;
    use flate2::GzBuilder;

    const MIN_ZIP_TIMESTAMP: u64 = 315_532_800; // 1980-01-01T00:00:00Z

    fn should_skip_entry(entry: &walkdir::DirEntry) -> bool {
        let name = entry.file_name();
        matches!(
            name.to_str().unwrap_or_default(),
            ".git"
                | ".px"
                | "__pycache__"
                | ".pytest_cache"
                | ".mypy_cache"
                | ".ruff_cache"
                | ".cache"
                | ".venv"
                | ".tox"
                | "target"
                | "dist"
                | "build"
                | "node_modules"
                | ".idea"
                | ".vscode"
        ) || name == OsStr::new("px.lock")
            || name == OsStr::new("px.workspace.lock")
    }

    let prefix = prefix.trim().trim_end_matches('/');
    if prefix.is_empty() {
        return Err(anyhow!("sdist archive prefix must be non-empty"));
    }

    let encoder = GzBuilder::new()
        .mtime(0)
        .write(Vec::new(), Compression::default());
    let mut builder = tar::Builder::new(encoder);
    builder.follow_symlinks(false);
    for entry in walkdir::WalkDir::new(source_dir)
        .sort_by(|a, b| a.path().cmp(b.path()))
        .into_iter()
        .filter_entry(|entry| !should_skip_entry(entry))
    {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                tracing::debug!(%err, root=%source_dir.display(), "skipping path during archive walk");
                continue;
            }
        };
        let path = entry.path();
        if path == source_dir {
            continue;
        }
        let rel = path
            .strip_prefix(source_dir)
            .with_context(|| format!("failed to relativize {}", path.display()))?;
        let rel_path = rel.to_string_lossy().replace('\\', "/");
        if rel_path.is_empty() {
            continue;
        }
        let rel_path = format!("{prefix}/{rel_path}");

        let metadata = match fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == ErrorKind::PermissionDenied => {
                tracing::debug!(path=%path.display(), "skipping unreadable path during archive");
                continue;
            }
            Err(err) => return Err(err.into()),
        };

        let file_type = metadata.file_type();
        let mut header = tar::Header::new_gnu();
        header.set_mtime(MIN_ZIP_TIMESTAMP);
        header.set_uid(0);
        header.set_gid(0);
        let _ = header.set_username("");
        let _ = header.set_groupname("");
        if file_type.is_dir() {
            header.set_entry_type(tar::EntryType::Directory);
            header.set_mode(0o755);
            header.set_size(0);
            builder.append_data(&mut header, Path::new(&rel_path), std::io::empty())?;
        } else if file_type.is_file() {
            header.set_entry_type(tar::EntryType::Regular);
            header.set_mode(0o644);
            header.set_size(metadata.len());
            let file = match File::open(path) {
                Ok(file) => file,
                Err(err) if err.kind() == ErrorKind::PermissionDenied => {
                    tracing::debug!(path=%path.display(), "skipping unreadable file during archive");
                    continue;
                }
                Err(err) => return Err(err.into()),
            };
            builder.append_data(&mut header, Path::new(&rel_path), file)?;
        } else if file_type.is_symlink() {
            let target = match fs::read_link(path) {
                Ok(link) => link,
                Err(err) if err.kind() == ErrorKind::PermissionDenied => {
                    tracing::debug!(
                        path = %path.display(),
                        "skipping unreadable symlink during archive"
                    );
                    continue;
                }
                Err(err) => return Err(err.into()),
            };
            let target = if target.is_absolute() {
                target
            } else {
                path.parent()
                    .unwrap_or(source_dir)
                    .join(target)
                    .to_path_buf()
            };
            let target = match fs::canonicalize(&target) {
                Ok(path) => path,
                Err(err) if err.kind() == ErrorKind::NotFound => {
                    tracing::debug!(
                        symlink = %path.display(),
                        target = %target.display(),
                        "skipping missing symlink target during archive"
                    );
                    continue;
                }
                Err(err) => return Err(err.into()),
            };
            let meta = match fs::symlink_metadata(&target) {
                Ok(meta) => meta,
                Err(err) => return Err(err.into()),
            };
            if meta.is_file() {
                header.set_entry_type(tar::EntryType::Regular);
                header.set_mode(0o644);
                header.set_size(meta.len());
                let file = File::open(&target)?;
                builder.append_data(&mut header, Path::new(&rel_path), file)?;
            } else if meta.is_dir() {
                // Archive the linked directory contents at the symlink path.
                header.set_entry_type(tar::EntryType::Directory);
                header.set_mode(0o755);
                header.set_size(0);
                builder.append_data(&mut header, Path::new(&rel_path), std::io::empty())?;

                for entry in walkdir::WalkDir::new(&target)
                    .sort_by(|a, b| a.path().cmp(b.path()))
                    .into_iter()
                    .filter_entry(|entry| !should_skip_entry(entry))
                {
                    let entry = match entry {
                        Ok(entry) => entry,
                        Err(err) => {
                            tracing::debug!(
                                %err,
                                target = %target.display(),
                                "skipping path during symlink target archive walk"
                            );
                            continue;
                        }
                    };
                    let nested = entry.path();
                    if nested == target {
                        continue;
                    }
                    let target_rel = match nested.strip_prefix(&target) {
                        Ok(rel) => rel,
                        Err(_) => continue,
                    };
                    let target_rel = target_rel.to_string_lossy().replace('\\', "/");
                    if target_rel.is_empty() {
                        continue;
                    }
                    let nested_archive_path = format!("{rel_path}/{target_rel}");

                    let meta = match fs::symlink_metadata(nested) {
                        Ok(meta) => meta,
                        Err(err) if err.kind() == ErrorKind::PermissionDenied => {
                            tracing::debug!(
                                path = %nested.display(),
                                "skipping unreadable symlink target path during archive"
                            );
                            continue;
                        }
                        Err(err) => return Err(err.into()),
                    };
                    let file_type = meta.file_type();
                    let mut header = tar::Header::new_gnu();
                    header.set_mtime(MIN_ZIP_TIMESTAMP);
                    header.set_uid(0);
                    header.set_gid(0);
                    let _ = header.set_username("");
                    let _ = header.set_groupname("");
                    if file_type.is_dir() {
                        header.set_entry_type(tar::EntryType::Directory);
                        header.set_mode(0o755);
                        header.set_size(0);
                        builder.append_data(
                            &mut header,
                            Path::new(&nested_archive_path),
                            std::io::empty(),
                        )?;
                    } else if file_type.is_file() {
                        header.set_entry_type(tar::EntryType::Regular);
                        header.set_mode(0o644);
                        header.set_size(meta.len());
                        let file = match File::open(nested) {
                            Ok(file) => file,
                            Err(err) if err.kind() == ErrorKind::PermissionDenied => {
                                tracing::debug!(
                                    path = %nested.display(),
                                    "skipping unreadable file during symlink target archive"
                                );
                                continue;
                            }
                            Err(err) => return Err(err.into()),
                        };
                        builder.append_data(&mut header, Path::new(&nested_archive_path), file)?;
                    }
                }
            }
        }
    }
    builder.finish()?;
    let encoder = builder.into_inner()?;
    Ok(encoder.finish()?)
}

pub(crate) fn resolve_pins(
    ctx: &CommandContext,
    pins: &[PinSpec],
    force_sdist: bool,
) -> Result<Vec<ResolvedDependency>> {
    if pins.is_empty() {
        return Ok(Vec::new());
    }

    let python = detect_interpreter()?;
    let tags = Arc::new(detect_interpreter_tags(&python)?);
    let cache = ctx.cache().clone();
    let effects = ctx.shared_effects();

    let progress = ProgressReporter::bar("Downloading artifacts", pins.len());
    let worker_count = download_concurrency(pins.len());
    let (job_tx, job_rx) = mpsc::channel();
    for pin in pins {
        job_tx.send(pin.clone()).expect("queue artifacts");
    }
    drop(job_tx);

    let job_rx = Arc::new(Mutex::new(job_rx));
    let (result_tx, result_rx) = mpsc::channel();

    for _ in 0..worker_count {
        let work_rx = Arc::clone(&job_rx);
        let result_tx = result_tx.clone();
        let effects = effects.clone();
        let cache = cache.clone();
        let python = python.clone();
        let tags = Arc::clone(&tags);
        thread::spawn(move || {
            let pypi = effects.pypi();
            let cache_store = effects.cache();
            loop {
                let pin = {
                    let guard = work_rx.lock().expect("lock job receiver");
                    match guard.recv() {
                        Ok(pin) => pin,
                        Err(_) => break,
                    }
                };

                let outcome = download_artifact(
                    pypi,
                    cache_store,
                    &cache,
                    &python,
                    tags.as_ref(),
                    pin,
                    force_sdist,
                );
                if result_tx.send(outcome).is_err() {
                    break;
                }
            }
        });
    }
    drop(result_tx);

    let mut resolved = Vec::with_capacity(pins.len());
    for result in result_rx {
        progress.increment();
        match result {
            Ok(dep) => resolved.push(dep),
            Err(err) => return Err(err),
        }
    }

    progress.finish(format!("Downloaded {} artifacts", resolved.len()));
    Ok(resolved)
}

fn download_artifact(
    pypi: &dyn effects::PypiClient,
    cache_store: &dyn effects::CacheStore,
    cache: &CacheLocation,
    python: &str,
    tags: &InterpreterTags,
    pin: PinSpec,
    force_sdist: bool,
) -> Result<ResolvedDependency> {
    let builder = builder_identity_from_tags(tags);
    let default_build_hash = wheel_build_options_hash(python)?;
    let artifact = match pin.source.as_ref() {
        Some(ResolvedDistSource::Directory { path }) => build_wheel_via_directory(
            cache_store,
            cache,
            &pin,
            python,
            &default_build_hash,
            &builder.builder_id,
            &cache.path,
            path,
        )?,
        Some(other) => {
            return Err(InstallUserError::new(
                "unsupported dependency source",
                json!({
                    "specifier": pin.specifier,
                    "source": format!("{other:?}"),
                    "hint": "Use registry-based requirements or adopt the dependency into the workspace.",
                }),
            )
            .into())
        }
        None => {
            let release = pypi.fetch_release(&pin.normalized, &pin.version, &pin.specifier)?;
            if force_sdist {
                build_wheel_via_sdist(
                    cache_store,
                    cache,
                    &release,
                    &pin,
                    python,
                    &default_build_hash,
                    &builder.builder_id,
                    &cache.path,
                )?
            } else {
                match select_wheel(&release.urls, tags, &pin.specifier) {
                    Ok(wheel) => {
                        let request = ArtifactRequest {
                            name: &pin.normalized,
                            version: &pin.version,
                            filename: &wheel.filename,
                            url: &wheel.url,
                            sha256: &wheel.sha256,
                        };
                        let cached = cache_store.cache_wheel(&cache.path, &request)?;
                        LockedArtifact {
                            filename: wheel.filename.clone(),
                            url: wheel.url.clone(),
                            sha256: wheel.sha256.clone(),
                            size: cached.size,
                            cached_path: cached.wheel_path.display().to_string(),
                            python_tag: wheel.python_tag.clone(),
                            abi_tag: wheel.abi_tag.clone(),
                            platform_tag: wheel.platform_tag.clone(),
                            build_options_hash: default_build_hash.clone(),
                            is_direct_url: false,
                        }
                    }
                    Err(_) => build_wheel_via_sdist(
                        cache_store,
                        cache,
                        &release,
                        &pin,
                        python,
                        &default_build_hash,
                        &builder.builder_id,
                        &cache.path,
                    )?,
                }
            }
        }
    };

    Ok(ResolvedDependency {
        name: pin.name,
        specifier: pin.specifier,
        extras: pin.extras,
        marker: pin.marker,
        artifact,
        direct: pin.direct,
        requires: pin.requires,
        source: None,
    })
}

#[allow(clippy::too_many_arguments)]
fn build_wheel_via_directory(
    cache_store: &dyn effects::CacheStore,
    cache: &CacheLocation,
    pin: &PinSpec,
    python: &str,
    default_build_hash: &str,
    builder_id: &str,
    builder_root: &Path,
    source_dir: &Path,
) -> Result<LockedArtifact> {
    if !source_dir.exists() {
        return Err(InstallUserError::new(
            "dependency source directory not found",
            json!({
                "specifier": pin.specifier,
                "path": source_dir.display().to_string(),
            }),
        )
        .into());
    }

    let archive_prefix = format!("{}-{}", pin.normalized, pin.version);
    let archive = archive_source_dir_for_sdist(source_dir, &archive_prefix)?;
    let sha256 = hex::encode(Sha256::digest(&archive));
    let archive_root = cache
        .path
        .join("workspace-sources")
        .join(&pin.normalized)
        .join(&pin.version);
    std::fs::create_dir_all(&archive_root)?;
    let archive_path = archive_root.join(format!("{sha256}.tar.gz"));
    if !archive_path.exists() {
        std::fs::write(&archive_path, &archive)?;
    }
    let archive_url = archive_path.display().to_string();
    let filename = format!("{}-{}.tar.gz", pin.normalized, pin.version);
    let built = cache_store.ensure_sdist_build(
        &cache.path,
        &SdistRequest {
            normalized_name: &pin.normalized,
            version: &pin.version,
            filename: &filename,
            url: &archive_url,
            sha256: Some(&sha256),
            source_subdir: None,
            python_path: python,
            builder_id,
            builder_root: Some(builder_root.to_path_buf()),
        },
    )?;
    let build_options_hash = if built.build_options_hash.is_empty() {
        default_build_hash.to_string()
    } else {
        built.build_options_hash.clone()
    };
    Ok(LockedArtifact {
        filename: built.filename,
        url: source_dir.display().to_string(),
        sha256: built.sha256,
        size: built.size,
        cached_path: built.cached_path.display().to_string(),
        python_tag: built.python_tag,
        abi_tag: built.abi_tag,
        platform_tag: built.platform_tag,
        build_options_hash,
        is_direct_url: true,
    })
}

#[allow(clippy::too_many_arguments)]
fn build_wheel_via_sdist(
    cache_store: &dyn effects::CacheStore,
    cache: &CacheLocation,
    release: &PypiReleaseResponse,
    pin: &PinSpec,
    python: &str,
    default_build_hash: &str,
    builder_id: &str,
    builder_root: &Path,
) -> Result<LockedArtifact> {
    let sdist = select_sdist(&release.urls, &pin.specifier)?;
    let built = cache_store.ensure_sdist_build(
        &cache.path,
        &SdistRequest {
            normalized_name: &pin.normalized,
            version: &pin.version,
            filename: &sdist.filename,
            url: &sdist.url,
            sha256: Some(&sdist.digests.sha256),
            source_subdir: None,
            python_path: python,
            builder_id,
            builder_root: Some(builder_root.to_path_buf()),
        },
    )?;
    let build_options_hash = if built.build_options_hash.is_empty() {
        default_build_hash.to_string()
    } else {
        built.build_options_hash.clone()
    };
    Ok(LockedArtifact {
        filename: built.filename,
        url: built.url,
        sha256: built.sha256,
        size: built.size,
        cached_path: built.cached_path.display().to_string(),
        python_tag: built.python_tag,
        abi_tag: built.abi_tag,
        platform_tag: built.platform_tag,
        build_options_hash,
        is_direct_url: false,
    })
}

fn select_sdist<'a>(files: &'a [PypiFile], specifier: &str) -> Result<&'a PypiFile> {
    files
        .iter()
        .find(|file| file.packagetype == "sdist" && !file.yanked.unwrap_or(false))
        .ok_or_else(|| {
            InstallUserError::new(
                format!("PyPI does not provide an sdist for {specifier}"),
                json!({ "specifier": specifier }),
            )
            .into()
        })
}

pub(crate) fn build_http_client() -> Result<Client> {
    let keep_proxies = std::env::var("PX_KEEP_PROXIES")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let builder = Client::builder()
        .user_agent(format!("px/{PX_VERSION}"))
        .timeout(Duration::from_secs(60));
    let builder = if keep_proxies {
        builder
    } else {
        builder.no_proxy()
    };
    builder.build().context("failed to build HTTP client")
}

pub(crate) fn fetch_release(
    client: &Client,
    normalized: &str,
    version: &str,
    specifier: &str,
) -> Result<PypiReleaseResponse> {
    if std::env::var("PX_ONLINE").ok().is_some_and(|value| {
        matches!(
            value.to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off" | ""
        )
    }) {
        return Err(InstallUserError::new(
            "PX_ONLINE=1 required to query PyPI",
            json!({
                "reason": "offline",
                "package": normalized,
                "version": version,
                "specifier": specifier,
                "hint": "Re-run with --online / set PX_ONLINE=1, or prefetch artifacts while online.",
            }),
        )
        .into());
    }
    let url = format!("{PYPI_BASE_URL}/{normalized}/{version}/json");
    let mut last_json_error = None;
    let mut last_send_error = None;
    for attempt in 1..=3 {
        let response = match client.get(&url).send() {
            Ok(response) => response,
            Err(err) => {
                last_send_error = Some(err);
                thread::sleep(Duration::from_millis(150 * attempt));
                continue;
            }
        };
        if response.status() == StatusCode::NOT_FOUND {
            return Err(InstallUserError::new(
                format!("PyPI does not provide {specifier}"),
                json!({ "specifier": specifier }),
            )
            .into());
        }
        let response = response
            .error_for_status()
            .map_err(|err| anyhow!("PyPI returned an error for {specifier}: {err}"))?;
        match response.json::<PypiReleaseResponse>() {
            Ok(result) => return Ok(result),
            Err(err) => {
                last_json_error = Some(err);
                thread::sleep(Duration::from_millis(150 * attempt));
            }
        }
    }
    if let Some(err) = last_send_error {
        return Err(anyhow!("failed to query PyPI for {specifier}: {err}"));
    }
    Err(anyhow!(
        "invalid JSON for {specifier}: {}",
        last_json_error
            .map(|err| err.to_string())
            .unwrap_or_else(|| "unknown error".to_string())
    ))
}

#[derive(Clone, Debug)]
pub struct WheelCandidate {
    pub filename: String,
    pub url: String,
    pub sha256: String,
    pub python_tag: String,
    pub abi_tag: String,
    pub platform_tag: String,
}

pub fn select_wheel(
    files: &[PypiFile],
    tags: &InterpreterTags,
    specifier: &str,
) -> Result<WheelCandidate> {
    let mut candidates = Vec::new();
    for file in files {
        if file.packagetype != "bdist_wheel" || file.yanked.unwrap_or(false) {
            continue;
        }
        let Some((python_tag, abi_tag, platform_tag)) = parse_wheel_tags(&file.filename) else {
            continue;
        };
        let candidate = WheelCandidate {
            filename: file.filename.clone(),
            url: file.url.clone(),
            sha256: file.digests.sha256.clone(),
            python_tag,
            abi_tag,
            platform_tag,
        };
        if wheel_supported(&candidate, tags) {
            candidates.push(candidate);
        }
    }

    if let Some(universal) = candidates
        .iter()
        .find(|c| c.python_tag == "py3" && c.abi_tag == "none" && c.platform_tag == "any")
    {
        return Ok(universal.clone());
    }

    let mut best: Option<(i32, WheelCandidate)> = None;
    for candidate in candidates {
        let score = score_candidate(&candidate, tags);
        match &mut best {
            Some((best_score, best_candidate)) => match score.cmp(best_score) {
                Ordering::Greater => {
                    *best_score = score;
                    *best_candidate = candidate;
                }
                Ordering::Equal => {
                    if candidate.filename < best_candidate.filename {
                        *best_candidate = candidate;
                    }
                }
                Ordering::Less => {}
            },
            None => best = Some((score, candidate)),
        }
    }

    best.map(|(_, candidate)| candidate).ok_or_else(|| {
        InstallUserError::new(
            format!("PyPI did not provide any wheels for {specifier}"),
            json!({ "specifier": specifier }),
        )
        .into()
    })
}

fn score_candidate(candidate: &WheelCandidate, tags: &InterpreterTags) -> i32 {
    let mut score = 0;
    if matches_any(&tags.python, &candidate.python_tag) {
        score += 100;
    } else if candidate.python_tag.starts_with("py3") {
        score += 50;
    }

    if matches_any(&tags.abi, &candidate.abi_tag) {
        score += 40;
    } else if candidate.abi_tag == "none" {
        score += 20;
    }

    if candidate.platform_tag == "any" {
        score += 30;
    } else if matches_any(&tags.platform, &candidate.platform_tag) {
        score += 25;
    }

    score
}

fn matches_any(values: &[String], candidate: &str) -> bool {
    candidate
        .split('.')
        .any(|part| values.iter().any(|val| part.eq_ignore_ascii_case(val)))
}

fn wheel_supported(candidate: &WheelCandidate, tags: &InterpreterTags) -> bool {
    let combos = candidate_tag_combos(candidate);
    if !tags.supported.is_empty()
        && combos
            .iter()
            .any(|(py, abi, platform)| tags.supports_triple(py, abi, platform))
    {
        return true;
    }
    fallback_python(&candidate.python_tag, &tags.python)
        && fallback_abi(&candidate.abi_tag, &tags.abi)
        && fallback_platform(&candidate.platform_tag, &tags.platform)
}

fn candidate_tag_combos(candidate: &WheelCandidate) -> Vec<(String, String, String)> {
    let python = split_tag_values(&candidate.python_tag);
    let abi = split_tag_values(&candidate.abi_tag);
    let platform = split_tag_values(&candidate.platform_tag);
    let mut combos = Vec::new();
    for py in &python {
        for abi_tag in &abi {
            for plat in &platform {
                combos.push((py.clone(), abi_tag.clone(), plat.clone()));
            }
        }
    }
    combos
}

fn split_tag_values(value: &str) -> Vec<String> {
    let mut values = value
        .split('.')
        .map(|part| part.trim().to_ascii_lowercase())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if values.is_empty() {
        values.push(value.to_ascii_lowercase());
    }
    values
}

fn fallback_python(tag: &str, supported: &[String]) -> bool {
    split_tag_values(tag)
        .iter()
        .any(|token| token == "py3" || supported.iter().any(|val| val == token))
}

fn fallback_abi(tag: &str, supported: &[String]) -> bool {
    split_tag_values(tag)
        .iter()
        .any(|token| token == "none" || supported.iter().any(|val| val == token))
}

fn fallback_platform(tag: &str, supported: &[String]) -> bool {
    split_tag_values(tag)
        .iter()
        .any(|token| platform_token_supported(supported, token))
}

fn platform_token_supported(supported: &[String], token: &str) -> bool {
    if token == "any" {
        return true;
    }
    let normalized = normalize_platform_value(token);
    for platform in supported {
        let normalized_platform = normalize_platform_value(platform);
        if normalized_platform == "any" {
            continue;
        }
        if normalized_platform == normalized
            || same_platform_family(&normalized_platform, &normalized)
        {
            return true;
        }
    }
    false
}

fn normalize_platform_value(value: &str) -> String {
    value.replace('-', "_").to_ascii_lowercase()
}

fn same_platform_family(interpreter: &str, candidate: &str) -> bool {
    if interpreter.starts_with("linux") && candidate.contains("linux") {
        return arch_overlap(interpreter, candidate);
    }
    if interpreter.starts_with("macosx") && candidate.starts_with("macosx") {
        return arch_overlap(interpreter, candidate);
    }
    if interpreter.starts_with("win") && candidate.starts_with("win") {
        return arch_overlap(interpreter, candidate);
    }
    false
}

const ARCH_ALIASES: &[(&str, &str)] = &[
    ("x86_64", "x86_64"),
    ("amd64", "x86_64"),
    ("aarch64", "aarch64"),
    ("arm64", "arm64"),
    ("armv7l", "armv7l"),
    ("armv6l", "armv6l"),
    ("i686", "i686"),
    ("i386", "i386"),
    ("ppc64le", "ppc64le"),
    ("s390x", "s390x"),
];

fn arch_overlap(a: &str, b: &str) -> bool {
    match (arch_hint(a), arch_hint(b)) {
        (Some(left), Some(right)) => left == right,
        (None, None) => true,
        _ => false,
    }
}

fn arch_hint(value: &str) -> Option<&'static str> {
    let lower = value.to_ascii_lowercase();
    for (alias, canonical) in ARCH_ALIASES {
        if lower.contains(alias) {
            return Some(*canonical);
        }
    }
    None
}

fn parse_wheel_tags(filename: &str) -> Option<(String, String, String)> {
    let path = Path::new(filename);
    if !path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("whl"))
    {
        return None;
    }
    let trimmed = path.file_stem()?.to_str()?;
    let parts: Vec<&str> = trimmed.split('-').collect();
    if parts.len() < 5 {
        return None;
    }
    let python_tag = parts[parts.len() - 3].to_string();
    let abi_tag = parts[parts.len() - 2].to_string();
    let platform_tag = parts[parts.len() - 1].to_string();
    Some((python_tag, abi_tag, platform_tag))
}

pub(crate) fn ensure_exact_pins(
    marker_env: &MarkerEnvironment,
    specs: &[String],
) -> Result<Vec<PinSpec>> {
    let mut pins = Vec::new();
    for spec in specs {
        if !marker_applies(spec, marker_env) {
            continue;
        }
        pins.push(parse_exact_pin(spec)?);
    }
    Ok(pins)
}

pub fn parse_exact_pin(spec: &str) -> Result<PinSpec> {
    let trimmed_raw = spec.trim();
    let trimmed = strip_wrapping_quotes(trimmed_raw);
    if trimmed.is_empty() {
        return Err(InstallUserError::new(
            "dependency specifier cannot be empty",
            json!({ "specifier": spec }),
        )
        .into());
    }

    let requirement = PepRequirement::from_str(trimmed).map_err(|err| {
        InstallUserError::new(
            format!("invalid requirement `{trimmed}`: {err}"),
            json!({ "specifier": trimmed }),
        )
    })?;

    let name = dependency_name(trimmed);
    if name.is_empty() {
        return Err(InstallUserError::new(
            "dependency name missing before `==`",
            json!({ "specifier": trimmed }),
        )
        .into());
    }

    let version_spec = match requirement.version_or_url.as_ref() {
        Some(VersionOrUrl::VersionSpecifier(specifiers)) => specifiers.to_string(),
        Some(VersionOrUrl::Url(_)) => {
            return Err(InstallUserError::new(
                "URL requirements are not supported in pinned installs",
                json!({ "specifier": trimmed }),
            )
            .into())
        }
        None => {
            return Err(InstallUserError::new(
                format!("px sync requires `name==version`; `{trimmed}` is not pinned"),
                json!({ "specifier": trimmed }),
            )
            .into())
        }
    };
    let parsed = VersionSpecifiers::from_str(&version_spec).map_err(|_| {
        InstallUserError::new(
            format!("px sync requires `name==version`; `{trimmed}` is not pinned"),
            json!({ "specifier": trimmed }),
        )
    })?;
    let mut iter = parsed.iter();
    let Some(first) = iter.next() else {
        return Err(InstallUserError::new(
            format!("px sync requires `name==version`; `{trimmed}` is not pinned"),
            json!({ "specifier": trimmed }),
        )
        .into());
    };
    if iter.next().is_some() || !matches!(first.operator(), Operator::Equal | Operator::ExactEqual)
    {
        return Err(InstallUserError::new(
            format!("px sync requires `name==version`; `{trimmed}` is not pinned"),
            json!({ "specifier": trimmed }),
        )
        .into());
    }
    let version_str = first.version().to_string();

    let extras = canonical_extras(
        &requirement
            .extras
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
    );
    let marker = requirement.marker.as_ref().map(ToString::to_string);
    let normalized = normalize_dist_name(&name);
    Ok(PinSpec {
        name,
        specifier: format_specifier(&normalized, &extras, &version_str, marker.as_deref()),
        version: version_str,
        normalized,
        extras,
        marker,
        direct: true,
        requires: Vec::new(),
        source: None,
    })
}

pub(crate) fn dependency_name(spec: &str) -> String {
    let trimmed = strip_wrapping_quotes(spec.trim());
    let mut end = trimmed.len();
    for (idx, ch) in trimmed.char_indices() {
        if ch.is_ascii_whitespace() || matches!(ch, '<' | '>' | '=' | '!' | '~' | ';') {
            end = idx;
            break;
        }
    }
    let head = &trimmed[..end];
    let base = head.split('[').next().unwrap_or(head);
    canonicalize_package_name(base)
}

pub(crate) fn strip_wrapping_quotes(input: &str) -> &str {
    if input.len() >= 2 {
        let bytes = input.as_bytes();
        let first = bytes[0];
        let last = bytes[input.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &input[1..input.len() - 1];
        }
    }
    input
}

const PYPI_BASE_URL: &str = "https://pypi.org/pypi";

#[cfg(test)]
mod tests {
    use super::*;

    use std::{
        path::{Path, PathBuf},
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
    };

    use crate::pypi::PypiDigests;
    use crate::store::{
        BuiltWheel, CachePruneResult, CacheUsage, CacheWalk, CachedArtifact, PrefetchOptions,
        PrefetchSpec, PrefetchSummary,
    };

    struct TestPypiClient {
        urls: Vec<PypiFile>,
    }

    impl effects::PypiClient for TestPypiClient {
        fn fetch_release(
            &self,
            _normalized: &str,
            _version: &str,
            _specifier: &str,
        ) -> Result<PypiReleaseResponse> {
            Ok(PypiReleaseResponse {
                urls: self.urls.clone(),
            })
        }
    }

    #[derive(Clone)]
    struct TestCacheStore {
        wheel_calls: Arc<AtomicUsize>,
        cache_root: PathBuf,
    }

    impl effects::CacheStore for TestCacheStore {
        fn resolve_store_path(&self) -> Result<CacheLocation> {
            Ok(CacheLocation {
                path: self.cache_root.clone(),
                source: "test",
            })
        }

        fn compute_usage(&self, _path: &Path) -> Result<CacheUsage> {
            Ok(CacheUsage {
                exists: true,
                total_entries: 0,
                total_size_bytes: 0,
            })
        }

        fn collect_walk(&self, _path: &Path) -> Result<CacheWalk> {
            Ok(CacheWalk::default())
        }

        fn prune(&self, _walk: &CacheWalk) -> CachePruneResult {
            CachePruneResult::default()
        }

        fn prefetch(
            &self,
            _cache: &Path,
            _specs: &[PrefetchSpec<'_>],
            _options: PrefetchOptions,
        ) -> Result<PrefetchSummary> {
            Ok(PrefetchSummary::default())
        }

        fn cache_wheel(&self, cache: &Path, request: &ArtifactRequest) -> Result<CachedArtifact> {
            self.wheel_calls.fetch_add(1, Ordering::SeqCst);

            std::fs::create_dir_all(cache)?;
            let wheel_path = cache.join(request.filename);
            std::fs::write(&wheel_path, b"test wheel")?;
            Ok(CachedArtifact {
                wheel_path,
                dist_path: cache.join("dist"),
                size: 9,
            })
        }

        fn ensure_sdist_build(&self, _cache: &Path, _request: &SdistRequest) -> Result<BuiltWheel> {
            panic!("unexpected sdist build: compatible wheel should be used instead")
        }
    }

    #[test]
    fn download_artifact_prefers_wheel_over_sdist_for_native_wheels() -> Result<()> {
        let python = detect_interpreter().unwrap_or_else(|_| "/usr/bin/python3".to_string());
        let tags = detect_interpreter_tags(&python)?;

        let python_tag = tags
            .python
            .first()
            .cloned()
            .unwrap_or_else(|| "py3".to_string());
        let abi_tag = tags
            .abi
            .iter()
            .find(|tag| !tag.eq_ignore_ascii_case("none"))
            .cloned()
            .or_else(|| {
                eprintln!("skipping test: unable to determine a native ABI tag");
                None
            });
        let platform_tag = tags
            .platform
            .iter()
            .find(|tag| !tag.eq_ignore_ascii_case("any"))
            .cloned()
            .or_else(|| {
                eprintln!("skipping test: unable to determine a native platform tag");
                None
            });

        let (Some(abi_tag), Some(platform_tag)) = (abi_tag, platform_tag) else {
            return Ok(());
        };

        let wheel_filename = format!("demo-1.0.0-{python_tag}-{abi_tag}-{platform_tag}.whl");
        let sha256 = "0000000000000000000000000000000000000000000000000000000000000000";

        let pypi = TestPypiClient {
            urls: vec![
                PypiFile {
                    filename: wheel_filename.clone(),
                    url: "https://example.invalid/demo.whl".to_string(),
                    packagetype: "bdist_wheel".to_string(),
                    yanked: None,
                    digests: PypiDigests {
                        sha256: sha256.to_string(),
                    },
                },
                PypiFile {
                    filename: "demo-1.0.0.tar.gz".to_string(),
                    url: "https://example.invalid/demo.tar.gz".to_string(),
                    packagetype: "sdist".to_string(),
                    yanked: None,
                    digests: PypiDigests {
                        sha256: sha256.to_string(),
                    },
                },
            ],
        };

        let wheel_calls = Arc::new(AtomicUsize::new(0));
        let cache_root = tempfile::tempdir()?;
        let cache_dir = tempfile::tempdir()?;
        let cache_store = TestCacheStore {
            wheel_calls: Arc::clone(&wheel_calls),
            cache_root: cache_root.path().to_path_buf(),
        };

        let cache = CacheLocation {
            path: cache_dir.path().to_path_buf(),
            source: "test",
        };

        let pin = parse_exact_pin("demo==1.0.0")?;
        let resolved = download_artifact(&pypi, &cache_store, &cache, &python, &tags, pin, false)?;

        assert_eq!(wheel_calls.load(Ordering::SeqCst), 1);
        assert_eq!(resolved.artifact.filename, wheel_filename);
        assert!(
            PathBuf::from(&resolved.artifact.cached_path).exists(),
            "expected cached wheel to exist at {}",
            resolved.artifact.cached_path
        );

        Ok(())
    }
}
