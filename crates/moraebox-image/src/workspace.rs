use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read},
    path::{Component, Path, PathBuf},
    process::{Command as StdCommand, Stdio},
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use tokio::io::AsyncReadExt;

use crate::{Digest, durability::sync_directory};

const MIN_IMAGE_BYTES: u64 = 64 * 1024 * 1024;
const IMAGE_HEADROOM_BYTES: u64 = 32 * 1024 * 1024;
const EXT4_BLOCK_BYTES: u64 = 4 * 1024;
const EXT4_INODE_BYTES: u64 = 256;
const EXT4_FAST_SYMLINK_BYTES: u64 = 60;
const MIN_INODE_HEADROOM: u64 = 128;
const WORKSPACE_IMAGE_METADATA_SCHEMA_VERSION: u32 = 1;
const MKE2FS_STDERR_LIMIT: usize = 64 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceStage {
    ScanSource,
    CreateImage,
    PopulateFilesystem,
    HashImage,
    VerifySource,
}

impl std::fmt::Display for WorkspaceStage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::ScanSource => "scanning source",
            Self::CreateImage => "creating sparse image",
            Self::PopulateFilesystem => "populating ext4 image with mke2fs",
            Self::HashImage => "checking ext4 image digest",
            Self::VerifySource => "verifying source is unchanged",
        })
    }
}

#[derive(Debug, Clone)]
pub struct WorkspaceSnapshot {
    pub source: PathBuf,
    pub source_digest: Digest,
    pub source_metadata_digest: Digest,
    pub image_path: PathBuf,
    pub image_digest: Digest,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct SourceTreeStats {
    content_bytes: u64,
    entry_count: u64,
    estimated_payload_bytes: u64,
}

#[derive(Debug, Clone)]
struct SourceScan {
    content_digest: Digest,
    metadata_digest: Digest,
    stats: SourceTreeStats,
}

impl WorkspaceSnapshot {
    pub fn create(source: &Path, cache_root: &Path, mke2fs: &Path) -> Result<Self, WorkspaceError> {
        Self::create_with_managed_roots(source, cache_root, &[], mke2fs)
    }

    pub fn create_with_managed_roots(
        source: &Path,
        cache_root: &Path,
        managed_roots: &[PathBuf],
        mke2fs: &Path,
    ) -> Result<Self, WorkspaceError> {
        let (source, cache_root) = resolve_workspace_paths(source, cache_root, managed_roots)?;
        let scan = scan_source(&source)?;
        let source_digest = scan.content_digest;
        let directory = cache_root.join("workspaces/sha256");
        fs::create_dir_all(&directory)?;
        let image_path = directory.join(format!("{}.ext4", source_digest.hex()));
        if !image_path.exists() {
            let staging = StagingImage::create(&directory, &source_digest, scan.stats)?;
            let output = StdCommand::new(mke2fs)
                .args(["-q", "-t", "ext4", "-F", "-N"])
                .arg(provisioned_inode_count(scan.stats).to_string())
                .arg("-d")
                .arg(&source)
                .arg(staging.path())
                .env_clear()
                .output()?;
            if !output.status.success() {
                return Err(WorkspaceError::Mke2fs {
                    status: output.status.code(),
                    stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
                });
            }
            set_read_only(staging.path())?;
            staging.publish(&image_path)?;
        }
        let image_digest = workspace_image_digest(
            &image_path,
            &workspace_image_metadata_path(&cache_root, &source_digest),
            &source_digest,
            None,
        )?;
        let source_metadata_digest =
            verify_source_state(&source, &source_digest, &scan.metadata_digest, None)?;
        Ok(Self {
            source,
            source_digest,
            source_metadata_digest,
            image_path,
            image_digest,
        })
    }

    pub async fn create_async<F>(
        source: &Path,
        cache_root: &Path,
        mke2fs: &Path,
        stage_timeout: Option<Duration>,
        progress: F,
    ) -> Result<Self, WorkspaceError>
    where
        F: FnMut(WorkspaceStage),
    {
        Self::create_async_with_managed_roots(
            source,
            cache_root,
            &[],
            mke2fs,
            stage_timeout,
            progress,
        )
        .await
    }

    pub async fn create_async_with_managed_roots<F>(
        source: &Path,
        cache_root: &Path,
        managed_roots: &[PathBuf],
        mke2fs: &Path,
        stage_timeout: Option<Duration>,
        mut progress: F,
    ) -> Result<Self, WorkspaceError>
    where
        F: FnMut(WorkspaceStage),
    {
        let deadline = WorkspaceDeadline::new(stage_timeout);
        let (source, cache_root) = resolve_workspace_paths(source, cache_root, managed_roots)?;

        let scan_timeout = deadline.remaining(WorkspaceStage::ScanSource)?;
        progress(WorkspaceStage::ScanSource);
        let digest_source = source.clone();
        let scan = run_blocking_stage(WorkspaceStage::ScanSource, scan_timeout, move |cancelled| {
            scan_source_cancel(&digest_source, &cancelled)
        })
        .await
        .map_err(|error| deadline.normalize_timeout(error))?;
        let source_digest = scan.content_digest.clone();

        let directory = cache_root.join("workspaces/sha256");
        let image_path = directory.join(format!("{}.ext4", source_digest.hex()));
        if !image_path.exists() {
            let create_timeout = deadline.remaining(WorkspaceStage::CreateImage)?;
            progress(WorkspaceStage::CreateImage);
            let staging_directory = directory.clone();
            let staging_digest = source_digest.clone();
            let staging = run_blocking_stage(
                WorkspaceStage::CreateImage,
                create_timeout,
                move |cancelled| {
                    check_cancelled(Some(&cancelled))?;
                    fs::create_dir_all(&staging_directory)?;
                    StagingImage::create(&staging_directory, &staging_digest, scan.stats)
                },
            )
            .await
            .map_err(|error| deadline.normalize_timeout(error))?;
            let populate_timeout = deadline.remaining(WorkspaceStage::PopulateFilesystem)?;
            progress(WorkspaceStage::PopulateFilesystem);
            populate_ext4(
                mke2fs,
                &source,
                staging.path(),
                provisioned_inode_count(scan.stats),
                populate_timeout,
            )
            .await
            .map_err(|error| deadline.normalize_timeout(error))?;
            set_read_only(staging.path())?;
            staging.publish(&image_path)?;
        }

        let hash_timeout = deadline.remaining(WorkspaceStage::HashImage)?;
        progress(WorkspaceStage::HashImage);
        let digest_image_path = image_path.clone();
        let digest_metadata_path = workspace_image_metadata_path(&cache_root, &source_digest);
        let digest_source = source_digest.clone();
        let image_digest =
            run_blocking_stage(WorkspaceStage::HashImage, hash_timeout, move |cancelled| {
                workspace_image_digest(
                    &digest_image_path,
                    &digest_metadata_path,
                    &digest_source,
                    Some(&cancelled),
                )
            })
            .await
            .map_err(|error| deadline.normalize_timeout(error))?;

        let verify_timeout = deadline.remaining(WorkspaceStage::VerifySource)?;
        progress(WorkspaceStage::VerifySource);
        let verify_source = source.clone();
        let expected_source_digest = source_digest.clone();
        let expected_metadata_digest = scan.metadata_digest;
        let source_metadata_digest = run_blocking_stage(
            WorkspaceStage::VerifySource,
            verify_timeout,
            move |cancelled| {
                verify_source_state(
                    &verify_source,
                    &expected_source_digest,
                    &expected_metadata_digest,
                    Some(&cancelled),
                )
            },
        )
        .await
        .map_err(|error| deadline.normalize_timeout(error))?;

        Ok(Self {
            source,
            source_digest,
            source_metadata_digest,
            image_path,
            image_digest,
        })
    }

    pub fn verify_source_unchanged(&self) -> Result<(), WorkspaceError> {
        verify_source_state(
            &self.source,
            &self.source_digest,
            &self.source_metadata_digest,
            None,
        )
        .map(|_| ())
    }

    pub fn validate_managed_roots(
        source: &Path,
        cache_root: &Path,
        managed_roots: &[PathBuf],
    ) -> Result<(), WorkspaceError> {
        resolve_workspace_paths(source, cache_root, managed_roots).map(|_| ())
    }
}

#[derive(Debug, Clone, Copy)]
struct WorkspaceDeadline {
    deadline: Option<Instant>,
    limit: Option<Duration>,
}

impl WorkspaceDeadline {
    fn new(limit: Option<Duration>) -> Self {
        Self {
            deadline: limit.map(|duration| Instant::now() + duration),
            limit,
        }
    }

    fn remaining(&self, stage: WorkspaceStage) -> Result<Option<Duration>, WorkspaceError> {
        let Some(deadline) = self.deadline else {
            return Ok(None);
        };
        let now = Instant::now();
        if now >= deadline {
            return Err(WorkspaceError::TimedOut {
                stage,
                timeout: self.limit.expect("limited deadline must have a timeout"),
            });
        }
        Ok(Some(deadline - now))
    }

    fn normalize_timeout(&self, error: WorkspaceError) -> WorkspaceError {
        match (error, self.limit) {
            (WorkspaceError::TimedOut { stage, .. }, Some(timeout)) => {
                WorkspaceError::TimedOut { stage, timeout }
            }
            (error, _) => error,
        }
    }
}

pub fn digest_tree(root: &Path) -> Result<Digest, WorkspaceError> {
    scan_source(root).map(|scan| scan.content_digest)
}

fn scan_source(root: &Path) -> Result<SourceScan, WorkspaceError> {
    scan_source_impl(root, None)
}

fn scan_source_cancel(root: &Path, cancelled: &AtomicBool) -> Result<SourceScan, WorkspaceError> {
    scan_source_impl(root, Some(cancelled))
}

fn scan_source_impl(
    root: &Path,
    cancelled: Option<&AtomicBool>,
) -> Result<SourceScan, WorkspaceError> {
    let mut content_hasher = Sha256::new();
    let mut metadata_hasher = Sha256::new();
    let mut stats = SourceTreeStats {
        estimated_payload_bytes: EXT4_BLOCK_BYTES,
        ..SourceTreeStats::default()
    };
    walk(root, root, cancelled, &mut |relative, metadata| {
        check_cancelled(cancelled)?;
        stats.entry_count = stats.entry_count.saturating_add(1);
        hash_path(&mut content_hasher, relative);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            content_hasher.update(metadata.mode().to_le_bytes());
        }
        let kind = metadata.file_type();
        if kind.is_file() {
            content_hasher.update(b"file\0");
            hash_entry_metadata(&mut metadata_hasher, relative, metadata, None);
            let before = source_entry_fingerprint(metadata);
            let mut file = File::open(root.join(relative))?;
            let mut buffer = vec![0_u8; 64 * 1024];
            let mut file_bytes = 0_u64;
            loop {
                check_cancelled(cancelled)?;
                let count = file.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                content_hasher.update(&buffer[..count]);
                file_bytes = file_bytes.saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
            }
            stats.content_bytes = stats.content_bytes.saturating_add(file_bytes);
            stats.estimated_payload_bytes = stats
                .estimated_payload_bytes
                .saturating_add(round_up_to_block(file_bytes));
            let path = root.join(relative);
            let after = fs::symlink_metadata(&path)?;
            if source_entry_fingerprint(&after) != before {
                return Err(WorkspaceError::SourceChangedDuringScan(path));
            }
        } else if kind.is_dir() {
            content_hasher.update(b"dir\0");
            hash_entry_metadata(&mut metadata_hasher, relative, metadata, None);
            stats.estimated_payload_bytes = stats
                .estimated_payload_bytes
                .saturating_add(EXT4_BLOCK_BYTES);
        } else if kind.is_symlink() {
            content_hasher.update(b"symlink\0");
            let target = fs::read_link(root.join(relative))?;
            hash_path(&mut content_hasher, &target);
            hash_entry_metadata(&mut metadata_hasher, relative, metadata, Some(&target));
            if metadata.len() > EXT4_FAST_SYMLINK_BYTES {
                stats.estimated_payload_bytes = stats
                    .estimated_payload_bytes
                    .saturating_add(round_up_to_block(metadata.len()));
            }
        } else {
            return Err(WorkspaceError::UnsupportedFile(root.join(relative)));
        }
        Ok(())
    })?;
    Ok(SourceScan {
        content_digest: Digest::from_sha256(content_hasher.finalize().into()),
        metadata_digest: Digest::from_sha256(metadata_hasher.finalize().into()),
        stats,
    })
}

fn verify_source_state(
    root: &Path,
    expected_content: &Digest,
    _expected_metadata: &Digest,
    cancelled: Option<&AtomicBool>,
) -> Result<Digest, WorkspaceError> {
    // Metadata equality is not a safe unchanged shortcut: some filesystems coalesce a same-size
    // rewrite into the same timestamp tick. Always verify the content digest before trusting the
    // prepared snapshot or declaring that the host source survived a run unchanged.
    let actual = scan_source_impl(root, cancelled)?;
    if &actual.content_digest == expected_content {
        Ok(actual.metadata_digest)
    } else {
        Err(WorkspaceError::SourceChanged {
            expected: expected_content.clone(),
            actual: actual.content_digest,
        })
    }
}

fn hash_entry_metadata(
    hasher: &mut Sha256,
    relative: &Path,
    metadata: &fs::Metadata,
    symlink_target: Option<&Path>,
) {
    hash_path(hasher, relative);
    let kind = metadata.file_type();
    hasher.update(if kind.is_file() {
        b"file\0".as_slice()
    } else if kind.is_dir() {
        b"dir\0".as_slice()
    } else if kind.is_symlink() {
        b"symlink\0".as_slice()
    } else {
        b"other\0".as_slice()
    });
    hasher.update(metadata.len().to_le_bytes());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        hasher.update(metadata.mode().to_le_bytes());
        hasher.update(metadata.dev().to_le_bytes());
        hasher.update(metadata.ino().to_le_bytes());
        hasher.update(metadata.mtime().to_le_bytes());
        hasher.update(metadata.mtime_nsec().to_le_bytes());
        hasher.update(metadata.ctime().to_le_bytes());
        hasher.update(metadata.ctime_nsec().to_le_bytes());
    }
    #[cfg(not(unix))]
    {
        hasher.update([u8::from(metadata.permissions().readonly())]);
        let elapsed = metadata
            .modified()
            .ok()
            .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok());
        if let Some(elapsed) = elapsed {
            hasher.update(elapsed.as_secs().to_le_bytes());
            hasher.update(elapsed.subsec_nanos().to_le_bytes());
        }
    }
    if let Some(target) = symlink_target {
        hash_path(hasher, target);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SourceEntryFingerprint {
    len: u64,
    #[cfg(unix)]
    mode: u32,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(unix)]
    mtime: i64,
    #[cfg(unix)]
    mtime_nsec: i64,
    #[cfg(unix)]
    ctime: i64,
    #[cfg(unix)]
    ctime_nsec: i64,
}

fn source_entry_fingerprint(metadata: &fs::Metadata) -> SourceEntryFingerprint {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        SourceEntryFingerprint {
            len: metadata.len(),
            mode: metadata.mode(),
            dev: metadata.dev(),
            ino: metadata.ino(),
            mtime: metadata.mtime(),
            mtime_nsec: metadata.mtime_nsec(),
            ctime: metadata.ctime(),
            ctime_nsec: metadata.ctime_nsec(),
        }
    }
    #[cfg(not(unix))]
    SourceEntryFingerprint {
        len: metadata.len(),
    }
}

fn walk(
    root: &Path,
    directory: &Path,
    cancelled: Option<&AtomicBool>,
    visitor: &mut impl FnMut(&Path, &fs::Metadata) -> Result<(), WorkspaceError>,
) -> Result<(), WorkspaceError> {
    check_cancelled(cancelled)?;
    let mut children = fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
    children.sort_by_key(fs::DirEntry::file_name);
    for child in children {
        check_cancelled(cancelled)?;
        let path = child.path();
        let relative = path
            .strip_prefix(root)
            .map_err(|_| WorkspaceError::UnsafePath(path.clone()))?;
        validate_relative(relative)?;
        let metadata = fs::symlink_metadata(&path)?;
        visitor(relative, &metadata)?;
        if metadata.is_dir() {
            walk(root, &path, cancelled, visitor)?;
        }
    }
    Ok(())
}

fn validate_relative(path: &Path) -> Result<(), WorkspaceError> {
    if path
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
    {
        Ok(())
    } else {
        Err(WorkspaceError::UnsafePath(path.into()))
    }
}

fn hash_path(hasher: &mut Sha256, path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        hasher.update(path.as_os_str().as_bytes());
    }
    #[cfg(not(unix))]
    hasher.update(path.to_string_lossy().as_bytes());
    hasher.update([0]);
}

fn digest_file_impl(path: &Path, cancelled: Option<&AtomicBool>) -> Result<Digest, WorkspaceError> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        check_cancelled(cancelled)?;
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(Digest::from_sha256(hasher.finalize().into()))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct WorkspaceImageMetadata {
    schema_version: u32,
    source_digest: String,
    image_digest: String,
    image: WorkspaceImageFingerprint,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct WorkspaceImageFingerprint {
    len: u64,
    readonly: bool,
    #[cfg(unix)]
    mode: u32,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(unix)]
    mtime: i64,
    #[cfg(unix)]
    mtime_nsec: i64,
    #[cfg(unix)]
    ctime: i64,
    #[cfg(unix)]
    ctime_nsec: i64,
    #[cfg(not(unix))]
    modified_secs: Option<u64>,
    #[cfg(not(unix))]
    modified_nanos: Option<u32>,
}

fn workspace_image_metadata_path(cache_root: &Path, source_digest: &Digest) -> PathBuf {
    cache_root
        .join("workspaces/metadata/sha256")
        .join(format!("{}.json", source_digest.hex()))
}

fn workspace_image_digest(
    image_path: &Path,
    metadata_path: &Path,
    source_digest: &Digest,
    cancelled: Option<&AtomicBool>,
) -> Result<Digest, WorkspaceError> {
    let before = workspace_image_fingerprint(image_path)?;
    let cached_digest = read_workspace_image_metadata(metadata_path)?
        .filter(|metadata| {
            metadata.schema_version == WORKSPACE_IMAGE_METADATA_SCHEMA_VERSION
                && metadata.source_digest == source_digest.to_string()
                && metadata.image == before
        })
        .and_then(|metadata| Digest::from_str(&metadata.image_digest).ok());
    if let Some(digest) = cached_digest {
        return Ok(digest);
    }

    let image_digest = digest_file_impl(image_path, cancelled)?;
    let after = workspace_image_fingerprint(image_path)?;
    if after != before {
        return Err(WorkspaceError::ImageChangedDuringHash(image_path.into()));
    }
    write_workspace_image_metadata(
        metadata_path,
        &WorkspaceImageMetadata {
            schema_version: WORKSPACE_IMAGE_METADATA_SCHEMA_VERSION,
            source_digest: source_digest.to_string(),
            image_digest: image_digest.to_string(),
            image: after,
        },
    )?;
    Ok(image_digest)
}

fn read_workspace_image_metadata(
    path: &Path,
) -> Result<Option<WorkspaceImageMetadata>, WorkspaceError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Ok(None);
    }
    let bytes = fs::read(path)?;
    Ok(serde_json::from_slice(&bytes).ok())
}

fn workspace_image_fingerprint(path: &Path) -> Result<WorkspaceImageFingerprint, WorkspaceError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(WorkspaceError::InvalidWorkspaceImage(path.into()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        Ok(WorkspaceImageFingerprint {
            len: metadata.len(),
            readonly: metadata.permissions().readonly(),
            mode: metadata.mode(),
            dev: metadata.dev(),
            ino: metadata.ino(),
            mtime: metadata.mtime(),
            mtime_nsec: metadata.mtime_nsec(),
            ctime: metadata.ctime(),
            ctime_nsec: metadata.ctime_nsec(),
        })
    }
    #[cfg(not(unix))]
    {
        let modified = metadata
            .modified()
            .ok()
            .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok());
        Ok(WorkspaceImageFingerprint {
            len: metadata.len(),
            readonly: metadata.permissions().readonly(),
            modified_secs: modified.map(|value| value.as_secs()),
            modified_nanos: modified.map(|value| value.subsec_nanos()),
        })
    }
}

fn write_workspace_image_metadata(
    path: &Path,
    value: &WorkspaceImageMetadata,
) -> Result<(), WorkspaceError> {
    let parent = path
        .parent()
        .ok_or_else(|| WorkspaceError::InvalidWorkspaceMetadata(path.into()))?;
    fs::create_dir_all(parent)?;
    if fs::symlink_metadata(path)
        .is_ok_and(|metadata| !metadata.is_file() || metadata.file_type().is_symlink())
    {
        return Err(WorkspaceError::InvalidWorkspaceMetadata(path.into()));
    }
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| WorkspaceError::InvalidWorkspaceMetadata(path.into()))?;
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(".{name}.{}.{}.tmp", std::process::id(), sequence));
    let result = (|| -> Result<(), WorkspaceError> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        serde_json::to_writer_pretty(&file, value)?;
        file.sync_all()?;
        drop(file);
        match fs::rename(&temporary, path) {
            Ok(()) => {}
            Err(_)
                if fs::symlink_metadata(path).is_ok_and(|metadata| {
                    metadata.is_file() && !metadata.file_type().is_symlink()
                }) =>
            {
                fs::remove_file(path)?;
                fs::rename(&temporary, path)?;
            }
            Err(error) => return Err(error.into()),
        }
        sync_directory(parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn check_cancelled(cancelled: Option<&AtomicBool>) -> Result<(), WorkspaceError> {
    if cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
        Err(WorkspaceError::Cancelled)
    } else {
        Ok(())
    }
}

fn normalize_absolute(path: &Path) -> Result<PathBuf, WorkspaceError> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(WorkspaceError::UnsafePath(absolute));
                }
            }
            Component::Normal(name) => normalized.push(name),
        }
    }
    Ok(normalized)
}

fn resolve_for_overlap(path: &Path) -> Result<PathBuf, WorkspaceError> {
    let absolute = normalize_absolute(path)?;
    if absolute.exists() {
        return Ok(absolute.canonicalize()?);
    }

    let mut existing = absolute.as_path();
    let mut missing = Vec::new();
    while !existing.exists() {
        let name = existing
            .file_name()
            .ok_or_else(|| WorkspaceError::UnsafePath(absolute.clone()))?;
        missing.push(name.to_owned());
        existing = existing
            .parent()
            .ok_or_else(|| WorkspaceError::UnsafePath(absolute.clone()))?;
    }
    let mut resolved = existing.canonicalize()?;
    for name in missing.iter().rev() {
        resolved.push(name);
    }
    Ok(resolved)
}

fn resolve_workspace_paths(
    source: &Path,
    cache_root: &Path,
    managed_roots: &[PathBuf],
) -> Result<(PathBuf, PathBuf), WorkspaceError> {
    let source = source.canonicalize()?;
    if !source.is_dir() {
        return Err(WorkspaceError::NotDirectory(source));
    }
    let cache_root = resolve_for_overlap(cache_root)?;
    ensure_disjoint(&source, &cache_root)?;
    for managed_root in managed_roots {
        let managed_root = resolve_for_overlap(managed_root)?;
        if source.starts_with(&managed_root) || managed_root.starts_with(&source) {
            return Err(WorkspaceError::OverlappingManagedPath {
                workspace_source: source,
                managed_path: managed_root,
            });
        }
    }
    Ok((source, cache_root))
}

fn ensure_disjoint(source: &Path, cache_root: &Path) -> Result<(), WorkspaceError> {
    if source.starts_with(cache_root) || cache_root.starts_with(source) {
        Err(WorkspaceError::OverlappingPaths {
            workspace_source: source.to_owned(),
            cache: cache_root.to_owned(),
        })
    } else {
        Ok(())
    }
}

async fn run_blocking_stage<T, F>(
    stage: WorkspaceStage,
    timeout: Option<Duration>,
    work: F,
) -> Result<T, WorkspaceError>
where
    T: Send + 'static,
    F: FnOnce(Arc<AtomicBool>) -> Result<T, WorkspaceError> + Send + 'static,
{
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut cancel_on_drop = CancelOnDrop::new(Arc::clone(&cancelled));
    let mut task = tokio::task::spawn_blocking(move || work(cancelled));
    let result = if let Some(limit) = timeout {
        if let Ok(result) = tokio::time::timeout(limit, &mut task).await {
            join_blocking(stage, result)
        } else {
            cancel_on_drop.cancel();
            Err(WorkspaceError::TimedOut {
                stage,
                timeout: limit,
            })
        }
    } else {
        join_blocking(stage, task.await)
    };
    cancel_on_drop.disarm();
    result
}

fn join_blocking<T>(
    stage: WorkspaceStage,
    result: Result<Result<T, WorkspaceError>, tokio::task::JoinError>,
) -> Result<T, WorkspaceError> {
    result.map_err(|error| WorkspaceError::Task {
        stage,
        message: error.to_string(),
    })?
}

struct CancelOnDrop {
    cancelled: Arc<AtomicBool>,
    armed: bool,
}

impl CancelOnDrop {
    fn new(cancelled: Arc<AtomicBool>) -> Self {
        Self {
            cancelled,
            armed: true,
        }
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.cancel();
        }
    }
}

struct StagingImage {
    path: PathBuf,
}

impl StagingImage {
    fn create(
        directory: &Path,
        source_digest: &Digest,
        stats: SourceTreeStats,
    ) -> Result<Self, WorkspaceError> {
        for _ in 0..100 {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = directory.join(format!(
                ".{}.{}.{}.tmp",
                source_digest.hex(),
                std::process::id(),
                sequence
            ));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    let staging = Self { path };
                    file.set_len(image_size(stats))?;
                    return Ok(staging);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique workspace staging image",
        )
        .into())
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn publish(self, destination: &Path) -> Result<(), WorkspaceError> {
        match fs::hard_link(&self.path, destination) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg_attr(
    windows,
    allow(
        clippy::permissions_set_readonly_false,
        reason = "Windows must clear the readonly attribute before deleting the staging file"
    )
)]
impl Drop for StagingImage {
    fn drop(&mut self) {
        #[cfg(windows)]
        if let Ok(mut permissions) = fs::metadata(&self.path).map(|metadata| metadata.permissions())
        {
            permissions.set_readonly(false);
            let _ = fs::set_permissions(&self.path, permissions);
        }
        let _ = fs::remove_file(&self.path);
    }
}

async fn populate_ext4(
    mke2fs: &Path,
    source: &Path,
    image: &Path,
    inode_count: u64,
    timeout: Option<Duration>,
) -> Result<(), WorkspaceError> {
    let mut command = tokio::process::Command::new(mke2fs);
    command
        .args(["-q", "-t", "ext4", "-F", "-N"])
        .arg(inode_count.to_string())
        .arg("-d")
        .arg(source)
        .arg(image)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .env_clear()
        .kill_on_drop(true);
    let mut child = ReapedChild::spawn(&mut command)?;
    let stderr = child
        .take_stderr()
        .ok_or_else(|| io::Error::other("mke2fs stderr pipe is unavailable"))?;
    let stderr_task = tokio::spawn(read_bounded_stderr(stderr));

    let status = if let Some(limit) = timeout {
        if let Ok(status) = tokio::time::timeout(limit, child.wait()).await {
            status?
        } else {
            child.kill_and_wait().await?;
            let _ = join_stderr(stderr_task, WorkspaceStage::PopulateFilesystem).await;
            return Err(WorkspaceError::TimedOut {
                stage: WorkspaceStage::PopulateFilesystem,
                timeout: limit,
            });
        }
    } else {
        child.wait().await?
    };
    child.mark_reaped();
    let stderr = join_stderr(stderr_task, WorkspaceStage::PopulateFilesystem).await?;
    if status.success() {
        Ok(())
    } else {
        Err(WorkspaceError::Mke2fs {
            status: status.code(),
            stderr: stderr.trim().to_owned(),
        })
    }
}

struct ReapedChild(Option<tokio::process::Child>);

impl ReapedChild {
    fn spawn(command: &mut tokio::process::Command) -> Result<Self, WorkspaceError> {
        Ok(Self(Some(command.spawn()?)))
    }

    fn child_mut(&mut self) -> &mut tokio::process::Child {
        self.0.as_mut().expect("child is present until reaped")
    }

    fn take_stderr(&mut self) -> Option<tokio::process::ChildStderr> {
        self.child_mut().stderr.take()
    }

    async fn wait(&mut self) -> io::Result<std::process::ExitStatus> {
        self.child_mut().wait().await
    }

    async fn kill_and_wait(&mut self) -> io::Result<()> {
        let _ = self.child_mut().start_kill();
        self.child_mut().wait().await.map(|_| ())
    }

    fn mark_reaped(&mut self) {
        self.0.take();
    }
}

impl Drop for ReapedChild {
    fn drop(&mut self) {
        let Some(mut child) = self.0.take() else {
            return;
        };
        let _ = child.start_kill();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = child.wait().await;
            });
        }
    }
}

async fn read_bounded_stderr(mut stderr: tokio::process::ChildStderr) -> io::Result<String> {
    let mut retained = Vec::new();
    let mut buffer = [0_u8; 8 * 1024];
    let mut truncated = false;
    loop {
        let count = stderr.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        let available = MKE2FS_STDERR_LIMIT.saturating_sub(retained.len());
        let retained_count = available.min(count);
        retained.extend_from_slice(&buffer[..retained_count]);
        truncated |= retained_count < count;
    }
    let mut output = String::from_utf8_lossy(&retained).into_owned();
    if truncated {
        output.push_str("\n[mke2fs stderr truncated]");
    }
    Ok(output)
}

async fn join_stderr(
    task: tokio::task::JoinHandle<io::Result<String>>,
    stage: WorkspaceStage,
) -> Result<String, WorkspaceError> {
    task.await
        .map_err(|error| WorkspaceError::Task {
            stage,
            message: error.to_string(),
        })?
        .map_err(WorkspaceError::Io)
}

fn image_size(stats: SourceTreeStats) -> u64 {
    let inode_table_bytes = provisioned_inode_count(stats).saturating_mul(EXT4_INODE_BYTES);
    let estimated = stats
        .estimated_payload_bytes
        .max(stats.content_bytes)
        .saturating_add(inode_table_bytes);
    let requested = estimated
        .saturating_mul(2)
        .saturating_add(IMAGE_HEADROOM_BYTES)
        .max(MIN_IMAGE_BYTES);
    align_up_saturating(requested, 4 * 1024 * 1024)
}

fn provisioned_inode_count(stats: SourceTreeStats) -> u64 {
    let required = stats.entry_count.saturating_add(1);
    required.saturating_add((required / 4).max(MIN_INODE_HEADROOM))
}

fn round_up_to_block(bytes: u64) -> u64 {
    align_up_saturating(bytes, EXT4_BLOCK_BYTES)
}

fn align_up_saturating(bytes: u64, alignment: u64) -> u64 {
    let remainder = bytes % alignment;
    if remainder == 0 {
        bytes
    } else {
        bytes
            .checked_add(alignment - remainder)
            .unwrap_or(u64::MAX - (u64::MAX % alignment))
    }
}

fn set_read_only(path: &Path) -> Result<(), WorkspaceError> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_readonly(true);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

#[derive(Debug, Error)]
pub enum WorkspaceError {
    #[error("workspace is not a directory: {}", .0.display())]
    NotDirectory(PathBuf),
    #[error("unsafe workspace path: {}", .0.display())]
    UnsafePath(PathBuf),
    #[error("unsupported workspace file type: {}", .0.display())]
    UnsupportedFile(PathBuf),
    #[error("workspace file changed while it was being scanned: {}", .0.display())]
    SourceChangedDuringScan(PathBuf),
    #[error("workspace image must be a regular non-symlink file: {}", .0.display())]
    InvalidWorkspaceImage(PathBuf),
    #[error("invalid workspace image metadata path: {}", .0.display())]
    InvalidWorkspaceMetadata(PathBuf),
    #[error("workspace image changed while it was being hashed: {}", .0.display())]
    ImageChangedDuringHash(PathBuf),
    #[error(
        "workspace source and cache must not overlap: source={}, cache={}",
        workspace_source.display(),
        cache.display()
    )]
    OverlappingPaths {
        workspace_source: PathBuf,
        cache: PathBuf,
    },
    #[error(
        "workspace source and managed path must not overlap: source={}, managed={}",
        workspace_source.display(),
        managed_path.display()
    )]
    OverlappingManagedPath {
        workspace_source: PathBuf,
        managed_path: PathBuf,
    },
    #[error("workspace preparation timed out after {timeout:?} while {stage}")]
    TimedOut {
        stage: WorkspaceStage,
        timeout: Duration,
    },
    #[error("workspace preparation was cancelled")]
    Cancelled,
    #[error("workspace preparation task failed while {stage}: {message}")]
    Task {
        stage: WorkspaceStage,
        message: String,
    },
    #[error("mke2fs failed with status {status:?}: {stderr}")]
    Mke2fs { status: Option<i32>, stderr: String },
    #[error("host workspace changed while the sandbox ran: expected {expected}, got {actual}")]
    SourceChanged { expected: Digest, actual: Digest },
    #[error("workspace snapshot I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("workspace snapshot metadata is invalid: {0}")]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_stages_share_one_absolute_deadline() {
        let limit = Duration::from_secs(1);
        let mut deadline = WorkspaceDeadline::new(Some(limit));
        let first = deadline
            .remaining(WorkspaceStage::ScanSource)
            .unwrap()
            .unwrap();
        let second = deadline
            .remaining(WorkspaceStage::CreateImage)
            .unwrap()
            .unwrap();
        assert!(second <= first);

        deadline.deadline = Some(Instant::now());
        assert!(matches!(
            deadline.remaining(WorkspaceStage::PopulateFilesystem),
            Err(WorkspaceError::TimedOut {
                stage: WorkspaceStage::PopulateFilesystem,
                timeout,
            }) if timeout == limit
        ));
    }

    #[cfg(unix)]
    fn executable_script(root: &Path, contents: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = root.join("fake-mke2fs");
        fs::write(&path, contents).unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&path, permissions).unwrap();
        path
    }

    #[cfg(unix)]
    fn staging_files(cache: &Path) -> Vec<PathBuf> {
        let directory = cache.join("workspaces/sha256");
        fs::read_dir(directory)
            .map(|entries| {
                entries
                    .map(|entry| entry.unwrap().path())
                    .filter(|path| {
                        path.file_name()
                            .is_some_and(|name| name.to_string_lossy().ends_with(".tmp"))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn tree_digest_is_stable_and_content_sensitive() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("a"), b"one").unwrap();
        let first = digest_tree(root.path()).unwrap();
        assert_eq!(first, digest_tree(root.path()).unwrap());
        fs::write(root.path().join("a"), b"two").unwrap();
        assert_ne!(first, digest_tree(root.path()).unwrap());
    }

    #[test]
    fn source_scan_collects_stats_and_metadata_verification_preserves_content_semantics() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("file");
        fs::write(&file, b"content").unwrap();
        let first = scan_source(root.path()).unwrap();
        assert_eq!(first.stats.content_bytes, 7);
        assert_eq!(first.stats.entry_count, 1);

        let replacement = root.path().join("replacement");
        fs::write(&replacement, b"content").unwrap();
        fs::rename(&replacement, &file).unwrap();
        let verified = verify_source_state(
            root.path(),
            &first.content_digest,
            &first.metadata_digest,
            None,
        )
        .unwrap();
        assert_ne!(verified, first.metadata_digest);
        assert_eq!(digest_tree(root.path()).unwrap(), first.content_digest);

        fs::write(&file, b"changed").unwrap();
        assert!(matches!(
            verify_source_state(root.path(), &first.content_digest, &verified, None,),
            Err(WorkspaceError::SourceChanged { .. })
        ));
    }

    #[test]
    fn image_has_headroom_and_alignment() {
        assert_eq!(
            image_size(SourceTreeStats {
                content_bytes: 1,
                entry_count: 1,
                estimated_payload_bytes: EXT4_BLOCK_BYTES * 2,
            }),
            MIN_IMAGE_BYTES
        );
        assert_eq!(
            image_size(SourceTreeStats {
                content_bytes: 100 * 1024 * 1024,
                entry_count: 1,
                estimated_payload_bytes: 100 * 1024 * 1024,
            }) % (4 * 1024 * 1024),
            0
        );
    }

    #[test]
    fn entry_heavy_trees_increase_image_and_inode_capacity() {
        let small = SourceTreeStats {
            content_bytes: 1,
            entry_count: 1,
            estimated_payload_bytes: EXT4_BLOCK_BYTES * 2,
        };
        let many_tiny_files = SourceTreeStats {
            content_bytes: 100_000,
            entry_count: 100_000,
            estimated_payload_bytes: EXT4_BLOCK_BYTES * 100_001,
        };

        assert!(image_size(many_tiny_files) > image_size(small));
        assert_eq!(provisioned_inode_count(small), 130);
        assert_eq!(provisioned_inode_count(many_tiny_files), 125_001);
    }

    #[test]
    fn rejects_cache_nested_inside_workspace_before_creating_it() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("file"), b"content").unwrap();
        let cache = source.join(".moraebox/cache");

        let error = WorkspaceSnapshot::create(&source, &cache, Path::new("unused")).unwrap_err();

        assert!(matches!(error, WorkspaceError::OverlappingPaths { .. }));
        assert!(!cache.exists());
    }

    #[test]
    fn rejects_state_nested_inside_workspace_before_scanning_it() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let cache = root.path().join("cache");
        let state = source.join(".moraebox/state");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("file"), b"content").unwrap();

        let error = WorkspaceSnapshot::validate_managed_roots(
            &source,
            &cache,
            std::slice::from_ref(&state),
        )
        .unwrap_err();

        assert!(matches!(
            error,
            WorkspaceError::OverlappingManagedPath { .. }
        ));
        assert!(!cache.exists());
        assert!(!state.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn async_snapshot_reports_stages_and_publishes_read_only_image() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let cache = root.path().join("cache");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("file"), b"content").unwrap();
        let mke2fs = executable_script(
            root.path(),
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"${0}.args\"\nexit 0\n",
        );
        let mut stages = Vec::new();

        let snapshot = WorkspaceSnapshot::create_async(
            &source,
            &cache,
            &mke2fs,
            Some(Duration::from_secs(10)),
            |stage| stages.push(stage),
        )
        .await
        .unwrap();

        assert_eq!(
            stages,
            [
                WorkspaceStage::ScanSource,
                WorkspaceStage::CreateImage,
                WorkspaceStage::PopulateFilesystem,
                WorkspaceStage::HashImage,
                WorkspaceStage::VerifySource,
            ]
        );
        assert!(snapshot.image_path.is_file());
        assert!(
            fs::metadata(&snapshot.image_path)
                .unwrap()
                .permissions()
                .readonly()
        );
        assert!(staging_files(&cache).is_empty());
        let arguments = fs::read_to_string(root.path().join("fake-mke2fs.args")).unwrap();
        let arguments = arguments.lines().collect::<Vec<_>>();
        assert!(arguments.windows(2).any(|pair| pair == ["-N", "130"]));
        assert!(
            workspace_image_metadata_path(&cache, &snapshot.source_digest).is_file(),
            "workspace image digest metadata was not published"
        );
    }

    #[test]
    fn workspace_image_digest_metadata_is_reused_and_refreshed_after_change() {
        let root = tempfile::tempdir().unwrap();
        let image = root.path().join("image.ext4");
        let metadata = root.path().join("metadata/image.json");
        let source = Digest::from_bytes(b"source");
        fs::write(&image, b"first").unwrap();

        let first = workspace_image_digest(&image, &metadata, &source, None).unwrap();
        assert!(metadata.is_file());
        assert_eq!(
            workspace_image_digest(&image, &metadata, &source, None).unwrap(),
            first
        );

        fs::write(&image, b"second").unwrap();
        let second = workspace_image_digest(&image, &metadata, &source, None).unwrap();
        assert_ne!(second, first);
        assert_eq!(
            workspace_image_digest(&image, &metadata, &source, None).unwrap(),
            second
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_mke2fs_reports_stderr_and_removes_staging_image() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let cache = root.path().join("cache");
        fs::create_dir(&source).unwrap();
        let mke2fs = executable_script(
            root.path(),
            "#!/bin/sh\nprintf 'deliberate failure' >&2\nexit 9\n",
        );

        let error = WorkspaceSnapshot::create_async(
            &source,
            &cache,
            &mke2fs,
            Some(Duration::from_secs(1)),
            |_| {},
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            WorkspaceError::Mke2fs {
                status: Some(9),
                ref stderr,
            } if stderr == "deliberate failure"
        ));
        assert!(staging_files(&cache).is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timed_out_mke2fs_is_killed_reaped_and_removes_staging_image() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let cache = root.path().join("cache");
        fs::create_dir(&source).unwrap();
        let mke2fs = executable_script(
            root.path(),
            "#!/bin/sh\nprintf '%s' \"$$\" > \"${0}.pid\"\nwhile :; do :; done\n",
        );
        // The deadline covers scan and image allocation too. Leave enough headroom for the
        // fake mke2fs process to start even when the full workspace suite runs in parallel.
        let timeout = Duration::from_secs(3);

        let error =
            WorkspaceSnapshot::create_async(&source, &cache, &mke2fs, Some(timeout), |_| {})
                .await
                .unwrap_err();

        assert!(matches!(
            error,
            WorkspaceError::TimedOut {
                stage: WorkspaceStage::PopulateFilesystem,
                timeout: actual,
            } if actual == timeout
        ));
        assert!(staging_files(&cache).is_empty());
        let pid_file = root.path().join("fake-mke2fs.pid");
        let pid = fs::read_to_string(pid_file).unwrap();
        let status = StdCommand::new("/bin/kill")
            .args(["-0", pid.trim()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(!status.success(), "timed-out mke2fs process still exists");
    }
}
