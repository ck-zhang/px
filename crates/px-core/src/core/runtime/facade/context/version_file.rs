use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, Result};
use toml_edit::{Item, Value as TomlValue};
use tracing::warn;

pub(crate) fn ensure_version_file(manifest_path: &Path) -> Result<()> {
    let contents = fs::read_to_string(manifest_path)?;
    let doc: toml_edit::DocumentMut = contents.parse()?;
    let manifest_dir = manifest_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();

    let hatch_describe = hatch_git_describe_command(&doc);
    let hatch_simplified_semver = hatch_prefers_simplified_semver(&doc);
    let hatch_drop_local = hatch_drops_local_version(&doc);
    if let Some(version_file) = hatch_version_file(&doc) {
        ensure_version_stub(
            &manifest_dir,
            &version_file,
            VersionFileStyle::HatchVcsHook,
            VersionDeriveOptions {
                git_describe_command: hatch_describe.as_deref(),
                simplified_semver: hatch_simplified_semver,
                drop_local: hatch_drop_local,
            },
        )?;
    }

    if let Some(version_file) = setuptools_scm_version_file(&doc) {
        ensure_version_stub(
            &manifest_dir,
            &version_file,
            VersionFileStyle::SetuptoolsScm,
            VersionDeriveOptions::default(),
        )?;
    }

    if let Some(version_file) = pdm_version_file(&doc) {
        ensure_version_stub(
            &manifest_dir,
            &version_file,
            VersionFileStyle::Plain,
            VersionDeriveOptions::default(),
        )?;
    }

    ensure_inline_version_module(&manifest_dir, &doc)?;

    Ok(())
}

pub(in super::super) fn uses_hatch_vcs(doc: &toml_edit::DocumentMut) -> bool {
    doc.get("tool")
        .and_then(|tool| tool.get("hatch"))
        .and_then(|hatch| hatch.get("version"))
        .and_then(|version| version.get("source"))
        .and_then(|value| value.as_str())
        .map(|value| value.eq_ignore_ascii_case("vcs"))
        .unwrap_or(false)
}

pub(in super::super) fn hatch_version_file(doc: &toml_edit::DocumentMut) -> Option<PathBuf> {
    doc.get("tool")
        .and_then(|tool| tool.get("hatch"))
        .and_then(|hatch| hatch.get("build"))
        .and_then(|build| build.get("hooks"))
        .and_then(|hooks| hooks.get("vcs"))
        .and_then(|vcs| vcs.get("version-file"))
        .and_then(|item| item.as_str())
        .map(PathBuf::from)
}

pub(in super::super) fn hatch_git_describe_command(
    doc: &toml_edit::DocumentMut,
) -> Option<Vec<String>> {
    doc.get("tool")
        .and_then(|tool| tool.get("hatch"))
        .and_then(|hatch| hatch.get("version"))
        .and_then(|version| version.get("raw-options"))
        .and_then(|raw| raw.get("git_describe_command"))
        .and_then(string_vec_from_item)
}

pub(in super::super) fn hatch_prefers_simplified_semver(doc: &toml_edit::DocumentMut) -> bool {
    hatch_version_raw_option(doc, "version_scheme")
        .map(|value| value == "python-simplified-semver")
        .unwrap_or(false)
}

pub(in super::super) fn hatch_drops_local_version(doc: &toml_edit::DocumentMut) -> bool {
    hatch_version_raw_option(doc, "local_scheme")
        .map(|value| value == "no-local-version")
        .unwrap_or(false)
}

fn hatch_version_raw_option(doc: &toml_edit::DocumentMut, key: &str) -> Option<String> {
    doc.get("tool")
        .and_then(|tool| tool.get("hatch"))
        .and_then(|hatch| hatch.get("version"))
        .and_then(|version| version.get("raw-options"))
        .and_then(|raw| raw.get(key))
        .and_then(|item| item.as_str())
        .map(str::to_string)
}

fn string_vec_from_item(item: &Item) -> Option<Vec<String>> {
    match item {
        Item::Value(TomlValue::Array(items)) => {
            let mut values = Vec::new();
            for entry in items.iter() {
                let value = entry.as_str()?.to_string();
                values.push(value);
            }
            if values.is_empty() {
                None
            } else {
                Some(values)
            }
        }
        Item::Value(TomlValue::String(value)) => {
            let values: Vec<String> = value
                .value()
                .split_whitespace()
                .map(|entry| entry.to_string())
                .collect();
            if values.is_empty() {
                None
            } else {
                Some(values)
            }
        }
        _ => None,
    }
}

pub(in super::super) fn setuptools_scm_version_file(
    doc: &toml_edit::DocumentMut,
) -> Option<PathBuf> {
    doc.get("tool")
        .and_then(|tool| tool.get("setuptools_scm"))
        .and_then(|cfg| cfg.get("write_to").or_else(|| cfg.get("version_file")))
        .and_then(|item| item.as_str())
        .map(PathBuf::from)
}

pub(in super::super) fn pdm_version_file(doc: &toml_edit::DocumentMut) -> Option<PathBuf> {
    doc.get("tool")
        .and_then(|tool| tool.get("pdm"))
        .and_then(|pdm| pdm.get("version"))
        .and_then(|version| version.get("write_to"))
        .and_then(|item| item.as_str())
        .map(PathBuf::from)
}

fn ensure_inline_version_module(manifest_dir: &Path, doc: &toml_edit::DocumentMut) -> Result<()> {
    let Some(project) = doc.get("project").and_then(Item::as_table) else {
        return Ok(());
    };
    if project
        .get("dynamic")
        .and_then(Item::as_array)
        .is_some_and(|items| {
            items
                .iter()
                .any(|item| item.as_str().is_some_and(|value| value == "version"))
        })
    {
        return Ok(());
    }

    let Some(name) = project.get("name").and_then(Item::as_str) else {
        return Ok(());
    };
    let Some(version) = project.get("version").and_then(Item::as_str) else {
        return Ok(());
    };

    let module = name.replace(['-', '.'], "_").to_lowercase();
    let candidates = [
        manifest_dir.join("src").join(&module),
        manifest_dir.join("python").join(&module),
        manifest_dir.join(&module),
    ];
    let Some(package_dir) = candidates.iter().find(|path| path.exists()) else {
        return Ok(());
    };
    let version_pyi = package_dir.join("version.pyi");
    if !version_pyi.exists() {
        return Ok(());
    }
    let version_py = package_dir.join("version.py");

    let (version_value, git_revision) = inline_version_values(manifest_dir, version);
    let release_flag = if !version_value.contains("dev") && !version_value.contains('+') {
        "True"
    } else {
        "False"
    };
    let contents = format!(
        "\"\"\"\nModule to expose more detailed version info for the installed `{name}`\n\"\"\"\n\
version = \"{version_value}\"\n\
__version__ = version\n\
full_version = version\n\n\
git_revision = \"{git_revision}\"\n\
release = {release_flag}\n\
short_version = version.split(\"+\")[0]\n"
    );

    if let Some(parent) = version_py.parent() {
        fs::create_dir_all(parent)?;
    }
    if version_py.exists() {
        if let Ok(current) = fs::read_to_string(&version_py) {
            if current == contents {
                return Ok(());
            }
        }
    }
    fs::write(&version_py, contents)?;
    Ok(())
}

fn inline_version_values(manifest_dir: &Path, version: &str) -> (String, String) {
    let mut version_value = version.to_string();
    let mut git_revision = String::new();

    if let Some((hash, date)) = latest_git_commit(manifest_dir) {
        git_revision = hash.clone();
        if version_value.contains("dev") && !date.is_empty() {
            let short = hash.chars().take(7).collect::<String>();
            if !short.is_empty() {
                version_value = format!("{version_value}+git{date}.{short}");
            }
        }
    }

    (version_value, git_revision)
}

fn latest_git_commit(manifest_dir: &Path) -> Option<(String, String)> {
    let output = Command::new("git")
        .args([
            "-c",
            "log.showSignature=false",
            "log",
            "-1",
            "--format=\"%H %aI\"",
        ])
        .current_dir(manifest_dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut parts = stdout.trim().trim_matches('"').split_whitespace();
    let hash = parts.next().unwrap_or_default();
    if hash.is_empty() {
        return None;
    }
    let timestamp = parts.next().unwrap_or_default();
    let date = timestamp
        .split('T')
        .next()
        .unwrap_or_default()
        .replace('-', "");
    Some((hash.to_string(), date))
}

#[derive(Clone, Copy)]
enum VersionFileStyle {
    HatchVcsHook,
    SetuptoolsScm,
    Plain,
}

#[derive(Default)]
pub(in super::super) struct VersionDeriveOptions<'a> {
    pub(in super::super) git_describe_command: Option<&'a [String]>,
    pub(in super::super) simplified_semver: bool,
    pub(in super::super) drop_local: bool,
}

fn ensure_version_stub(
    root: &Path,
    target: &Path,
    style: VersionFileStyle,
    derive_opts: VersionDeriveOptions<'_>,
) -> Result<()> {
    let version_path = root.join(target);
    let mut rewrite = false;
    if version_path.exists() {
        match style {
            VersionFileStyle::HatchVcsHook => {
                if let Ok(contents) = fs::read_to_string(&version_path) {
                    let has_version = contents.contains("version =");
                    let has_alias = contents.contains("__version__");
                    let fallback_version = contents.lines().find_map(|line| {
                        let trimmed = line.trim_start();
                        if !trimmed.starts_with("version =") {
                            return None;
                        }
                        let value = trimmed
                            .split_once('=')
                            .map(|(_, rhs)| rhs.trim().trim_matches('"'))
                            .unwrap_or_default();
                        Some(
                            value == "unknown"
                                || value.starts_with("0.0.0+")
                                || value.starts_with("0+"),
                        )
                    });
                    let needs_upgrade = fallback_version.unwrap_or(false);
                    if !(has_version && has_alias) || needs_upgrade {
                        rewrite = true;
                    }
                } else {
                    rewrite = true;
                }
            }
            VersionFileStyle::SetuptoolsScm => {
                if let Ok(contents) = fs::read_to_string(&version_path) {
                    let has_version = contents.contains("version =");
                    let has_alias = contents.contains("__version__");
                    let has_tuple = contents.contains("version_tuple = tuple(_v.release)");
                    let has_packaging =
                        contents.contains("from packaging.version import Version as _Version");
                    let fallback_version = contents.lines().find_map(|line| {
                        let trimmed = line.trim_start();
                        if !trimmed.starts_with("version =") {
                            return None;
                        }
                        let value = trimmed
                            .split_once('=')
                            .map(|(_, rhs)| rhs.trim().trim_matches('"'))
                            .unwrap_or_default();
                        Some(
                            value == "unknown"
                                || value.starts_with("0.0.0+")
                                || value.starts_with("0+"),
                        )
                    });
                    let needs_upgrade = fallback_version.unwrap_or(false);
                    if !(has_version && has_alias && has_tuple && has_packaging) || needs_upgrade {
                        rewrite = true;
                    }
                } else {
                    rewrite = true;
                }
            }
            VersionFileStyle::Plain => {
                if let Ok(contents) = fs::read_to_string(&version_path) {
                    let trimmed = contents.trim();
                    if trimmed.is_empty()
                        || trimmed == "unknown"
                        || trimmed.starts_with("0.0.0+")
                        || trimmed.starts_with("0+")
                    {
                        rewrite = true;
                    }
                } else {
                    rewrite = true;
                }
            }
        }
        if !rewrite {
            return Ok(());
        }
    }
    if !version_path.exists() || rewrite {
        if let Some(parent) = version_path.parent() {
            fs::create_dir_all(parent)?;
        }
    }

    let derived = match derive_vcs_version(root, &derive_opts) {
        Ok(version) => version,
        Err(err) => {
            warn!(
                error = %err,
                path = %root.display(),
                "git metadata unavailable; writing fallback vcs version"
            );
            if derive_opts.drop_local {
                "0.0.0".to_string()
            } else {
                "0.0.0+unknown".to_string()
            }
        }
    };

    let contents = match style {
        VersionFileStyle::HatchVcsHook => format!(
            "version = \"{derived}\"\n\
__version__ = version\n\
__all__ = [\"__version__\", \"version\"]\n"
        ),
        VersionFileStyle::SetuptoolsScm => format!(
            "from packaging.version import Version as _Version\n\
version = \"{derived}\"\n\
__version__ = version\n\
_v = _Version(version)\n\
version_tuple = tuple(_v.release)\n\
__all__ = [\"__version__\", \"version\", \"version_tuple\"]\n"
        ),
        VersionFileStyle::Plain => format!("{derived}\n"),
    };
    if rewrite || !version_path.exists() {
        fs::write(&version_path, contents)?;
    }
    Ok(())
}

pub(in super::super) fn derive_vcs_version(
    manifest_dir: &Path,
    derive_opts: &VersionDeriveOptions<'_>,
) -> Result<String> {
    if let Some(command) = derive_opts.git_describe_command {
        if let Some(info) = describe_with_command(command, manifest_dir) {
            if let Some(version) = format_version_from_describe(&info, derive_opts) {
                return Ok(version);
            }
        }
    }

    let default_describe = [
        "git".to_string(),
        "describe".to_string(),
        "--tags".to_string(),
        "--dirty".to_string(),
        "--long".to_string(),
    ];
    if let Some(info) = describe_with_command(&default_describe, manifest_dir) {
        if let Some(version) = format_version_from_describe(&info, derive_opts) {
            return Ok(version);
        }
    }

    if let Ok(output) = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(manifest_dir)
        .output()
    {
        if output.status.success() {
            let hash = String::from_utf8_lossy(&output.stdout)
                .trim()
                .trim_start_matches('g')
                .to_string();
            if !hash.is_empty() {
                if derive_opts.drop_local {
                    return Ok("0.0.0".to_string());
                }
                return Ok(format!("0.0.0+g{hash}"));
            }
        }
    }

    Err(anyhow!(
        "unable to derive version from git; add tags or version-file"
    ))
}

fn describe_with_command(command: &[String], manifest_dir: &Path) -> Option<GitDescribeInfo> {
    let (program, args) = command.split_first()?;
    if program.trim().is_empty() {
        return None;
    }
    let output = Command::new(program)
        .args(args)
        .current_dir(manifest_dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_git_describe(String::from_utf8_lossy(&output.stdout).trim())
}

fn format_version_from_describe(
    info: &GitDescribeInfo,
    derive_opts: &VersionDeriveOptions<'_>,
) -> Option<String> {
    if derive_opts.simplified_semver {
        if let Some(version) = simplified_semver_from_describe(info, derive_opts.drop_local) {
            return Some(version);
        }
    }
    pep440_from_info(info, derive_opts.drop_local)
}

#[cfg(test)]
pub(in super::super) fn pep440_from_describe(desc: &str) -> Option<String> {
    parse_git_describe(desc).and_then(|info| pep440_from_info(&info, false))
}

fn pep440_from_info(info: &GitDescribeInfo, drop_local: bool) -> Option<String> {
    let tag = info.tag.trim_start_matches('v');
    let mut version = tag.to_string();
    if !drop_local {
        version.push_str(&format!("+{}.g{}", info.commits_since_tag, info.sha));
        if info.dirty {
            version.push_str(".dirty");
        }
    }
    Some(version)
}

fn simplified_semver_from_describe(info: &GitDescribeInfo, drop_local: bool) -> Option<String> {
    let numeric_start = info
        .tag
        .find(|ch: char| ch.is_ascii_digit())
        .or_else(|| info.tag.find('v').map(|index| index + 1))?;
    let base = info.tag[numeric_start..]
        .trim_start_matches('v')
        .to_string();
    if base.is_empty() {
        return None;
    }
    let mut release_parts: Vec<u64> = base
        .split('.')
        .map(|part| part.parse::<u64>().ok())
        .collect::<Option<Vec<_>>>()?;
    if release_parts.is_empty() {
        return None;
    }
    if info.commits_since_tag > 0 {
        if let Some(last) = release_parts.last_mut() {
            *last += 1;
        }
    }
    let mut version = release_parts
        .iter()
        .map(|part| part.to_string())
        .collect::<Vec<_>>()
        .join(".");
    if info.commits_since_tag > 0 {
        version.push_str(&format!(".dev{}", info.commits_since_tag));
    }
    let has_local = !drop_local && (info.commits_since_tag > 0 || info.dirty);
    if has_local {
        version.push_str(&format!("+g{}", info.sha));
        if info.dirty {
            version.push_str(".dirty");
        }
    }
    Some(version)
}

fn parse_git_describe(desc: &str) -> Option<GitDescribeInfo> {
    let trimmed = desc.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut dirty = false;
    let mut core = trimmed.to_string();
    if core.ends_with("-dirty") {
        dirty = true;
        core = core.trim_end_matches("-dirty").to_string();
    }
    let mut iter = core.rsplitn(3, '-');
    let sha_part = iter.next()?;
    let commits_part = iter.next()?;
    let tag_part = iter.next()?;

    Some(GitDescribeInfo {
        tag: tag_part.to_string(),
        commits_since_tag: commits_part.parse::<usize>().ok()?,
        sha: sha_part.trim_start_matches('g').to_string(),
        dirty,
    })
}

struct GitDescribeInfo {
    tag: String,
    commits_since_tag: usize,
    sha: String,
    dirty: bool,
}
