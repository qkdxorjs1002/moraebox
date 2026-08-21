use std::{
    collections::BTreeSet,
    fs,
    future::Future,
    str::FromStr,
    sync::Arc,
    time::{Duration, SystemTime},
};

use futures_util::{StreamExt, TryStreamExt, stream};
use reqwest::{RequestBuilder, StatusCode, Url, header, redirect};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Mutex;

use crate::{
    Cas, Digest, LayerCompression, RegistryReference,
    cas::{CasError, PutStreamError},
    layer::{LayerBudget, LayerError, LayerLimits, apply_layer_with_budget},
    reference::Selector,
};

const ACCEPT_MANIFESTS: &str = concat!(
    "application/vnd.oci.image.index.v1+json,",
    "application/vnd.oci.image.manifest.v1+json,",
    "application/vnd.docker.distribution.manifest.list.v2+json,",
    "application/vnd.docker.distribution.manifest.v2+json"
);
const MAX_TOKEN_RESPONSE_BYTES: u64 = 1024 * 1024;
const MAX_CONCURRENT_LAYER_DOWNLOADS: usize = 4;
const MAX_REQUEST_ATTEMPTS: usize = 3;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const BASE_RETRY_DELAY: Duration = Duration::from_millis(100);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Platform {
    pub os: String,
    pub architecture: String,
    #[serde(default)]
    pub variant: Option<String>,
}

impl Platform {
    pub fn host_linux() -> Self {
        let architecture = match std::env::consts::ARCH {
            "aarch64" => "arm64",
            "x86_64" => "amd64",
            architecture => architecture,
        };
        Self {
            os: "linux".into(),
            architecture: architecture.into(),
            variant: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageManifest {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    #[serde(rename = "mediaType", default)]
    pub media_type: Option<String>,
    pub config: Descriptor,
    pub layers: Vec<Descriptor>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Descriptor {
    #[serde(rename = "mediaType")]
    pub media_type: String,
    pub digest: String,
    pub size: u64,
    #[serde(default)]
    pub platform: Option<Platform>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImagePullLimits {
    pub max_manifest_bytes: u64,
    pub max_config_bytes: u64,
    pub max_layer_bytes: u64,
    pub max_compressed_bytes: u64,
    pub max_layers: usize,
    pub extraction: LayerLimits,
}

impl Default for ImagePullLimits {
    fn default() -> Self {
        Self {
            max_manifest_bytes: 8 * 1024 * 1024,
            max_config_bytes: 16 * 1024 * 1024,
            max_layer_bytes: 1024 * 1024 * 1024,
            max_compressed_bytes: 4 * 1024 * 1024 * 1024,
            max_layers: 128,
            extraction: LayerLimits::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PulledImage {
    pub reference: RegistryReference,
    pub source_manifest_digest: Digest,
    pub manifest_digest: Digest,
    pub manifest: ImageManifest,
    pub config_digest: Digest,
    pub layer_digests: Vec<Digest>,
    pub limits: ImagePullLimits,
}

impl PulledImage {
    pub fn materialize_rootfs(
        &self,
        cas: &Cas,
        root: &std::path::Path,
    ) -> Result<(), RegistryError> {
        let complete = root.join(".moraebox-rootfs-complete");
        if complete.is_file() {
            return Ok(());
        }
        if root.exists() {
            return Err(RegistryError::RootfsExists(root.into()));
        }
        let parent = root
            .parent()
            .ok_or_else(|| RegistryError::RootfsExists(root.into()))?;
        fs::create_dir_all(parent)?;
        ensure_available_space(parent, self.limits.extraction.max_expanded_bytes)?;
        let temporary = parent.join(format!(
            ".{}.{}.tmp",
            self.manifest_digest.hex(),
            std::process::id()
        ));
        fs::create_dir(&temporary)?;
        let mut budget = LayerBudget::new(self.limits.extraction);
        let materialized = self
            .manifest
            .layers
            .iter()
            .zip(&self.layer_digests)
            .try_for_each(|(descriptor, digest)| {
                let file = fs::File::open(cas.blob_path(digest))?;
                let compression = LayerCompression::from_media_type(&descriptor.media_type)?;
                apply_layer_with_budget(file, compression, &temporary, &mut budget)
            });
        if let Err(error) = materialized {
            let _ = fs::remove_dir_all(&temporary);
            return Err(error.into());
        }
        fs::write(
            temporary.join(".moraebox-rootfs-complete"),
            self.manifest_digest.to_string(),
        )?;
        fs::rename(temporary, root)?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct RealmOrigin {
    host: String,
    port: u16,
}

impl RealmOrigin {
    fn from_url(url: &Url) -> Result<Self, RegistryError> {
        let host = url
            .host_str()
            .ok_or(RegistryError::InvalidTokenRealm)?
            .trim_end_matches('.')
            .to_ascii_lowercase();
        let port = url
            .port_or_known_default()
            .ok_or(RegistryError::InvalidTokenRealm)?;
        Ok(Self { host, port })
    }

    fn from_registry(reference: &RegistryReference) -> Result<Self, RegistryError> {
        let endpoint = Url::parse(&format!("https://{}/", reference.endpoint_registry()))
            .map_err(|_| RegistryError::InvalidRegistryEndpoint)?;
        Self::from_url(&endpoint).map_err(|_| RegistryError::InvalidRegistryEndpoint)
    }
}

#[derive(Debug, Clone, Default)]
struct Authorization {
    token: Arc<Mutex<Option<String>>>,
}

impl Authorization {
    async fn current(&self) -> Option<String> {
        self.token.lock().await.clone()
    }
}

#[derive(Debug, Clone)]
pub struct RegistryClient {
    client: reqwest::Client,
    token_client: reqwest::Client,
    credentials: Option<Credentials>,
    allowed_credential_realms: BTreeSet<RealmOrigin>,
    limits: ImagePullLimits,
}

impl RegistryClient {
    pub fn new(credentials: Option<Credentials>) -> Result<Self, RegistryError> {
        let user_agent = concat!("moraebox/", env!("CARGO_PKG_VERSION"));
        Ok(Self {
            client: reqwest::Client::builder()
                .user_agent(user_agent)
                .https_only(true)
                .connect_timeout(CONNECT_TIMEOUT)
                .read_timeout(READ_TIMEOUT)
                .timeout(REQUEST_TIMEOUT)
                .build()?,
            token_client: reqwest::Client::builder()
                .user_agent(user_agent)
                .https_only(true)
                .redirect(redirect::Policy::none())
                .connect_timeout(CONNECT_TIMEOUT)
                .read_timeout(READ_TIMEOUT)
                .timeout(REQUEST_TIMEOUT)
                .build()?,
            credentials,
            allowed_credential_realms: BTreeSet::new(),
            limits: ImagePullLimits::default(),
        })
    }

    #[must_use]
    pub fn with_limits(mut self, limits: ImagePullLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn with_allowed_credential_realm_origin(
        mut self,
        realm: &str,
    ) -> Result<Self, RegistryError> {
        let realm = validate_token_realm(realm)?;
        self.allowed_credential_realms
            .insert(RealmOrigin::from_url(&realm)?);
        Ok(self)
    }

    pub async fn pull(
        &self,
        reference: RegistryReference,
        platform: &Platform,
        cas: &Cas,
    ) -> Result<PulledImage, RegistryError> {
        let authorization = Authorization::default();
        let (source_manifest_bytes, source_manifest_digest) = self
            .get_manifest(&reference, reference.selector(), None, &authorization, cas)
            .await?;
        verify_reference_digest(&reference, &source_manifest_digest)?;

        let envelope: ManifestEnvelope = serde_json::from_slice(&source_manifest_bytes)?;
        let (manifest_digest, manifest) = match envelope {
            ManifestEnvelope::Index(index) => {
                if index.schema_version != 2 {
                    return Err(RegistryError::UnsupportedSchema(index.schema_version));
                }
                let descriptor = index
                    .manifests
                    .into_iter()
                    .find(|descriptor| platform_matches(descriptor.platform.as_ref(), platform))
                    .ok_or_else(|| RegistryError::PlatformNotFound(platform.clone()))?;
                verify_download_limit(
                    "selected manifest",
                    descriptor.size,
                    self.limits.max_manifest_bytes,
                )?;
                let expected = Digest::from_str(&descriptor.digest)?;
                let (selected, actual) = self
                    .get_manifest(
                        &reference,
                        &descriptor.digest,
                        Some(descriptor.size),
                        &authorization,
                        cas,
                    )
                    .await?;
                verify_manifest_descriptor(&descriptor, &expected, &actual, selected.len())?;
                let manifest = serde_json::from_slice(&selected)?;
                (actual, manifest)
            }
            ManifestEnvelope::Manifest(manifest) => (source_manifest_digest.clone(), manifest),
        };

        if manifest.schema_version != 2 {
            return Err(RegistryError::UnsupportedSchema(manifest.schema_version));
        }
        validate_manifest_limits(&manifest, &self.limits)?;
        let config_digest = self
            .fetch_blob(&reference, &manifest.config, &authorization, cas)
            .await?;
        let layer_reference = &reference;
        let layer_authorization = &authorization;
        let layer_digests = try_buffered_map(
            manifest.layers.clone(),
            MAX_CONCURRENT_LAYER_DOWNLOADS,
            |layer| async move {
                self.fetch_blob(layer_reference, &layer, layer_authorization, cas)
                    .await
            },
        )
        .await?;
        Ok(PulledImage {
            reference,
            source_manifest_digest,
            manifest_digest,
            manifest,
            config_digest,
            layer_digests,
            limits: self.limits,
        })
    }

    async fn get_manifest(
        &self,
        reference: &RegistryReference,
        selector: &str,
        expected_size: Option<u64>,
        authorization: &Authorization,
        cas: &Cas,
    ) -> Result<(Vec<u8>, Digest), RegistryError> {
        let url = format!(
            "https://{}/v2/{}/manifests/{selector}",
            reference.endpoint_registry(),
            reference.repository
        );
        let selector_digest = selector
            .starts_with("sha256:")
            .then(|| Digest::from_str(selector))
            .transpose()?;
        for attempt in 0..MAX_REQUEST_ATTEMPTS {
            let response = self
                .get_authenticated(&url, reference, authorization, Some(ACCEPT_MANIFESTS))
                .await?;
            if let Some(declared) = response.content_length() {
                verify_download_limit("manifest", declared, self.limits.max_manifest_bytes)?;
                if let Some(expected) = expected_size
                    && declared != expected
                {
                    return Err(RegistryError::DescriptorSizeMismatch {
                        digest: selector.into(),
                        expected,
                        actual: declared,
                    });
                }
            }
            let header_digest = response
                .headers()
                .get("docker-content-digest")
                .and_then(|value| value.to_str().ok())
                .map(Digest::from_str)
                .transpose()?;
            if let (Some(expected), Some(reported)) = (&selector_digest, &header_digest)
                && expected != reported
            {
                return Err(RegistryError::ManifestDigestMismatch {
                    expected: expected.clone(),
                    actual: reported.clone(),
                });
            }
            let expected_digest = selector_digest.as_ref().or(header_digest.as_ref());
            match cas
                .put_stream(
                    expected_digest,
                    expected_size,
                    self.limits.max_manifest_bytes,
                    response.bytes_stream(),
                )
                .await
            {
                Ok(digest) => {
                    let bytes = cas.read(&digest).await?;
                    return Ok((bytes, digest));
                }
                Err(PutStreamError::Source(error))
                    if retryable_transport_error(&error) && attempt + 1 < MAX_REQUEST_ATTEMPTS =>
                {
                    tokio::time::sleep(default_retry_delay(attempt)).await;
                }
                Err(error) => {
                    return Err(map_manifest_stream_error(
                        error,
                        selector,
                        expected_size,
                        self.limits.max_manifest_bytes,
                    ));
                }
            }
        }
        unreachable!("manifest download retry loop always returns")
    }

    async fn fetch_blob(
        &self,
        reference: &RegistryReference,
        descriptor: &Descriptor,
        authorization: &Authorization,
        cas: &Cas,
    ) -> Result<Digest, RegistryError> {
        let digest = Digest::from_str(&descriptor.digest)?;
        let blob_path = cas.blob_path(&digest);
        if tokio::fs::try_exists(&blob_path).await? {
            verify_descriptor_size_u64(descriptor, tokio::fs::metadata(&blob_path).await?.len())?;
            cas.verify(&digest).await?;
            return Ok(digest);
        }
        let url = format!(
            "https://{}/v2/{}/blobs/{digest}",
            reference.endpoint_registry(),
            reference.repository
        );
        for attempt in 0..MAX_REQUEST_ATTEMPTS {
            let response = self
                .get_authenticated(&url, reference, authorization, None)
                .await?;
            if let Some(declared) = response.content_length()
                && declared != descriptor.size
            {
                return Err(RegistryError::DescriptorSizeMismatch {
                    digest: descriptor.digest.clone(),
                    expected: descriptor.size,
                    actual: declared,
                });
            }
            match cas
                .put_stream(
                    Some(&digest),
                    Some(descriptor.size),
                    descriptor.size,
                    response.bytes_stream(),
                )
                .await
            {
                Ok(digest) => return Ok(digest),
                Err(PutStreamError::Source(error))
                    if retryable_transport_error(&error) && attempt + 1 < MAX_REQUEST_ATTEMPTS =>
                {
                    tokio::time::sleep(default_retry_delay(attempt)).await;
                }
                Err(error) => return Err(map_descriptor_stream_error(error, descriptor)),
            }
        }
        unreachable!("blob download retry loop always returns")
    }

    async fn get_authenticated(
        &self,
        url: &str,
        reference: &RegistryReference,
        authorization: &Authorization,
        accept: Option<&str>,
    ) -> Result<reqwest::Response, RegistryError> {
        let current = authorization.current().await;
        let mut response = self
            .send_retrying(self.authenticated_request(url, accept, current.as_deref()))
            .await?;
        if response.status() == StatusCode::UNAUTHORIZED {
            let challenge = response
                .headers()
                .get(header::WWW_AUTHENTICATE)
                .and_then(|value| value.to_str().ok())
                .ok_or(RegistryError::MissingChallenge)?
                .to_owned();
            let mut token = authorization.token.lock().await;
            if token.as_ref() == current.as_ref() {
                *token = Some(self.exchange_token(&challenge, reference).await?);
            }
            let refreshed = token.clone().ok_or(RegistryError::MissingToken)?;
            drop(token);
            response = self
                .send_retrying(self.authenticated_request(url, accept, Some(&refreshed)))
                .await?;
        }
        if !response.status().is_success() {
            return Err(RegistryError::HttpStatus {
                status: response.status(),
                url: url.into(),
            });
        }
        Ok(response)
    }

    fn authenticated_request(
        &self,
        url: &str,
        accept: Option<&str>,
        token: Option<&str>,
    ) -> RequestBuilder {
        let mut request = self.client.get(url);
        if let Some(value) = accept {
            request = request.header(header::ACCEPT, value);
        }
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        request
    }

    async fn send_retrying(
        &self,
        request: RequestBuilder,
    ) -> Result<reqwest::Response, RegistryError> {
        for attempt in 0..MAX_REQUEST_ATTEMPTS {
            let retry = request
                .try_clone()
                .ok_or(RegistryError::RequestNotCloneable)?;
            match retry.send().await {
                Ok(response)
                    if retryable_status(response.status())
                        && attempt + 1 < MAX_REQUEST_ATTEMPTS =>
                {
                    let delay = retry_delay(response.headers(), attempt, SystemTime::now());
                    drop(response);
                    tokio::time::sleep(delay).await;
                }
                Ok(response) => return Ok(response),
                Err(error)
                    if retryable_transport_error(&error) && attempt + 1 < MAX_REQUEST_ATTEMPTS =>
                {
                    tokio::time::sleep(default_retry_delay(attempt)).await;
                }
                Err(error) => return Err(error.into()),
            }
        }
        unreachable!("request retry loop always returns")
    }

    async fn exchange_token(
        &self,
        challenge: &str,
        reference: &RegistryReference,
    ) -> Result<String, RegistryError> {
        for attempt in 0..MAX_REQUEST_ATTEMPTS {
            let request = self.token_request(challenge, reference)?;
            let response = self.send_retrying(request).await?;
            if response.status().is_redirection() {
                return Err(RegistryError::TokenRedirectDisallowed(response.status()));
            }
            if !response.status().is_success() {
                return Err(RegistryError::TokenStatus(response.status()));
            }
            match read_bounded_response(
                response,
                "registry token response",
                MAX_TOKEN_RESPONSE_BYTES,
            )
            .await
            {
                Ok(bytes) => {
                    let token: TokenResponse = serde_json::from_slice(&bytes)?;
                    return token
                        .token
                        .or(token.access_token)
                        .filter(|token| !token.is_empty())
                        .ok_or(RegistryError::MissingToken);
                }
                Err(RegistryError::Http(error))
                    if retryable_transport_error(&error) && attempt + 1 < MAX_REQUEST_ATTEMPTS =>
                {
                    tokio::time::sleep(default_retry_delay(attempt)).await;
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("token response retry loop always returns")
    }

    fn token_request(
        &self,
        challenge: &str,
        reference: &RegistryReference,
    ) -> Result<reqwest::RequestBuilder, RegistryError> {
        let challenge = parse_bearer_challenge(challenge)?;
        let realm = validate_token_realm(&challenge.realm)?;
        let mut request = self.token_client.get(realm.clone());
        let scope = challenge.scope.unwrap_or_else(|| reference.scope());
        let mut query = vec![("scope", scope.as_str())];
        if let Some(service) = challenge.service.as_deref() {
            query.push(("service", service));
        }
        request = request.query(&query);
        if let Some(credentials) = &self.credentials {
            if !credential_realm_is_trusted(reference, &realm, &self.allowed_credential_realms)? {
                let origin = RealmOrigin::from_url(&realm)?;
                return Err(RegistryError::UntrustedCredentialRealm {
                    host: origin.host,
                    port: origin.port,
                });
            }
            request = request.basic_auth(&credentials.username, Some(&credentials.password));
        }
        Ok(request)
    }
}

async fn try_buffered_map<I, F, Fut, T, E>(
    items: I,
    maximum_concurrency: usize,
    map: F,
) -> Result<Vec<T>, E>
where
    I: IntoIterator,
    F: FnMut(I::Item) -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    stream::iter(items)
        .map(map)
        .buffered(maximum_concurrency)
        .try_collect()
        .await
}

fn map_manifest_stream_error(
    error: PutStreamError<reqwest::Error>,
    selector: &str,
    expected_size: Option<u64>,
    maximum_size: u64,
) -> RegistryError {
    match error {
        PutStreamError::Source(error) => RegistryError::Http(error),
        PutStreamError::Cas(CasError::DigestMismatch { expected, actual }) => {
            RegistryError::ManifestDigestMismatch { expected, actual }
        }
        PutStreamError::Cas(error) => RegistryError::Cas(error),
        PutStreamError::SizeExceeded { actual } | PutStreamError::SizeMismatch { actual, .. }
            if expected_size.is_some() =>
        {
            RegistryError::DescriptorSizeMismatch {
                digest: selector.into(),
                expected: expected_size.expect("guarded by is_some"),
                actual,
            }
        }
        PutStreamError::SizeExceeded { actual } => RegistryError::DownloadLimitExceeded {
            resource: "manifest",
            actual,
            maximum: maximum_size,
        },
        PutStreamError::SizeMismatch { expected, actual } => {
            RegistryError::DescriptorSizeMismatch {
                digest: selector.into(),
                expected,
                actual,
            }
        }
    }
}

fn map_descriptor_stream_error(
    error: PutStreamError<reqwest::Error>,
    descriptor: &Descriptor,
) -> RegistryError {
    match error {
        PutStreamError::Source(error) => RegistryError::Http(error),
        PutStreamError::Cas(error) => RegistryError::Cas(error),
        PutStreamError::SizeExceeded { actual } | PutStreamError::SizeMismatch { actual, .. } => {
            RegistryError::DescriptorSizeMismatch {
                digest: descriptor.digest.clone(),
                expected: descriptor.size,
                actual,
            }
        }
    }
}

fn retryable_transport_error(error: &reqwest::Error) -> bool {
    error.is_connect() || error.is_timeout() || error.is_body() || error.is_request()
}

fn retryable_status(status: StatusCode) -> bool {
    status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

fn retry_delay(headers: &header::HeaderMap, attempt: usize, now: SystemTime) -> Duration {
    headers
        .get(header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| parse_retry_after(value, now))
        .unwrap_or_else(|| default_retry_delay(attempt))
        .min(MAX_RETRY_DELAY)
}

fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    value
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
        .or_else(|| {
            let retry_at = httpdate::parse_http_date(value).ok()?;
            Some(retry_at.duration_since(now).unwrap_or(Duration::ZERO))
        })
}

fn default_retry_delay(attempt: usize) -> Duration {
    let multiplier = 1_u32.checked_shl(u32::try_from(attempt).unwrap_or(u32::MAX));
    multiplier
        .map_or(MAX_RETRY_DELAY, |value| {
            BASE_RETRY_DELAY.saturating_mul(value)
        })
        .min(MAX_RETRY_DELAY)
}

async fn read_bounded_response(
    response: reqwest::Response,
    resource: &'static str,
    maximum: u64,
) -> Result<Vec<u8>, RegistryError> {
    if let Some(declared) = response.content_length() {
        verify_download_limit(resource, declared, maximum)?;
    }
    let mut bytes = Vec::new();
    let mut length = 0_u64;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        length = checked_download_length(resource, length, chunk.len(), maximum)?;
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn platform_matches(actual: Option<&Platform>, expected: &Platform) -> bool {
    actual.is_some_and(|actual| {
        actual.os == expected.os
            && actual.architecture == expected.architecture
            && expected
                .variant
                .as_ref()
                .is_none_or(|variant| actual.variant.as_ref() == Some(variant))
    })
}

fn verify_reference_digest(
    reference: &RegistryReference,
    actual: &Digest,
) -> Result<(), RegistryError> {
    if let Selector::Digest(expected) = &reference.selector {
        let expected = Digest::from_str(expected)?;
        if expected != *actual {
            return Err(RegistryError::ManifestDigestMismatch {
                expected,
                actual: actual.clone(),
            });
        }
    }
    Ok(())
}

fn verify_manifest_descriptor(
    descriptor: &Descriptor,
    expected: &Digest,
    actual: &Digest,
    actual_size: usize,
) -> Result<(), RegistryError> {
    if expected != actual {
        return Err(RegistryError::ManifestDigestMismatch {
            expected: expected.clone(),
            actual: actual.clone(),
        });
    }
    verify_descriptor_size(descriptor, actual_size)
}

fn validate_manifest_limits(
    manifest: &ImageManifest,
    limits: &ImagePullLimits,
) -> Result<(), RegistryError> {
    if manifest.layers.len() > limits.max_layers {
        return Err(RegistryError::LayerCountLimitExceeded {
            actual: manifest.layers.len(),
            maximum: limits.max_layers,
        });
    }
    verify_download_limit(
        "configuration blob",
        manifest.config.size,
        limits.max_config_bytes,
    )?;
    let mut compressed = manifest.config.size;
    for layer in &manifest.layers {
        verify_download_limit("layer blob", layer.size, limits.max_layer_bytes)?;
        compressed = compressed.saturating_add(layer.size);
        if compressed > limits.max_compressed_bytes {
            return Err(RegistryError::CompressedSizeLimitExceeded {
                attempted: compressed,
                maximum: limits.max_compressed_bytes,
            });
        }
    }
    if compressed > limits.max_compressed_bytes {
        return Err(RegistryError::CompressedSizeLimitExceeded {
            attempted: compressed,
            maximum: limits.max_compressed_bytes,
        });
    }
    Ok(())
}

fn verify_download_limit(
    resource: &'static str,
    actual: u64,
    maximum: u64,
) -> Result<(), RegistryError> {
    if actual > maximum {
        return Err(RegistryError::DownloadLimitExceeded {
            resource,
            actual,
            maximum,
        });
    }
    Ok(())
}

fn checked_download_length(
    resource: &'static str,
    current: u64,
    chunk: usize,
    maximum: u64,
) -> Result<u64, RegistryError> {
    let actual = current.saturating_add(u64::try_from(chunk).unwrap_or(u64::MAX));
    verify_download_limit(resource, actual, maximum)?;
    Ok(actual)
}

fn ensure_available_space(path: &std::path::Path, required: u64) -> Result<(), RegistryError> {
    let available = fs2::available_space(path)?;
    verify_available_space(path, required, available)
}

fn verify_available_space(
    path: &std::path::Path,
    required: u64,
    available: u64,
) -> Result<(), RegistryError> {
    if available < required {
        return Err(RegistryError::InsufficientSpace {
            path: path.to_path_buf(),
            required,
            available,
        });
    }
    Ok(())
}

fn verify_descriptor_size(
    descriptor: &Descriptor,
    actual_size: usize,
) -> Result<(), RegistryError> {
    let actual = u64::try_from(actual_size).unwrap_or(u64::MAX);
    verify_descriptor_size_u64(descriptor, actual)
}

fn verify_descriptor_size_u64(descriptor: &Descriptor, actual: u64) -> Result<(), RegistryError> {
    if descriptor.size != actual {
        return Err(RegistryError::DescriptorSizeMismatch {
            digest: descriptor.digest.clone(),
            expected: descriptor.size,
            actual,
        });
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct BearerChallenge {
    realm: String,
    service: Option<String>,
    scope: Option<String>,
}

fn parse_bearer_challenge(value: &str) -> Result<BearerChallenge, RegistryError> {
    let value = value.trim();
    let scheme_end = value
        .find(char::is_whitespace)
        .ok_or_else(|| RegistryError::UnsupportedChallenge(value.into()))?;
    if !value[..scheme_end].eq_ignore_ascii_case("bearer") {
        return Err(RegistryError::UnsupportedChallenge(value.into()));
    }
    let parameters = parse_auth_parameters(value[scheme_end..].trim())?;
    let realm = unique_parameter(&parameters, "realm")?.ok_or(RegistryError::MissingChallenge)?;
    Ok(BearerChallenge {
        realm,
        service: unique_parameter(&parameters, "service")?,
        scope: unique_parameter(&parameters, "scope")?,
    })
}

fn parse_auth_parameters(value: &str) -> Result<Vec<(String, String)>, RegistryError> {
    let bytes = value.as_bytes();
    let mut offset = 0;
    let mut parameters = Vec::new();
    while offset < bytes.len() {
        while offset < bytes.len() && (bytes[offset].is_ascii_whitespace() || bytes[offset] == b',')
        {
            offset += 1;
        }
        if offset == bytes.len() {
            break;
        }

        let key_start = offset;
        while offset < bytes.len() && is_auth_token_byte(bytes[offset]) {
            offset += 1;
        }
        if key_start == offset {
            return Err(RegistryError::InvalidChallenge);
        }
        let key = value[key_start..offset].to_ascii_lowercase();
        while offset < bytes.len() && bytes[offset].is_ascii_whitespace() {
            offset += 1;
        }
        if bytes.get(offset) != Some(&b'=') {
            return Err(RegistryError::InvalidChallenge);
        }
        offset += 1;
        while offset < bytes.len() && bytes[offset].is_ascii_whitespace() {
            offset += 1;
        }

        let parsed = if bytes.get(offset) == Some(&b'"') {
            offset += 1;
            let mut decoded = Vec::new();
            let mut closed = false;
            while offset < bytes.len() {
                match bytes[offset] {
                    b'\\' => {
                        offset += 1;
                        let escaped = bytes.get(offset).ok_or(RegistryError::InvalidChallenge)?;
                        decoded.push(*escaped);
                        offset += 1;
                    }
                    b'"' => {
                        offset += 1;
                        closed = true;
                        break;
                    }
                    b'\r' | b'\n' => return Err(RegistryError::InvalidChallenge),
                    byte => {
                        decoded.push(byte);
                        offset += 1;
                    }
                }
            }
            if !closed {
                return Err(RegistryError::InvalidChallenge);
            }
            while offset < bytes.len() && bytes[offset].is_ascii_whitespace() {
                offset += 1;
            }
            if offset < bytes.len() && bytes[offset] != b',' {
                return Err(RegistryError::InvalidChallenge);
            }
            String::from_utf8(decoded).map_err(|_| RegistryError::InvalidChallenge)?
        } else {
            let value_start = offset;
            while offset < bytes.len() && bytes[offset] != b',' {
                offset += 1;
            }
            let parsed = value[value_start..offset].trim();
            if parsed.is_empty() {
                return Err(RegistryError::InvalidChallenge);
            }
            parsed.to_owned()
        };
        parameters.push((key, parsed));
        if offset < bytes.len() {
            offset += 1;
        }
    }
    Ok(parameters)
}

fn is_auth_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

fn unique_parameter(
    parameters: &[(String, String)],
    key: &str,
) -> Result<Option<String>, RegistryError> {
    let mut matches = parameters
        .iter()
        .filter(|(candidate, _)| candidate == key)
        .map(|(_, value)| value.clone());
    let first = matches.next();
    if matches.next().is_some() {
        return Err(RegistryError::InvalidChallenge);
    }
    Ok(first)
}

fn validate_token_realm(value: &str) -> Result<Url, RegistryError> {
    let url = Url::parse(value).map_err(|_| RegistryError::InvalidTokenRealm)?;
    if url.scheme() != "https" {
        return Err(RegistryError::InsecureTokenRealm);
    }
    if url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(RegistryError::InvalidTokenRealm);
    }
    Ok(url)
}

fn credential_realm_is_trusted(
    reference: &RegistryReference,
    realm: &Url,
    explicitly_allowed: &BTreeSet<RealmOrigin>,
) -> Result<bool, RegistryError> {
    let registry = RealmOrigin::from_registry(reference)?;
    let realm = RealmOrigin::from_url(realm)?;
    let same_or_child = registry.port == realm.port
        && (registry.host == realm.host
            || realm
                .host
                .strip_suffix(&registry.host)
                .is_some_and(|prefix| prefix.ends_with('.') && prefix.len() > 1));
    let docker_hub = registry.host == "registry-1.docker.io"
        && registry.port == 443
        && realm.host == "auth.docker.io"
        && realm.port == 443;
    Ok(same_or_child || docker_hub || explicitly_allowed.contains(&realm))
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ManifestEnvelope {
    Index(ImageIndex),
    Manifest(ImageManifest),
}

#[derive(Debug, Deserialize)]
struct ImageIndex {
    #[serde(rename = "schemaVersion")]
    schema_version: u32,
    manifests: Vec<Descriptor>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    token: Option<String>,
    access_token: Option<String>,
}

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("registry HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("registry request cannot be cloned for bounded retry")]
    RequestNotCloneable,
    #[error("registry returned {status} for {url}")]
    HttpStatus { status: StatusCode, url: String },
    #[error("registry token exchange returned {0}")]
    TokenStatus(StatusCode),
    #[error("registry token endpoint redirect is not allowed: {0}")]
    TokenRedirectDisallowed(StatusCode),
    #[error("registry did not provide a bearer challenge")]
    MissingChallenge,
    #[error("unsupported registry authentication challenge: {0}")]
    UnsupportedChallenge(String),
    #[error("registry bearer challenge is malformed or ambiguous")]
    InvalidChallenge,
    #[error("registry bearer token realm is not a valid absolute URL")]
    InvalidTokenRealm,
    #[error("registry bearer token realm must use HTTPS")]
    InsecureTokenRealm,
    #[error("registry endpoint cannot be represented as a secure origin")]
    InvalidRegistryEndpoint,
    #[error("refusing to send registry credentials to untrusted token realm {host}:{port}")]
    UntrustedCredentialRealm { host: String, port: u16 },
    #[error("registry token response did not contain a token")]
    MissingToken,
    #[error("no manifest exists for platform {0:?}")]
    PlatformNotFound(Platform),
    #[error("manifest digest mismatch: expected {expected}, got {actual}")]
    ManifestDigestMismatch { expected: Digest, actual: Digest },
    #[error("OCI descriptor {digest} size mismatch: expected {expected} bytes, got {actual}")]
    DescriptorSizeMismatch {
        digest: String,
        expected: u64,
        actual: u64,
    },
    #[error("{resource} is {actual} bytes, exceeding the {maximum}-byte download limit")]
    DownloadLimitExceeded {
        resource: &'static str,
        actual: u64,
        maximum: u64,
    },
    #[error("OCI manifest has {actual} layers, exceeding the limit of {maximum}")]
    LayerCountLimitExceeded { actual: usize, maximum: usize },
    #[error("OCI blobs total {attempted} compressed bytes, exceeding the {maximum}-byte limit")]
    CompressedSizeLimitExceeded { attempted: u64, maximum: u64 },
    #[error(
        "insufficient space at {}: {available} bytes available, {required} bytes required",
        path.display()
    )]
    InsufficientSpace {
        path: std::path::PathBuf,
        required: u64,
        available: u64,
    },
    #[error("unsupported OCI schema version {0}")]
    UnsupportedSchema(u32),
    #[error("refusing to overwrite incomplete rootfs: {}", .0.display())]
    RootfsExists(std::path::PathBuf),
    #[error("invalid registry JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Cas(#[from] CasError),
    #[error(transparent)]
    Layer(#[from] LayerError),
    #[error("registry cache I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Cursor, Write},
        path::Path,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use flate2::{Compression, write::GzEncoder};

    use super::*;

    fn descriptor(size: u64) -> Descriptor {
        Descriptor {
            media_type: "application/vnd.oci.image.layer.v1.tar+gzip".into(),
            digest: Digest::from_bytes(&size.to_le_bytes()).to_string(),
            size,
            platform: None,
        }
    }

    fn limits_for_tests() -> ImagePullLimits {
        ImagePullLimits {
            max_manifest_bytes: 100,
            max_config_bytes: 10,
            max_layer_bytes: 10,
            max_compressed_bytes: 20,
            max_layers: 2,
            extraction: LayerLimits::default(),
        }
    }

    fn gzip_tar(path: &str, contents: &[u8]) -> Vec<u8> {
        let mut archive = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut archive);
            let mut header = tar::Header::new_gnu();
            header.set_size(u64::try_from(contents.len()).unwrap());
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, path, Cursor::new(contents))
                .unwrap();
            builder.finish().unwrap();
        }
        let mut encoder = GzEncoder::new(Vec::new(), Compression::best());
        encoder.write_all(&archive).unwrap();
        encoder.finish().unwrap()
    }

    fn credentials() -> Credentials {
        Credentials {
            username: "user".into(),
            password: "secret".into(),
        }
    }

    fn bearer_challenge(realm: &str) -> String {
        format!(r#"Bearer realm="{realm}",service="registry.example",scope="repository:a/b:pull""#)
    }

    #[tokio::test]
    async fn buffered_downloads_bound_concurrency_and_preserve_descriptor_order() {
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let result = try_buffered_map(0_u64..8, 3, |index| {
            let active = Arc::clone(&active);
            let maximum = Arc::clone(&maximum);
            async move {
                let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                maximum.fetch_max(current, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis((8 - index) * 2)).await;
                active.fetch_sub(1, Ordering::SeqCst);
                Ok::<_, ()>(index)
            }
        })
        .await
        .unwrap();

        assert_eq!(result, (0_u64..8).collect::<Vec<_>>());
        assert_eq!(maximum.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn retry_policy_is_bounded_and_honors_delta_and_date_retry_after() {
        assert!(retryable_status(StatusCode::REQUEST_TIMEOUT));
        assert!(retryable_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(retryable_status(StatusCode::SERVICE_UNAVAILABLE));
        assert!(!retryable_status(StatusCode::UNAUTHORIZED));
        assert_eq!(default_retry_delay(0), Duration::from_millis(100));
        assert_eq!(default_retry_delay(1), Duration::from_millis(200));

        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert_eq!(parse_retry_after("3", now), Some(Duration::from_secs(3)));
        let date = httpdate::fmt_http_date(now + Duration::from_secs(4));
        assert_eq!(parse_retry_after(&date, now), Some(Duration::from_secs(4)));

        let mut headers = header::HeaderMap::new();
        headers.insert(header::RETRY_AFTER, header::HeaderValue::from_static("60"));
        assert_eq!(retry_delay(&headers, 0, now), MAX_RETRY_DELAY);
    }

    #[test]
    fn parses_bearer_challenge() {
        let challenge = parse_bearer_challenge(
            r#"bEaReR realm="https://auth.example/token?labels=a,b",service="registry.example",scope="repository:a/b:pull,push",note="quoted\"value""#,
        )
        .unwrap();
        assert_eq!(challenge.realm, "https://auth.example/token?labels=a,b");
        assert_eq!(challenge.service.as_deref(), Some("registry.example"));
        assert_eq!(challenge.scope.as_deref(), Some("repository:a/b:pull,push"));
    }

    #[test]
    fn rejects_malformed_or_ambiguous_bearer_challenges() {
        assert!(matches!(
            parse_bearer_challenge(
                r#"Bearer realm="https://one.example/token",realm="https://two.example/token""#
            ),
            Err(RegistryError::InvalidChallenge)
        ));
        assert!(matches!(
            parse_bearer_challenge(r#"Bearer realm="https://auth.example/token"#),
            Err(RegistryError::InvalidChallenge)
        ));
        assert!(matches!(
            parse_bearer_challenge("Basic realm=registry"),
            Err(RegistryError::UnsupportedChallenge(_))
        ));
    }

    #[test]
    fn token_realm_requires_a_clean_https_url() {
        validate_token_realm("https://auth.example/token?client=moraebox").unwrap();
        assert!(matches!(
            validate_token_realm("http://auth.example/token"),
            Err(RegistryError::InsecureTokenRealm)
        ));
        assert!(matches!(
            validate_token_realm("https://user:password@auth.example/token"),
            Err(RegistryError::InvalidTokenRealm)
        ));
        assert!(matches!(
            validate_token_realm("https://auth.example/token#fragment"),
            Err(RegistryError::InvalidTokenRealm)
        ));
    }

    #[test]
    fn credentials_are_added_only_for_trusted_realm_origins() {
        let reference: RegistryReference = "registry.example/a/b:latest".parse().unwrap();
        let client = RegistryClient::new(Some(credentials())).unwrap();
        for realm in [
            "https://registry.example/token",
            "https://auth.registry.example/token",
        ] {
            let request = client
                .token_request(&bearer_challenge(realm), &reference)
                .unwrap()
                .build()
                .unwrap();
            assert!(request.headers().contains_key(header::AUTHORIZATION));
        }

        assert!(matches!(
            client.token_request(&bearer_challenge("https://auth.example/token"), &reference),
            Err(RegistryError::UntrustedCredentialRealm { .. })
        ));
        assert!(matches!(
            client.token_request(
                &bearer_challenge("https://registry.example:444/token"),
                &reference
            ),
            Err(RegistryError::UntrustedCredentialRealm { .. })
        ));
    }

    #[test]
    fn docker_hub_and_explicit_cross_host_realms_can_receive_credentials() {
        let docker: RegistryReference = "python:3.12".parse().unwrap();
        let client = RegistryClient::new(Some(credentials())).unwrap();
        let request = client
            .token_request(&bearer_challenge("https://auth.docker.io/token"), &docker)
            .unwrap()
            .build()
            .unwrap();
        assert!(request.headers().contains_key(header::AUTHORIZATION));

        let private: RegistryReference = "registry.example/a/b:latest".parse().unwrap();
        let client = RegistryClient::new(Some(credentials()))
            .unwrap()
            .with_allowed_credential_realm_origin("https://identity.example/token")
            .unwrap();
        let request = client
            .token_request(
                &bearer_challenge("https://identity.example/registry-token"),
                &private,
            )
            .unwrap()
            .build()
            .unwrap();
        assert!(request.headers().contains_key(header::AUTHORIZATION));
    }

    #[test]
    fn anonymous_cross_host_token_requests_remain_supported() {
        let reference: RegistryReference = "registry.example/a/b:latest".parse().unwrap();
        let client = RegistryClient::new(None).unwrap();
        let request = client
            .token_request(
                &bearer_challenge("https://identity.unrelated/token"),
                &reference,
            )
            .unwrap()
            .build()
            .unwrap();

        assert!(!request.headers().contains_key(header::AUTHORIZATION));
    }

    #[test]
    fn maps_host_architecture_to_oci_name() {
        let platform = Platform::host_linux();
        assert_eq!(platform.os, "linux");
        assert!(["arm64", "amd64"].contains(&platform.architecture.as_str()));
    }

    #[test]
    fn digest_selector_must_match_the_top_level_manifest() {
        let expected = Digest::from_bytes(b"expected");
        let reference: RegistryReference = format!("example.com/a/b@{expected}").parse().unwrap();
        let actual = Digest::from_bytes(b"different");

        assert!(matches!(
            verify_reference_digest(&reference, &actual),
            Err(RegistryError::ManifestDigestMismatch {
                expected: reported_expected,
                actual: reported_actual,
            }) if reported_expected == expected && reported_actual == actual
        ));
    }

    #[test]
    fn selected_manifest_descriptor_checks_digest_and_exact_size() {
        let bytes = br#"{"schemaVersion":2}"#;
        let expected = Digest::from_bytes(bytes);
        let descriptor = Descriptor {
            media_type: "application/vnd.oci.image.manifest.v1+json".into(),
            digest: expected.to_string(),
            size: u64::try_from(bytes.len()).unwrap(),
            platform: Some(Platform {
                os: "linux".into(),
                architecture: "arm64".into(),
                variant: None,
            }),
        };

        verify_manifest_descriptor(&descriptor, &expected, &expected, bytes.len()).unwrap();
        assert!(matches!(
            verify_manifest_descriptor(&descriptor, &expected, &expected, bytes.len() + 1),
            Err(RegistryError::DescriptorSizeMismatch {
                expected: descriptor_size,
                actual,
                ..
            }) if descriptor_size == descriptor.size && actual == descriptor.size + 1
        ));
        let actual = Digest::from_bytes(b"different");
        assert!(matches!(
            verify_manifest_descriptor(&descriptor, &expected, &actual, bytes.len()),
            Err(RegistryError::ManifestDigestMismatch { .. })
        ));
    }

    #[test]
    fn rejects_excessive_layer_count_before_download() {
        let manifest = ImageManifest {
            schema_version: 2,
            media_type: None,
            config: descriptor(1),
            layers: vec![descriptor(1), descriptor(1), descriptor(1)],
        };

        assert!(matches!(
            validate_manifest_limits(&manifest, &limits_for_tests()),
            Err(RegistryError::LayerCountLimitExceeded {
                actual: 3,
                maximum: 2
            })
        ));
    }

    #[test]
    fn rejects_excessive_individual_and_aggregate_compressed_sizes() {
        let limits = limits_for_tests();
        let oversized_layer = ImageManifest {
            schema_version: 2,
            media_type: None,
            config: descriptor(1),
            layers: vec![descriptor(11)],
        };
        assert!(matches!(
            validate_manifest_limits(&oversized_layer, &limits),
            Err(RegistryError::DownloadLimitExceeded {
                resource: "layer blob",
                actual: 11,
                maximum: 10
            })
        ));

        let aggregate = ImageManifest {
            schema_version: 2,
            media_type: None,
            config: descriptor(5),
            layers: vec![descriptor(8), descriptor(8)],
        };
        assert!(matches!(
            validate_manifest_limits(&aggregate, &limits),
            Err(RegistryError::CompressedSizeLimitExceeded {
                attempted: 21,
                maximum: 20
            })
        ));
    }

    #[test]
    fn bounded_body_length_rejects_the_first_oversized_chunk() {
        assert_eq!(checked_download_length("manifest", 4, 5, 9).unwrap(), 9);
        assert!(matches!(
            checked_download_length("manifest", 9, 1, 9),
            Err(RegistryError::DownloadLimitExceeded {
                resource: "manifest",
                actual: 10,
                maximum: 9
            })
        ));
    }

    #[test]
    fn free_space_preflight_reports_required_and_available_bytes() {
        let path = Path::new("/cache");
        verify_available_space(path, 10, 10).unwrap();
        assert!(matches!(
            verify_available_space(path, 10, 9),
            Err(RegistryError::InsufficientSpace {
                required: 10,
                available: 9,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn extraction_quota_failure_removes_the_staging_rootfs() {
        let directory = tempfile::tempdir().unwrap();
        let cas = Cas::new(directory.path().join("cas"));
        let compressed = gzip_tar("large", &[0; 4096]);
        let layer_digest = Digest::from_bytes(&compressed);
        cas.put_verified(&layer_digest, &compressed).await.unwrap();
        let manifest_digest = Digest::from_bytes(b"manifest");
        let layer = Descriptor {
            media_type: "application/vnd.oci.image.layer.v1.tar+gzip".into(),
            digest: layer_digest.to_string(),
            size: u64::try_from(compressed.len()).unwrap(),
            platform: None,
        };
        let image = PulledImage {
            reference: "example.com/a/b:latest".parse().unwrap(),
            source_manifest_digest: manifest_digest.clone(),
            manifest_digest: manifest_digest.clone(),
            manifest: ImageManifest {
                schema_version: 2,
                media_type: None,
                config: descriptor(0),
                layers: vec![layer],
            },
            config_digest: Digest::from_bytes(&[]),
            layer_digests: vec![layer_digest],
            limits: ImagePullLimits {
                extraction: LayerLimits {
                    max_expanded_bytes: 1024,
                    max_file_bytes: 1024,
                    max_entries: 10,
                },
                ..limits_for_tests()
            },
        };
        let rootfs = directory.path().join("rootfs");
        let staging = directory.path().join(format!(
            ".{}.{}.tmp",
            manifest_digest.hex(),
            std::process::id()
        ));

        let error = image.materialize_rootfs(&cas, &rootfs).unwrap_err();

        assert!(matches!(
            error,
            RegistryError::Layer(LayerError::FileSizeLimitExceeded { .. })
        ));
        assert!(!rootfs.exists());
        assert!(!staging.exists());
    }
}
