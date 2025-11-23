use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use flate2::{write::GzEncoder, Compression};
use serde::Serialize;
use serde_json::json;
use sha2::Digest;
use tar::Builder;
use toml_edit::{DocumentMut, Table};
use uv_build_backend::{build_source_dist, build_wheel};
use walkdir::WalkDir;
use zip::{write::FileOptions, CompressionMethod, ZipWriter};

use crate::{
    project_table, python_context, relative_path_str, CommandContext, ExecutionOutcome,
    PythonContext,
};

#[derive(Clone, Debug)]
pub struct BuildRequest {
    pub include_sdist: bool,
    pub include_wheel: bool,
    pub out: Option<PathBuf>,
    pub dry_run: bool,
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
struct ProjectMetadata {
    name: String,
    normalized_name: String,
    version: String,
    requires_python: Option<String>,
    requires_dist: Vec<String>,
    optional_requires: BTreeMap<String, Vec<String>>,
    summary: Option<String>,
    entry_points: BTreeMap<String, BTreeMap<String, String>>,
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
struct SourceAsset {
    relative: String,
    content: SourceContent,
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
enum SourceContent {
    File(PathBuf),
    Inline(Vec<u8>),
}

/// Builds the configured project artifacts.
///
/// # Errors
/// Returns an error if the build environment is unavailable or packaging fails.
pub fn build_project(ctx: &CommandContext, request: &BuildRequest) -> Result<ExecutionOutcome> {
    build_project_outcome(ctx, request)
}

fn build_project_outcome(ctx: &CommandContext, request: &BuildRequest) -> Result<ExecutionOutcome> {
    let py_ctx = match python_context(ctx) {
        Ok(py) => py,
        Err(outcome) => return Ok(outcome),
    };
    let targets = build_targets_from_request(request);
    let out_dir = resolve_output_dir_from_request(&py_ctx, request.out.as_ref());

    if request.dry_run {
        let artifacts = collect_artifact_summaries(&out_dir, None, &py_ctx)?;
        let details = json!({
            "artifacts": artifacts,
            "out_dir": relative_path_str(&out_dir, &py_ctx.project_root),
            "format": targets.label(),
            "dry_run": true,
        });
        let message = format!(
            "px build: dry-run (format={}, out={})",
            targets.label(),
            relative_path_str(&out_dir, &py_ctx.project_root)
        );
        return Ok(ExecutionOutcome::success(message, details));
    }

    ctx.fs()
        .create_dir_all(&out_dir)
        .with_context(|| format!("creating output directory at {}", out_dir.display()))?;
    let produced = build_with_uv(ctx, &py_ctx, targets, &out_dir)?;

    let artifacts = summarize_selected_artifacts(&produced, &py_ctx)?;
    if artifacts.is_empty() {
        return Ok(ExecutionOutcome::user_error(
            "px build: build completed but produced no artifacts",
            json!({
                "out_dir": relative_path_str(&out_dir, &py_ctx.project_root),
                "format": targets.label(),
            }),
        ));
    }

    let first = &artifacts[0];
    let sha_short: String = first.sha256.chars().take(12).collect();
    let message = if artifacts.len() == 1 {
        format!(
            "px build: wrote {} ({}, sha256={}…)",
            first.path,
            format_bytes(first.bytes),
            sha_short
        )
    } else {
        format!(
            "px build: wrote {} artifacts ({}, sha256={}…)",
            artifacts.len(),
            format_bytes(first.bytes),
            sha_short
        )
    };
    let details = json!({
        "artifacts": artifacts,
        "out_dir": relative_path_str(&out_dir, &py_ctx.project_root),
        "format": targets.label(),
        "dry_run": false,
        "skip_tests": ctx.config().test.skip_tests_flag.clone(),
    });
    Ok(ExecutionOutcome::success(message, details))
}

#[derive(Clone, Serialize)]
pub(crate) struct ArtifactSummary {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Clone, Copy)]
pub(crate) struct BuildTargets {
    sdist: bool,
    wheel: bool,
}

impl BuildTargets {
    fn label(self) -> &'static str {
        match (self.sdist, self.wheel) {
            (true, true) => "both",
            (true, false) => "sdist",
            (false, true) => "wheel",
            (false, false) => "none",
        }
    }
}

fn build_targets_from_request(request: &BuildRequest) -> BuildTargets {
    let mut targets = BuildTargets {
        sdist: request.include_sdist,
        wheel: request.include_wheel,
    };
    if !targets.sdist && !targets.wheel {
        targets = BuildTargets {
            sdist: true,
            wheel: true,
        };
    }
    targets
}

fn resolve_output_dir_from_request(ctx: &PythonContext, out: Option<&PathBuf>) -> PathBuf {
    match out {
        Some(path) if path.is_absolute() => path.clone(),
        Some(path) => ctx.project_root.join(path),
        None => ctx.project_root.join("dist"),
    }
}

fn build_with_uv(
    ctx: &CommandContext,
    py_ctx: &PythonContext,
    targets: BuildTargets,
    out_dir: &Path,
) -> Result<Vec<PathBuf>> {
    let mut produced = Vec::new();
    ctx.fs()
        .create_dir_all(out_dir)
        .with_context(|| format!("creating output directory at {}", out_dir.display()))?;
    ensure_package_stub(&py_ctx.project_root)?;
    if targets.sdist {
        let filename = build_source_dist(&py_ctx.project_root, out_dir, crate::PX_VERSION)
            .context("building source distribution")?;
        produced.push(out_dir.join(filename.to_string()));
    }
    if targets.wheel {
        let filename = build_wheel(&py_ctx.project_root, out_dir, None, crate::PX_VERSION)
            .context("building wheel")?;
        produced.push(out_dir.join(filename.to_string()));
    }
    Ok(produced)
}

fn ensure_package_stub(project_root: &Path) -> Result<()> {
    let pyproject_path = project_root.join("pyproject.toml");
    let contents = fs::read_to_string(&pyproject_path)
        .with_context(|| format!("reading {}", pyproject_path.display()))?;
    let doc: DocumentMut = contents.parse()?;
    let name = doc["project"]["name"]
        .as_str()
        .ok_or_else(|| anyhow!("pyproject missing [project].name"))?
        .to_string();
    let package = name.replace('-', "_");
    let src_root = project_root.join("src");
    let module_dir = src_root.join(&package);
    let existing_package = project_root.join(&package);
    if !module_dir.exists() && existing_package.exists() {
        copy_package_tree(&existing_package, &module_dir)?;
    }
    fs::create_dir_all(&module_dir)?;
    let init_py = module_dir.join("__init__.py");
    if !init_py.exists() {
        fs::write(&init_py, b"")?;
    }
    Ok(())
}

fn copy_package_tree(from: &Path, to: &Path) -> Result<()> {
    for entry in WalkDir::new(from) {
        let entry = entry?;
        let path = entry.path();
        let relative = path.strip_prefix(from).unwrap_or(path);
        let dest = to.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&dest)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(path, &dest)?;
        }
    }
    Ok(())
}

pub(crate) fn collect_artifact_summaries(
    dir: &Path,
    targets: Option<BuildTargets>,
    ctx: &PythonContext,
) -> Result<Vec<ArtifactSummary>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if !path.is_file() {
            continue;
        }
        if let Some(targets) = targets {
            if !artifact_matches_format(&path, targets) {
                continue;
            }
        }
        let bytes = fs::metadata(&path)?.len();
        let sha256 = compute_file_sha256(&path)?;
        entries.push(ArtifactSummary {
            path: relative_path_str(&path, &ctx.project_root),
            bytes,
            sha256,
        });
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(entries)
}

fn artifact_matches_format(path: &Path, targets: BuildTargets) -> bool {
    if targets.sdist {
        if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
            if ext.eq_ignore_ascii_case("gz") {
                return true;
            }
        }
    }
    if targets.wheel {
        if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
            if ext.eq_ignore_ascii_case("whl") {
                return true;
            }
        }
    }
    false
}

fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;

    fn format_scaled(value: u64, unit: u64, suffix: &str) -> String {
        let whole = value / unit;
        let remainder = value % unit;
        let tenths = (remainder * 10) / unit;
        format!("{whole}.{tenths} {suffix}")
    }

    if bytes >= MB {
        format_scaled(bytes, MB, "MB")
    } else if bytes >= KB {
        format_scaled(bytes, KB, "KB")
    } else {
        format!("{bytes} B")
    }
}

pub(crate) fn summarize_selected_artifacts(
    paths: &[PathBuf],
    ctx: &PythonContext,
) -> Result<Vec<ArtifactSummary>> {
    let mut entries = Vec::new();
    for path in paths {
        let bytes = fs::metadata(path)?.len();
        let sha256 = compute_file_sha256(path)?;
        entries.push(ArtifactSummary {
            path: relative_path_str(path, &ctx.project_root),
            bytes,
            sha256,
        });
    }
    Ok(entries)
}

#[allow(dead_code)]
fn write_sdist(ctx: &PythonContext, out_dir: &Path, metadata: &ProjectMetadata) -> Result<PathBuf> {
    let filename = format!("{}-{}.tar.gz", metadata.name, metadata.version);
    let path = out_dir.join(filename);
    let file = File::create(&path)?;
    let encoder = GzEncoder::new(file, Compression::default());
    let mut tar = Builder::new(encoder);
    let base = format!("{}-{}", metadata.name, metadata.version);

    append_metadata_files(&ctx.project_root, &mut tar, &base, metadata)?;
    append_sources_to_sdist(&ctx.project_root, &mut tar, &base, metadata)?;

    tar.finish()?;
    let encoder = tar.into_inner()?;
    encoder.finish()?;
    Ok(path)
}

#[allow(dead_code)]
fn write_wheel(ctx: &PythonContext, out_dir: &Path, metadata: &ProjectMetadata) -> Result<PathBuf> {
    let filename = format!(
        "{}-{}-py3-none-any.whl",
        metadata.normalized_name, metadata.version
    );
    let path = out_dir.join(filename);
    let file = File::create(&path)?;
    let mut zip = ZipWriter::new(file);
    let options = FileOptions::default().compression_method(CompressionMethod::Deflated);

    let source_assets = collect_source_assets(&ctx.project_root, metadata)?;
    let mut records = Vec::new();
    for asset in &source_assets {
        let data = read_asset_bytes(asset)?;
        zip.start_file(&asset.relative, options)?;
        zip.write_all(&data)?;
        records.push(record_entry(&asset.relative, &data));
    }

    let dist_info = format!(
        "{}-{}.dist-info",
        metadata.normalized_name, metadata.version
    );
    let metadata_path = format!("{dist_info}/METADATA");
    let metadata_body = render_metadata(metadata);
    zip.start_file(&metadata_path, options)?;
    zip.write_all(metadata_body.as_bytes())?;
    records.push(record_entry(&metadata_path, metadata_body.as_bytes()));

    let wheel_path = format!("{dist_info}/WHEEL");
    let wheel_body =
        "Wheel-Version: 1.0\nGenerator: px\nRoot-Is-Purelib: true\nTag: py3-none-any\n".to_string();
    zip.start_file(&wheel_path, options)?;
    zip.write_all(wheel_body.as_bytes())?;
    records.push(record_entry(&wheel_path, wheel_body.as_bytes()));

    if let Some(entry_points_body) = render_entry_points(metadata) {
        let ep_path = format!("{dist_info}/entry_points.txt");
        zip.start_file(&ep_path, options)?;
        zip.write_all(entry_points_body.as_bytes())?;
        records.push(record_entry(&ep_path, entry_points_body.as_bytes()));
    }

    let record_path = format!("{dist_info}/RECORD");
    records.push(format!("{record_path},,")); // RECORD has no hash/size
    let mut record_body = records.join("\n");
    record_body.push('\n');
    zip.start_file(&record_path, options)?;
    zip.write_all(record_body.as_bytes())?;

    zip.finish()?;
    Ok(path)
}

#[allow(dead_code)]
fn append_sources_to_sdist(
    project_root: &Path,
    tar: &mut Builder<GzEncoder<File>>,
    base: &str,
    metadata: &ProjectMetadata,
) -> Result<()> {
    let assets = collect_source_assets(project_root, metadata)?;
    for asset in &assets {
        let mut header = tar::Header::new_gnu();
        let data = read_asset_bytes(asset)?;
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        let path_in_tgz = format!("{base}/{}", asset.relative);
        tar.append_data(&mut header, path_in_tgz, data.as_slice())?;
    }
    Ok(())
}

#[allow(dead_code)]
fn append_metadata_files(
    project_root: &Path,
    tar: &mut Builder<GzEncoder<File>>,
    base: &str,
    metadata: &ProjectMetadata,
) -> Result<()> {
    let pyproject = project_root.join("pyproject.toml");
    if pyproject.exists() {
        tar.append_path_with_name(&pyproject, format!("{base}/pyproject.toml"))?;
    }
    for candidate in ["README.md", "README.rst", "LICENSE", "LICENSE.txt"] {
        let path = project_root.join(candidate);
        if path.exists() {
            tar.append_path_with_name(&path, format!("{base}/{candidate}"))?;
        }
    }

    let pkg_info_body = render_metadata(metadata);
    let mut header = tar::Header::new_gnu();
    header.set_size(pkg_info_body.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append_data(
        &mut header,
        format!("{base}/PKG-INFO"),
        pkg_info_body.as_bytes(),
    )?;
    Ok(())
}

#[allow(dead_code)]
fn load_project_metadata(project_root: &Path) -> Result<ProjectMetadata> {
    let pyproject_path = project_root.join("pyproject.toml");
    let contents = fs::read_to_string(&pyproject_path)
        .with_context(|| format!("reading {}", pyproject_path.display()))?;
    let doc: toml_edit::DocumentMut = contents.parse()?;
    let project = project_table(&doc)?;
    let name = project
        .get("name")
        .and_then(toml_edit::Item::as_str)
        .ok_or_else(|| anyhow!("pyproject missing [project].name"))?
        .to_string();
    let version = project
        .get("version")
        .and_then(toml_edit::Item::as_str)
        .ok_or_else(|| anyhow!("pyproject missing [project].version"))?
        .to_string();
    let requires_python = project
        .get("requires-python")
        .and_then(toml_edit::Item::as_str)
        .map(ToString::to_string);
    let requires_dist = project
        .get("dependencies")
        .and_then(toml_edit::Item::as_array)
        .map(|array| {
            array
                .iter()
                .filter_map(|value| value.as_str().map(ToString::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let summary = project
        .get("description")
        .and_then(toml_edit::Item::as_str)
        .map(ToString::to_string);
    let entry_points = collect_entry_points(project);
    let optional_requires = collect_optional_dependencies(project);
    let normalized_name = normalize_package_name(&name);

    Ok(ProjectMetadata {
        name,
        normalized_name,
        version,
        requires_python,
        requires_dist,
        optional_requires,
        summary,
        entry_points,
    })
}

fn collect_entry_points(project: &Table) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut groups = BTreeMap::new();
    collect_entry_point_group(project, "scripts", "console_scripts", &mut groups);
    collect_entry_point_group(project, "gui-scripts", "gui_scripts", &mut groups);
    if let Some(ep_table) = project
        .get("entry-points")
        .and_then(toml_edit::Item::as_table)
    {
        for (group, table) in ep_table.iter() {
            if let Some(entries) = table.as_table() {
                let mut mapped = BTreeMap::new();
                for (name, value) in entries.iter() {
                    if let Some(target) = value.as_str() {
                        mapped.insert(name.to_string(), target.to_string());
                    }
                }
                if !mapped.is_empty() {
                    groups.insert(group.to_string(), mapped);
                }
            }
        }
    }
    groups
}

fn collect_entry_point_group(
    project: &Table,
    project_key: &str,
    entry_point_group: &str,
    groups: &mut BTreeMap<String, BTreeMap<String, String>>,
) {
    if let Some(scripts) = project.get(project_key).and_then(toml_edit::Item::as_table) {
        let mut mapped = BTreeMap::new();
        for (name, value) in scripts.iter() {
            if let Some(target) = value.as_str() {
                mapped.insert(name.to_string(), target.to_string());
            }
        }
        if !mapped.is_empty() {
            groups.insert(entry_point_group.to_string(), mapped);
        }
    }
}

fn collect_optional_dependencies(project: &Table) -> BTreeMap<String, Vec<String>> {
    let mut extras = BTreeMap::new();
    if let Some(optional) = project
        .get("optional-dependencies")
        .and_then(toml_edit::Item::as_table)
    {
        for (name, array) in optional.iter() {
            if let Some(values) = array.as_array() {
                let mut deps = Vec::new();
                for value in values {
                    if let Some(spec) = value.as_str() {
                        deps.push(spec.to_string());
                    }
                }
                if !deps.is_empty() {
                    extras.insert(name.to_string(), deps);
                }
            }
        }
    }
    extras
}

#[allow(dead_code)]
fn collect_source_assets(
    project_root: &Path,
    metadata: &ProjectMetadata,
) -> Result<Vec<SourceAsset>> {
    let mut assets = Vec::new();
    let mut seen = HashSet::new();
    let src = project_root.join("src");
    if src.exists() {
        add_tree_assets(&src, &src, Path::new(""), &mut assets, &mut seen)?;
    }
    let pkg_root = project_root.join(&metadata.normalized_name);
    if pkg_root.exists() {
        add_tree_assets(
            &pkg_root,
            project_root,
            Path::new(""),
            &mut assets,
            &mut seen,
        )?;
    }
    assets.sort_by(|a, b| a.relative.cmp(&b.relative));
    if assets.is_empty() {
        let placeholder = format!("__version__ = \"{}\"\n", metadata.version);
        assets.push(SourceAsset {
            relative: format!("{}/__init__.py", metadata.normalized_name),
            content: SourceContent::Inline(placeholder.into_bytes()),
        });
    }
    if assets.is_empty() {
        return Err(anyhow!(
            "no package sources found (expected src/ or {name}/)",
            name = metadata.normalized_name
        ));
    }
    Ok(assets)
}

#[allow(dead_code)]
fn add_tree_assets(
    path: &Path,
    strip_prefix: &Path,
    dest_prefix: &Path,
    assets: &mut Vec<SourceAsset>,
    seen: &mut HashSet<String>,
) -> Result<()> {
    if path
        .file_name()
        .is_some_and(|name| name.to_string_lossy().starts_with('.'))
    {
        return Ok(());
    }
    if path.file_name().is_some_and(|name| name == "__pycache__") {
        return Ok(());
    }
    if path.is_dir() {
        let mut entries: Vec<_> = fs::read_dir(path)?.collect::<Result<_, _>>()?;
        entries.sort_by_key(|entry| entry.path());
        for entry in entries {
            add_tree_assets(&entry.path(), strip_prefix, dest_prefix, assets, seen)?;
        }
        return Ok(());
    }
    let relative = path
        .strip_prefix(strip_prefix)
        .unwrap_or(path)
        .to_path_buf();
    let mut dest = PathBuf::from(dest_prefix);
    dest.push(relative);
    let rel = normalize_archive_path(&dest);
    if seen.insert(rel.clone()) {
        assets.push(SourceAsset {
            relative: rel,
            content: SourceContent::File(path.to_path_buf()),
        });
    }
    Ok(())
}

#[allow(dead_code)]
fn render_metadata(metadata: &ProjectMetadata) -> String {
    let mut lines = Vec::new();
    lines.push("Metadata-Version: 2.1".to_string());
    lines.push(format!("Name: {}", metadata.name));
    lines.push(format!("Version: {}", metadata.version));
    if let Some(summary) = &metadata.summary {
        lines.push(format!("Summary: {summary}"));
    }
    if let Some(rp) = &metadata.requires_python {
        lines.push(format!("Requires-Python: {rp}"));
    }
    for extra in metadata.optional_requires.keys() {
        lines.push(format!("Provides-Extra: {extra}"));
    }
    for req in &metadata.requires_dist {
        lines.push(format!("Requires-Dist: {req}"));
    }
    for (extra, reqs) in &metadata.optional_requires {
        for req in reqs {
            lines.push(format!(r#"Requires-Dist: {req} ; extra == "{extra}""#));
        }
    }
    lines.push(String::new());
    lines.join("\n")
}

#[allow(dead_code)]
fn render_entry_points(metadata: &ProjectMetadata) -> Option<String> {
    if metadata.entry_points.is_empty() {
        return None;
    }
    let mut sections = Vec::new();
    for (group, entries) in &metadata.entry_points {
        sections.push(format!("[{group}]"));
        for (name, target) in entries {
            sections.push(format!("{name} = {target}"));
        }
        sections.push(String::new());
    }
    Some(sections.join("\n"))
}

fn record_entry(path: &str, data: &[u8]) -> String {
    let mut hasher = sha2::Sha256::new();
    hasher.update(data);
    let digest = hasher.finalize();
    let hash = URL_SAFE_NO_PAD.encode(digest);
    format!("{path},sha256={hash},{}", data.len())
}

pub(super) fn compute_file_sha256(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};

    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    io::copy(&mut file, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

fn read_asset_bytes(asset: &SourceAsset) -> Result<Vec<u8>> {
    match &asset.content {
        SourceContent::File(path) => {
            fs::read(path).with_context(|| format!("reading source file {}", path.display()))
        }
        SourceContent::Inline(bytes) => Ok(bytes.clone()),
    }
}

fn normalize_package_name(name: &str) -> String {
    let mut result = String::new();
    for ch in name.chars() {
        if matches!(ch, '-' | '.' | ' ') {
            result.push('_');
        } else {
            result.push(ch);
        }
    }
    result
}

fn normalize_archive_path(path: &Path) -> String {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn resolve_output_dir_handles_relative_and_absolute() -> Result<()> {
        let root = tempdir()?;
        let ctx = PythonContext {
            project_root: root.path().to_path_buf(),
            python: "/usr/bin/python".to_string(),
            pythonpath: String::new(),
            allowed_paths: Vec::new(),
        };

        let rel = PathBuf::from("custom/dist");
        let resolved_rel = resolve_output_dir_from_request(&ctx, Some(&rel));
        assert_eq!(resolved_rel, root.path().join("custom/dist"));

        let abs = root.path().join("abs/dist");
        let resolved_abs = resolve_output_dir_from_request(&ctx, Some(&abs));
        assert_eq!(resolved_abs, abs);
        Ok(())
    }

    #[test]
    fn artifact_matches_format_respects_targets() {
        let sdist = PathBuf::from("dist/demo-0.1.0.tar.gz");
        let wheel = PathBuf::from("dist/demo-0.1.0-py3-none-any.whl");

        let sdist_only = BuildTargets {
            sdist: true,
            wheel: false,
        };
        assert!(artifact_matches_format(&sdist, sdist_only));
        assert!(!artifact_matches_format(&wheel, sdist_only));

        let wheel_only = BuildTargets {
            sdist: false,
            wheel: true,
        };
        assert!(artifact_matches_format(&wheel, wheel_only));
        assert!(!artifact_matches_format(&sdist, wheel_only));

        let both = BuildTargets {
            sdist: true,
            wheel: true,
        };
        assert!(artifact_matches_format(&sdist, both));
        assert!(artifact_matches_format(&wheel, both));
    }

    #[test]
    fn build_targets_default_to_both_when_not_selected() {
        let request = BuildRequest {
            include_sdist: false,
            include_wheel: false,
            out: None,
            dry_run: false,
        };

        let targets = build_targets_from_request(&request);
        assert!(targets.sdist, "sdist should be selected by default");
        assert!(targets.wheel, "wheel should be selected by default");
    }

    #[test]
    fn format_bytes_scales_values() {
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(2048), "2.0 KB");
        assert_eq!(format_bytes(1_572_864), "1.5 MB");
    }
}
