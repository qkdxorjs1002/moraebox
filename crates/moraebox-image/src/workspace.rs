use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read},
    path::{Component, Path, PathBuf},
    process::{Command as StdCommand, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use sha2::{Digest as _, Sha256};
use thiserror::Error;
use tokio::io::AsyncReadExt;

use crate::Digest;

const MIN_IMAGE_BYTES: u64 = 64 * 1024 * 1024;
const IMAGE_HEADROOM_BYTES: u64 = 32 * 1024 * 1024;
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
            Self::HashImage => "hashing ext4 image",
            Self::VerifySource => "verifying source is unchanged",
        })
    }
}

#[derive(Debug, Clone)]
pub struct WorkspaceSnapshot {
    pub source: PathBuf,
    pub source_digest: Digest,
    pub image_path: PathBuf,
    pub image_digest: Digest,
}

impl WorkspaceSnapshot {
    pub fn create(source: &Path, cache_root: &Path, mke2fs: &Path) -> Result<Self, WorkspaceError> {
        let source = source.canonicalize()?;
        if !source.is_dir() {
            return Err(WorkspaceError::NotDirectory(source));
        }
        let cache_root = resolve_for_overlap(cache_root)?;
        ensure_disjoint(&source, &cache_root)?;
        let (source_digest, content_bytes) = digest_tree_with_size(&source)?;
        let directory = cache_root.join("workspaces/sha256");
        fs::create_dir_all(&directory)?;
        let image_path = directory.join(format!("{}.ext4", source_digest.hex()));
        if !image_path.exists() {
            let staging = StagingImage::create(&directory, &source_digest, content_bytes)?;
            let output = StdCommand::new(mke2fs)
                .args(["-q", "-t", "ext4", "-F", "-d"])
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
        let image_digest = digest_file(&image_path)?;
        let snapshot = Self {
            source,
            source_digest,
            image_path,
            image_digest,
        };
        snapshot.verify_source_unchanged()?;
        Ok(snapshot)
    }

    pub async fn create_async<F>(
        source: &Path,
        cache_root: &Path,
        mke2fs: &Path,
        stage_timeout: Option<Duration>,
        mut progress: F,
    ) -> Result<Self, WorkspaceError>
    where
        F: FnMut(WorkspaceStage),
    {
        let deadline = WorkspaceDeadline::new(stage_timeout);
        let source = source.canonicalize()?;
        if !source.is_dir() {
            return Err(WorkspaceError::NotDirectory(source));
        }
        let cache_root = resolve_for_overlap(cache_root)?;
        ensure_disjoint(&source, &cache_root)?;

        let scan_timeout = deadline.remaining(WorkspaceStage::ScanSource)?;
        progress(WorkspaceStage::ScanSource);
        let digest_source = source.clone();
        let (source_digest, content_bytes) =
            run_blocking_stage(WorkspaceStage::ScanSource, scan_timeout, move |cancelled| {
                digest_tree_with_size_cancel(&digest_source, &cancelled)
            })
            .await
            .map_err(|error| deadline.normalize_timeout(error))?;

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
                    StagingImage::create(&staging_directory, &staging_digest, content_bytes)
                },
            )
            .await
            .map_err(|error| deadline.normalize_timeout(error))?;
            let populate_timeout = deadline.remaining(WorkspaceStage::PopulateFilesystem)?;
            progress(WorkspaceStage::PopulateFilesystem);
            populate_ext4(mke2fs, &source, staging.path(), populate_timeout)
                .await
                .map_err(|error| deadline.normalize_timeout(error))?;
            set_read_only(staging.path())?;
            staging.publish(&image_path)?;
        }

        let hash_timeout = deadline.remaining(WorkspaceStage::HashImage)?;
        progress(WorkspaceStage::HashImage);
        let digest_image_path = image_path.clone();
        let image_digest =
            run_blocking_stage(WorkspaceStage::HashImage, hash_timeout, move |cancelled| {
                digest_file_cancel(&digest_image_path, &cancelled)
            })
            .await
            .map_err(|error| deadline.normalize_timeout(error))?;

        let verify_timeout = deadline.remaining(WorkspaceStage::VerifySource)?;
        progress(WorkspaceStage::VerifySource);
        let verify_source = source.clone();
        let actual = run_blocking_stage(
            WorkspaceStage::VerifySource,
            verify_timeout,
            move |cancelled| digest_tree_cancel(&verify_source, &cancelled),
        )
        .await
        .map_err(|error| deadline.normalize_timeout(error))?;
        if actual != source_digest {
            return Err(WorkspaceError::SourceChanged {
                expected: source_digest,
                actual,
            });
        }

        Ok(Self {
            source,
            source_digest,
            image_path,
            image_digest,
        })
    }

    pub fn verify_source_unchanged(&self) -> Result<(), WorkspaceError> {
        let actual = digest_tree(&self.source)?;
        if actual == self.source_digest {
            Ok(())
        } else {
            Err(WorkspaceError::SourceChanged {
                expected: self.source_digest.clone(),
                actual,
            })
        }
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
    digest_tree_with_size(root).map(|(digest, _)| digest)
}

fn digest_tree_with_size(root: &Path) -> Result<(Digest, u64), WorkspaceError> {
    digest_tree_with_size_impl(root, None)
}

fn digest_tree_with_size_cancel(
    root: &Path,
    cancelled: &AtomicBool,
) -> Result<(Digest, u64), WorkspaceError> {
    digest_tree_with_size_impl(root, Some(cancelled))
}

fn digest_tree_with_size_impl(
    root: &Path,
    cancelled: Option<&AtomicBool>,
) -> Result<(Digest, u64), WorkspaceError> {
    let mut hasher = Sha256::new();
    let mut content_bytes = 0_u64;
    walk(root, root, cancelled, &mut |relative, metadata| {
        check_cancelled(cancelled)?;
        hash_path(&mut hasher, relative);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            hasher.update(metadata.mode().to_le_bytes());
        }
        let kind = metadata.file_type();
        if kind.is_file() {
            hasher.update(b"file\0");
            let mut file = File::open(root.join(relative))?;
            let mut buffer = vec![0_u8; 64 * 1024];
            loop {
                check_cancelled(cancelled)?;
                let count = file.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                hasher.update(&buffer[..count]);
                content_bytes =
                    content_bytes.saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
            }
        } else if kind.is_dir() {
            hasher.update(b"dir\0");
        } else if kind.is_symlink() {
            hasher.update(b"symlink\0");
            let target = fs::read_link(root.join(relative))?;
            hash_path(&mut hasher, &target);
        } else {
            return Err(WorkspaceError::UnsupportedFile(root.join(relative)));
        }
        Ok(())
    })?;
    Ok((Digest::from_sha256(hasher.finalize().into()), content_bytes))
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

fn digest_file(path: &Path) -> Result<Digest, WorkspaceError> {
    digest_file_impl(path, None)
}

fn digest_file_cancel(path: &Path, cancelled: &AtomicBool) -> Result<Digest, WorkspaceError> {
    digest_file_impl(path, Some(cancelled))
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

fn digest_tree_cancel(root: &Path, cancelled: &AtomicBool) -> Result<Digest, WorkspaceError> {
    digest_tree_with_size_cancel(root, cancelled).map(|(digest, _)| digest)
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
        content_bytes: u64,
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
                    file.set_len(image_size(content_bytes))?;
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
    timeout: Option<Duration>,
) -> Result<(), WorkspaceError> {
    let mut command = tokio::process::Command::new(mke2fs);
    command
        .args(["-q", "-t", "ext4", "-F", "-d"])
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

fn image_size(content_bytes: u64) -> u64 {
    let requested = content_bytes
        .saturating_mul(2)
        .saturating_add(IMAGE_HEADROOM_BYTES)
        .max(MIN_IMAGE_BYTES);
    requested.next_multiple_of(4 * 1024 * 1024)
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
    #[error(
        "workspace source and cache must not overlap: source={}, cache={}",
        workspace_source.display(),
        cache.display()
    )]
    OverlappingPaths {
        workspace_source: PathBuf,
        cache: PathBuf,
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_stages_share_one_absolute_deadline() {
        let limit = Duration::from_millis(50);
        let deadline = WorkspaceDeadline::new(Some(limit));
        let first = deadline
            .remaining(WorkspaceStage::ScanSource)
            .unwrap()
            .unwrap();
        std::thread::sleep(Duration::from_millis(10));
        let second = deadline
            .remaining(WorkspaceStage::CreateImage)
            .unwrap()
            .unwrap();
        assert!(second < first);

        std::thread::sleep(second + Duration::from_millis(2));
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
    fn image_has_headroom_and_alignment() {
        assert_eq!(image_size(1), MIN_IMAGE_BYTES);
        assert_eq!(image_size(100 * 1024 * 1024) % (4 * 1024 * 1024), 0);
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

    #[cfg(unix)]
    #[tokio::test]
    async fn async_snapshot_reports_stages_and_publishes_read_only_image() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let cache = root.path().join("cache");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("file"), b"content").unwrap();
        let mke2fs = executable_script(root.path(), "#!/bin/sh\nexit 0\n");
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
        let timeout = Duration::from_secs(1);

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
