use std::fs;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use reqwest::{blocking::Client, StatusCode};
use serde_json::json;
use toml_edit::{DocumentMut, Item, Table, Value as TomlValue};

use crate::{
    build_http_client, project_table, python_context, CommandContext, ExecutionOutcome,
    InstallUserError,
};

use super::artifacts::ArtifactSummary;
use super::plan::{plan_publish, PublishPlanning, PublishRegistry, PublishRequest};

#[derive(Clone, Debug)]
struct PublishMetadata {
    name: String,
    version: String,
    summary: Option<String>,
    description: Option<String>,
    description_content_type: Option<String>,
    keywords: Option<String>,
    license: Option<String>,
    home_page: Option<String>,
    project_urls: Vec<String>,
    classifiers: Vec<String>,
    requires_python: Option<String>,
}

#[derive(Clone, Debug)]
enum ArtifactUploadKind {
    Wheel {
        pyversion: String,
        abi: String,
        platform: String,
    },
    Sdist,
}

/// Publishes the built artifacts to the selected Python package registry.
///
/// # Errors
/// Returns an error when metadata cannot be loaded or an upload request fails.
pub fn publish_project(ctx: &CommandContext, request: &PublishRequest) -> Result<ExecutionOutcome> {
    publish_project_outcome(ctx, request)
}

fn publish_project_outcome(
    ctx: &CommandContext,
    request: &PublishRequest,
) -> Result<ExecutionOutcome> {
    let py_ctx = match python_context(ctx) {
        Ok(py) => py,
        Err(outcome) => return Ok(outcome),
    };
    let plan = match plan_publish(ctx, &py_ctx, request)? {
        PublishPlanning::Plan(plan) => plan,
        PublishPlanning::Outcome(outcome) => return Ok(outcome),
    };

    if plan.dry_run {
        let details = json!({
            "registry": plan.registry.label,
            "token_env": plan.token_env,
            "dry_run": true,
            "artifacts": plan.artifacts.clone(),
        });
        let message = format!(
            "px publish: dry-run to {} ({} artifacts)",
            plan.registry.label,
            plan.artifacts.len()
        );
        return Ok(ExecutionOutcome::success(message, details));
    }

    let metadata = load_publish_metadata(&py_ctx.project_root)?;
    let client = build_http_client()?;
    let token_value = plan
        .token
        .as_ref()
        .ok_or_else(|| anyhow!("publish plan missing token"))?;
    for summary in &plan.artifacts {
        let file_path = py_ctx.project_root.join(&summary.path);
        upload_artifact(
            &client,
            &plan.registry,
            token_value,
            &metadata,
            summary,
            &file_path,
        )?;
    }

    let count = plan.artifacts.len();
    let details = json!({
        "registry": plan.registry.label,
        "token_env": plan.token_env,
        "dry_run": false,
        "artifacts": plan.artifacts,
    });
    let message = format!(
        "px publish: uploaded {} artifacts to {}",
        count, plan.registry.label
    );
    Ok(ExecutionOutcome::success(message, details))
}

fn load_publish_metadata(project_root: &Path) -> Result<PublishMetadata> {
    let pyproject_path = project_root.join("pyproject.toml");
    let contents = fs::read_to_string(&pyproject_path)
        .with_context(|| format!("reading {}", pyproject_path.display()))?;
    let doc: DocumentMut = contents.parse()?;
    let project = project_table(&doc)?;
    let name = project
        .get("name")
        .and_then(Item::as_str)
        .ok_or_else(|| anyhow!("pyproject missing [project].name"))?
        .to_string();
    let version = project
        .get("version")
        .and_then(Item::as_str)
        .ok_or_else(|| anyhow!("pyproject missing [project].version"))?
        .to_string();
    let summary = project
        .get("description")
        .and_then(Item::as_str)
        .map(ToString::to_string);
    let description = summary.clone();
    let description_content_type = summary
        .as_ref()
        .map(|_| "text/plain; charset=UTF-8".to_string());
    let keywords = {
        let values = parse_string_list(project.get("keywords"));
        if values.is_empty() {
            None
        } else {
            Some(values.join(" "))
        }
    };
    let license = parse_license(project.get("license"));
    let requires_python = project
        .get("requires-python")
        .and_then(Item::as_str)
        .map(ToString::to_string);
    let project_urls = collect_project_urls(project.get("urls").and_then(Item::as_table));
    let home_page = project_urls.iter().find_map(|entry| {
        let mut parts = entry.splitn(2, ',');
        let label = parts.next()?.trim().to_ascii_lowercase();
        let value = parts.next()?.trim();
        if label == "homepage" {
            Some(value.to_string())
        } else {
            None
        }
    });
    let classifiers = parse_string_list(project.get("classifiers"));
    Ok(PublishMetadata {
        name,
        version,
        summary,
        description,
        description_content_type,
        keywords,
        license,
        home_page,
        project_urls,
        classifiers,
        requires_python,
    })
}

fn parse_license(item: Option<&Item>) -> Option<String> {
    let entry = item?;
    if let Some(value) = entry.as_str() {
        return Some(value.to_string());
    }
    if let Some(table) = entry.as_inline_table() {
        if let Some(text) = table.get("text").and_then(TomlValue::as_str) {
            return Some(text.to_string());
        }
    }
    if let Some(table) = entry.as_table() {
        if let Some(text) = table.get("text").and_then(Item::as_str) {
            return Some(text.to_string());
        }
    }
    None
}

fn parse_string_list(item: Option<&Item>) -> Vec<String> {
    let Some(array) = item.and_then(Item::as_array) else {
        return Vec::new();
    };
    array
        .iter()
        .filter_map(TomlValue::as_str)
        .map(ToString::to_string)
        .collect()
}

fn collect_project_urls(table: Option<&Table>) -> Vec<String> {
    let mut urls = Vec::new();
    if let Some(entries) = table {
        for (label, value) in entries {
            if let Some(url) = value.as_str() {
                urls.push(format!("{label}, {url}"));
            }
        }
    }
    urls
}

fn upload_artifact(
    client: &Client,
    registry: &PublishRegistry,
    token: &str,
    metadata: &PublishMetadata,
    summary: &ArtifactSummary,
    file_path: &Path,
) -> Result<()> {
    let filename = Path::new(&summary.path)
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("invalid artifact path {}", summary.path))?;
    let kind = classify_artifact(filename)?;
    let bytes = fs::read(file_path).with_context(|| format!("reading {}", file_path.display()))?;
    let boundary = format!(
        "----pxpublish{}",
        &summary.sha256[..summary.sha256.len().min(12)],
    );
    let body = build_upload_body(&boundary, metadata, summary, &kind, filename, &bytes);

    let response = client
        .post(&registry.url)
        .basic_auth("__token__", Some(token))
        .header(
            "Content-Type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(body)
        .send()
        .with_context(|| format!("failed to upload {filename}"))?;
    if response.status() == StatusCode::FORBIDDEN {
        return Err(InstallUserError::new(
            format!("registry {} rejected the provided credentials", registry.label),
            json!({
                "registry": registry.label,
                "reason": "auth_forbidden",
                "hint": "Confirm the token environment variable and permissions for this repository.",
            }),
        )
        .into());
    }
    response
        .error_for_status()
        .with_context(|| format!("upload failed for {filename}"))?;
    Ok(())
}

fn build_upload_body(
    boundary: &str,
    metadata: &PublishMetadata,
    summary: &ArtifactSummary,
    kind: &ArtifactUploadKind,
    filename: &str,
    bytes: &[u8],
) -> Vec<u8> {
    let mut body = Vec::new();
    append_form_field(&mut body, boundary, ":action", "file_upload");
    append_form_field(&mut body, boundary, "protocol_version", "1");
    append_form_field(&mut body, boundary, "metadata_version", "2.1");
    append_form_field(&mut body, boundary, "name", &metadata.name);
    append_form_field(&mut body, boundary, "version", &metadata.version);
    append_form_field(
        &mut body,
        boundary,
        "summary",
        metadata.summary.as_deref().unwrap_or(""),
    );
    append_form_field(
        &mut body,
        boundary,
        "description",
        metadata.description.as_deref().unwrap_or(""),
    );
    append_form_field(
        &mut body,
        boundary,
        "description_content_type",
        metadata.description_content_type.as_deref().unwrap_or(""),
    );
    append_form_field(
        &mut body,
        boundary,
        "keywords",
        metadata.keywords.as_deref().unwrap_or(""),
    );
    append_form_field(
        &mut body,
        boundary,
        "home_page",
        metadata.home_page.as_deref().unwrap_or(""),
    );
    append_form_field(
        &mut body,
        boundary,
        "license",
        metadata.license.as_deref().unwrap_or(""),
    );
    if let Some(req) = metadata.requires_python.as_deref() {
        append_form_field(&mut body, boundary, "requires_python", req);
    }
    for classifier in &metadata.classifiers {
        append_form_field(&mut body, boundary, "classifiers", classifier);
    }
    for entry in &metadata.project_urls {
        append_form_field(&mut body, boundary, "project_urls", entry);
    }
    append_form_field(&mut body, boundary, "sha256_digest", &summary.sha256);
    append_form_field(&mut body, boundary, "size", &summary.bytes.to_string());
    append_form_field(&mut body, boundary, "comment", "");
    match kind {
        ArtifactUploadKind::Wheel {
            pyversion,
            abi,
            platform,
        } => {
            append_form_field(&mut body, boundary, "filetype", "bdist_wheel");
            append_form_field(&mut body, boundary, "pyversion", pyversion);
            append_form_field(&mut body, boundary, "platform", platform);
            append_form_field(&mut body, boundary, "abi", abi);
        }
        ArtifactUploadKind::Sdist => {
            append_form_field(&mut body, boundary, "filetype", "sdist");
            append_form_field(&mut body, boundary, "pyversion", "source");
        }
    }
    append_file_field(&mut body, boundary, "content", filename, bytes);
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    body
}

fn append_form_field(buf: &mut Vec<u8>, boundary: &str, name: &str, value: &str) {
    buf.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    buf.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
    );
    buf.extend_from_slice(value.as_bytes());
    buf.extend_from_slice(b"\r\n");
}

fn append_file_field(buf: &mut Vec<u8>, boundary: &str, name: &str, filename: &str, bytes: &[u8]) {
    buf.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    buf.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\n")
            .as_bytes(),
    );
    buf.extend_from_slice(b"Content-Type: application/octet-stream\r\n\r\n");
    buf.extend_from_slice(bytes);
    buf.extend_from_slice(b"\r\n");
}

fn has_case_insensitive_extension(path: &Path, extension: &str) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case(extension))
}

fn classify_artifact(filename: &str) -> Result<ArtifactUploadKind, InstallUserError> {
    let path = Path::new(filename);
    if has_case_insensitive_extension(path, "whl") {
        let stem = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or_else(|| {
                InstallUserError::new(
                    "wheel filename missing python tag",
                    json!({
                        "reason": "invalid_wheel_filename",
                        "file": filename,
                        "hint": "Wheel names must include tags like py3-none-any (PEP 427).",
                    }),
                )
            })?;
        let mut parts = stem.rsplit('-');
        let platform = parts.next().ok_or_else(|| {
            InstallUserError::new(
                "wheel filename missing platform tag",
                json!({
                    "reason": "invalid_wheel_filename",
                    "file": filename,
                    "hint": "Wheel names must include platform and ABI tags (e.g. py3-none-any).",
                }),
            )
        })?;
        let abi = parts.next().ok_or_else(|| {
            InstallUserError::new(
                "wheel filename missing abi tag",
                json!({
                    "reason": "invalid_wheel_filename",
                    "file": filename,
                    "hint": "Wheel names must include ABI tags (e.g. cp311-cp311-manylinux).",
                }),
            )
        })?;
        let pyversion = parts.next().ok_or_else(|| {
            InstallUserError::new(
                "wheel filename missing python tag",
                json!({
                    "reason": "invalid_wheel_filename",
                    "file": filename,
                    "hint": "Wheel names must include a python tag (e.g. py3).",
                }),
            )
        })?;
        return Ok(ArtifactUploadKind::Wheel {
            pyversion: pyversion.to_string(),
            abi: abi.to_string(),
            platform: platform.to_string(),
        });
    }
    if has_case_insensitive_extension(path, "gz") {
        if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) {
            if has_case_insensitive_extension(Path::new(stem), "tar") {
                return Ok(ArtifactUploadKind::Sdist);
            }
        }
    }
    if has_case_insensitive_extension(path, "zip") {
        return Ok(ArtifactUploadKind::Sdist);
    }
    Err(InstallUserError::new(
        format!("unsupported artifact type: {filename}"),
        json!({
            "reason": "unsupported_artifact",
            "file": filename,
            "hint": "Only wheels (.whl) and source distributions (.tar.gz/.zip) can be published.",
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::super::artifacts::compute_file_sha256;
    use super::*;
    use httptest::{matchers::*, responders::*, Expectation, Server};
    use std::panic;
    use tempfile::tempdir;

    fn sample_metadata() -> PublishMetadata {
        PublishMetadata {
            name: "demo".into(),
            version: "0.1.0".into(),
            summary: Some("demo package".into()),
            description: None,
            description_content_type: None,
            keywords: None,
            license: None,
            home_page: None,
            project_urls: Vec::new(),
            classifiers: Vec::new(),
            requires_python: None,
        }
    }

    #[test]
    fn classify_artifact_detects_known_formats() -> Result<()> {
        let wheel = classify_artifact("demo-0.1.0-py3-none-any.whl")?;
        match wheel {
            ArtifactUploadKind::Wheel {
                pyversion,
                abi,
                platform,
            } => {
                assert_eq!(pyversion, "py3");
                assert_eq!(abi, "none");
                assert_eq!(platform, "any");
            }
            _ => panic!("expected wheel classification"),
        }

        let tarball = classify_artifact("demo-0.1.0.tar.gz")?;
        assert!(matches!(tarball, ArtifactUploadKind::Sdist));

        let zip = classify_artifact("demo-0.1.0.zip")?;
        assert!(matches!(zip, ArtifactUploadKind::Sdist));

        assert!(
            classify_artifact("demo-0.1.0.whl.txt").is_err(),
            "non-artifact extensions should be rejected"
        );
        Ok(())
    }

    #[test]
    fn upload_artifact_reports_forbidden_credentials() -> Result<()> {
        let server = match panic::catch_unwind(Server::run) {
            Ok(server) => server,
            Err(_) => {
                eprintln!(
                    "skipping publish forbidden-credentials test (httptest server unavailable)"
                );
                return Ok(());
            }
        };
        server.expect(
            Expectation::matching(all_of![
                request::method_path("POST", "/"),
                request::body(matches("filetype")),
            ])
            .respond_with(status_code(403)),
        );

        let tmp = tempdir()?;
        let file_path = tmp.path().join("demo-0.1.0.tar.gz");
        fs::write(&file_path, b"dummy sdist")?;
        let summary = ArtifactSummary {
            path: file_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_string(),
            bytes: fs::metadata(&file_path)?.len(),
            sha256: compute_file_sha256(&file_path)?,
        };
        let registry = PublishRegistry {
            label: "mock-registry".into(),
            url: server.url_str("/"),
        };
        let client = build_http_client()?;
        let err = upload_artifact(
            &client,
            &registry,
            "secret-token",
            &sample_metadata(),
            &summary,
            &file_path,
        )
        .expect_err("forbidden credentials should bubble up");
        assert!(
            err.to_string()
                .contains("rejected the provided credentials"),
            "unexpected error text: {err}"
        );
        Ok(())
    }
}
