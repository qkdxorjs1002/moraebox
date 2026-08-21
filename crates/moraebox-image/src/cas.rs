use std::{
    fmt,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    str::FromStr,
    sync::atomic::{AtomicU64, Ordering},
};

use futures_util::{Stream, StreamExt};
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use tokio::io::AsyncWriteExt;

use crate::lock::AdvisoryLock;

const VERIFY_BUFFER_SIZE: usize = 64 * 1024;
static TEMPORARY_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
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

#[derive(Debug)]
pub(crate) enum PutStreamError<E> {
    Source(E),
    Cas(CasError),
    SizeExceeded { actual: u64 },
    SizeMismatch { expected: u64, actual: u64 },
}

#[derive(Debug)]
struct StagedBlob {
    path: PathBuf,
    file: Option<tokio::fs::File>,
    published: bool,
}

impl StagedBlob {
    fn new(path: PathBuf, file: tokio::fs::File) -> Self {
        Self {
            path,
            file: Some(file),
            published: false,
        }
    }

    fn file(&mut self) -> &mut tokio::fs::File {
        self.file.as_mut().expect("staged blob file is open")
    }

    fn close(&mut self) {
        drop(self.file.take());
    }
}

impl Drop for StagedBlob {
    fn drop(&mut self) {
        self.close();
        if !self.published {
            let _ = std::fs::remove_file(&self.path);
        }
    }
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
        let _lock = AdvisoryLock::acquire(&self.lock_path(expected)).await?;
        let destination = self.blob_path(expected);
        if tokio::fs::try_exists(&destination).await? {
            self.verify(expected).await?;
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
                self.read(expected).await?;
                Ok(destination)
            }
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) async fn put_stream<S, B, E>(
        &self,
        expected_digest: Option<&Digest>,
        expected_size: Option<u64>,
        maximum_size: u64,
        stream: S,
    ) -> Result<Digest, PutStreamError<E>>
    where
        S: Stream<Item = Result<B, E>>,
        B: AsRef<[u8]>,
    {
        let mut digest_lock = match expected_digest {
            Some(digest) => Some(
                AdvisoryLock::acquire(&self.lock_path(digest))
                    .await
                    .map_err(CasError::from)
                    .map_err(PutStreamError::Cas)?,
            ),
            None => None,
        };
        let staging_directory = self.root.join("tmp");
        tokio::fs::create_dir_all(&staging_directory)
            .await
            .map_err(CasError::from)
            .map_err(PutStreamError::Cas)?;
        let mut staging = create_staged_blob(&staging_directory)
            .await
            .map_err(PutStreamError::Cas)?;
        let mut hasher = Sha256::new();
        let mut size = 0_u64;
        let mut stream = std::pin::pin!(stream);
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(PutStreamError::Source)?;
            let bytes = chunk.as_ref();
            size = size.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
            verify_stream_size(
                size,
                expected_size.filter(|expected| size > *expected),
                maximum_size,
            )?;
            hasher.update(bytes);
            staging
                .file()
                .write_all(bytes)
                .await
                .map_err(CasError::from)
                .map_err(PutStreamError::Cas)?;
        }
        verify_stream_size(size, expected_size, maximum_size)?;
        staging
            .file()
            .flush()
            .await
            .map_err(CasError::from)
            .map_err(PutStreamError::Cas)?;
        staging.close();

        let actual = Digest::from_sha256(hasher.finalize().into());
        if let Some(expected) = expected_digest
            && expected != &actual
        {
            return Err(PutStreamError::Cas(CasError::DigestMismatch {
                expected: expected.clone(),
                actual,
            }));
        }
        if digest_lock.is_none() {
            digest_lock = Some(
                AdvisoryLock::acquire(&self.lock_path(&actual))
                    .await
                    .map_err(CasError::from)
                    .map_err(PutStreamError::Cas)?,
            );
        }

        let destination = self.blob_path(&actual);
        if tokio::fs::try_exists(&destination)
            .await
            .map_err(CasError::from)
            .map_err(PutStreamError::Cas)?
        {
            let existing_size = self.verify(&actual).await.map_err(PutStreamError::Cas)?;
            verify_stream_size(existing_size, expected_size, maximum_size)?;
            return Ok(actual);
        }
        let parent = destination.parent().expect("CAS blob has a parent");
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(CasError::from)
            .map_err(PutStreamError::Cas)?;
        tokio::fs::rename(&staging.path, &destination)
            .await
            .map_err(CasError::from)
            .map_err(PutStreamError::Cas)?;
        staging.published = true;
        drop(digest_lock);
        Ok(actual)
    }

    pub(crate) async fn verify(&self, digest: &Digest) -> Result<u64, CasError> {
        let path = self.blob_path(digest);
        let expected = digest.clone();
        tokio::task::spawn_blocking(move || verify_file(&path, &expected))
            .await
            .map_err(|error| CasError::Task(error.to_string()))?
    }

    pub async fn read(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        let path = self.blob_path(digest);
        let expected = digest.clone();
        tokio::task::spawn_blocking(move || read_verified_file(&path, &expected))
            .await
            .map_err(|error| CasError::Task(error.to_string()))?
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn lock_path(&self, digest: &Digest) -> PathBuf {
        self.root
            .join("locks/sha256")
            .join(format!("{}.lock", digest.hex()))
    }
}

fn verify_file(path: &Path, expected: &Digest) -> Result<u64, CasError> {
    let mut file = File::open(path)?;
    let mut buffer = vec![0_u8; VERIFY_BUFFER_SIZE];
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        size = size.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
        hasher.update(&buffer[..read]);
    }
    verify_digest(expected, Digest::from_sha256(hasher.finalize().into()))?;
    Ok(size)
}

fn read_verified_file(path: &Path, expected: &Digest) -> Result<Vec<u8>, CasError> {
    let bytes = std::fs::read(path)?;
    verify_digest(expected, Digest::from_bytes(&bytes))?;
    Ok(bytes)
}

fn verify_digest(expected: &Digest, actual: Digest) -> Result<(), CasError> {
    if actual == *expected {
        Ok(())
    } else {
        Err(CasError::DigestMismatch {
            expected: expected.clone(),
            actual,
        })
    }
}

async fn create_staged_blob(directory: &Path) -> Result<StagedBlob, CasError> {
    loop {
        let sequence = TEMPORARY_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!(".download.{}.{}.tmp", std::process::id(), sequence));
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
        {
            Ok(file) => return Ok(StagedBlob::new(path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
}

fn verify_stream_size<E>(
    actual: u64,
    expected: Option<u64>,
    maximum: u64,
) -> Result<(), PutStreamError<E>> {
    if actual > maximum {
        return Err(PutStreamError::SizeExceeded { actual });
    }
    if let Some(expected) = expected
        && actual != expected
    {
        return Err(PutStreamError::SizeMismatch { expected, actual });
    }
    Ok(())
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
    #[error("CAS blocking task failed: {0}")]
    Task(String),
}

#[cfg(test)]
mod tests {
    use futures_util::stream;

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

    #[tokio::test]
    async fn rejects_a_corrupt_existing_blob_instead_of_trusting_its_path() {
        let directory = tempfile::tempdir().unwrap();
        let cas = Cas::new(directory.path());
        let digest = Digest::from_bytes(b"expected");
        let path = cas.blob_path(&digest);
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&path, b"corrupt").await.unwrap();

        let error = cas.put_verified(&digest, b"expected").await.unwrap_err();

        assert!(matches!(
            error,
            CasError::DigestMismatch { expected, actual }
                if expected == digest && actual == Digest::from_bytes(b"corrupt")
        ));
        assert_eq!(tokio::fs::read(path).await.unwrap(), b"corrupt");
    }

    #[tokio::test]
    async fn concurrent_same_digest_publish_is_double_checked() {
        let directory = tempfile::tempdir().unwrap();
        let cas = Cas::new(directory.path());
        let digest = Digest::from_bytes(b"shared");

        let (first, second) = tokio::join!(
            cas.put_verified(&digest, b"shared"),
            cas.put_verified(&digest, b"shared")
        );

        assert_eq!(first.unwrap(), cas.blob_path(&digest));
        assert_eq!(second.unwrap(), cas.blob_path(&digest));
        assert_eq!(
            tokio::fs::read(cas.blob_path(&digest)).await.unwrap(),
            b"shared"
        );
    }

    #[tokio::test]
    async fn streams_hashes_and_publishes_without_buffering_the_blob() {
        let directory = tempfile::tempdir().unwrap();
        let cas = Cas::new(directory.path());
        let digest = Digest::from_bytes(b"hello");
        let chunks = stream::iter([Ok::<_, std::io::Error>(b"he".to_vec()), Ok(b"llo".to_vec())]);

        let actual = cas
            .put_stream(Some(&digest), Some(5), 5, chunks)
            .await
            .unwrap();

        assert_eq!(actual, digest);
        assert_eq!(
            tokio::fs::read(cas.blob_path(&digest)).await.unwrap(),
            b"hello"
        );
        assert_eq!(cas.verify(&digest).await.unwrap(), 5);
        assert!(staging_is_empty(&cas).await);
    }

    #[tokio::test]
    async fn stream_failures_remove_staging_and_never_publish() {
        let directory = tempfile::tempdir().unwrap();
        let cas = Cas::new(directory.path());
        let digest = Digest::from_bytes(b"hello");
        let chunks = stream::iter([
            Ok(b"he".to_vec()),
            Err(std::io::Error::other("connection reset")),
        ]);

        let error = cas
            .put_stream(Some(&digest), Some(5), 5, chunks)
            .await
            .unwrap_err();

        assert!(matches!(error, PutStreamError::Source(_)));
        assert!(!cas.blob_path(&digest).exists());
        assert!(staging_is_empty(&cas).await);
    }

    #[tokio::test]
    async fn stream_size_and_digest_mismatches_never_publish() {
        let directory = tempfile::tempdir().unwrap();
        let cas = Cas::new(directory.path());
        let digest = Digest::from_bytes(b"hello");
        let short = stream::iter([Ok::<_, std::io::Error>(b"hell".to_vec())]);
        let error = cas
            .put_stream(Some(&digest), Some(5), 5, short)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            PutStreamError::SizeMismatch {
                expected: 5,
                actual: 4
            }
        ));

        let oversized = stream::iter([Ok::<_, std::io::Error>(b"hello!".to_vec())]);
        let error = cas.put_stream(None, None, 5, oversized).await.unwrap_err();
        assert!(matches!(error, PutStreamError::SizeExceeded { actual: 6 }));

        let wrong = stream::iter([Ok::<_, std::io::Error>(b"world".to_vec())]);
        let error = cas
            .put_stream(Some(&digest), Some(5), 5, wrong)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            PutStreamError::Cas(CasError::DigestMismatch { .. })
        ));
        assert!(!cas.blob_path(&digest).exists());
        assert!(staging_is_empty(&cas).await);
    }

    #[tokio::test]
    async fn concurrent_stream_publish_double_checks_the_digest_destination() {
        let directory = tempfile::tempdir().unwrap();
        let cas = Cas::new(directory.path());
        let digest = Digest::from_bytes(b"shared stream");
        let first = stream::iter([Ok::<_, std::io::Error>(b"shared stream".to_vec())]);
        let second = stream::iter([Ok::<_, std::io::Error>(b"shared stream".to_vec())]);

        let (first_result, second_result) = tokio::join!(
            cas.put_stream(Some(&digest), Some(13), 13, first),
            cas.put_stream(Some(&digest), Some(13), 13, second)
        );

        assert_eq!(first_result.unwrap(), digest);
        assert_eq!(second_result.unwrap(), digest);
        assert_eq!(
            tokio::fs::read(cas.blob_path(&digest)).await.unwrap(),
            b"shared stream"
        );
        assert!(staging_is_empty(&cas).await);
    }

    async fn staging_is_empty(cas: &Cas) -> bool {
        let path = cas.root().join("tmp");
        let mut entries = match tokio::fs::read_dir(path).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return true,
            Err(error) => panic!("failed to inspect staging directory: {error}"),
        };
        entries.next_entry().await.unwrap().is_none()
    }
}
