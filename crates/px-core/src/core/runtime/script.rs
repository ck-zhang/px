use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use toml_edit::{Array, DocumentMut, Item, Table, Value as TomlValue};

use crate::core::project::evaluate_project_state;
use crate::core::runtime::facade::{
    attach_autosync_details, auto_sync_environment, build_pythonpath,
    ensure_environment_with_guard, manifest_snapshot_at, prepare_project_runtime,
    select_python_from_site, EnvironmentIssue, EnvironmentSyncReport, ManifestSnapshot,
    PythonContext,
};
use crate::core::runtime::run::{
    run_project_script, sandbox_runner_for_context, CommandRunner, HostCommandRunner,
    SandboxRunContext,
};
use crate::core::runtime::EnvGuard;
use crate::{CommandContext, ExecutionOutcome, InstallUserError};
use px_domain::project::manifest::manifest_fingerprint;
use px_domain::{ProjectStateKind, ProjectStateReport, PxOptions};

#[derive(Clone, Debug)]
pub(crate) struct InlineScript {
    pub(crate) path: PathBuf,
    canonical_path: PathBuf,
    working_dir: PathBuf,
    metadata: InlineScriptMetadata,
}

#[derive(Clone, Debug)]
struct InlineScriptMetadata {
    requires_python: String,
    dependencies: Vec<String>,
}

#[derive(Deserialize)]
struct RawInlineMetadata {
    #[serde(rename = "requires-python")]
    requires_python: Option<String>,
    dependencies: Option<Vec<String>>,
}

/// Attempt to locate and parse an inline `# /// px` metadata block near the
/// top of the requested target.
pub(crate) fn detect_inline_script(target: &str) -> Result<Option<InlineScript>, ExecutionOutcome> {
    let path = Path::new(target);
    let cwd = env::current_dir().map_err(|err| {
        ExecutionOutcome::failure(
            "unable to resolve current directory",
            json!({ "error": err.to_string() }),
        )
    })?;
    let abs_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    if !abs_path.is_file() {
        return Ok(None);
    }
    let is_python = abs_path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("py"))
        .unwrap_or(false);
    if !is_python {
        return Ok(None);
    }

    let contents = fs::read_to_string(&abs_path).map_err(|err| {
        ExecutionOutcome::failure(
            "unable to read script",
            json!({
                "script": abs_path.display().to_string(),
                "error": err.to_string(),
            }),
        )
    })?;

    let Some(block) = extract_inline_block(&contents, &abs_path)? else {
        return Ok(None);
    };
    let metadata = parse_inline_metadata(&block, &abs_path)?;
    let canonical_path = fs::canonicalize(&abs_path).map_err(|err| {
        ExecutionOutcome::failure(
            "unable to canonicalize script path",
            json!({
                "script": abs_path.display().to_string(),
                "error": err.to_string(),
            }),
        )
    })?;

    Ok(Some(InlineScript {
        path: abs_path,
        canonical_path,
        working_dir: cwd,
        metadata,
    }))
}

/// Resolve, lock, and run an inline script using the metadata in its px block.
pub(crate) fn run_inline_script(
    ctx: &CommandContext,
    mut sandbox: Option<&mut SandboxRunContext>,
    script: InlineScript,
    extra_args: &[String],
    command_args: &serde_json::Value,
    interactive: bool,
    strict: bool,
) -> Result<ExecutionOutcome, ExecutionOutcome> {
    let snapshot = prepare_inline_snapshot(ctx, &script)?;
    let state_report = evaluate_project_state(ctx, &snapshot)
        .map_err(|err| map_install_error(err, "failed to evaluate inline script state"))?;
    let mut sync_report = None;

    if strict {
        if let Some(issue) = desired_issue(&state_report) {
            return Err(inline_strict_outcome(&script.path, issue));
        }
    } else if let Some(issue) = desired_issue(&state_report) {
        sync_report = auto_sync_environment(ctx, &snapshot, issue)
            .map_err(|err| map_install_error(err, "failed to prepare inline script environment"))?;
    }

    let guard = if strict {
        EnvGuard::Strict
    } else {
        EnvGuard::AutoSync
    };
    let (py_ctx, report) = inline_python_context(ctx, &snapshot, guard, &script)?;
    if sync_report.is_none() {
        sync_report = report;
    }

    let host_runner = HostCommandRunner::new(ctx);
    let sandbox_runner = match sandbox {
        Some(ref mut sbx) => match sandbox_runner_for_context(&py_ctx, sbx, &script.working_dir) {
            Ok(runner) => Some(runner),
            Err(outcome) => return Err(outcome),
        },
        None => None,
    };
    let runner: &dyn CommandRunner = match sandbox_runner.as_ref() {
        Some(runner) => runner,
        None => &host_runner,
    };

    let mut outcome = match run_project_script(
        ctx,
        runner,
        &py_ctx,
        &script.path,
        extra_args,
        command_args,
        &script.working_dir,
        interactive,
        if sandbox.is_some() {
            "python"
        } else {
            &py_ctx.python
        },
    ) {
        Ok(result) => result,
        Err(err) => {
            return Err(ExecutionOutcome::failure(
                "inline script failed",
                json!({
                    "script": script.path.display().to_string(),
                    "error": err.to_string(),
                }),
            ))
        }
    };
    attach_autosync_details(&mut outcome, sync_report);
    Ok(outcome)
}

fn desired_issue(report: &ProjectStateReport) -> Option<EnvironmentIssue> {
    if !report.lock_exists {
        return Some(EnvironmentIssue::MissingLock);
    }
    if matches!(report.canonical, ProjectStateKind::NeedsLock) {
        return Some(EnvironmentIssue::LockDrift);
    }
    if report.env_clean {
        return None;
    }
    if let Some(details) = report.env_issue.as_ref() {
        if let Some(issue) = crate::issue_from_details(details) {
            return Some(issue);
        }
    }
    if !report.env_exists {
        return Some(EnvironmentIssue::MissingEnv);
    }
    Some(EnvironmentIssue::EnvOutdated)
}

fn inline_strict_outcome(script_path: &Path, issue: EnvironmentIssue) -> ExecutionOutcome {
    let (reason, message) = match issue {
        EnvironmentIssue::MissingLock => (
            "missing_lock",
            "inline script lock is missing in strict mode",
        ),
        EnvironmentIssue::LockDrift => (
            "lock_drift",
            "inline script metadata changed; rerun without --frozen",
        ),
        EnvironmentIssue::MissingArtifacts => (
            "missing_artifacts",
            "inline script environment cache is incomplete",
        ),
        EnvironmentIssue::MissingEnv => (
            "missing_env",
            "inline script environment is missing in strict mode",
        ),
        EnvironmentIssue::EnvOutdated => {
            ("env_outdated", "inline script environment is out of sync")
        }
        EnvironmentIssue::RuntimeMismatch => (
            "runtime_mismatch",
            "inline script environment uses the wrong runtime",
        ),
    };
    ExecutionOutcome::user_error(
        message,
        json!({
            "script": script_path.display().to_string(),
            "reason": reason,
            "hint": "rerun `px run` without --frozen to refresh inline metadata",
        }),
    )
}

fn prepare_inline_snapshot(
    ctx: &CommandContext,
    script: &InlineScript,
) -> Result<ManifestSnapshot, ExecutionOutcome> {
    let identity = script_identity(&script.canonical_path);
    let project_name = script_project_name(&script.path, &identity);
    let dependencies = sanitized_dependencies(&script.metadata.dependencies)?;
    let doc = build_inline_manifest_doc(
        &project_name,
        &script.metadata.requires_python,
        &dependencies,
    );
    let fingerprint = manifest_fingerprint(&doc, &dependencies, &[], &PxOptions::default())
        .map_err(|err| {
            ExecutionOutcome::failure(
                "failed to fingerprint inline px metadata",
                json!({
                    "script": script.path.display().to_string(),
                    "error": err.to_string(),
                }),
            )
        })?;
    let root = script_root(ctx, &identity, &fingerprint);
    write_manifest(&root.join("pyproject.toml"), doc.to_string())?;
    manifest_snapshot_at(&root).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to load inline px manifest",
            json!({
                "script": script.path.display().to_string(),
                "manifest": root.join("pyproject.toml").display().to_string(),
                "error": err.to_string(),
            }),
        )
    })
}

fn inline_python_context(
    ctx: &CommandContext,
    snapshot: &ManifestSnapshot,
    guard: EnvGuard,
    script: &InlineScript,
) -> Result<(PythonContext, Option<EnvironmentSyncReport>), ExecutionOutcome> {
    let runtime = prepare_project_runtime(snapshot)
        .map_err(|err| map_install_error(err, "failed to prepare inline script runtime"))?;
    let sync_report = ensure_environment_with_guard(ctx, snapshot, guard)
        .map_err(|err| map_install_error(err, "failed to prepare inline script environment"))?;
    let paths = build_pythonpath(ctx.fs(), &snapshot.root, None).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to assemble inline script PYTHONPATH",
            json!({
                "script": script.path.display().to_string(),
                "error": err.to_string(),
            }),
        )
    })?;
    let mut allowed_paths = paths.allowed_paths;
    push_unique_path(
        &mut allowed_paths,
        script
            .canonical_path
            .parent()
            .unwrap_or_else(|| Path::new(".")),
    );
    push_unique_path(&mut allowed_paths, &script.working_dir);
    let pythonpath = env::join_paths(&allowed_paths)
        .map_err(|err| {
            ExecutionOutcome::failure(
                "failed to assemble inline script PYTHONPATH",
                json!({ "error": err.to_string() }),
            )
        })?
        .into_string()
        .map_err(|_| {
            ExecutionOutcome::failure(
                "failed to assemble inline script PYTHONPATH",
                json!({ "error": "contains non-utf8 data" }),
            )
        })?;
    let python = select_python_from_site(
        &paths.site_bin,
        &runtime.record.path,
        &runtime.record.full_version,
    );
    let py_ctx = PythonContext {
        project_root: script.working_dir.clone(),
        project_name: snapshot.name.clone(),
        python,
        pythonpath,
        allowed_paths,
        site_bin: paths.site_bin,
        pep582_bin: paths.pep582_bin,
        px_options: snapshot.px_options.clone(),
    };
    Ok((py_ctx, sync_report))
}

fn extract_inline_block(
    contents: &str,
    script_path: &Path,
) -> Result<Option<String>, ExecutionOutcome> {
    let mut started = false;
    let mut body = String::new();
    for line in contents.lines() {
        let trimmed = line.trim_start();
        if !started {
            if trimmed.is_empty() {
                continue;
            }
            if trimmed.starts_with("#!") {
                continue;
            }
            if trimmed.starts_with('#') {
                let after_hash = trimmed.trim_start_matches('#').trim_start();
                if after_hash.starts_with("/// px") {
                    started = true;
                }
                continue;
            }
            break;
        }

        if trimmed.starts_with('#') {
            let after_hash = trimmed.trim_start_matches('#').trim_start();
            if after_hash.starts_with("///") {
                return Ok(Some(body));
            }
            body.push_str(after_hash);
            body.push('\n');
            continue;
        }

        return Err(ExecutionOutcome::user_error(
            "inline px block must use commented TOML",
            json!({
                "script": script_path.display().to_string(),
                "reason": "invalid_inline_block",
            }),
        ));
    }

    if started {
        return Err(ExecutionOutcome::user_error(
            "inline px block is unterminated",
            json!({
                "script": script_path.display().to_string(),
                "reason": "unterminated_inline_block",
            }),
        ));
    }

    Ok(None)
}

fn parse_inline_metadata(
    block: &str,
    script_path: &Path,
) -> Result<InlineScriptMetadata, ExecutionOutcome> {
    let raw: RawInlineMetadata = toml_edit::de::from_str(block).map_err(|err| {
        ExecutionOutcome::user_error(
            "invalid inline px metadata",
            json!({
                "script": script_path.display().to_string(),
                "error": err.to_string(),
                "reason": "invalid_inline_toml",
            }),
        )
    })?;

    let requires_python = raw
        .requires_python
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            ExecutionOutcome::user_error(
                "inline px block missing requires-python",
                json!({
                    "script": script_path.display().to_string(),
                    "reason": "missing_requires_python",
                }),
            )
        })?;

    let deps = raw.dependencies.ok_or_else(|| {
        ExecutionOutcome::user_error(
            "inline px block missing dependencies",
            json!({
                "script": script_path.display().to_string(),
                "reason": "missing_dependencies",
            }),
        )
    })?;

    Ok(InlineScriptMetadata {
        requires_python,
        dependencies: deps,
    })
}

fn sanitized_dependencies(deps: &[String]) -> Result<Vec<String>, ExecutionOutcome> {
    let mut cleaned = Vec::new();
    for dep in deps {
        let trimmed = dep.trim();
        if trimmed.is_empty() {
            return Err(ExecutionOutcome::user_error(
                "inline px dependency entry is empty",
                json!({ "reason": "empty_dependency" }),
            ));
        }
        cleaned.push(trimmed.to_string());
    }
    Ok(cleaned)
}

fn script_identity(script_path: &Path) -> String {
    let digest = Sha256::digest(script_path.display().to_string().as_bytes());
    hex::encode(digest)
}

fn script_project_name(path: &Path, identity: &str) -> String {
    let stem = path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("px-script");
    let mut normalized = String::new();
    for ch in stem.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' {
            normalized.push(ch.to_ascii_lowercase());
        } else if ch == '_' || ch == '.' || ch == ' ' {
            normalized.push('-');
        }
    }
    if normalized.is_empty() {
        normalized.push_str("px-script");
    }
    let short = identity.chars().take(12).collect::<String>();
    format!("{normalized}-px-{short}")
}

fn script_root(ctx: &CommandContext, identity: &str, fingerprint: &str) -> PathBuf {
    ctx.cache()
        .path
        .join("scripts")
        .join(identity)
        .join(fingerprint)
}

fn build_inline_manifest_doc(
    name: &str,
    requires_python: &str,
    dependencies: &[String],
) -> DocumentMut {
    let mut doc = DocumentMut::new();
    let mut project = Table::new();
    project.insert("name", Item::Value(TomlValue::from(name)));
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

fn write_manifest(path: &Path, contents: String) -> Result<(), ExecutionOutcome> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            ExecutionOutcome::failure(
                "failed to create inline px cache directory",
                json!({
                    "path": parent.display().to_string(),
                    "error": err.to_string(),
                }),
            )
        })?;
    }
    let needs_write = match fs::read_to_string(path) {
        Ok(existing) => existing != contents,
        Err(_) => true,
    };
    if needs_write {
        fs::write(path, contents).map_err(|err| {
            ExecutionOutcome::failure(
                "failed to write inline px manifest",
                json!({
                    "manifest": path.display().to_string(),
                    "error": err.to_string(),
                }),
            )
        })?;
    }
    Ok(())
}

fn push_unique_path(paths: &mut Vec<PathBuf>, candidate: &Path) {
    let canonical = candidate
        .canonicalize()
        .unwrap_or_else(|_| candidate.to_path_buf());
    if !paths.iter().any(|p| p == &canonical) {
        paths.push(canonical);
    }
}

fn map_install_error(err: anyhow::Error, message: &str) -> ExecutionOutcome {
    match err.downcast::<InstallUserError>() {
        Ok(user) => ExecutionOutcome::user_error(user.message, user.details),
        Err(other) => ExecutionOutcome::failure(message, json!({ "error": other.to_string() })),
    }
}
