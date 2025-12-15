use std::env;
use std::path::{Path, PathBuf};

use anyhow::Result;
use url::Url;

use super::super::RepoSnapshotHeader;
use super::errors::{redact_repo_locator, repo_snapshot_user_error, RepoSnapshotIssue};
use super::RepoSnapshotSpec;

#[derive(Clone, Debug)]
pub(super) struct ResolvedRepoSnapshotSpec {
    pub(super) header: RepoSnapshotHeader,
    pub(super) locator: ResolvedRepoLocator,
    pub(super) subdir_rel: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub(super) enum ResolvedRepoLocator {
    File {
        canonical: String,
        repo_path: PathBuf,
    },
    Remote {
        canonical: String,
    },
}

pub(super) fn px_online_enabled() -> bool {
    match env::var("PX_ONLINE") {
        Ok(value) => {
            let lowered = value.to_ascii_lowercase();
            !matches!(lowered.as_str(), "0" | "false" | "no" | "off" | "")
        }
        Err(_) => true,
    }
}

pub(super) fn normalize_commit_sha(commit: &str) -> std::result::Result<String, RepoSnapshotIssue> {
    let commit = commit.trim();
    let normalized = commit.to_ascii_lowercase();
    let len = normalized.len();
    let is_hex = normalized.chars().all(|c| c.is_ascii_hexdigit());
    if !(is_hex && (len == 40 || len == 64)) {
        return Err(RepoSnapshotIssue::InvalidCommit {
            commit: commit.to_string(),
        });
    }
    Ok(normalized)
}

fn normalize_subdir(
    subdir: Option<&Path>,
) -> std::result::Result<(Option<String>, Option<PathBuf>), RepoSnapshotIssue> {
    let Some(subdir) = subdir else {
        return Ok((None, None));
    };
    if subdir.as_os_str().is_empty() {
        return Ok((None, None));
    }
    if subdir.is_absolute() {
        return Err(RepoSnapshotIssue::InvalidSubdir {
            subdir: subdir.display().to_string(),
        });
    }

    let mut rel = PathBuf::new();
    for component in subdir.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::Normal(part) => rel.push(part),
            _ => {
                return Err(RepoSnapshotIssue::InvalidSubdir {
                    subdir: subdir.display().to_string(),
                })
            }
        }
    }
    if rel.as_os_str().is_empty() {
        return Ok((None, None));
    }
    let rel_str = rel.to_string_lossy().replace('\\', "/");
    Ok((Some(rel_str), Some(rel)))
}

fn normalize_absolute_path_lexical(path: &Path) -> Result<PathBuf, RepoSnapshotIssue> {
    use std::path::Component;

    if !path.is_absolute() {
        return Err(RepoSnapshotIssue::InvalidLocator {
            locator: redact_repo_locator(&format!("git+file://{}", path.display())),
        });
    }

    let mut prefix = None;
    let mut has_root = false;
    let mut parts: Vec<std::ffi::OsString> = Vec::new();
    for component in path.components() {
        match component {
            Component::Prefix(p) => prefix = Some(p),
            Component::RootDir => has_root = true,
            Component::CurDir => {}
            Component::Normal(part) => parts.push(part.to_os_string()),
            Component::ParentDir => {
                if parts.pop().is_none() {
                    return Err(RepoSnapshotIssue::InvalidLocator {
                        locator: redact_repo_locator(&format!("git+file://{}", path.display())),
                    });
                }
            }
        }
    }

    let mut normalized = PathBuf::new();
    if let Some(prefix) = prefix {
        normalized.push(prefix.as_os_str());
    }
    if has_root {
        normalized.push(std::path::MAIN_SEPARATOR_STR);
    }
    for part in parts {
        normalized.push(part);
    }

    if !normalized.is_absolute() {
        return Err(RepoSnapshotIssue::InvalidLocator {
            locator: redact_repo_locator(&format!("git+file://{}", path.display())),
        });
    }

    Ok(normalized)
}

fn resolve_repo_locator(
    locator: &str,
) -> std::result::Result<ResolvedRepoLocator, RepoSnapshotIssue> {
    let locator = locator.trim();
    if !locator.starts_with("git+") {
        return Err(RepoSnapshotIssue::InvalidLocator {
            locator: redact_repo_locator(locator),
        });
    }
    let transport = &locator["git+".len()..];
    let url = Url::parse(transport).map_err(|_| RepoSnapshotIssue::InvalidLocator {
        locator: redact_repo_locator(locator),
    })?;
    if url.username() != "" || url.password().is_some() {
        let mut redacted = url.clone();
        let _ = redacted.set_username("");
        let _ = redacted.set_password(None);
        redacted.set_query(None);
        redacted.set_fragment(None);
        return Err(RepoSnapshotIssue::LocatorContainsCredentials {
            locator: format!("git+{}", redacted),
        });
    }
    if url.query().is_some() || url.fragment().is_some() {
        let mut redacted = url.clone();
        let _ = redacted.set_username("");
        let _ = redacted.set_password(None);
        redacted.set_query(None);
        redacted.set_fragment(None);
        return Err(RepoSnapshotIssue::LocatorContainsQueryOrFragment {
            locator: format!("git+{}", redacted),
        });
    }
    match url.scheme() {
        "file" => {
            let repo_path = url
                .to_file_path()
                .map_err(|_| RepoSnapshotIssue::InvalidLocator {
                    locator: redact_repo_locator(locator),
                })?;
            let repo_path = if repo_path.is_absolute() {
                repo_path
            } else {
                return Err(RepoSnapshotIssue::InvalidLocator {
                    locator: redact_repo_locator(locator),
                });
            };
            let repo_path = normalize_absolute_path_lexical(&repo_path)?;
            let canonical_url =
                Url::from_file_path(&repo_path).map_err(|_| RepoSnapshotIssue::InvalidLocator {
                    locator: redact_repo_locator(locator),
                })?;
            Ok(ResolvedRepoLocator::File {
                canonical: format!("git+{}", canonical_url),
                repo_path,
            })
        }
        "http" | "https" => Ok(ResolvedRepoLocator::Remote {
            canonical: format!("git+{}", url),
        }),
        _ => Err(RepoSnapshotIssue::UnsupportedLocator {
            locator: locator.to_string(),
        }),
    }
}

pub(super) fn resolve_repo_snapshot_spec(
    spec: &RepoSnapshotSpec,
) -> Result<ResolvedRepoSnapshotSpec> {
    let commit = normalize_commit_sha(&spec.commit).map_err(repo_snapshot_user_error)?;
    let locator = resolve_repo_locator(&spec.locator).map_err(repo_snapshot_user_error)?;
    let canonical_locator = match &locator {
        ResolvedRepoLocator::File { canonical, .. }
        | ResolvedRepoLocator::Remote { canonical, .. } => canonical.clone(),
    };
    let (subdir_str, subdir_rel) =
        normalize_subdir(spec.subdir.as_deref()).map_err(repo_snapshot_user_error)?;
    Ok(ResolvedRepoSnapshotSpec {
        header: RepoSnapshotHeader {
            locator: canonical_locator,
            commit,
            subdir: subdir_str,
        },
        locator,
        subdir_rel,
    })
}
