use super::super::*;
use super::{EphemeralInput, DEFAULT_EPHEMERAL_REQUIRES_PYTHON, EPHEMERAL_PROJECT_NAME};

use std::env;
use std::fs;
use std::path::Path;

use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use toml_edit::{Array, DocumentMut, Item, Table, Value as TomlValue};

use crate::core::runtime::facade::ensure_environment_with_guard;
use crate::core::runtime::runtime_manager;
use crate::core::runtime::EnvGuard;
use crate::python_sys::detect_interpreter_tags;
use crate::EnvironmentSyncReport;

#[derive(Clone, Debug, Serialize)]
struct EphemeralKeyPayload<'a> {
    kind: &'a str,
    requires_python: &'a str,
    deps: &'a [String],
    runtime: &'a str,
    platform: &'a str,
    indexes: &'a [String],
    force_sdist: bool,
}

pub(super) fn prepare_ephemeral_snapshot(
    ctx: &CommandContext,
    invocation_root: &Path,
    input: &EphemeralInput,
    frozen: bool,
) -> Result<
    (
        ManifestSnapshot,
        runtime_manager::RuntimeSelection,
        Option<EnvironmentSyncReport>,
    ),
    ExecutionOutcome,
> {
    let (requires_python, deps) = match input {
        EphemeralInput::InlineScript {
            requires_python,
            deps,
        } => (requires_python.clone(), deps.clone()),
        EphemeralInput::Pyproject {
            requires_python,
            deps,
        } => (requires_python.clone(), deps.clone()),
        EphemeralInput::Requirements { deps } => {
            (DEFAULT_EPHEMERAL_REQUIRES_PYTHON.to_string(), deps.clone())
        }
        EphemeralInput::Empty => (DEFAULT_EPHEMERAL_REQUIRES_PYTHON.to_string(), Vec::new()),
    };

    let manifest_doc = build_ephemeral_manifest_doc(&requires_python, &deps);
    let manifest_contents = manifest_doc.to_string();
    let temp_snapshot = px_domain::api::ProjectSnapshot::from_contents(
        invocation_root,
        invocation_root.join("pyproject.toml"),
        &manifest_contents,
    )
    .map_err(|err| {
        ExecutionOutcome::failure(
            "failed to assemble ephemeral project snapshot",
            json!({ "error": err.to_string() }),
        )
    })?;

    let runtime = prepare_project_runtime(&temp_snapshot).map_err(|err| {
        install_error_outcome(err, "python runtime unavailable for ephemeral run")
    })?;
    let tags = detect_interpreter_tags(&runtime.record.path).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to detect python interpreter tags for ephemeral run",
            json!({ "error": err.to_string() }),
        )
    })?;
    let platform = tags
        .platform
        .first()
        .cloned()
        .unwrap_or_else(|| "any".to_string());
    let indexes = resolver_indexes();
    let kind = match input {
        EphemeralInput::InlineScript { .. } => "pep723",
        EphemeralInput::Pyproject { .. } => "pyproject",
        EphemeralInput::Requirements { .. } => "requirements",
        EphemeralInput::Empty => "empty",
    };
    let key = ephemeral_cache_key(EphemeralKeyPayload {
        kind,
        requires_python: &requires_python,
        deps: &deps,
        runtime: &runtime.record.full_version,
        platform: &platform,
        indexes: &indexes,
        force_sdist: ctx.config().resolver.force_sdist,
    });
    let root = ctx.cache().path.join("ephemeral").join(&key);
    fs::create_dir_all(&root).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to create ephemeral cache directory",
            json!({
                "error": err.to_string(),
                "path": root.display().to_string(),
            }),
        )
    })?;
    let manifest_path = root.join("pyproject.toml");
    write_if_missing(&manifest_path, &manifest_contents).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to write ephemeral pyproject.toml",
            json!({
                "error": err.to_string(),
                "manifest": manifest_path.display().to_string(),
            }),
        )
    })?;
    let snapshot = px_domain::api::ProjectSnapshot::read_from(&root).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to load ephemeral project snapshot",
            json!({
                "error": err.to_string(),
                "root": root.display().to_string(),
            }),
        )
    })?;

    let guard = if frozen {
        EnvGuard::Strict
    } else {
        EnvGuard::AutoSync
    };
    let sync_report = ensure_environment_with_guard(ctx, &snapshot, guard).map_err(|err| {
        install_error_outcome(err, "failed to prepare ephemeral python environment")
    })?;
    Ok((snapshot, runtime, sync_report))
}

fn build_ephemeral_manifest_doc(requires_python: &str, dependencies: &[String]) -> DocumentMut {
    let mut doc = DocumentMut::new();
    let mut project = Table::new();
    project.insert("name", Item::Value(TomlValue::from(EPHEMERAL_PROJECT_NAME)));
    project.insert("version", Item::Value(TomlValue::from("0.0.0")));
    project.insert(
        "requires-python",
        Item::Value(TomlValue::from(requires_python)),
    );
    let mut deps = Array::new();
    for dep in dependencies {
        deps.push(dep.as_str());
    }
    project.insert("dependencies", Item::Value(TomlValue::Array(deps)));
    doc.insert("project", Item::Table(project));
    let mut tool = Table::new();
    tool.insert("px", Item::Table(Table::new()));
    doc.insert("tool", Item::Table(tool));
    doc
}

fn write_if_missing(path: &Path, contents: &str) -> std::io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    fs::write(path, contents)?;
    Ok(())
}

fn ephemeral_cache_key(payload: EphemeralKeyPayload<'_>) -> String {
    let bytes = serde_json::to_vec(&payload).unwrap_or_default();
    hex::encode(Sha256::digest(&bytes))
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
