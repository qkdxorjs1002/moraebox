use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};

use fs2::FileExt;
use moraebox_core::BoxId;
use serde::{Deserialize, Serialize};

use super::{
    BoxDiskFormat, BoxMetadata, BoxState, BoxStore, BoxStoreError, CreateBox,
    GarbageCollectionReport, LOCK_FILE, METADATA_FILE, ROOT_DISK_FILE, TEMPORARY_SEQUENCE,
    allocated_size_bytes, copy_disk, garbage_collection_candidate_is_old, garbage_collection_lock,
    now_unix_millis, remove_managed_directory, secure_directory, set_file_permissions,
    set_read_only, sync_parent, validate_directory, validate_labels, validate_optional_name,
    validate_regular_file, validate_tags, write_json_atomic,
};

const CHECKPOINTS_DIRECTORY: &str = "checkpoints";
const CHECKPOINT_SCHEMA_VERSION: u32 = 1;
const CHECKPOINT_METADATA_LOCK_FILE: &str = ".metadata.lock";

/// Identifies an immutable, point-in-time copy of a Box root disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CheckpointId(BoxId);

impl CheckpointId {
    pub fn new() -> Self {
        Self(BoxId::new())
    }
}

impl Default for CheckpointId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for CheckpointId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for CheckpointId {
    type Err = <BoxId as FromStr>::Err;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        BoxId::from_str(value).map(Self)
    }
}

/// Durable metadata written beside every checkpoint disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointMetadata {
    pub schema_version: u32,
    pub checkpoint_id: CheckpointId,
    pub source_box_id: BoxId,
    pub source_generation: u64,
    pub manifest_digest: String,
    pub platform: String,
    pub disk_format: BoxDiskFormat,
    pub virtual_size_bytes: u64,
    pub physical_size_bytes: u64,
    pub created_at_unix_ms: u64,
    pub owner_uid: Option<u32>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub tags: BTreeSet<String>,
}

/// Optional user metadata attached to a checkpoint at creation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CreateCheckpoint {
    pub name: Option<String>,
    pub labels: BTreeMap<String, String>,
    pub tags: BTreeSet<String>,
}

impl CreateCheckpoint {
    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    #[must_use]
    pub fn with_labels(mut self, labels: BTreeMap<String, String>) -> Self {
        self.labels = labels;
        self
    }

    #[must_use]
    pub fn with_tags(mut self, tags: BTreeSet<String>) -> Self {
        self.tags = tags;
        self
    }

    fn validate(&self) -> Result<(), BoxStoreError> {
        validate_optional_name(self.name.as_deref())?;
        validate_labels(&self.labels)?;
        validate_tags(&self.tags)
    }
}

/// Optional metadata overrides used when a checkpoint is forked into a Box.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ForkCheckpoint {
    pub name: Option<String>,
    pub labels: Option<BTreeMap<String, String>>,
    pub tags: Option<BTreeSet<String>>,
}

impl ForkCheckpoint {
    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    #[must_use]
    pub fn with_labels(mut self, labels: BTreeMap<String, String>) -> Self {
        self.labels = Some(labels);
        self
    }

    #[must_use]
    pub fn with_tags(mut self, tags: BTreeSet<String>) -> Self {
        self.tags = Some(tags);
        self
    }

    fn validate(&self) -> Result<(), BoxStoreError> {
        validate_optional_name(self.name.as_deref())?;
        if let Some(labels) = &self.labels {
            validate_labels(labels)?;
        }
        if let Some(tags) = &self.tags {
            validate_tags(tags)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointEntryError {
    pub entry_name: String,
    pub checkpoint_id: Option<CheckpointId>,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointListReport {
    pub checkpoints: Vec<CheckpointMetadata>,
    pub errors: Vec<CheckpointEntryError>,
}

/// Filesystem store for immutable Box checkpoints.
#[derive(Debug, Clone)]
pub struct CheckpointStore {
    state_root: PathBuf,
}

#[derive(Debug)]
struct CheckpointLease {
    lock_file: File,
    directory: PathBuf,
    disk_path: PathBuf,
    metadata: CheckpointMetadata,
}

impl Drop for CheckpointLease {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.lock_file);
    }
}

impl CheckpointStore {
    pub fn new(state_root: impl Into<PathBuf>) -> Self {
        Self {
            state_root: state_root.into(),
        }
    }

    pub fn state_root(&self) -> &Path {
        &self.state_root
    }

    pub fn checkpoints_directory(&self) -> PathBuf {
        self.state_root.join(CHECKPOINTS_DIRECTORY)
    }

    pub fn create(&self, source_box_id: BoxId) -> Result<CheckpointMetadata, BoxStoreError> {
        self.create_with(source_box_id, &CreateCheckpoint::default())
    }

    pub fn create_with(
        &self,
        source_box_id: BoxId,
        request: &CreateCheckpoint,
    ) -> Result<CheckpointMetadata, BoxStoreError> {
        self.ensure_root()?;
        request.validate()?;
        secure_directory(&self.checkpoints_directory())?;
        let source_store = BoxStore::new(&self.state_root);
        let source = source_store.try_acquire(source_box_id)?;
        if source.metadata().state != BoxState::Ready {
            return Err(BoxStoreError::CheckpointSourceNotReady {
                box_id: source_box_id,
                state: source.metadata().state,
            });
        }

        loop {
            let checkpoint_id = CheckpointId::new();
            let destination = self.checkpoint_directory(checkpoint_id);
            if destination.symlink_metadata().is_ok() {
                continue;
            }
            return self.create_with_id(
                checkpoint_id,
                source_box_id,
                source.metadata(),
                source.disk_path(),
                request,
            );
        }
    }

    pub fn get(&self, checkpoint_id: CheckpointId) -> Result<CheckpointMetadata, BoxStoreError> {
        Ok(self.checked_paths(checkpoint_id)?.metadata)
    }

    pub fn list(&self) -> Result<CheckpointListReport, BoxStoreError> {
        self.ensure_root()?;
        let directory = self.checkpoints_directory();
        if !directory.exists() {
            return Ok(CheckpointListReport::default());
        }
        validate_directory(&directory, "checkpoint store")?;
        let mut report = CheckpointListReport::default();
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let name = entry.file_name();
            if name.to_string_lossy().starts_with('.') {
                continue;
            }
            let Some(name) = name.to_str() else {
                report.errors.push(CheckpointEntryError {
                    entry_name: name.to_string_lossy().into_owned(),
                    checkpoint_id: None,
                    message: "checkpoint directory name is not valid UTF-8".into(),
                });
                continue;
            };
            let Ok(checkpoint_id) = CheckpointId::from_str(name) else {
                report.errors.push(CheckpointEntryError {
                    entry_name: name.into(),
                    checkpoint_id: None,
                    message: format!("invalid checkpoint directory name: {name}"),
                });
                continue;
            };
            match self.get(checkpoint_id) {
                Ok(metadata) => report.checkpoints.push(metadata),
                Err(error) => report.errors.push(CheckpointEntryError {
                    entry_name: name.into(),
                    checkpoint_id: Some(checkpoint_id),
                    message: error.to_string(),
                }),
            }
        }
        report
            .checkpoints
            .sort_by_key(|metadata| metadata.checkpoint_id.to_string());
        report
            .errors
            .sort_by(|left, right| left.entry_name.cmp(&right.entry_name));
        Ok(report)
    }

    pub fn delete(&self, checkpoint_id: CheckpointId) -> Result<CheckpointMetadata, BoxStoreError> {
        let lease = self.try_acquire(checkpoint_id)?;
        let metadata = lease.metadata.clone();
        let directory = lease.directory.clone();
        #[cfg(windows)]
        drop(lease);
        remove_managed_directory(&directory)?;
        #[cfg(not(windows))]
        drop(lease);
        sync_parent(&directory)?;
        Ok(metadata)
    }

    pub fn fork(
        &self,
        checkpoint_id: CheckpointId,
        request: &ForkCheckpoint,
    ) -> Result<BoxMetadata, BoxStoreError> {
        request.validate()?;
        let checkpoint = self.try_acquire(checkpoint_id)?;
        let metadata = &checkpoint.metadata;
        let mut create = CreateBox::new(
            metadata.manifest_digest.clone(),
            metadata.platform.clone(),
            metadata.virtual_size_bytes,
        );
        if let Some(name) = &request.name {
            create = create.with_name(name.clone());
        }
        create = create.with_labels(
            request
                .labels
                .clone()
                .unwrap_or_else(|| metadata.labels.clone()),
        );
        create = create.with_tags(
            request
                .tags
                .clone()
                .unwrap_or_else(|| metadata.tags.clone()),
        );
        BoxStore::new(&self.state_root).create(&create, &checkpoint.disk_path)
    }

    /// Removes only stale directories with the exact checkpoint-creation staging name.
    pub fn garbage_collect_older_than(
        &self,
        minimum_age: Duration,
    ) -> Result<GarbageCollectionReport, BoxStoreError> {
        self.ensure_root()?;
        let directory = self.checkpoints_directory();
        if !directory.exists() {
            return Ok(GarbageCollectionReport::default());
        }
        validate_directory(&directory, "checkpoint store")?;
        let mut report = GarbageCollectionReport::default();
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !is_checkpoint_staging_name(&name) {
                continue;
            }
            let path = entry.path();
            validate_directory(&path, "checkpoint staging directory")?;
            if !garbage_collection_candidate_is_old(&path, minimum_age)? {
                report.skipped_young += 1;
                continue;
            }
            match garbage_collection_lock(&path)? {
                super::GarbageCollectionLock::Acquired(lock) => {
                    #[cfg(windows)]
                    drop(lock);
                    remove_managed_directory(&path)?;
                    sync_parent(&path)?;
                    #[cfg(not(windows))]
                    drop(lock);
                    report.removed += 1;
                }
                super::GarbageCollectionLock::Busy => report.skipped_busy += 1,
            }
        }
        Ok(report)
    }

    fn create_with_id(
        &self,
        checkpoint_id: CheckpointId,
        source_box_id: BoxId,
        source_metadata: &BoxMetadata,
        source_disk: &Path,
        request: &CreateCheckpoint,
    ) -> Result<CheckpointMetadata, BoxStoreError> {
        let staging = self.temporary_path(checkpoint_id);
        let destination = self.checkpoint_directory(checkpoint_id);
        secure_directory(&staging)?;
        let result = (|| {
            let lock_path = staging.join(LOCK_FILE);
            let lock = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&lock_path)?;
            set_file_permissions(&lock_path)?;
            FileExt::lock_exclusive(&lock)?;
            let disk = staging.join(ROOT_DISK_FILE);
            copy_disk(source_disk, &disk)?;
            set_read_only(&disk)?;
            let metadata = CheckpointMetadata {
                schema_version: CHECKPOINT_SCHEMA_VERSION,
                checkpoint_id,
                source_box_id,
                source_generation: source_metadata.generation,
                manifest_digest: source_metadata.manifest_digest.clone(),
                platform: source_metadata.platform.clone(),
                disk_format: BoxDiskFormat::RawExt4,
                virtual_size_bytes: source_metadata.virtual_size_bytes,
                physical_size_bytes: allocated_size_bytes(&disk)?,
                created_at_unix_ms: now_unix_millis()?,
                owner_uid: super::owner_uid(&staging)?,
                name: request.name.clone(),
                labels: request.labels.clone(),
                tags: request.tags.clone(),
            };
            write_json_atomic(&staging.join(METADATA_FILE), &metadata)?;
            lock.sync_all()?;
            #[cfg(windows)]
            {
                let _ = FileExt::unlock(&lock);
                drop(lock);
            }
            fs::rename(&staging, &destination)?;
            sync_parent(&destination)?;
            #[cfg(not(windows))]
            {
                let _ = FileExt::unlock(&lock);
                drop(lock);
            }
            Ok(metadata)
        })();
        if result.is_err() && staging.symlink_metadata().is_ok() {
            let _ = remove_managed_directory(&staging);
        }
        result
    }

    fn try_acquire(&self, checkpoint_id: CheckpointId) -> Result<CheckpointLease, BoxStoreError> {
        let paths = self.checked_paths(checkpoint_id)?;
        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&paths.lock)?;
        set_file_permissions(&paths.lock)?;
        FileExt::try_lock_exclusive(&lock_file).map_err(|source| {
            BoxStoreError::CheckpointBusy {
                checkpoint_id,
                source,
            }
        })?;
        Ok(CheckpointLease {
            lock_file,
            directory: paths.directory,
            disk_path: paths.disk,
            metadata: paths.metadata,
        })
    }

    fn checked_paths(&self, checkpoint_id: CheckpointId) -> Result<CheckpointPaths, BoxStoreError> {
        self.ensure_root()?;
        let directory = self.checkpoint_directory(checkpoint_id);
        match directory.symlink_metadata() {
            Ok(_) => validate_directory(&directory, "checkpoint directory")?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(BoxStoreError::CheckpointNotFound(checkpoint_id));
            }
            Err(error) => return Err(error.into()),
        }
        let metadata_path = directory.join(METADATA_FILE);
        let disk = directory.join(ROOT_DISK_FILE);
        let lock = directory.join(LOCK_FILE);
        validate_regular_file(&metadata_path, "checkpoint metadata")?;
        validate_regular_file(&disk, "checkpoint root disk")?;
        if lock.exists() {
            validate_regular_file(&lock, "checkpoint lock")?;
        }
        let metadata = read_metadata(&metadata_path, checkpoint_id, &disk)?;
        Ok(CheckpointPaths {
            directory,
            disk,
            lock,
            metadata,
        })
    }

    fn ensure_root(&self) -> Result<(), BoxStoreError> {
        moraebox_core::ensure_private_storage_root(&self.state_root)?;
        Ok(())
    }

    fn checkpoint_directory(&self, checkpoint_id: CheckpointId) -> PathBuf {
        self.checkpoints_directory().join(checkpoint_id.to_string())
    }

    fn temporary_path(&self, checkpoint_id: CheckpointId) -> PathBuf {
        self.checkpoints_directory().join(format!(
            ".creating-{checkpoint_id}-{}-{}",
            std::process::id(),
            TEMPORARY_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }

    #[allow(dead_code)]
    fn lock_metadata_index(&self) -> Result<File, BoxStoreError> {
        let path = self
            .checkpoints_directory()
            .join(CHECKPOINT_METADATA_LOCK_FILE);
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        set_file_permissions(&path)?;
        FileExt::lock_exclusive(&lock)?;
        Ok(lock)
    }
}

#[derive(Debug)]
struct CheckpointPaths {
    directory: PathBuf,
    disk: PathBuf,
    lock: PathBuf,
    metadata: CheckpointMetadata,
}

fn read_metadata(
    path: &Path,
    expected_id: CheckpointId,
    disk: &Path,
) -> Result<CheckpointMetadata, BoxStoreError> {
    let bytes = fs::read(path)?;
    let metadata: CheckpointMetadata = serde_json::from_slice(&bytes).map_err(|source| {
        BoxStoreError::InvalidMetadata(format!("{}: {source}", path.display()))
    })?;
    if metadata.schema_version != CHECKPOINT_SCHEMA_VERSION {
        return Err(BoxStoreError::UnsupportedSchema {
            expected: CHECKPOINT_SCHEMA_VERSION,
            actual: metadata.schema_version,
        });
    }
    if metadata.checkpoint_id != expected_id {
        return Err(BoxStoreError::CorruptStore(format!(
            "checkpoint metadata at {} belongs to {} instead of {expected_id}",
            path.display(),
            metadata.checkpoint_id
        )));
    }
    if metadata.manifest_digest.trim().is_empty()
        || metadata.platform.trim().is_empty()
        || metadata.virtual_size_bytes == 0
        || metadata.disk_format != BoxDiskFormat::RawExt4
    {
        return Err(BoxStoreError::InvalidMetadata(format!(
            "required fields are missing in {}",
            path.display()
        )));
    }
    validate_optional_name(metadata.name.as_deref())?;
    validate_labels(&metadata.labels)?;
    validate_tags(&metadata.tags)?;
    validate_regular_file(disk, "checkpoint root disk")?;
    let disk_size = fs::metadata(disk)?.len();
    if disk_size != metadata.virtual_size_bytes {
        return Err(BoxStoreError::CorruptStore(format!(
            "checkpoint root disk size {disk_size} does not match metadata virtual size {}",
            metadata.virtual_size_bytes
        )));
    }
    let physical_size_bytes = allocated_size_bytes(disk)?;
    if physical_size_bytes != metadata.physical_size_bytes {
        return Err(BoxStoreError::CorruptStore(format!(
            "checkpoint root disk allocation does not match metadata at {}",
            path.display()
        )));
    }
    Ok(metadata)
}

fn is_checkpoint_staging_name(name: &str) -> bool {
    let Some(value) = name.strip_prefix(".creating-") else {
        return false;
    };
    let mut parts = value.rsplitn(3, '-');
    let Some(sequence) = parts.next() else {
        return false;
    };
    let Some(process) = parts.next() else {
        return false;
    };
    let Some(checkpoint_id) = parts.next() else {
        return false;
    };
    parts.next().is_none()
        && !sequence.is_empty()
        && sequence.bytes().all(|byte| byte.is_ascii_digit())
        && !process.is_empty()
        && process.bytes().all(|byte| byte.is_ascii_digit())
        && CheckpointId::from_str(checkpoint_id).is_ok()
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::{Seek, SeekFrom, Write},
        time::Duration,
    };

    use tempfile::TempDir;

    use super::*;

    const DISK_BYTES: u64 = 1024 * 1024;

    fn fixture() -> (TempDir, BoxStore, BoxMetadata) {
        let temporary = TempDir::new().unwrap();
        let source = temporary.path().join("source.ext4");
        let disk = File::create(&source).unwrap();
        disk.set_len(DISK_BYTES).unwrap();
        let store = BoxStore::new(temporary.path().join("state"));
        let metadata = store
            .create(
                &CreateBox::new("sha256:checkpoint", "linux/arm64", DISK_BYTES),
                &source,
            )
            .unwrap();
        (temporary, store, metadata)
    }

    #[test]
    fn create_requires_an_idle_ready_box() {
        let (_temporary, store, metadata) = fixture();
        let lease = store.try_acquire(metadata.box_id).unwrap();
        assert!(matches!(
            store.create_checkpoint(metadata.box_id),
            Err(BoxStoreError::Busy { .. })
        ));
        drop(lease);
        let mut lease = store.try_acquire(metadata.box_id).unwrap();
        store.begin_writable_use(&mut lease).unwrap();
        drop(lease);
        assert!(matches!(
            store.create_checkpoint(metadata.box_id),
            Err(BoxStoreError::CheckpointSourceNotReady {
                state: BoxState::Dirty,
                ..
            })
        ));
    }

    #[test]
    fn checkpoint_is_immutable_and_can_be_deleted() {
        let (_temporary, store, metadata) = fixture();
        let checkpoint = store.create_checkpoint(metadata.box_id).unwrap();
        let disk = store
            .checkpoint_store()
            .checkpoint_directory(checkpoint.checkpoint_id)
            .join(ROOT_DISK_FILE);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&disk).unwrap().permissions().mode() & 0o222, 0);
        }
        assert_eq!(
            store.get_checkpoint(checkpoint.checkpoint_id).unwrap(),
            checkpoint
        );
        assert_eq!(
            store.delete_checkpoint(checkpoint.checkpoint_id).unwrap(),
            checkpoint
        );
        assert!(matches!(
            store.get_checkpoint(checkpoint.checkpoint_id),
            Err(BoxStoreError::CheckpointNotFound(_))
        ));
    }

    #[test]
    fn corrupted_checkpoint_metadata_is_reported_without_following_it() {
        let (_temporary, store, source) = fixture();
        let checkpoint = store.create_checkpoint(source.box_id).unwrap();
        let metadata_path = store
            .checkpoint_store()
            .checkpoint_directory(checkpoint.checkpoint_id)
            .join(METADATA_FILE);
        fs::write(metadata_path, b"{").unwrap();
        assert!(matches!(
            store.get_checkpoint(checkpoint.checkpoint_id),
            Err(BoxStoreError::InvalidMetadata(_))
        ));
        let report = store.list_checkpoints().unwrap();
        assert!(report.checkpoints.is_empty());
        assert_eq!(report.errors.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn checkpoint_metadata_symlink_is_rejected() {
        use std::os::unix::fs::symlink;

        let (temporary, store, source) = fixture();
        let checkpoint = store.create_checkpoint(source.box_id).unwrap();
        let metadata_path = store
            .checkpoint_store()
            .checkpoint_directory(checkpoint.checkpoint_id)
            .join(METADATA_FILE);
        fs::remove_file(&metadata_path).unwrap();
        let outside = temporary.path().join("outside.json");
        fs::write(&outside, b"{}").unwrap();
        symlink(&outside, metadata_path).unwrap();
        assert!(matches!(
            store.get_checkpoint(checkpoint.checkpoint_id),
            Err(BoxStoreError::UnsafeFileType { .. })
        ));
    }

    #[test]
    fn garbage_collection_only_removes_exact_stale_staging() {
        let (temporary, store, _metadata) = fixture();
        let checkpoints = store.checkpoint_store();
        let stale = checkpoints.temporary_path(CheckpointId::new());
        secure_directory(&stale).unwrap();
        let unrelated = checkpoints
            .checkpoints_directory()
            .join(".creating-not-a-checkpoint");
        secure_directory(&unrelated).unwrap();
        let report = checkpoints
            .garbage_collect_older_than(Duration::ZERO)
            .unwrap();
        assert_eq!(report.removed, 1);
        assert!(!stale.exists());
        assert!(unrelated.exists());
        drop(temporary);
    }

    #[test]
    fn fork_copies_content_independently() {
        let (_temporary, store, source) = fixture();
        let source_disk = store.box_directory(source.box_id).join(ROOT_DISK_FILE);
        let mut source_file = OpenOptions::new().write(true).open(&source_disk).unwrap();
        source_file.seek(SeekFrom::Start(0)).unwrap();
        source_file.write_all(b"checkpoint-content").unwrap();
        source_file.sync_all().unwrap();
        let checkpoint = store.create_checkpoint(source.box_id).unwrap();
        let fork = store
            .fork_checkpoint(
                checkpoint.checkpoint_id,
                &ForkCheckpoint::default().with_name("forked"),
            )
            .unwrap();
        let fork_disk = store.box_directory(fork.box_id).join(ROOT_DISK_FILE);
        assert_eq!(fs::read(&fork_disk).unwrap()[..18], *b"checkpoint-content");
        let mut fork_file = OpenOptions::new().write(true).open(&fork_disk).unwrap();
        fork_file.seek(SeekFrom::Start(0)).unwrap();
        fork_file.write_all(b"mutated-content!!!").unwrap();
        fork_file.sync_all().unwrap();
        let checkpoint_disk = store
            .checkpoint_store()
            .checkpoint_directory(checkpoint.checkpoint_id)
            .join(ROOT_DISK_FILE);
        assert_eq!(
            fs::read(checkpoint_disk).unwrap()[..18],
            *b"checkpoint-content"
        );
    }
}
