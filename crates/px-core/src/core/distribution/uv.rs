use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest_retry::policies::ExponentialBackoff;
use tokio::sync::Semaphore;
use url::Url;
use uv_auth::{Credentials, PyxTokenStore};
use uv_cache::Cache;
use uv_client::{
    AuthIntegration, BaseClient, BaseClientBuilder, Connectivity, RegistryClientBuilder,
    DEFAULT_RETRIES,
};
use uv_configuration::{KeyringProviderType, TrustedHost};
use uv_distribution_filename::DistFilename;
use uv_distribution_types::{Index, IndexCapabilities, IndexLocations, IndexUrl};
use uv_publish::{
    check_url, files_for_publishing, upload, validate, CheckUrlClient, FormMetadata, PublishError,
    Reporter,
};
use uv_redacted::DisplaySafeUrl;

use super::artifacts::{compute_file_sha256, ArtifactSummary, BuildTargets};
use crate::{relative_path_str, PX_VERSION};

#[derive(Clone, Debug)]
pub(crate) struct PublishArtifact {
    summary: ArtifactSummary,
    raw_filename: String,
    dist_filename: DistFilename,
    absolute_path: PathBuf,
}

impl PublishArtifact {
    pub(crate) fn summary(&self) -> &ArtifactSummary {
        &self.summary
    }

    pub(crate) fn absolute_path(&self) -> &Path {
        &self.absolute_path
    }

    pub(crate) fn raw_name(&self) -> &str {
        &self.raw_filename
    }

    pub(crate) fn dist_name(&self) -> &DistFilename {
        &self.dist_filename
    }
}

pub(crate) fn build_distributions(
    project_root: &Path,
    targets: BuildTargets,
    out_dir: &Path,
) -> Result<Vec<PathBuf>> {
    let mut produced = Vec::new();
    if targets.sdist {
        let filename =
            uv_build_backend::build_source_dist(project_root, out_dir, crate::PX_VERSION)
                .context("building source distribution")?;
        produced.push(out_dir.join(filename.to_string()));
    }
    if targets.wheel {
        let filename =
            uv_build_backend::build_wheel(project_root, out_dir, None, crate::PX_VERSION)
                .context("building wheel")?;
        produced.push(out_dir.join(filename.to_string()));
    }
    Ok(produced)
}

pub(crate) fn discover_publish_artifacts(
    project_root: &Path,
    dist_dir: &Path,
) -> Result<Vec<PublishArtifact>> {
    if !dist_dir.exists() {
        return Ok(Vec::new());
    }
    let pattern = dist_dir.join("*").to_string_lossy().to_string();
    let mut artifacts = Vec::new();
    for (path, raw_filename, dist_filename) in files_for_publishing(vec![pattern])? {
        let bytes = std::fs::metadata(&path)
            .with_context(|| format!("reading metadata for {}", path.display()))?
            .len();
        let sha256 =
            compute_file_sha256(&path).with_context(|| format!("hashing {}", path.display()))?;
        let summary = ArtifactSummary {
            path: relative_path_str(&path, project_root),
            bytes,
            sha256,
        };
        artifacts.push(PublishArtifact {
            summary,
            raw_filename,
            dist_filename,
            absolute_path: path,
        });
    }
    artifacts.sort_by(|a, b| a.summary.path.cmp(&b.summary.path));
    Ok(artifacts)
}

#[derive(Debug, Default)]
pub(crate) struct PublishUploadReport {
    pub uploaded: usize,
    pub skipped_existing: usize,
}

pub(crate) struct UvPublishSession {
    registry: DisplaySafeUrl,
    credentials: Credentials,
    client: BaseClient,
    retry_policy: ExponentialBackoff,
    token_store: PyxTokenStore,
    check: Option<CheckContext>,
    download_concurrency: Arc<Semaphore>,
}

struct CheckContext {
    index_url: IndexUrl,
    registry_client_builder: RegistryClientBuilder<'static>,
    cache: Cache,
}

impl UvPublishSession {
    pub(crate) fn new(registry: &str, token: &str, cache_root: &Path) -> Result<Self> {
        let registry = DisplaySafeUrl::from_str(registry).context("parsing registry upload URL")?;
        let px_agent = format!("px/{PX_VERSION}");
        let timeout = Duration::from_secs(60);
        let keep_proxies = std::env::var("PX_KEEP_PROXIES")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let builder = reqwest::Client::builder()
            .user_agent(px_agent)
            .timeout(timeout);
        let builder = if keep_proxies {
            builder
        } else {
            builder.no_proxy()
        };
        let http_client = builder.build().context("building px HTTP client")?;

        let base_builder = BaseClientBuilder::default()
            .connectivity(Connectivity::Online)
            .native_tls(false)
            .allow_insecure_host(Vec::<TrustedHost>::new())
            .timeout(timeout)
            .retries(DEFAULT_RETRIES)
            .auth_integration(AuthIntegration::OnlyAuthenticated)
            .keyring(KeyringProviderType::Disabled)
            .custom_client(http_client);

        let retry_policy = base_builder.retry_policy();
        let client = base_builder.clone().retries(0).build();
        let token_store = PyxTokenStore::from_settings().context("loading token store")?;
        let credentials =
            Credentials::basic(Some("__token__".to_string()), Some(token.to_string()));

        let check_url = derive_index_url(&registry);
        let check = if let Some(index_url) = check_url {
            let cache_root = cache_root.join("publish-cache");
            let cache = Cache::from_path(&cache_root)
                .init()
                .context("initializing publish cache")?;
            let index_locations = IndexLocations::new(
                vec![Index::from_index_url(index_url.clone())],
                Vec::new(),
                false,
            );
            let registry_client_builder =
                RegistryClientBuilder::new(base_builder.clone(), cache.clone())
                    .index_locations(index_locations)
                    .keyring(KeyringProviderType::Disabled);
            Some(CheckContext {
                index_url,
                registry_client_builder,
                cache,
            })
        } else {
            None
        };

        Ok(Self {
            registry,
            credentials,
            client,
            retry_policy,
            token_store,
            check,
            download_concurrency: Arc::new(Semaphore::new(1)),
        })
    }

    pub(crate) async fn publish(
        &self,
        artifacts: &[PublishArtifact],
    ) -> Result<PublishUploadReport, PublishError> {
        let mut report = PublishUploadReport::default();
        let check_url_client = self.check.as_ref().map(|check| CheckUrlClient {
            index_url: check.index_url.clone(),
            registry_client_builder: check.registry_client_builder.clone(),
            client: &self.client,
            index_capabilities: IndexCapabilities::default(),
            cache: &check.cache,
        });

        for artifact in artifacts {
            if let Some(check_client) = check_url_client.as_ref() {
                if check_url(
                    check_client,
                    artifact.absolute_path(),
                    artifact.dist_name(),
                    &self.download_concurrency,
                )
                .await?
                {
                    report.skipped_existing += 1;
                    continue;
                }
            }

            let form_metadata =
                FormMetadata::read_from_file(artifact.absolute_path(), artifact.dist_name())
                    .await
                    .map_err(|err| {
                        PublishError::PublishPrepare(
                            artifact.absolute_path().to_path_buf(),
                            Box::new(err),
                        )
                    })?;

            validate(
                artifact.absolute_path(),
                &form_metadata,
                artifact.raw_name(),
                &self.registry,
                &self.token_store,
                &self.client,
                &self.credentials,
            )
            .await?;

            let reporter = Arc::new(NoopReporter);
            let uploaded = upload(
                artifact.absolute_path(),
                &form_metadata,
                artifact.raw_name(),
                artifact.dist_name(),
                &self.registry,
                &self.client,
                self.retry_policy,
                &self.credentials,
                check_url_client.as_ref(),
                &self.download_concurrency,
                reporter,
            )
            .await?;
            if uploaded {
                report.uploaded += 1;
            } else {
                report.skipped_existing += 1;
            }
        }

        Ok(report)
    }
}

struct NoopReporter;

impl Reporter for NoopReporter {
    fn on_progress(&self, _name: &str, _id: usize) {}
    fn on_upload_start(&self, _name: &str, _size: Option<u64>) -> usize {
        0
    }
    fn on_upload_progress(&self, _id: usize, _inc: u64) {}
    fn on_upload_complete(&self, _id: usize) {}
}

fn derive_index_url(registry: &DisplaySafeUrl) -> Option<IndexUrl> {
    let parsed = Url::parse(registry.as_ref()).ok()?;
    let host = parsed.host_str().unwrap_or_default();
    if host.eq_ignore_ascii_case("upload.pypi.org") {
        return IndexUrl::from_str("https://pypi.org/simple").ok();
    }
    if host.eq_ignore_ascii_case("test.pypi.org") {
        return IndexUrl::from_str("https://test.pypi.org/simple").ok();
    }

    let mut segments: Vec<String> = parsed
        .path_segments()
        .map(|segments| segments.map(ToString::to_string).collect())
        .unwrap_or_default();
    if let Some(last) = segments.last_mut() {
        if last == "legacy" {
            *last = "simple".to_string();
        }
    }
    if segments.is_empty() {
        segments.push("simple".to_string());
    }
    let mut simple = parsed;
    simple.set_path(&format!("{}/", segments.join("/")));
    simple.set_query(None);
    simple.set_fragment(None);
    IndexUrl::from_str(simple.as_str()).ok()
}
