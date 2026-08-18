use std::{
    fmt,
    path::{Path, PathBuf},
    str::FromStr,
};

use sha2::{Digest as _, Sha256};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Digest([u8; 32]);

impl Digest {
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }

    pub fn hex(&self) -> String {
        hex(&self.0)
    }

    pub(crate) fn from_sha256(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "sha256:{}", self.hex())
    }
}

impl FromStr for Digest {
    type Err = CasError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let encoded = value
            .strip_prefix("sha256:")
            .ok_or_else(|| CasError::UnsupportedDigest(value.into()))?;
        if encoded.len() != 64 {
            return Err(CasError::InvalidDigest(value.into()));
        }
        let mut bytes = [0_u8; 32];
        for (slot, pair) in bytes.iter_mut().zip(encoded.as_bytes().chunks_exact(2)) {
            let text =
                std::str::from_utf8(pair).map_err(|_| CasError::InvalidDigest(value.into()))?;
            *slot =
                u8::from_str_radix(text, 16).map_err(|_| CasError::InvalidDigest(value.into()))?;
        }
        Ok(Self(bytes))
    }
}

#[derive(Debug, Clone)]
pub struct Cas {
    root: PathBuf,
}

impl Cas {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn blob_path(&self, digest: &Digest) -> PathBuf {
        self.root.join("blobs/sha256").join(digest.hex())
    }

    pub async fn put_verified(&self, expected: &Digest, bytes: &[u8]) -> Result<PathBuf, CasError> {
        let actual = Digest::from_bytes(bytes);
        if &actual != expected {
            return Err(CasError::DigestMismatch {
                expected: expected.clone(),
                actual,
            });
        }
        let destination = self.blob_path(expected);
        if tokio::fs::try_exists(&destination).await? {
            return Ok(destination);
        }
        let parent = destination.parent().expect("CAS blob has a parent");
        tokio::fs::create_dir_all(parent).await?;
        let temporary = parent.join(format!(".{}.{}.tmp", expected.hex(), std::process::id()));
        tokio::fs::write(&temporary, bytes).await?;
        match tokio::fs::rename(&temporary, &destination).await {
            Ok(()) => Ok(destination),
            Err(_error) if tokio::fs::try_exists(&destination).await? => {
                let _ = tokio::fs::remove_file(&temporary).await;
                Ok(destination)
            }
            Err(error) => Err(error.into()),
        }
    }

    pub async fn read(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        let path = self.blob_path(digest);
        let bytes = tokio::fs::read(path).await?;
        let actual = Digest::from_bytes(&bytes);
        if actual != *digest {
            return Err(CasError::DigestMismatch {
                expected: digest.clone(),
                actual,
            });
        }
        Ok(bytes)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(DIGITS[usize::from(byte >> 4)]));
        output.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    output
}

#[derive(Debug, Error)]
pub enum CasError {
    #[error("only sha256 OCI digests are supported: {0}")]
    UnsupportedDigest(String),
    #[error("invalid sha256 digest: {0}")]
    InvalidDigest(String),
    #[error("blob digest mismatch: expected {expected}, got {actual}")]
    DigestMismatch { expected: Digest, actual: Digest },
    #[error("CAS I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn verifies_before_publishing_blob() {
        let directory = tempfile::tempdir().unwrap();
        let cas = Cas::new(directory.path());
        let digest = Digest::from_bytes(b"hello");
        let path = cas.put_verified(&digest, b"hello").await.unwrap();
        assert_eq!(tokio::fs::read(path).await.unwrap(), b"hello");
        assert!(cas.put_verified(&digest, b"tampered").await.is_err());
    }
}
