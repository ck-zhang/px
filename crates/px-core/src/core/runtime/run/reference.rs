use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde_json::json;
use toml_edit::DocumentMut;
use tracing::{debug, warn};

use super::RunRequest;
use crate::core::runtime::script::{load_inline_python_script, run_inline_script};
use crate::{CommandContext, ExecutionOutcome};

#[derive(Clone, Debug)]
pub(crate) enum RunReferenceTarget {
    Script {
        locator: String,
        git_ref: Option<String>,
        script_path: PathBuf,
    },
    Repo {
        locator: String,
        git_ref: Option<String>,
        subdir: Option<PathBuf>,
    },
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
    if let Some(reference) = parse_run_reference_target_url(target)? {
        return Ok(Some(reference));
    }
    Ok(None)
}

fn github_test_locator_override() -> Option<String> {
    let value = std::env::var("PX_TEST_GITHUB_FILE_REPO").ok()?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(format!("git+file://{trimmed}"))
}

fn github_locator(org: &str, repo: &str) -> String {
    if let Some(locator) = github_test_locator_override() {
        return locator;
    }
    let org = org.trim().to_ascii_lowercase();
    let repo = repo.trim().to_ascii_lowercase();
    format!("git+https://github.com/{org}/{repo}.git")
}

fn parse_run_reference_target_url(
    target: &str,
) -> Result<Option<RunReferenceTarget>, ExecutionOutcome> {
    use serde_json::json;

    let url = match url::Url::parse(target) {
        Ok(url) => url,
        Err(_) => return Ok(None),
    };
    match url.scheme() {
        "http" | "https" => {}
        _ => return Ok(None),
    }
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    match host.as_str() {
        "github.com" | "www.github.com" => Ok(Some(parse_github_web_url(&url)?)),
        "raw.githubusercontent.com" => Ok(Some(parse_github_raw_url(&url)?)),
        _ => Err(ExecutionOutcome::user_error(
            "unsupported URL run target",
            json!({
                "reason": "run_reference_unsupported_url",
                "url": target,
                "hint": "Supported URL forms: https://github.com/<org>/<repo>/blob/<sha>/<path/to/script.py>, https://raw.githubusercontent.com/<org>/<repo>/<sha>/<path/to/script.py>, and https://github.com/<org>/<repo>/tree/<sha>/",
            }),
        )),
    }
}

fn parse_github_web_url(url: &url::Url) -> Result<RunReferenceTarget, ExecutionOutcome> {
    use serde_json::json;

    let segments = url
        .path_segments()
        .map(|iter| iter.filter(|seg| !seg.is_empty()).collect::<Vec<_>>())
        .unwrap_or_default();
    if segments.len() < 2 {
        return Err(ExecutionOutcome::user_error(
            "invalid GitHub URL run target",
            json!({
                "reason": "run_reference_invalid_url",
                "url": url.as_str(),
                "hint": "Expected a URL like https://github.com/<org>/<repo>",
            }),
        ));
    }
    let org = segments[0].trim();
    let repo_raw = segments[1].trim();
    let repo = match repo_raw.get(repo_raw.len().saturating_sub(4)..) {
        Some(suffix) if suffix.eq_ignore_ascii_case(".git") => &repo_raw[..repo_raw.len() - 4],
        _ => repo_raw,
    };
    if org.is_empty() || repo.is_empty() {
        return Err(ExecutionOutcome::user_error(
            "invalid GitHub URL run target",
            json!({
                "reason": "run_reference_invalid_url",
                "url": url.as_str(),
                "hint": "Expected a URL like https://github.com/<org>/<repo>",
            }),
        ));
    }
    let locator = github_locator(org, repo);

    if segments.len() == 2 {
        return Ok(RunReferenceTarget::Repo {
            locator,
            git_ref: None,
            subdir: None,
        });
    }

    match segments[2] {
        "blob" => {
            if segments.len() < 5 {
                return Err(ExecutionOutcome::user_error(
                    "invalid GitHub blob URL run target",
                    json!({
                        "reason": "run_reference_invalid_url",
                        "url": url.as_str(),
                        "hint": "Expected a URL like https://github.com/<org>/<repo>/blob/<sha>/<path/to/script.py>",
                    }),
                ));
            }
            let git_ref = segments[3].trim().to_string();
            let script_part = segments[4..].join("/");
            let script_path = normalize_repo_relative_path(&script_part)?;
            Ok(RunReferenceTarget::Script {
                locator,
                git_ref: Some(git_ref),
                script_path,
            })
        }
        "tree" => {
            if segments.len() < 4 {
                return Err(ExecutionOutcome::user_error(
                    "invalid GitHub tree URL run target",
                    json!({
                        "reason": "run_reference_invalid_url",
                        "url": url.as_str(),
                        "hint": "Expected a URL like https://github.com/<org>/<repo>/tree/<sha>/",
                    }),
                ));
            }
            let git_ref = segments[3].trim().to_string();
            let dir_prefix = if segments.len() > 4 {
                segments[4..].join("/")
            } else {
                String::new()
            };

            let mut entry = url
                .query_pairs()
                .find_map(|(k, v)| match k.as_ref() {
                    "px" | "entry" | "entrypoint" | "script" => Some(v.into_owned()),
                    _ => None,
                })
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty());
            if entry.is_none() {
                entry = url
                    .fragment()
                    .and_then(|frag| frag.strip_prefix("px="))
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty());
            }
            let Some(entry) = entry else {
                let subdir = if dir_prefix.trim().is_empty() {
                    None
                } else {
                    Some(normalize_repo_relative_path(&dir_prefix)?)
                };
                return Ok(RunReferenceTarget::Repo {
                    locator,
                    git_ref: Some(git_ref),
                    subdir,
                });
            };
            let entry = entry.trim_start_matches('/');
            let combined = if dir_prefix.is_empty() {
                entry.to_string()
            } else {
                format!("{dir_prefix}/{entry}")
            };
            let script_path = normalize_repo_relative_path(&combined)?;
            Ok(RunReferenceTarget::Script {
                locator,
                git_ref: Some(git_ref),
                script_path,
            })
        }
        "commit" => {
            if segments.len() < 4 {
                return Err(ExecutionOutcome::user_error(
                    "invalid GitHub commit URL run target",
                    json!({
                        "reason": "run_reference_invalid_url",
                        "url": url.as_str(),
                        "hint": "Expected a URL like https://github.com/<org>/<repo>/commit/<sha>",
                    }),
                ));
            }
            let git_ref = segments[3].trim().to_string();
            Ok(RunReferenceTarget::Repo {
                locator,
                git_ref: Some(git_ref),
                subdir: None,
            })
        }
        _ => Err(ExecutionOutcome::user_error(
            "unsupported GitHub URL run target",
            json!({
                "reason": "run_reference_unsupported_url",
                "url": url.as_str(),
                "hint": "Supported URL forms: https://github.com/<org>/<repo>/blob/<sha>/<path/to/script.py>, https://raw.githubusercontent.com/<org>/<repo>/<sha>/<path/to/script.py>, and https://github.com/<org>/<repo>/tree/<sha>/",
            }),
        )),
    }
}

fn parse_github_raw_url(url: &url::Url) -> Result<RunReferenceTarget, ExecutionOutcome> {
    use serde_json::json;

    let segments = url
        .path_segments()
        .map(|iter| iter.filter(|seg| !seg.is_empty()).collect::<Vec<_>>())
        .unwrap_or_default();
    if segments.len() < 4 {
        return Err(ExecutionOutcome::user_error(
            "invalid GitHub raw URL run target",
            json!({
                "reason": "run_reference_invalid_url",
                "url": url.as_str(),
                "hint": "Expected a URL like https://raw.githubusercontent.com/<org>/<repo>/<sha>/<path/to/script.py>",
            }),
        ));
    }
    let org = segments[0].trim();
    let repo_raw = segments[1].trim();
    let repo = match repo_raw.get(repo_raw.len().saturating_sub(4)..) {
        Some(suffix) if suffix.eq_ignore_ascii_case(".git") => &repo_raw[..repo_raw.len() - 4],
        _ => repo_raw,
    };
    if org.is_empty() || repo.is_empty() {
        return Err(ExecutionOutcome::user_error(
            "invalid GitHub raw URL run target",
            json!({
                "reason": "run_reference_invalid_url",
                "url": url.as_str(),
                "hint": "Expected a URL like https://raw.githubusercontent.com/<org>/<repo>/<sha>/<path/to/script.py>",
            }),
        ));
    }
    let locator = github_locator(org, repo);
    let git_ref = segments[2].trim().to_string();
    let script_part = segments[3..].join("/");
    let script_path = normalize_repo_relative_path(&script_part)?;
    Ok(RunReferenceTarget::Script {
        locator,
        git_ref: Some(git_ref),
        script_path,
    })
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
    Ok(RunReferenceTarget::Script {
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
    Ok(RunReferenceTarget::Script {
        locator,
        git_ref,
        script_path,
    })
}

fn extract_pyproject_entrypoints(doc: &DocumentMut) -> std::collections::BTreeMap<String, String> {
    let mut names = std::collections::BTreeMap::new();
    let Some(project) = doc.get("project").and_then(toml_edit::Item::as_table) else {
        return names;
    };

    for (project_key, group_key) in [
        ("scripts", "console_scripts"),
        ("gui-scripts", "gui_scripts"),
    ] {
        if let Some(table) = project.get(project_key).and_then(toml_edit::Item::as_table) {
            for (name, value) in table.iter() {
                let Some(target) = value.as_str() else {
                    continue;
                };
                let trimmed_name = name.trim();
                let trimmed_target = target.trim();
                if trimmed_name.is_empty() || trimmed_target.is_empty() {
                    continue;
                }
                let key = format!("{group_key}:{trimmed_name}");
                names.insert(key, trimmed_target.to_string());
            }
        }
    }

    if let Some(entry_points) = project
        .get("entry-points")
        .and_then(toml_edit::Item::as_table)
    {
        for (group, table) in entry_points.iter() {
            let Some(entries) = table.as_table() else {
                continue;
            };
            let group = group.trim();
            if group.is_empty() {
                continue;
            }
            for (name, value) in entries.iter() {
                let Some(target) = value.as_str() else {
                    continue;
                };
                let trimmed_name = name.trim();
                let trimmed_target = target.trim();
                if trimmed_name.is_empty() || trimmed_target.is_empty() {
                    continue;
                }
                let key = format!("{group}:{trimmed_name}");
                names.insert(key, trimmed_target.to_string());
            }
        }
    }

    names
}

fn infer_repo_entrypoint(execution_root: &Path) -> Result<String, ExecutionOutcome> {
    use serde_json::json;

    let manifest = execution_root.join("pyproject.toml");
    if manifest.exists() {
        let contents = fs::read_to_string(&manifest).map_err(|err| {
            ExecutionOutcome::failure(
                "failed to read pyproject.toml for URL run",
                json!({
                    "reason": "run_reference_manifest_read_failed",
                    "pyproject": manifest.display().to_string(),
                    "error": err.to_string(),
                }),
            )
        })?;
        if let Ok(doc) = DocumentMut::from_str(&contents) {
            let entries = extract_pyproject_entrypoints(&doc);
            let console_scripts: Vec<String> = entries
                .keys()
                .filter_map(|key| key.strip_prefix("console_scripts:").map(str::to_string))
                .collect();
            if console_scripts.len() == 1 {
                return Ok(console_scripts[0].clone());
            }
            if console_scripts.len() > 1 {
                return Err(ExecutionOutcome::user_error(
                    "URL repo target has multiple console scripts",
                    json!({
                        "reason": "run_reference_ambiguous_repo_entrypoint",
                        "root": execution_root.display().to_string(),
                        "candidates": console_scripts,
                        "hint": "Specify one explicitly: px run <URL> <console_script> [-- args...]",
                    }),
                ));
            }
        }
    }

    const MAIN_LIKE: &[&str] = &[
        "main.py",
        "app.py",
        "cli.py",
        "manage.py",
        "server.py",
        "__main__.py",
        "run.py",
    ];
    let mut files = Vec::new();
    for name in MAIN_LIKE {
        let path = execution_root.join(name);
        if path.is_file() {
            files.push(name.to_string());
        }
    }
    if files.len() == 1 {
        return Ok(files[0].clone());
    }
    if files.len() > 1 {
        return Err(ExecutionOutcome::user_error(
            "URL repo target has multiple plausible entry scripts",
            json!({
                "reason": "run_reference_ambiguous_repo_entrypoint",
                "root": execution_root.display().to_string(),
                "candidates": files,
                "hint": "Specify one explicitly: px run <URL> <path/to/script.py> [-- args...]",
            }),
        ));
    }

    Err(ExecutionOutcome::user_error(
        "unable to infer entrypoint for URL repo target",
        json!({
            "reason": "run_reference_missing_repo_entrypoint",
            "root": execution_root.display().to_string(),
            "hint": "Provide an entrypoint after the URL (example: px run <URL> <console_script> [-- args...])",
        }),
    ))
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

fn is_http_url_target(target: &str) -> bool {
    match url::Url::parse(target) {
        Ok(url) => matches!(url.scheme(), "http" | "https"),
        Err(_) => false,
    }
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

fn validate_run_reference_locator(locator: &str) -> Result<(), ExecutionOutcome> {
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

    Ok(())
}

pub(super) fn run_reference_target(
    ctx: &CommandContext,
    request: &RunRequest,
    reference: &RunReferenceTarget,
    requested_target: &str,
    interactive: bool,
    strict: bool,
) -> Result<ExecutionOutcome> {
    match reference {
        RunReferenceTarget::Script {
            locator,
            git_ref,
            script_path,
        } => run_reference_script_target(
            ctx,
            request,
            locator,
            git_ref.as_deref(),
            script_path,
            requested_target,
            interactive,
            strict,
        ),
        RunReferenceTarget::Repo {
            locator,
            git_ref,
            subdir,
        } => run_reference_repo_target(
            ctx,
            request,
            locator,
            git_ref.as_deref(),
            subdir.as_deref(),
            requested_target,
            interactive,
            strict,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_reference_script_target(
    ctx: &CommandContext,
    request: &RunRequest,
    locator: &str,
    git_ref: Option<&str>,
    script_path: &Path,
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

    let (header, oid, materialized_root, provenance_log) = match prepare_repo_snapshot(
        ctx,
        request,
        locator,
        git_ref,
        requested_target,
        strict,
        Some(script_path),
    ) {
        Ok(result) => result,
        Err(outcome) => return Ok(outcome),
    };

    let inline = match load_inline_python_script(script_path, &materialized_root) {
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
            "script": script_path.to_string_lossy(),
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
    attach_reference_details(
        &mut outcome,
        &header,
        &oid,
        &materialized_root,
        Some(script_path),
        provenance_log.as_deref(),
        None,
    );
    Ok(outcome)
}

#[allow(clippy::too_many_arguments)]
fn run_reference_repo_target(
    ctx: &CommandContext,
    request: &RunRequest,
    locator: &str,
    git_ref: Option<&str>,
    subdir: Option<&Path>,
    requested_target: &str,
    interactive: bool,
    strict: bool,
) -> Result<ExecutionOutcome> {
    if request.at.is_some() {
        return Ok(ExecutionOutcome::user_error(
            "px run <url> does not support --at",
            json!({
                "reason": "run_reference_at_ref_unsupported",
                "hint": "omit --at for URL targets; pin the repository commit in the URL instead",
            }),
        ));
    }
    if request.sandbox {
        return Ok(ExecutionOutcome::user_error(
            "px run <url> does not support --sandbox",
            json!({
                "reason": "run_reference_sandbox_unsupported",
                "hint": "omit --sandbox for URL targets",
            }),
        ));
    }

    let (header, oid, materialized_root, _provenance_log) = match prepare_repo_snapshot(
        ctx,
        request,
        locator,
        git_ref,
        requested_target,
        strict,
        None,
    ) {
        Ok(result) => result,
        Err(outcome) => return Ok(outcome),
    };

    let execution_root = if let Some(subdir) = subdir {
        materialized_root.join(subdir)
    } else {
        materialized_root.clone()
    };
    if !execution_root.exists() {
        return Ok(ExecutionOutcome::user_error(
            "URL repo target subdir does not exist in snapshot",
            json!({
                "reason": "run_reference_subdir_missing",
                "url": requested_target,
                "subdir": subdir.map(|p| p.to_string_lossy()).unwrap_or_default(),
                "snapshot_root": materialized_root.display().to_string(),
            }),
        ));
    }

    let (entrypoint_opt, forwarded_args) = split_repo_entrypoint_args(&request.args);
    let entrypoint = match entrypoint_opt {
        Some(value) => value,
        None => match infer_repo_entrypoint(&execution_root) {
            Ok(value) => value,
            Err(outcome) => return Ok(outcome),
        },
    };

    let invocation_root = execution_root.clone();
    let input = match super::ephemeral::detect_ephemeral_input(&invocation_root, Some(&entrypoint))
    {
        Ok(input) => input,
        Err(outcome) => return Ok(outcome),
    };
    let pinned_required = request.frozen || ctx.env_flag_enabled("CI");
    if pinned_required {
        if let Err(outcome) =
            super::ephemeral::enforce_pinned_inputs("run", &invocation_root, &input, request.frozen)
        {
            return Ok(outcome);
        }
    }

    let (snapshot, runtime, sync_report) = match super::ephemeral::prepare_ephemeral_snapshot(
        ctx,
        &invocation_root,
        &input,
        request.frozen,
    ) {
        Ok(result) => result,
        Err(outcome) => return Ok(outcome),
    };
    let workdir = super::invocation_workdir(&invocation_root);
    let host_runner = super::HostCommandRunner::new(ctx);

    let cas_native_fallback =
        match super::prepare_cas_native_run_context(ctx, &snapshot, &invocation_root) {
            Ok(native_ctx) => {
                let deps = super::DependencyContext::from_sources(
                    &snapshot.requirements,
                    Some(&snapshot.lock_path),
                );
                let mut command_args = json!({
                    "target": requested_target,
                    "entrypoint": &entrypoint,
                    "args": &forwarded_args,
                    "repo_snapshot": {
                        "oid": &oid,
                        "locator": &header.locator,
                        "commit": &header.commit,
                        "subdir": subdir.map(|p| p.to_string_lossy()).unwrap_or_default(),
                    },
                });
                deps.inject(&mut command_args);
                let plan = super::plan_run_target(
                    &native_ctx.py_ctx,
                    &snapshot.manifest_path,
                    &entrypoint,
                    &workdir,
                )?;
                let outcome = match plan {
                    super::RunTargetPlan::Script(path) => super::run_project_script_cas_native(
                        ctx,
                        &host_runner,
                        &native_ctx,
                        &path,
                        &forwarded_args,
                        &command_args,
                        &workdir,
                        interactive,
                    )?,
                    super::RunTargetPlan::Executable(program) => super::run_executable_cas_native(
                        ctx,
                        &host_runner,
                        &native_ctx,
                        &program,
                        &forwarded_args,
                        &command_args,
                        &workdir,
                        interactive,
                    )?,
                };
                if let Some(reason) = super::cas_native_fallback_reason(&outcome) {
                    if super::is_integrity_failure(&outcome) {
                        return Ok(outcome);
                    }
                    Some(super::CasNativeFallback {
                        reason,
                        summary: super::cas_native_fallback_summary(&outcome),
                    })
                } else {
                    let mut outcome = outcome;
                    super::attach_autosync_details(&mut outcome, sync_report);
                    attach_reference_details(
                        &mut outcome,
                        &header,
                        &oid,
                        &materialized_root,
                        None,
                        None,
                        None,
                    );
                    return Ok(outcome);
                }
            }
            Err(outcome) => {
                let Some(reason) = super::cas_native_fallback_reason(&outcome) else {
                    return Ok(outcome);
                };
                if super::is_integrity_failure(&outcome) {
                    return Ok(outcome);
                }
                Some(super::CasNativeFallback {
                    reason,
                    summary: super::cas_native_fallback_summary(&outcome),
                })
            }
        };

    let py_ctx = match super::ephemeral::ephemeral_python_context(
        ctx,
        &snapshot,
        &runtime,
        &invocation_root,
    ) {
        Ok(py_ctx) => py_ctx,
        Err(outcome) => return Ok(outcome),
    };

    let deps =
        super::DependencyContext::from_sources(&snapshot.requirements, Some(&snapshot.lock_path));
    let mut command_args = json!({
        "target": requested_target,
        "entrypoint": &entrypoint,
        "args": &forwarded_args,
        "repo_snapshot": {
            "oid": &oid,
            "locator": &header.locator,
            "commit": &header.commit,
            "subdir": subdir.map(|p| p.to_string_lossy()).unwrap_or_default(),
        },
    });
    deps.inject(&mut command_args);
    let plan = super::plan_run_target(&py_ctx, &snapshot.manifest_path, &entrypoint, &workdir)?;
    let mut outcome = match plan {
        super::RunTargetPlan::Script(path) => super::run_project_script(
            ctx,
            &host_runner,
            &py_ctx,
            &path,
            &forwarded_args,
            &command_args,
            &workdir,
            interactive,
            &py_ctx.python,
        )?,
        super::RunTargetPlan::Executable(program) => super::run_executable(
            ctx,
            &host_runner,
            &py_ctx,
            &program,
            &forwarded_args,
            &command_args,
            &workdir,
            interactive,
        )?,
    };
    super::attach_autosync_details(&mut outcome, sync_report);
    if let Some(ref fallback) = cas_native_fallback {
        super::attach_cas_native_fallback(&mut outcome, fallback);
    }
    attach_reference_details(
        &mut outcome,
        &header,
        &oid,
        &materialized_root,
        None,
        None,
        None,
    );
    Ok(outcome)
}

fn split_repo_entrypoint_args(args: &[String]) -> (Option<String>, Vec<String>) {
    let Some((first, rest)) = args.split_first() else {
        return (None, Vec::new());
    };
    if first == "--" {
        return (None, rest.to_vec());
    }
    if first.starts_with('-') {
        return (None, args.to_vec());
    }
    (Some(first.clone()), rest.to_vec())
}

fn attach_reference_details(
    outcome: &mut ExecutionOutcome,
    header: &crate::RepoSnapshotHeader,
    oid: &str,
    materialized_root: &Path,
    script: Option<&Path>,
    provenance_log: Option<&Path>,
    extra: Option<serde_json::Value>,
) {
    if let serde_json::Value::Object(map) = &mut outcome.details {
        let mut source = json!({
            "locator": header.locator,
            "commit": header.commit,
            "repo_snapshot_oid": oid,
        });
        if let Some(script) = script {
            source["script"] = json!(script.to_string_lossy());
        }
        if let Some(serde_json::Value::Object(extra_map)) = extra {
            if let serde_json::Value::Object(source_map) = &mut source {
                for (k, v) in extra_map {
                    source_map.insert(k, v);
                }
            }
        }
        map.insert("source".to_string(), source);
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
}

#[allow(clippy::too_many_arguments)]
fn prepare_repo_snapshot(
    ctx: &CommandContext,
    request: &RunRequest,
    locator: &str,
    git_ref: Option<&str>,
    requested_target: &str,
    strict: bool,
    provenance_script_path: Option<&Path>,
) -> Result<(crate::RepoSnapshotHeader, String, PathBuf, Option<PathBuf>), ExecutionOutcome> {
    let commit = match git_ref.and_then(normalize_pinned_commit) {
        Some(commit) => commit,
        None => {
            let raw_ref = git_ref.unwrap_or_default().trim();
            if !request.allow_floating {
                if !raw_ref.is_empty()
                    && raw_ref.chars().all(|ch| ch.is_ascii_hexdigit())
                    && raw_ref.len() != 40
                    && raw_ref.len() != 64
                {
                    return Err(ExecutionOutcome::user_error(
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
                if is_http_url_target(requested_target) {
                    return Err(ExecutionOutcome::user_error(
                        "unpinned URL refused",
                        json!({
                            "reason": "run_url_requires_pin",
                            "url": requested_target,
                            "ref": raw_ref,
                            "hint": "use a commit-pinned URL (example: https://github.com/<org>/<repo>/tree/<sha>/)",
                            "recommendation": {
                                "command": "px run --allow-floating <URL> [-- args...]",
                                "hint": "use --allow-floating to resolve a branch/tag at runtime (refused under --frozen or CI=1)",
                            }
                        }),
                    ));
                }
                return Err(ExecutionOutcome::user_error(
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
                let hint = if is_http_url_target(requested_target) {
                    "use a commit-pinned URL containing a full SHA"
                } else {
                    "pin a full commit SHA in the run target (use @<sha>)"
                };
                return Err(ExecutionOutcome::user_error(
                    "floating git refs are disabled under --frozen or CI=1",
                    json!({
                        "reason": "run_reference_floating_disallowed",
                        "hint": hint,
                    }),
                ));
            }
            if !ctx.is_online() {
                return Err(ExecutionOutcome::user_error(
                    "floating git refs require online mode",
                    json!({
                        "reason": "run_reference_offline_floating",
                        "hint": "re-run with --online / set PX_ONLINE=1, or pin a full commit SHA",
                    }),
                ));
            }
            let ref_name = git_ref
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("HEAD");
            resolve_floating_git_ref(locator, ref_name)?
        }
    };

    validate_run_reference_locator(locator)?;

    let repo_spec = crate::RepoSnapshotSpec {
        locator: locator.to_string(),
        commit: commit.clone(),
        subdir: None,
    };
    let header = crate::store::cas::global_store()
        .resolve_repo_snapshot_header(&repo_spec)
        .map_err(|err| {
            ExecutionOutcome::failure(
                "failed to resolve repo snapshot header",
                json!({
                    "reason": "run_reference_header_resolution_failed",
                    "locator": redact_run_reference_locator(locator),
                    "commit": commit,
                    "error": err.to_string(),
                }),
            )
        })?;
    debug!("Source: {} @ {}", header.locator, header.commit);

    let oid = if ctx.is_online() {
        debug!(
            locator = %repo_spec.locator,
            commit = %repo_spec.commit,
            "run-by-reference ensuring repo-snapshot"
        );
        crate::ensure_repo_snapshot(&repo_spec).map_err(|err| {
            ExecutionOutcome::failure(
                "failed to fetch repo snapshot",
                json!({
                    "reason": "run_reference_snapshot_fetch_failed",
                    "locator": redact_run_reference_locator(locator),
                    "commit": repo_spec.commit.clone(),
                    "error": err.to_string(),
                }),
            )
        })?
    } else {
        let oid = crate::lookup_repo_snapshot_oid(&repo_spec).map_err(|err| {
            ExecutionOutcome::failure(
                "failed to look up repo snapshot",
                json!({
                    "reason": "run_reference_snapshot_lookup_failed",
                    "locator": redact_run_reference_locator(locator),
                    "commit": repo_spec.commit.clone(),
                    "error": err.to_string(),
                }),
            )
        })?;
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
                return Err(ExecutionOutcome::user_error(
                    "repo snapshot is not cached (offline mode)",
                    json!({
                        "reason": "run_reference_offline_missing_snapshot",
                        "hint": "re-run once without --offline to populate the CAS, then retry with --offline",
                    }),
                ));
            }
        }
    };

    let provenance_log = provenance_script_path
        .and_then(|path| record_run_reference_provenance(ctx, &header, &oid, path));
    let materialized_root = crate::store::cas::global_store()
        .root()
        .join(crate::store::cas::MATERIALIZED_REPO_SNAPSHOTS_DIR)
        .join(&oid);
    debug!(
        oid = %oid,
        dst = %materialized_root.display(),
        "run-by-reference materializing repo snapshot"
    );
    crate::materialize_repo_snapshot(&oid, &materialized_root).map_err(|err| {
        ExecutionOutcome::failure(
            "failed to materialize repo snapshot",
            json!({
                "reason": "run_reference_snapshot_materialize_failed",
                "oid": oid.clone(),
                "dst": materialized_root.display().to_string(),
                "error": err.to_string(),
            }),
        )
    })?;

    Ok((header, oid, materialized_root, provenance_log))
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
