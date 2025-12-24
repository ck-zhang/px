use std::borrow::Cow;
use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};

use crate::core::system_deps::{system_deps_from_names, write_sys_deps_metadata};

use super::builder::{build_with_container_builder, load_builder_system_deps};
use super::download::{download_sdist, persist_named_tempfile, persist_or_copy};
use super::native_libs::copy_native_libs;
use super::wheel::find_wheel;
use super::{compute_build_options_hash, sanitize_builder_id, BuildMethod};

use super::super::{
    cas::{
        archive_dir_canonical, global_store, pkg_build_lookup_key, source_lookup_key, ObjectKind,
        ObjectPayload, PkgBuildHeader, SourceHeader,
    },
    load_cached_build, persist_metadata,
    wheel::{compute_sha256, ensure_wheel_dist, parse_wheel_tags, wheel_path},
    BuiltWheel, SdistRequest,
};

/// Build or retrieve an sdist-derived wheel from the cache.
///
/// # Errors
///
/// Returns an error when the archive cannot be unpacked, hashed, or copied into
/// the cache directory.
pub fn ensure_sdist_build(cache_root: &Path, request: &SdistRequest<'_>) -> Result<BuiltWheel> {
    let cas = global_store();

    let precomputed_id = request.sha256.map(|sha| build_identifier(request, sha));
    if let Some(id) = &precomputed_id {
        let meta_path = cache_root.join("sdist-build").join(id).join("meta.json");
        if let Some(built) = load_cached_build(&meta_path)? {
            if cache_hit_matches(request, &built)? && built.cached_path.exists() {
                return Ok(built);
            }
        }
    }

    let (temp_file, download_sha) = download_sdist(request)?;
    if let Some(expected) = request.sha256 {
        if download_sha != expected {
            bail!(
                "sdist checksum mismatch for {} (expected {}, got {})",
                request.filename,
                expected,
                download_sha
            );
        }
    }
    let sdist_bytes = fs::read(temp_file.path())?;
    let source_header = SourceHeader {
        name: request.normalized_name.to_string(),
        version: request.version.to_string(),
        filename: request.filename.to_string(),
        index_url: request.url.to_string(),
        sha256: download_sha.clone(),
    };
    let source_key = source_lookup_key(&source_header);
    let source_oid = match cas.lookup_key(ObjectKind::Source, &source_key)? {
        Some(oid) => oid,
        None => {
            let payload = ObjectPayload::Source {
                header: source_header.clone(),
                bytes: Cow::Owned(sdist_bytes.clone()),
            };
            let stored = cas.store(&payload)?;
            cas.record_key(ObjectKind::Source, &source_key, &stored.oid)?;
            stored.oid
        }
    };
    let build_id = precomputed_id.unwrap_or_else(|| build_identifier(request, &download_sha));
    let build_root = cache_root.join("sdist-build").join(&build_id);
    let meta_path = build_root.join("meta.json");

    if let Some(built) = load_cached_build(&meta_path)? {
        if cache_hit_matches(request, &built)? && built.cached_path.exists() {
            return Ok(built);
        }
        if built.builder_id != request.builder_id && build_root.exists() {
            let _ = fs::remove_dir_all(&build_root);
        }
    }

    fs::create_dir_all(&build_root)?;
    let sdist_path = build_root.join(request.filename);
    persist_named_tempfile(temp_file, &sdist_path)
        .map_err(|err| anyhow!("unable to persist sdist download: {err}"))?;

    let dist_dir = build_root.join("dist");
    if dist_dir.exists() {
        fs::remove_dir_all(&dist_dir)?;
    }
    fs::create_dir_all(&dist_dir)?;

    fn build_with_host_pip_wheel(
        python: &str,
        sdist_path: &Path,
        dist_dir: &Path,
        build_root: &Path,
    ) -> Result<()> {
        let mut cmd = Command::new(python);
        cmd.current_dir(build_root)
            .env("PIP_DISABLE_PIP_VERSION_CHECK", "1")
            .env("PIP_NO_INPUT", "1")
            .env("PIP_PROGRESS_BAR", "off")
            .args(["-m", "pip", "wheel", "--no-deps", "--wheel-dir"])
            .arg(dist_dir)
            .arg(sdist_path);
        let output = cmd
            .output()
            .with_context(|| format!("failed to run `{python} -m pip wheel`"))?;
        if !output.status.success() {
            bail!(
                "pip wheel failed (code {}):\nstdout:\n{}\nstderr:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(())
    }

    let (build_method, build_python_path, builder_env_root) =
        match build_with_container_builder(request, &sdist_path, &dist_dir, &build_root) {
            Ok(builder) => (
                BuildMethod::BuilderWheel,
                builder.python_path.clone(),
                Some(builder.env_root.clone()),
            ),
            Err(container_err) => {
                if dist_dir.exists() {
                    let _ = fs::remove_dir_all(&dist_dir);
                }
                fs::create_dir_all(&dist_dir)?;
                build_with_host_pip_wheel(
                    request.python_path,
                    &sdist_path,
                    &dist_dir,
                    &build_root,
                )
                .map_err(|host_err| {
                    anyhow!(
                        "sdist build failed via container builder ({container_err}); fallback pip wheel also failed ({host_err})"
                    )
                })?;
                (BuildMethod::PipWheel, Path::new(request.python_path).to_path_buf(), None)
            }
        };
    let mut system_deps = load_builder_system_deps(&build_root);
    if system_deps.is_empty() {
        system_deps = system_deps_from_names([request.normalized_name]);
    }

    let built_wheel_path = find_wheel(&dist_dir)?;
    let filename = built_wheel_path
        .file_name()
        .ok_or_else(|| anyhow!("wheel missing filename"))?
        .to_string_lossy()
        .to_string();
    let (python_tag, abi_tag, platform_tag) = parse_wheel_tags(&filename)
        .ok_or_else(|| anyhow!("unable to parse wheel tags from {filename}"))?;

    let dest = wheel_path(
        cache_root,
        request.normalized_name,
        request.version,
        &filename,
    );
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    persist_or_copy(&built_wheel_path, &dest)?;

    let sha256 = compute_sha256(&dest)?;
    let size = fs::metadata(&dest)?.len();
    let dist_path = ensure_wheel_dist(&dest, &sha256)?;
    if let Some(env_root) = &builder_env_root {
        copy_native_libs(env_root, &dist_path)?;
    }
    if !system_deps.capabilities.is_empty() || !system_deps.apt_packages.is_empty() {
        write_sys_deps_metadata(&dist_path, request.normalized_name, &system_deps)?;
    }
    let runtime_abi = format!("{python_tag}-{abi_tag}-{platform_tag}");
    let mut build_options_hash = compute_build_options_hash(
        build_python_path.to_str().unwrap_or(request.python_path),
        build_method,
    )?;
    if builder_env_root.is_some() {
        build_options_hash = format!("{build_options_hash}-native-libs");
    }
    if let Some(fingerprint) = system_deps.fingerprint() {
        build_options_hash = format!("{build_options_hash}-sys{fingerprint}");
    }
    let archive = archive_dir_canonical(&dist_path)?;
    let pkg_header = PkgBuildHeader {
        source_oid,
        runtime_abi,
        builder_id: request.builder_id.to_string(),
        build_options_hash: build_options_hash.clone(),
    };
    let pkg_key = pkg_build_lookup_key(&pkg_header);
    let pkg_payload = ObjectPayload::PkgBuild {
        header: pkg_header.clone(),
        archive: Cow::Owned(archive),
    };
    let stored_pkg = cas.store(&pkg_payload)?;
    cas.record_key(ObjectKind::PkgBuild, &pkg_key, &stored_pkg.oid)?;

    let built = BuiltWheel {
        filename,
        url: request.url.to_string(),
        sha256,
        source_sha256: download_sha,
        size,
        cached_path: dest.clone(),
        dist_path: dist_path.clone(),
        python_tag,
        abi_tag,
        platform_tag,
        build_options_hash,
        system_deps: system_deps.clone(),
        build_method,
        builder_id: request.builder_id.to_string(),
    };
    persist_metadata(&meta_path, &built)?;

    let _ = fs::remove_dir_all(&dist_dir);

    Ok(built)
}

fn build_identifier(request: &SdistRequest<'_>, sha256: &str) -> String {
    let short_len = sha256.len().min(16);
    let builder = sanitize_builder_id(request.builder_id);
    format!(
        "{}-{}-{}-{}",
        request.normalized_name,
        request.version,
        &sha256[..short_len],
        builder
    )
}

fn cache_hit_matches(request: &SdistRequest<'_>, built: &BuiltWheel) -> Result<bool> {
    if !built.builder_id.is_empty() && built.builder_id != request.builder_id {
        return Ok(false);
    }
    if built.build_method == BuildMethod::BuilderWheel {
        let mut expected = compute_build_options_hash(request.python_path, built.build_method)?;
        expected = format!("{expected}-native-libs");
        if let Some(fingerprint) = built.system_deps.fingerprint() {
            expected = format!("{expected}-sys{fingerprint}");
        }
        return Ok(built.build_options_hash == expected);
    }
    if built.build_options_hash.is_empty() {
        return Ok(false);
    }
    let mut expected = compute_build_options_hash(request.python_path, built.build_method)?;
    if let Some(fingerprint) = built.system_deps.fingerprint() {
        expected = format!("{expected}-sys{fingerprint}");
    }
    Ok(built.build_options_hash == expected)
}
