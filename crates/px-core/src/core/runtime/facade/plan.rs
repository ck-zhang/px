use std::collections::{HashMap, HashSet};
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use crate::context::CommandContext;
use crate::core::runtime::artifacts::{dependency_name, ensure_exact_pins, resolve_pins};
use crate::core::runtime::Effects;
use crate::core::sandbox::{
    ensure_system_deps_rootfs, pin_system_deps, system_deps_mode, SystemDepsMode,
};
use crate::core::system_deps::system_deps_from_names;
use crate::diagnostics::commands as diag_commands;
use crate::outcome::InstallUserError;
use crate::progress::ProgressReporter;
use crate::python_sys::{detect_interpreter, detect_interpreter_tags, detect_marker_environment};
use anyhow::{anyhow, Result};
use pep508_rs::MarkerEnvironment;
use px_domain::api::{
    analyze_lock_diff, autopin_pin_key, autopin_spec_key, canonicalize_package_name,
    canonicalize_spec, detect_lock_drift, format_specifier, load_lockfile_optional, marker_applies,
    merge_resolved_dependencies, parse_lockfile, render_lockfile, resolve, spec_requires_pin,
    validate_lock_closure, verify_locked_artifacts, AutopinEntry, InstallOverride, PinSpec,
    ProjectSnapshot, ResolverRequest, ResolverTags,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use toml_edit::{Array, DocumentMut, Item, Table, Value as TomlValue};

use super::sandbox::{apply_system_lib_compatibility, SysEnvGuard};
use super::{prepare_project_runtime, ManifestSnapshot, PX_VERSION};

fn is_inline_snapshot(ctx: &CommandContext, snapshot: &ManifestSnapshot) -> bool {
    snapshot.root.starts_with(ctx.cache().path.join("scripts"))
}

pub(crate) struct InstallOutcome {
    pub(crate) state: InstallState,
    pub(crate) lockfile: String,
    pub(crate) drift: Vec<String>,
    #[allow(dead_code)]
    pub(crate) verified: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum InstallState {
    Installed,
    UpToDate,
    Drift,
    MissingLock,
}

pub fn lock_is_fresh(ctx: &CommandContext, snapshot: &ManifestSnapshot) -> Result<bool> {
    fn artifact_supported_by_runtime(
        tags: &crate::python_sys::InterpreterTags,
        artifact: &px_domain::api::LockedArtifact,
    ) -> bool {
        if artifact.filename.is_empty() {
            return true;
        }
        if !artifact.filename.to_ascii_lowercase().ends_with(".whl") {
            return true;
        }
        if artifact.python_tag.is_empty()
            || artifact.abi_tag.is_empty()
            || artifact.platform_tag.is_empty()
        {
            return true;
        }

        let supports = |py: &str, abi: &str, platform: &str| {
            if !tags.supported.is_empty() {
                tags.supports_triple(py, abi, platform)
            } else {
                tags.python.iter().any(|tag| tag == py)
                    && tags.abi.iter().any(|tag| tag == abi)
                    && tags.platform.iter().any(|tag| tag == platform)
            }
        };

        for py in artifact
            .python_tag
            .split('.')
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let py = py.to_ascii_lowercase();
            for abi in artifact
                .abi_tag
                .split('.')
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                let abi = abi.to_ascii_lowercase();
                for platform in artifact
                    .platform_tag
                    .split('.')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    let platform = platform.to_ascii_lowercase();
                    if supports(&py, &abi, &platform) {
                        return true;
                    }
                }
            }
        }
        false
    }

    let marker_env = marker_env_for_snapshot(snapshot);
    match load_lockfile_optional(&snapshot.lock_path)? {
        Some(lock) => {
            if !detect_lock_drift(snapshot, &lock, marker_env.as_ref()).is_empty() {
                return Ok(false);
            }
            if !validate_lock_closure(&lock, marker_env.as_ref()).is_empty() {
                return Ok(false);
            }
            if ctx.config().resolver.force_sdist
                && lock
                    .resolved
                    .iter()
                    .filter(|dep| dep.source.is_none())
                    .any(|dep| match dep.artifact.as_ref() {
                        Some(artifact) => {
                            !artifact.is_direct_url && artifact.build_options_hash.is_empty()
                        }
                        None => true,
                    })
            {
                return Ok(false);
            }
            if let Ok(runtime) = prepare_project_runtime(snapshot) {
                if let Ok(tags) = detect_interpreter_tags(&runtime.record.path) {
                    if lock
                        .resolved
                        .iter()
                        .filter_map(|dep| dep.artifact.as_ref())
                        .any(|artifact| !artifact_supported_by_runtime(&tags, artifact))
                    {
                        return Ok(false);
                    }
                }
            }
            if let Some(fingerprint) = &lock.manifest_fingerprint {
                Ok(fingerprint == &snapshot.manifest_fingerprint)
            } else {
                Ok(true)
            }
        }
        None => Ok(false),
    }
}

pub(crate) fn relative_path_str(path: &Path, root: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    let rendered = rel.display().to_string();
    if cfg!(windows) {
        rendered.replace('\\', "/")
    } else {
        rendered
    }
}

pub(crate) fn manifest_snapshot() -> Result<ManifestSnapshot> {
    ProjectSnapshot::read_current()
}

pub(crate) fn manifest_snapshot_at(root: &Path) -> Result<ManifestSnapshot> {
    ProjectSnapshot::read_from(root)
}

fn runtime_marker_environment(snapshot: &ManifestSnapshot) -> Result<MarkerEnvironment> {
    let runtime = prepare_project_runtime(snapshot)?;
    let resolver_env = detect_marker_environment(&runtime.record.path)?;
    resolver_env.to_marker_environment()
}

pub fn marker_env_for_snapshot(snapshot: &ManifestSnapshot) -> Option<MarkerEnvironment> {
    runtime_marker_environment(snapshot).ok().or_else(|| {
        detect_interpreter()
            .ok()
            .and_then(|python| detect_marker_environment(&python).ok())
            .and_then(|env| env.to_marker_environment().ok())
    })
}

pub(crate) fn install_snapshot(
    ctx: &CommandContext,
    snapshot: &ManifestSnapshot,
    frozen: bool,
    force_resolve: bool,
    override_pins: Option<&InstallOverride>,
) -> Result<InstallOutcome> {
    let inline = is_inline_snapshot(ctx, snapshot);
    let mut snapshot = snapshot.clone();
    let lockfile = snapshot.lock_path.display().to_string();
    let _ = prepare_project_runtime(&snapshot)?;

    if frozen {
        return verify_lock(&snapshot);
    }

    if !force_resolve && lock_is_fresh(ctx, &snapshot)? {
        Ok(InstallOutcome {
            state: InstallState::UpToDate,
            lockfile,
            drift: Vec::new(),
            verified: false,
        })
    } else {
        if let Some(parent) = snapshot.lock_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let has_foreign_lock =
            snapshot.root.join("uv.lock").exists() || snapshot.root.join("poetry.lock").exists();
        let mut manifest_updated = false;
        let mut manifest_dependencies = if let Some(override_data) = override_pins {
            override_data.dependencies.clone()
        } else {
            snapshot.dependencies.clone()
        };
        let requirements = merge_requirements(&manifest_dependencies, &snapshot.group_dependencies);
        if requirements.is_empty() {
            let contents = render_lockfile(&snapshot, &[], PX_VERSION)?;
            fs::write(&snapshot.lock_path, contents)?;
            return Ok(InstallOutcome {
                state: InstallState::Installed,
                lockfile,
                drift: Vec::new(),
                verified: false,
            });
        }

        let marker_env = ctx.marker_environment()?;
        let pins = if let Some(override_data) = override_pins {
            let filtered_pins: Vec<PinSpec> = override_data
                .pins
                .iter()
                .filter(|pin| marker_applies(&pin.specifier, &marker_env))
                .cloned()
                .collect();
            if filtered_pins.is_empty() {
                ensure_exact_pins(&marker_env, &requirements)?
            } else if pins_cover_requirements(&filtered_pins, &requirements, &marker_env) {
                filtered_pins
            } else {
                let mut resolution_snapshot = snapshot.clone();
                resolution_snapshot.dependencies = manifest_dependencies.clone();
                resolution_snapshot.requirements = requirements.clone();
                let resolved = resolve_dependencies(ctx, &resolution_snapshot)?;
                resolved.pins
            }
        } else {
            let resolved = resolve_dependencies(ctx, &snapshot)?;
            if snapshot.px_options.pin_manifest
                && !resolved.specs.is_empty()
                && !inline
                && !has_foreign_lock
            {
                manifest_dependencies = merge_resolved_dependencies(
                    &manifest_dependencies,
                    &resolved.specs,
                    &marker_env,
                );
                persist_resolved_dependencies(&snapshot, &manifest_dependencies)?;
                manifest_updated = true;
            }
            resolved.pins
        };
        if manifest_updated {
            snapshot = manifest_snapshot_at(&snapshot.root).map_err(|err| {
                InstallUserError::new(
                    "failed to reload project manifest",
                    json!({ "error": err.to_string() }),
                )
            })?;
        }
        let resolved = resolve_pins(
            ctx,
            &snapshot.root,
            &pins,
            ctx.config().resolver.force_sdist,
        )?;
        let contents = render_lockfile(&snapshot, &resolved, PX_VERSION)?;
        let parsed = parse_lockfile(&contents)?;
        let closure_issues = validate_lock_closure(&parsed, Some(&marker_env));
        if !closure_issues.is_empty() {
            return Err(InstallUserError::new(
                "generated px.lock is missing transitive dependencies",
                json!({
                    "reason": "incomplete_lock",
                    "lockfile": snapshot.lock_path.display().to_string(),
                    "issues": closure_issues,
                    "hint": "run `px sync` to regenerate px.lock",
                }),
            )
            .into());
        }
        fs::write(&snapshot.lock_path, contents)?;
        Ok(InstallOutcome {
            state: InstallState::Installed,
            lockfile,
            drift: Vec::new(),
            verified: false,
        })
    }
}

fn merge_requirements(base: &[String], groups: &[String]) -> Vec<String> {
    let mut merged = base.to_vec();
    merged.extend(groups.iter().cloned());
    merged.sort();
    merged.dedup();
    merged
}

fn normalize_pin_key(key: &str) -> String {
    match key.split_once('|') {
        Some((name, extras)) => format!("{}|{}", canonicalize_package_name(name), extras),
        None => canonicalize_package_name(key),
    }
}

fn pins_cover_requirements(
    pins: &[PinSpec],
    requirements: &[String],
    marker_env: &MarkerEnvironment,
) -> bool {
    if pins.is_empty() {
        return false;
    }

    let mut pin_keys = HashSet::with_capacity(pins.len());
    for pin in pins {
        pin_keys.insert(normalize_pin_key(&autopin_pin_key(pin)));
    }

    for spec in requirements {
        if !marker_applies(spec, marker_env) {
            continue;
        }
        let key = normalize_pin_key(&autopin_spec_key(spec));
        if !pin_keys.contains(&key) {
            return false;
        }
    }

    for pin in pins {
        for req in &pin.requires {
            if !marker_applies(req, marker_env) {
                continue;
            }
            let key = normalize_pin_key(&autopin_spec_key(req));
            if !pin_keys.contains(&key) {
                return false;
            }
        }
    }

    true
}
pub(crate) fn compute_lock_hash(lock_path: &Path) -> Result<String> {
    let contents = fs::read(lock_path)?;
    Ok(compute_lock_hash_bytes(&contents))
}

pub(crate) fn compute_lock_hash_bytes(contents: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(contents);
    format!("{:x}", hasher.finalize())
}
fn verify_lock(snapshot: &ManifestSnapshot) -> Result<InstallOutcome> {
    let lockfile = snapshot.lock_path.display().to_string();
    let marker_env = marker_env_for_snapshot(snapshot);
    match load_lockfile_optional(&snapshot.lock_path)? {
        Some(lock) => {
            let report = analyze_lock_diff(snapshot, &lock, marker_env.as_ref());
            let mut drift = report.to_messages();
            drift.extend(validate_lock_closure(&lock, marker_env.as_ref()));
            if drift.is_empty() {
                drift = verify_locked_artifacts(&lock);
            }
            if drift.is_empty() {
                Ok(InstallOutcome {
                    state: InstallState::UpToDate,
                    lockfile,
                    drift,
                    verified: true,
                })
            } else {
                Ok(InstallOutcome {
                    state: InstallState::Drift,
                    lockfile,
                    drift,
                    verified: true,
                })
            }
        }
        None => Ok(InstallOutcome {
            state: InstallState::MissingLock,
            lockfile,
            drift: Vec::new(),
            verified: true,
        }),
    }
}

pub(crate) struct ResolvedSpecOutput {
    pub(crate) specs: Vec<String>,
    pub(crate) pins: Vec<PinSpec>,
}

fn resolve_dependencies(
    ctx: &CommandContext,
    snapshot: &ManifestSnapshot,
) -> Result<ResolvedSpecOutput> {
    resolve_dependencies_with_effects(ctx.effects(), snapshot, true)
}

pub(crate) fn resolve_dependencies_with_effects(
    effects: &dyn Effects,
    snapshot: &ManifestSnapshot,
    show_progress: bool,
) -> Result<ResolvedSpecOutput> {
    let spinner = show_progress.then(|| ProgressReporter::spinner("Resolving dependencies"));
    let python = effects.python().detect_interpreter()?;
    let tags = detect_interpreter_tags(&python)?;
    let resolver_env = detect_marker_environment(&python)?;
    let marker_env = resolver_env
        .to_marker_environment()
        .map_err(|err| anyhow!("invalid marker environment: {err}"))?;
    let cache_dir = effects.cache().resolve_store_path()?.path;
    let requirements: Vec<String> = snapshot
        .requirements
        .iter()
        .filter(|spec| marker_applies(spec, &marker_env))
        .map(|spec| canonicalize_spec(spec))
        .filter(|spec| !spec.is_empty())
        .collect();
    let dep_names: Vec<String> = requirements
        .iter()
        .map(|spec| dependency_name(spec))
        .collect();
    let mut system_deps = system_deps_from_names(dep_names);
    if !system_deps.capabilities.is_empty()
        && !matches!(system_deps_mode(), SystemDepsMode::Offline)
    {
        pin_system_deps(&mut system_deps).map_err(anyhow::Error::from)?;
    }
    let requirements = apply_system_lib_compatibility(requirements, &system_deps)?;
    let host_supports_debian_rootfs = Path::new("/etc/debian_version").exists();
    let sysroot = if !host_supports_debian_rootfs
        || system_deps.capabilities.is_empty()
        || matches!(system_deps_mode(), SystemDepsMode::Offline)
    {
        None
    } else {
        ensure_system_deps_rootfs(&system_deps).map_err(anyhow::Error::from)?
    };
    let mut sys_env = SysEnvGuard::default();
    if let Some(root) = sysroot.as_ref() {
        sys_env.apply(root);
    }
    tracing::debug!(?requirements, "resolver_requirements");
    let request = ResolverRequest {
        project: snapshot.name.clone(),
        root: snapshot.root.clone(),
        requirements,
        tags: ResolverTags {
            python: tags.python.clone(),
            abi: tags.abi.clone(),
            platform: tags.platform.clone(),
        },
        env: resolver_env.clone(),
        indexes: resolver_indexes(),
        cache_dir,
        python: python.clone(),
    };
    let resolved = resolve(&request).map_err(|err| {
        InstallUserError::new(
            "dependency resolution failed",
            resolver_failure_details(&err),
        )
    })?;
    let mut pins = Vec::new();
    let mut autopin_lookup = HashMap::new();
    let mut seen = HashSet::new();
    for spec in resolved {
        let formatted = format_specifier(
            &spec.normalized,
            &spec.extras,
            &spec.selected_version,
            spec.marker.as_deref(),
        );
        let pin = PinSpec {
            name: spec.name,
            specifier: formatted.clone(),
            version: spec.selected_version,
            normalized: spec.normalized,
            extras: spec.extras,
            marker: spec.marker,
            direct: spec.direct,
            requires: spec.requires,
            source: spec.dist_source,
        };
        autopin_lookup.insert(autopin_pin_key(&pin), formatted);
        if seen.insert(pin.normalized.clone()) {
            pins.push(pin);
        }
    }

    let mut autopin_specs = Vec::new();
    for spec in &snapshot.dependencies {
        if spec_requires_pin(spec) && marker_applies(spec, &marker_env) {
            let key = autopin_spec_key(spec);
            if let Some(pinned) = autopin_lookup.get(&key) {
                autopin_specs.push(pinned.clone());
            } else {
                autopin_specs.push(spec.clone());
            }
        }
    }
    if let Some(spinner) = spinner {
        spinner.finish(format!("Resolved {} dependencies", pins.len()));
    }
    Ok(ResolvedSpecOutput {
        specs: autopin_specs,
        pins,
    })
}

fn resolver_indexes() -> Vec<String> {
    let mut indexes = Vec::new();
    if let Ok(primary) = env::var("PX_INDEX_URL")
        .or_else(|_| env::var("PIP_INDEX_URL"))
        .map(|value| value.trim().to_string())
    {
        if !primary.is_empty() {
            indexes.push(normalize_index_url(&primary));
        }
    }
    if let Ok(extra) = env::var("PIP_EXTRA_INDEX_URL") {
        for entry in extra.split_whitespace() {
            let trimmed = entry.trim();
            if !trimmed.is_empty() {
                indexes.push(normalize_index_url(trimmed));
            }
        }
    }
    if indexes.is_empty() {
        indexes.push("https://pypi.org/simple".to_string());
    }
    indexes
}

fn normalize_index_url(raw: &str) -> String {
    let mut url = raw.trim_end_matches('/').to_string();
    if url.ends_with("/simple") {
        return url;
    }
    if let Some(stripped) = url.strip_suffix("/pypi") {
        url = stripped.to_string();
    } else if let Some(stripped) = url.strip_suffix("/json") {
        url = stripped.to_string();
    }
    url.push_str("/simple");
    url
}

fn resolver_failure_details(err: &anyhow::Error) -> Value {
    let message = err.to_string();
    let mut issues = vec![message.clone()];
    issues.extend(err.chain().skip(1).map(std::string::ToString::to_string));
    let offline = std::env::var("PX_ONLINE").ok().is_some_and(|value| {
        matches!(
            value.to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off" | ""
        )
    });
    let lowered = message.to_ascii_lowercase();
    if offline
        && (lowered.contains("network connectivity is disabled")
            || lowered.contains("network was disabled")
            || lowered.contains("network is disabled")
            || lowered.contains("offline"))
    {
        return json!({
            "reason": "offline",
            "issues": issues,
            "hint": "PX_ONLINE=1 required for dependency resolution; rerun with --online or populate the cache while online.",
            "code": diag_commands::SYNC,
        });
    }
    let details = json!({
        "reason": "resolve_failed",
        "issues": issues,
        "hint": "Inspect dependency constraints and rerun `px sync`.",
        "code": diag_commands::SYNC,
    });
    if let Some(req) = extract_quoted_requirement(&message) {
        if message.contains("unable to resolve") {
            return json!({
                "reason": "resolve_no_match",
                "issues": issues,
                "requirement": req,
                "hint": format!("Relax or remove `{}` in pyproject.toml, then rerun `px sync`.", req),
                "code": diag_commands::SYNC,
            });
        }
        if message.contains("failed to parse requirement")
            || message.contains("failed to parse specifiers")
        {
            return json!({
                "reason": "invalid_requirement",
                "issues": issues,
                "requirement": req,
                "hint": format!("Fix `{}` to a valid PEP 508 requirement, then rerun `px sync`.", req),
                "code": diag_commands::SYNC,
            });
        }
    }
    if message.contains("failed to query PyPI") || message.contains("PyPI error") {
        return json!({
            "reason": "pypi_unreachable",
            "issues": issues,
            "hint": "Check your network connection (PX_ONLINE=1) and rerun `px sync`.",
            "code": diag_commands::SYNC,
        });
    }
    details
}

fn extract_quoted_requirement(message: &str) -> Option<String> {
    let start = message.find('`')?;
    let rest = &message[start + 1..];
    let end = rest.find('`')?;
    Some(rest[..end].to_string())
}

pub(crate) fn persist_resolved_dependencies(
    snapshot: &ManifestSnapshot,
    specs: &[String],
) -> Result<()> {
    let contents = fs::read_to_string(&snapshot.manifest_path)?;
    let mut doc: DocumentMut = contents.parse()?;
    write_dependencies(&mut doc, specs)?;
    fs::write(&snapshot.manifest_path, doc.to_string())?;
    Ok(())
}

pub(crate) fn summarize_autopins(entries: &[AutopinEntry]) -> Option<String> {
    if entries.is_empty() {
        return None;
    }
    let mut labels = Vec::new();
    for entry in entries.iter().take(3) {
        labels.push(entry.short_label());
    }
    let mut summary = format!(
        "Pinned {} package{} automatically",
        entries.len(),
        if entries.len() == 1 { "" } else { "s" }
    );
    if !labels.is_empty() {
        summary.push_str(" (");
        summary.push_str(&labels.join(", "));
        if entries.len() > 3 {
            let _ = write!(&mut summary, ", +{} more", entries.len() - 3);
        }
        summary.push(')');
    }
    Some(summary)
}

fn write_dependencies(doc: &mut DocumentMut, specs: &[String]) -> Result<()> {
    let table = project_table_mut(doc)?;
    let mut array = Array::new();
    for spec in specs {
        array.push_formatted(TomlValue::from(spec.clone()));
    }
    table.insert("dependencies", Item::Value(TomlValue::Array(array)));
    Ok(())
}

pub(crate) fn project_table(doc: &DocumentMut) -> Result<&Table> {
    doc.get("project")
        .and_then(Item::as_table)
        .ok_or_else(|| anyhow!("[project] must be a table"))
}

fn project_table_mut(doc: &mut DocumentMut) -> Result<&mut Table> {
    doc.entry("project")
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| anyhow!("[project] must be a table"))
}
