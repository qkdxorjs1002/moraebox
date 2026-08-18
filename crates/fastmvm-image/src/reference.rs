use std::{fmt, path::PathBuf, str::FromStr};

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageReference {
    Registry(RegistryReference),
    OciLayout(PathBuf),
    DockerArchive(PathBuf),
}

impl FromStr for ImageReference {
    type Err = ReferenceError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if let Some(path) = value.strip_prefix("oci-layout:") {
            return non_empty_path(path, "OCI layout").map(Self::OciLayout);
        }
        if let Some(path) = value.strip_prefix("docker-archive:") {
            return non_empty_path(path, "Docker archive").map(Self::DockerArchive);
        }
        value.parse().map(Self::Registry)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryReference {
    pub registry: String,
    pub repository: String,
    pub selector: Selector,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selector {
    Tag(String),
    Digest(String),
}

impl RegistryReference {
    pub fn endpoint_registry(&self) -> &str {
        if self.registry == "docker.io" {
            "registry-1.docker.io"
        } else {
            &self.registry
        }
    }

    pub fn selector(&self) -> &str {
        match &self.selector {
            Selector::Tag(tag) | Selector::Digest(tag) => tag,
        }
    }

    pub fn scope(&self) -> String {
        format!("repository:{}:pull", self.repository)
    }
}

impl FromStr for RegistryReference {
    type Err = ReferenceError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        if value.is_empty() || value.contains(char::is_whitespace) || value.contains("//") {
            return Err(ReferenceError::Invalid(value.to_owned()));
        }

        let (name, selector) = if let Some((name, suffix)) = value.rsplit_once('@') {
            if suffix.starts_with("sha256:") {
                (name, Selector::Digest(validate_digest(suffix)?))
            } else {
                // Compatibility alias requested by the public CLI: `alpine@latest`.
                (name, Selector::Tag(validate_component(suffix, "tag")?))
            }
        } else {
            let last_slash = value.rfind('/');
            let last_colon = value.rfind(':');
            if last_colon.is_some_and(|colon| last_slash.is_none_or(|slash| colon > slash)) {
                let colon = last_colon.expect("checked as some");
                (
                    &value[..colon],
                    Selector::Tag(validate_component(&value[colon + 1..], "tag")?),
                )
            } else {
                (value, Selector::Tag("latest".into()))
            }
        };
        if name.is_empty() {
            return Err(ReferenceError::Invalid(value.to_owned()));
        }

        let parts = name.split('/').collect::<Vec<_>>();
        if parts.iter().any(|part| part.is_empty()) {
            return Err(ReferenceError::Invalid(value.to_owned()));
        }
        let first_is_registry = parts.len() > 1
            && (parts[0].contains('.') || parts[0].contains(':') || parts[0] == "localhost");
        let (registry, repository_parts) = if first_is_registry {
            (parts[0].to_ascii_lowercase(), &parts[1..])
        } else {
            ("docker.io".to_owned(), parts.as_slice())
        };
        let mut repository = repository_parts.join("/").to_ascii_lowercase();
        if registry == "docker.io" && !repository.contains('/') {
            repository = format!("library/{repository}");
        }
        validate_repository(&repository)?;
        Ok(Self {
            registry,
            repository,
            selector,
        })
    }
}

impl fmt::Display for RegistryReference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.registry, self.repository)?;
        match &self.selector {
            Selector::Tag(tag) => write!(formatter, ":{tag}"),
            Selector::Digest(digest) => write!(formatter, "@{digest}"),
        }
    }
}

fn validate_repository(repository: &str) -> Result<(), ReferenceError> {
    let valid = !repository.is_empty()
        && repository.split('/').all(|component| {
            !component.is_empty()
                && component.bytes().all(|byte| {
                    byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
                })
        });
    if valid {
        Ok(())
    } else {
        Err(ReferenceError::Invalid(repository.into()))
    }
}

fn validate_component(value: &str, kind: &'static str) -> Result<String, ReferenceError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    {
        return Err(ReferenceError::InvalidComponent {
            kind,
            value: value.into(),
        });
    }
    Ok(value.into())
}

fn validate_digest(value: &str) -> Result<String, ReferenceError> {
    let hex = value.strip_prefix("sha256:").unwrap_or_default();
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ReferenceError::InvalidComponent {
            kind: "digest",
            value: value.into(),
        });
    }
    Ok(format!("sha256:{}", hex.to_ascii_lowercase()))
}

fn non_empty_path(value: &str, kind: &'static str) -> Result<PathBuf, ReferenceError> {
    if value.is_empty() {
        Err(ReferenceError::InvalidComponent {
            kind,
            value: value.into(),
        })
    } else {
        Ok(PathBuf::from(value))
    }
}

#[derive(Debug, Error)]
pub enum ReferenceError {
    #[error("invalid image reference: {0}")]
    Invalid(String),
    #[error("invalid {kind} in image reference: {value}")]
    InvalidComponent { kind: &'static str, value: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalizes_docker_hub_aliases() {
        let reference: RegistryReference = "alpine@latest".parse().unwrap();
        assert_eq!(reference.to_string(), "docker.io/library/alpine:latest");
        assert_eq!(reference.endpoint_registry(), "registry-1.docker.io");
    }

    #[test]
    fn preserves_registry_port_and_digest() {
        let digest = "a".repeat(64);
        let reference: RegistryReference = format!("localhost:5000/a/b@sha256:{digest}")
            .parse()
            .unwrap();
        assert_eq!(reference.registry, "localhost:5000");
        assert!(matches!(reference.selector, Selector::Digest(_)));
    }

    #[test]
    fn parses_local_sources() {
        assert!(matches!(
            "oci-layout:/tmp/image".parse::<ImageReference>().unwrap(),
            ImageReference::OciLayout(_)
        ));
    }
}
