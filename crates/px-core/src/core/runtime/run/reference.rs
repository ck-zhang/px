use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde_json::json;
use tracing::{debug, warn};

use super::RunRequest;
use crate::core::runtime::script::{load_inline_python_script, run_inline_script};
use crate::{CommandContext, ExecutionOutcome};

#[derive(Clone, Debug)]
pub(crate) struct RunReferenceTarget {
    pub(crate) locator: String,
    pub(crate) git_ref: Option<String>,
    pub(crate) script_path: PathBuf,
}

fn timestamp_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn record_run_reference_provenance(
    ctx: &CommandContext,
    header: &crate::RepoSnapshotHeader,
    oid: &str,
    script_path: &Path,
) -> Option<PathBuf> {
    const MAX_BYTES: u64 = 1_000_000;
    let root = ctx.cache().path.join("runs");
    let log_path = root.join("run-by-reference.jsonl");
    if let Err(err) = fs::create_dir_all(&root) {
        warn!(%err, root = %root.display(), "run-by-reference provenance directory creation failed");
        return None;
    }
    if let Ok(meta) = fs::metadata(&log_path) {
        if meta.len() > MAX_BYTES {
            let rotated = log_path.with_extension("jsonl.1");
            let _ = fs::remove_file(&rotated);
            if let Err(err) = fs::rename(&log_path, &rotated) {
                warn!(%err, path = %log_path.display(), "run-by-reference provenance log rotation failed");
            }
        }
    }

    let record = json!({
        "ts": timestamp_secs(),
        "locator": header.locator,
        "commit": header.commit,
        "repo_snapshot_oid": oid,
        "script": script_path.to_string_lossy(),
        "px_version": env!("CARGO_PKG_VERSION"),
    });
    let line = match serde_json::to_string(&record) {
        Ok(line) => line,
        Err(err) => {
            warn!(%err, "run-by-reference provenance serialization failed");
            return None;
        }
    };
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(mut file) => {
            if let Err(err) = writeln!(file, "{line}") {
                warn!(%err, path = %log_path.display(), "run-by-reference provenance write failed");
                return None;
            }
        }
        Err(err) => {
            warn!(%err, path = %log_path.display(), "run-by-reference provenance log open failed");
            return None;
        }
    }
    Some(log_path)
}

pub(crate) fn parse_run_reference_target(
    target: &str,
) -> Result<Option<RunReferenceTarget>, ExecutionOutcome> {
    let target = target.trim();
    if target.starts_with("gh:") {
        return Ok(Some(parse_run_reference_target_gh(target)?));
    }
    if target.starts_with("git+") {
        return Ok(Some(parse_run_reference_target_git(target)?));
    }
    Ok(None)
}

fn split_reference_target(target: &str) -> Result<(&str, &str), ExecutionOutcome> {
    let Some((repo_part, script_part)) = target.rsplit_once(':') else {
        return Err(ExecutionOutcome::user_error(
            "invalid run-by-reference target",
            json!({
                "reason": "invalid_run_reference_target",
                "hint": "use 'gh:ORG/REPO@<sha>:path/to/script.py' or 'git+file:///abs/path/to/repo@<sha>:path/to/script.py'",
            }),
        ));
    };
    let script_part = script_part.trim();
    if script_part.is_empty()
        || !script_part
            .rsplit_once('.')
            .map(|(_, ext)| ext.eq_ignore_ascii_case("py"))
            .unwrap_or(false)
    {
        return Err(ExecutionOutcome::user_error(
            "invalid run-by-reference target",
            json!({
                "reason": "invalid_run_reference_target",
                "hint": "run-by-reference targets must include a Python script path after ':' (example: :path/to/script.py)",
            }),
        ));
    }
    Ok((repo_part, script_part))
}

fn normalize_repo_relative_path(raw: &str) -> Result<PathBuf, ExecutionOutcome> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(ExecutionOutcome::user_error(
            "invalid run-by-reference target",
            json!({
                "reason": "invalid_run_reference_target",
                "hint": "provide a path like ':path/to/script.py'",
            }),
        ));
    }
    let path = Path::new(raw);
    if path.is_absolute() {
        return Err(ExecutionOutcome::user_error(
            "run-by-reference script path must be relative",
            json!({
                "reason": "run_reference_absolute_script_path",
                "path": raw,
                "hint": "drop the leading '/' and use a path relative to the repository root",
            }),
        ));
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => normalized.push(part),
            _ => {
                return Err(ExecutionOutcome::user_error(
                    "run-by-reference script path must not contain '..'",
                    json!({
                        "reason": "run_reference_invalid_script_path",
                        "path": raw,
                        "hint": "use a clean relative path within the repository",
                    }),
                ));
            }
        }
    }
    if normalized.as_os_str().is_empty() {
        return Err(ExecutionOutcome::user_error(
            "invalid run-by-reference target",
            json!({
                "reason": "invalid_run_reference_target",
                "hint": "provide a path like ':path/to/script.py'",
            }),
        ));
    }
    Ok(normalized)
}

fn parse_run_reference_target_gh(target: &str) -> Result<RunReferenceTarget, ExecutionOutcome> {
    let (repo_part, script_part) = split_reference_target(target)?;
    let script_path = normalize_repo_relative_path(script_part)?;
    let repo_ref = repo_part.strip_prefix("gh:").unwrap_or_default();
    let (repo_slug, git_ref) = repo_ref
        .rsplit_once('@')
        .map(|(slug, git_ref)| (slug, Some(git_ref.trim().to_string())))
        .unwrap_or((repo_ref, None));
    let mut pieces = repo_slug.split('/');
    let org = pieces.next().unwrap_or_default().trim();
    let repo = pieces.next().unwrap_or_default().trim();
    if org.is_empty() || repo.is_empty() || pieces.next().is_some() {
        return Err(ExecutionOutcome::user_error(
            "invalid GitHub shorthand",
            json!({
                "reason": "invalid_run_reference_github_shorthand",
                "hint": "use 'gh:ORG/REPO@<sha>:path/to/script.py'",
            }),
        ));
    }
    let repo = match repo.get(repo.len().saturating_sub(4)..) {
        Some(suffix) if suffix.eq_ignore_ascii_case(".git") => &repo[..repo.len() - 4],
        _ => repo,
    };
    let org = org.to_ascii_lowercase();
    let repo = repo.to_ascii_lowercase();
    let locator = format!("git+https://github.com/{org}/{repo}.git");
    Ok(RunReferenceTarget {
        locator,
        git_ref,
        script_path,
    })
}

fn parse_run_reference_target_git(target: &str) -> Result<RunReferenceTarget, ExecutionOutcome> {
    let (repo_part, script_part) = split_reference_target(target)?;
    let script_path = normalize_repo_relative_path(script_part)?;
    let repo_part = repo_part.trim();
    if !repo_part.starts_with("git+") {
        return Err(ExecutionOutcome::user_error(
            "invalid git run-by-reference target",
            json!({
                "reason": "invalid_run_reference_git_target",
                "hint": "use 'git+file:///abs/path/to/repo@<sha>:path/to/script.py'",
            }),
        ));
    }
    let (locator, git_ref) = repo_part
        .rsplit_once('@')
        .map(|(locator, git_ref)| (locator.to_string(), Some(git_ref.trim().to_string())))
        .unwrap_or((repo_part.to_string(), None));
    Ok(RunReferenceTarget {
        locator,
        git_ref,
        script_path,
    })
}

fn normalize_pinned_commit(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let normalized = trimmed.to_ascii_lowercase();
    let len = normalized.len();
    if !(len == 40 || len == 64) {
        return None;
    }
    if !normalized.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return None;
    }
    Some(normalized)
}

fn redact_run_reference_locator(locator: &str) -> String {
    let locator = locator.trim();
    if let Some(transport) = locator.strip_prefix("git+") {
        if let Ok(mut url) = url::Url::parse(transport) {
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.set_query(None);
            url.set_fragment(None);
            return format!("git+{}", url);
        }
    }

    let mut redacted = locator.to_string();
    if let Some(pos) = redacted.find('#') {
        redacted.truncate(pos);
    }
    if let Some(pos) = redacted.find('?') {
        redacted.truncate(pos);
    }
    if let Some(scheme_pos) = redacted.find("://") {
        let after_scheme = scheme_pos + 3;
        if let Some(at_rel) = redacted[after_scheme..].find('@') {
            let at_pos = after_scheme + at_rel;
            let next_slash = redacted[after_scheme..]
                .find('/')
                .map(|idx| after_scheme + idx);
            if next_slash.map(|slash| at_pos < slash).unwrap_or(true) {
                redacted.replace_range(after_scheme..at_pos, "***");
            }
        }
    }
    redacted
}

pub(super) fn run_reference_target(
    ctx: &CommandContext,
    request: &RunRequest,
    reference: &RunReferenceTarget,
    requested_target: &str,
    interactive: bool,
    strict: bool,
) -> Result<ExecutionOutcome> {
    if request.at.is_some() {
        return Ok(ExecutionOutcome::user_error(
            "px run <ref>:<script> does not support --at",
            json!({
                "reason": "run_reference_at_ref_unsupported",
                "hint": "remove --at and pin the repository commit in the run target instead",
            }),
        ));
    }
    if request.sandbox {
        return Ok(ExecutionOutcome::user_error(
            "px run <ref>:<script> does not support --sandbox",
            json!({
                "reason": "run_reference_sandbox_unsupported",
                "hint": "omit --sandbox for run-by-reference targets",
            }),
        ));
    }

    let commit = match reference
        .git_ref
        .as_deref()
        .and_then(normalize_pinned_commit)
    {
        Some(commit) => commit,
        None => {
            let raw_ref = reference.git_ref.as_deref().unwrap_or_default().trim();
            if !request.allow_floating {
                if !raw_ref.is_empty()
                    && raw_ref.chars().all(|ch| ch.is_ascii_hexdigit())
                    && raw_ref.len() != 40
                    && raw_ref.len() != 64
                {
                    return Ok(ExecutionOutcome::user_error(
                        "run-by-reference requires a full commit SHA",
                        json!({
                            "reason": "run_reference_requires_full_sha",
                            "ref": raw_ref,
                            "hint": "use a full 40-character commit SHA (example: git rev-parse HEAD)",
                            "recommendation": {
                                "command": "px run --allow-floating <TARGET> [-- args...]",
                                "hint": "use --allow-floating to resolve a short SHA or branch/tag at runtime",
                            }
                        }),
                    ));
                }
                return Ok(ExecutionOutcome::user_error(
                    "run-by-reference requires a pinned commit SHA",
                    json!({
                        "reason": "run_reference_requires_pin",
                        "hint": "add @<sha> to the repo reference, or pass --allow-floating to resolve a branch/tag at runtime",
                        "recommendation": {
                            "command": "px run --allow-floating <TARGET> [-- args...]",
                            "hint": "floating refs are refused under --frozen or CI=1",
                        }
                    }),
                ));
            }
            if strict {
                return Ok(ExecutionOutcome::user_error(
                    "floating git refs are disabled under --frozen or CI=1",
                    json!({
                        "reason": "run_reference_floating_disallowed",
                        "hint": "pin a full commit SHA in the run target (use @<sha>)",
                    }),
                ));
            }
            if !ctx.is_online() {
                return Ok(ExecutionOutcome::user_error(
                    "floating git refs require online mode",
                    json!({
                        "reason": "run_reference_offline_floating",
                        "hint": "re-run with --online / set PX_ONLINE=1, or pin a full commit SHA",
                    }),
                ));
            }
            let ref_name = reference
                .git_ref
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("HEAD");
            match resolve_floating_git_ref(&reference.locator, ref_name) {
                Ok(commit) => commit,
                Err(outcome) => return Ok(outcome),
            }
        }
    };

    let repo_spec = crate::RepoSnapshotSpec {
        locator: reference.locator.clone(),
        commit: commit.clone(),
        subdir: None,
    };
    let header = crate::store::cas::global_store().resolve_repo_snapshot_header(&repo_spec)?;
    debug!("Source: {} @ {}", header.locator, header.commit);

    let oid = if ctx.is_online() {
        debug!(
            locator = %repo_spec.locator,
            commit = %repo_spec.commit,
            "run-by-reference ensuring repo-snapshot"
        );
        crate::ensure_repo_snapshot(&repo_spec)?
    } else {
        let oid = crate::lookup_repo_snapshot_oid(&repo_spec)?;
        match oid {
            Some(oid) => {
                debug!(
                    oid = %oid,
                    locator = %repo_spec.locator,
                    commit = %repo_spec.commit,
                    "run-by-reference repo-snapshot cache hit (offline)"
                );
                oid
            }
            None => {
                return Ok(ExecutionOutcome::user_error(
                    "repo snapshot is not cached (offline mode)",
                    json!({
                        "reason": "run_reference_offline_missing_snapshot",
                        "hint": "re-run once without --offline to populate the CAS, then retry with --offline",
                    }),
                ));
            }
        }
    };

    let provenance_log =
        record_run_reference_provenance(ctx, &header, &oid, &reference.script_path);
    let materialized_root = crate::store::cas::global_store()
        .root()
        .join(crate::store::cas::MATERIALIZED_REPO_SNAPSHOTS_DIR)
        .join(&oid);
    debug!(
        oid = %oid,
        dst = %materialized_root.display(),
        "run-by-reference materializing repo snapshot"
    );
    crate::materialize_repo_snapshot(&oid, &materialized_root)?;

    let inline = match load_inline_python_script(&reference.script_path, &materialized_root) {
        Ok(inline) => inline,
        Err(outcome) => return Ok(outcome),
    };
    let command_args = json!({
        "target": requested_target,
        "args": &request.args,
        "repo_snapshot": {
            "oid": &oid,
            "locator": &header.locator,
            "commit": &header.commit,
            "script": reference.script_path.to_string_lossy(),
        },
    });
    let mut outcome = match run_inline_script(
        ctx,
        None,
        inline,
        &request.args,
        &command_args,
        interactive,
        strict,
    ) {
        Ok(outcome) => outcome,
        Err(outcome) => outcome,
    };
    if let serde_json::Value::Object(map) = &mut outcome.details {
        map.insert(
            "source".to_string(),
            json!({
                "locator": header.locator,
                "commit": header.commit,
                "repo_snapshot_oid": oid,
                "script": reference.script_path.to_string_lossy(),
            }),
        );
        map.insert(
            "repo_snapshot_materialized_root".to_string(),
            json!(materialized_root.display().to_string()),
        );
        if let Some(path) = provenance_log {
            map.insert(
                "provenance_log".to_string(),
                json!(path.display().to_string()),
            );
        }
    }
    Ok(outcome)
}

fn resolve_floating_git_ref(locator: &str, git_ref: &str) -> Result<String, ExecutionOutcome> {
    use url::Url;

    let redacted_locator = redact_run_reference_locator(locator);
    let transport = locator.strip_prefix("git+").unwrap_or(locator);
    let url = Url::parse(transport).map_err(|err| {
        ExecutionOutcome::user_error(
            "invalid git locator",
            json!({
                "reason": "invalid_run_reference_locator",
                "locator": redacted_locator,
                "error": err.to_string(),
                "hint": "use a locator like 'git+file:///abs/path/to/repo' or 'git+https://host/org/repo.git'",
            }),
        )
    })?;
    if url.username() != "" || url.password().is_some() {
        let mut cleaned = url.clone();
        let _ = cleaned.set_username("");
        let _ = cleaned.set_password(None);
        cleaned.set_query(None);
        cleaned.set_fragment(None);
        return Err(ExecutionOutcome::user_error(
            "git locators must not include credentials",
            json!({
                "reason": "invalid_run_reference_locator",
                "locator": format!("git+{}", cleaned),
                "hint": "remove credentials from the URL and use a git credential helper instead",
            }),
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        let mut cleaned = url.clone();
        let _ = cleaned.set_username("");
        let _ = cleaned.set_password(None);
        cleaned.set_query(None);
        cleaned.set_fragment(None);
        return Err(ExecutionOutcome::user_error(
            "git locators must not include a query or fragment",
            json!({
                "reason": "invalid_run_reference_locator",
                "locator": format!("git+{}", cleaned),
                "hint": "remove the query/fragment; use a plain git+https:// or git+file:// locator",
            }),
        ));
    }
    let mut cleaned = url.clone();
    let _ = cleaned.set_username("");
    let _ = cleaned.set_password(None);
    cleaned.set_query(None);
    cleaned.set_fragment(None);
    let cleaned_transport = cleaned.to_string();
    match url.scheme() {
        "file" => {
            let repo_path = url.to_file_path().map_err(|_| {
                ExecutionOutcome::user_error(
                    "invalid file git locator",
                    json!({
                        "reason": "invalid_run_reference_locator",
                        "locator": redacted_locator,
                        "hint": "use a locator like 'git+file:///abs/path/to/repo'",
                    }),
                )
            })?;
            let output = Command::new("git")
                .arg("-C")
                .arg(repo_path)
                .arg("rev-parse")
                .arg(format!("{git_ref}^{{commit}}"))
                .env("GIT_TERMINAL_PROMPT", "0")
                .output()
                .map_err(|err| {
                    ExecutionOutcome::user_error(
                        "git is required to resolve floating refs",
                        json!({
                            "reason": "run_reference_git_unavailable",
                            "error": err.to_string(),
                            "hint": "install git and ensure it is on PATH",
                        }),
                    )
                })?;
            if !output.status.success() {
                return Err(ExecutionOutcome::user_error(
                    "unable to resolve floating git ref",
                    json!({
                        "reason": "run_reference_ref_resolution_failed",
                        "locator": redacted_locator,
                        "ref": git_ref,
                        "stderr": String::from_utf8_lossy(&output.stderr).trim().to_string(),
                        "hint": "pin a full commit SHA (recommended) or verify the ref exists",
                    }),
                ));
            }
            let resolved = String::from_utf8_lossy(&output.stdout).trim().to_string();
            normalize_pinned_commit(&resolved).ok_or_else(|| {
                ExecutionOutcome::user_error(
                    "unable to resolve floating git ref",
                    json!({
                        "reason": "run_reference_ref_resolution_failed",
                        "locator": redacted_locator,
                        "ref": git_ref,
                        "stdout": resolved,
                        "hint": "pin a full commit SHA (recommended) or verify the ref exists",
                    }),
                )
            })
        }
        _ => {
            let output = Command::new("git")
                .arg("ls-remote")
                .arg("--quiet")
                .arg(&cleaned_transport)
                .arg(git_ref)
                .env("GIT_TERMINAL_PROMPT", "0")
                .output()
                .map_err(|err| {
                    ExecutionOutcome::user_error(
                        "git is required to resolve floating refs",
                        json!({
                            "reason": "run_reference_git_unavailable",
                            "error": err.to_string(),
                            "hint": "install git and ensure it is on PATH",
                        }),
                    )
                })?;
            if !output.status.success() {
                return Err(ExecutionOutcome::user_error(
                    "unable to resolve floating git ref",
                    json!({
                        "reason": "run_reference_ref_resolution_failed",
                        "locator": redacted_locator,
                        "ref": git_ref,
                        "stderr": String::from_utf8_lossy(&output.stderr).trim().to_string(),
                        "hint": "pin a full commit SHA (recommended) or verify the ref exists",
                    }),
                ));
            }
            let stdout = String::from_utf8_lossy(&output.stdout);
            let mut candidates = Vec::new();
            for line in stdout.lines() {
                let mut parts = line.split_whitespace();
                let Some(sha) = parts.next() else {
                    continue;
                };
                let refname = parts.next().unwrap_or_default();
                candidates.push((sha.to_string(), refname.to_string()));
            }
            if candidates.is_empty() {
                return Err(ExecutionOutcome::user_error(
                    "unable to resolve floating git ref",
                    json!({
                        "reason": "run_reference_ref_resolution_failed",
                        "locator": redacted_locator,
                        "ref": git_ref,
                        "hint": "pin a full commit SHA (recommended) or verify the ref exists",
                    }),
                ));
            }
            let preferred = candidates
                .iter()
                .find(|(_, refname)| refname.ends_with("^{}"))
                .or_else(|| candidates.iter().find(|(_, refname)| refname == "HEAD"))
                .map(|(sha, _)| sha.clone())
                .unwrap_or_else(|| candidates[0].0.clone());
            normalize_pinned_commit(&preferred).ok_or_else(|| {
                ExecutionOutcome::user_error(
                    "unable to resolve floating git ref",
                    json!({
                        "reason": "run_reference_ref_resolution_failed",
                        "locator": locator,
                        "ref": git_ref,
                        "stdout": stdout.trim(),
                        "hint": "pin a full commit SHA (recommended) or verify the ref exists",
                    }),
                )
            })
        }
    }
}
