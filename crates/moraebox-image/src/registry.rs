use std::{fs, str::FromStr};

use reqwest::{StatusCode, header};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    Cas, Digest, LayerCompression, RegistryReference, apply_layer, cas::CasError, layer::LayerError,
};

const ACCEPT_MANIFESTS: &str = concat!(
    "application/vnd.oci.image.index.v1+json,",
    "application/vnd.oci.image.manifest.v1+json,",
    "application/vnd.docker.distribution.manifest.list.v2+json,",
    "application/vnd.docker.distribution.manifest.v2+json"
);

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

#[derive(Debug, Clone)]
pub struct PulledImage {
    pub reference: RegistryReference,
    pub manifest_digest: Digest,
    pub manifest: ImageManifest,
    pub config_digest: Digest,
    pub layer_digests: Vec<Digest>,
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
        let temporary = parent.join(format!(
            ".{}.{}.tmp",
            self.manifest_digest.hex(),
            std::process::id()
        ));
        fs::create_dir(&temporary)?;
        let materialized = self
            .manifest
            .layers
            .iter()
            .zip(&self.layer_digests)
            .try_for_each(|(descriptor, digest)| {
                let file = fs::File::open(cas.blob_path(digest))?;
                let compression = LayerCompression::from_media_type(&descriptor.media_type)?;
                apply_layer(file, compression, &temporary)
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

#[derive(Debug, Clone)]
pub struct RegistryClient {
    client: reqwest::Client,
    credentials: Option<Credentials>,
}

impl RegistryClient {
    pub fn new(credentials: Option<Credentials>) -> Result<Self, RegistryError> {
        Ok(Self {
            client: reqwest::Client::builder()
                .user_agent(concat!("moraebox/", env!("CARGO_PKG_VERSION")))
                .build()?,
            credentials,
        })
    }

    pub async fn pull(
        &self,
        reference: RegistryReference,
        platform: &Platform,
        cas: &Cas,
    ) -> Result<PulledImage, RegistryError> {
        let mut authorization = None;
        let (mut manifest_bytes, mut manifest_digest) = self
            .get_manifest(&reference, reference.selector(), &mut authorization)
            .await?;

        let envelope: ManifestEnvelope = serde_json::from_slice(&manifest_bytes)?;
        let manifest = match envelope {
            ManifestEnvelope::Index(index) => {
                let descriptor = index
                    .manifests
                    .into_iter()
                    .find(|descriptor| platform_matches(descriptor.platform.as_ref(), platform))
                    .ok_or_else(|| RegistryError::PlatformNotFound(platform.clone()))?;
                let expected = Digest::from_str(&descriptor.digest)?;
                let (selected, actual) = self
                    .get_manifest(&reference, &descriptor.digest, &mut authorization)
                    .await?;
                if actual != expected {
                    return Err(RegistryError::ManifestDigestMismatch { expected, actual });
                }
                manifest_bytes = selected;
                manifest_digest = actual;
                serde_json::from_slice(&manifest_bytes)?
            }
            ManifestEnvelope::Manifest(manifest) => manifest,
        };

        cas.put_verified(&manifest_digest, &manifest_bytes).await?;
        if manifest.schema_version != 2 {
            return Err(RegistryError::UnsupportedSchema(manifest.schema_version));
        }
        let config_digest = Digest::from_str(&manifest.config.digest)?;
        self.fetch_blob(&reference, &config_digest, &mut authorization, cas)
            .await?;
        let mut layer_digests = Vec::with_capacity(manifest.layers.len());
        for layer in &manifest.layers {
            let digest = Digest::from_str(&layer.digest)?;
            self.fetch_blob(&reference, &digest, &mut authorization, cas)
                .await?;
            layer_digests.push(digest);
        }
        Ok(PulledImage {
            reference,
            manifest_digest,
            manifest,
            config_digest,
            layer_digests,
        })
    }

    async fn get_manifest(
        &self,
        reference: &RegistryReference,
        selector: &str,
        authorization: &mut Option<String>,
    ) -> Result<(Vec<u8>, Digest), RegistryError> {
        let url = format!(
            "https://{}/v2/{}/manifests/{selector}",
            reference.endpoint_registry(),
            reference.repository
        );
        let response = self
            .get_authenticated(&url, reference, authorization, Some(ACCEPT_MANIFESTS))
            .await?;
        let header_digest = response
            .headers()
            .get("docker-content-digest")
            .and_then(|value| value.to_str().ok())
            .map(Digest::from_str)
            .transpose()?;
        let bytes = response.bytes().await?.to_vec();
        let actual = Digest::from_bytes(&bytes);
        if let Some(expected) = header_digest
            && expected != actual
        {
            return Err(RegistryError::ManifestDigestMismatch { expected, actual });
        }
        Ok((bytes, actual))
    }

    async fn fetch_blob(
        &self,
        reference: &RegistryReference,
        digest: &Digest,
        authorization: &mut Option<String>,
        cas: &Cas,
    ) -> Result<(), RegistryError> {
        if tokio::fs::try_exists(cas.blob_path(digest)).await? {
            // Reading verifies an existing cache entry before trusting it.
            cas.read(digest).await?;
            return Ok(());
        }
        let url = format!(
            "https://{}/v2/{}/blobs/{digest}",
            reference.endpoint_registry(),
            reference.repository
        );
        let response = self
            .get_authenticated(&url, reference, authorization, None)
            .await?;
        let bytes = response.bytes().await?;
        cas.put_verified(digest, &bytes).await?;
        Ok(())
    }

    async fn get_authenticated(
        &self,
        url: &str,
        reference: &RegistryReference,
        authorization: &mut Option<String>,
        accept: Option<&str>,
    ) -> Result<reqwest::Response, RegistryError> {
        let mut request = self.client.get(url);
        if let Some(value) = accept {
            request = request.header(header::ACCEPT, value);
        }
        if let Some(token) = authorization.as_ref() {
            request = request.bearer_auth(token);
        }
        let mut response = request.send().await?;
        if response.status() == StatusCode::UNAUTHORIZED {
            let challenge = response
                .headers()
                .get(header::WWW_AUTHENTICATE)
                .and_then(|value| value.to_str().ok())
                .ok_or(RegistryError::MissingChallenge)?;
            let token = self.exchange_token(challenge, reference).await?;
            *authorization = Some(token.clone());
            let mut retry = self.client.get(url).bearer_auth(token);
            if let Some(value) = accept {
                retry = retry.header(header::ACCEPT, value);
            }
            response = retry.send().await?;
        }
        if !response.status().is_success() {
            return Err(RegistryError::HttpStatus {
                status: response.status(),
                url: url.into(),
            });
        }
        Ok(response)
    }

    async fn exchange_token(
        &self,
        challenge: &str,
        reference: &RegistryReference,
    ) -> Result<String, RegistryError> {
        let challenge = challenge
            .strip_prefix("Bearer ")
            .or_else(|| challenge.strip_prefix("bearer "))
            .ok_or_else(|| RegistryError::UnsupportedChallenge(challenge.into()))?;
        let values = parse_challenge(challenge);
        let realm = values
            .iter()
            .find(|(key, _)| key == "realm")
            .map(|(_, value)| value.as_str())
            .ok_or(RegistryError::MissingChallenge)?;
        let mut request = self.client.get(realm);
        let service = values
            .iter()
            .find(|(key, _)| key == "service")
            .map(|(_, value)| value.as_str());
        let scope = values
            .iter()
            .find(|(key, _)| key == "scope")
            .map_or_else(|| reference.scope(), |(_, value)| value.clone());
        let mut query = vec![("scope", scope.as_str())];
        if let Some(service) = service {
            query.push(("service", service));
        }
        request = request.query(&query);
        if let Some(credentials) = &self.credentials {
            request = request.basic_auth(&credentials.username, Some(&credentials.password));
        }
        let response = request.send().await?;
        if !response.status().is_success() {
            return Err(RegistryError::TokenStatus(response.status()));
        }
        let token: TokenResponse = response.json().await?;
        token
            .token
            .or(token.access_token)
            .ok_or(RegistryError::MissingToken)
    }
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

fn parse_challenge(value: &str) -> Vec<(String, String)> {
    value
        .split(',')
        .filter_map(|part| {
            let (key, value) = part.trim().split_once('=')?;
            Some((
                key.trim().to_ascii_lowercase(),
                value.trim().trim_matches('"').to_owned(),
            ))
        })
        .collect()
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ManifestEnvelope {
    Index(ImageIndex),
    Manifest(ImageManifest),
}

#[derive(Debug, Deserialize)]
struct ImageIndex {
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
    #[error("registry returned {status} for {url}")]
    HttpStatus { status: StatusCode, url: String },
    #[error("registry token exchange returned {0}")]
    TokenStatus(StatusCode),
    #[error("registry did not provide a bearer challenge")]
    MissingChallenge,
    #[error("unsupported registry authentication challenge: {0}")]
    UnsupportedChallenge(String),
    #[error("registry token response did not contain a token")]
    MissingToken,
    #[error("no manifest exists for platform {0:?}")]
    PlatformNotFound(Platform),
    #[error("manifest digest mismatch: expected {expected}, got {actual}")]
    ManifestDigestMismatch { expected: Digest, actual: Digest },
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
    use super::*;

    #[test]
    fn parses_bearer_challenge() {
        let values = parse_challenge(
            r#"realm="https://auth.example/token",service="registry.example",scope="repository:a/b:pull""#,
        );
        assert_eq!(
            values[0],
            ("realm".into(), "https://auth.example/token".into())
        );
        assert_eq!(values[2].1, "repository:a/b:pull");
    }

    #[test]
    fn maps_host_architecture_to_oci_name() {
        let platform = Platform::host_linux();
        assert_eq!(platform.os, "linux");
        assert!(["arm64", "amd64"].contains(&platform.architecture.as_str()));
    }
}
