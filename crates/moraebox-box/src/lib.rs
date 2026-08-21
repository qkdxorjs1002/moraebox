//! Persistent root filesystem state for otherwise disposable moraebox runs.

#![forbid(unsafe_code)]

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    str::FromStr,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use fs2::FileExt;
use moraebox_core::{BoxId, StorageRootError, ensure_private_storage_root};
use serde::{Deserialize, Serialize};
use thiserror::Error;

mod disk;

pub use disk::{
    BASE_DISK_LAYOUT_VERSION, BaseDisk, BaseDiskMetadata, BaseDiskSpec, BaseDiskStore,
    DEFAULT_BOX_DISK_SIZE_BYTES, EphemeralDisk, EphemeralDiskStore, EphemeralGcReport,
};

const SCHEMA_VERSION: u32 = 1;
const BOXES_DIRECTORY: &str = "boxes";
const METADATA_FILE: &str = "metadata.json";
const ROOT_DISK_FILE: &str = "root.ext4";
const LOCK_FILE: &str = ".lock";
static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoxState {
    Ready,
    Dirty,
    NeedsRepair,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoxDiskFormat {
    RawExt4,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoxMetadata {
    pub schema_version: u32,
    pub box_id: BoxId,
    pub state: BoxState,
    pub manifest_digest: String,
    pub platform: String,
    pub disk_format: BoxDiskFormat,
    pub virtual_size_bytes: u64,
    pub generation: u64,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
    pub owner_uid: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateBox {
    pub manifest_digest: String,
    pub platform: String,
    pub virtual_size_bytes: u64,
}

impl CreateBox {
    pub fn new(
        manifest_digest: impl Into<String>,
        platform: impl Into<String>,
        virtual_size_bytes: u64,
    ) -> Self {
        Self {
            manifest_digest: manifest_digest.into(),
            platform: platform.into(),
            virtual_size_bytes,
        }
    }

    fn validate(&self) -> Result<(), BoxStoreError> {
        if self.manifest_digest.trim().is_empty() {
            return Err(BoxStoreError::InvalidMetadata(
                "manifest digest must not be empty".into(),
            ));
        }
        if self.platform.trim().is_empty() {
            return Err(BoxStoreError::InvalidMetadata(
                "platform must not be empty".into(),
            ));
        }
        if self.virtual_size_bytes == 0 {
            return Err(BoxStoreError::InvalidMetadata(
                "virtual disk size must be greater than zero".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct BoxStore {
    state_root: PathBuf,
}

#[derive(Debug)]
pub struct BoxLease {
    lock_file: File,
    directory: PathBuf,
    disk_path: PathBuf,
    metadata: BoxMetadata,
}

impl BoxLease {
    pub fn id(&self) -> BoxId {
        self.metadata.box_id
    }

    pub fn metadata(&self) -> &BoxMetadata {
        &self.metadata
    }

    pub fn disk_path(&self) -> &Path {
        &self.disk_path
    }
}

impl BoxStore {
    pub fn new(state_root: impl Into<PathBuf>) -> Self {
        Self {
            state_root: state_root.into(),
        }
    }

    pub fn state_root(&self) -> &Path {
        &self.state_root
    }

    pub fn boxes_directory(&self) -> PathBuf {
        self.state_root.join(BOXES_DIRECTORY)
    }

    pub fn create(
        &self,
        request: &CreateBox,
        source_disk: &Path,
    ) -> Result<BoxMetadata, BoxStoreError> {
        self.ensure_root()?;
        request.validate()?;
        validate_regular_file(source_disk, "source root disk")?;
        let source_size = fs::metadata(source_disk)?.len();
        if source_size != request.virtual_size_bytes {
            return Err(BoxStoreError::InvalidMetadata(format!(
                "source root disk size {source_size} does not match virtual size {}",
                request.virtual_size_bytes
            )));
        }

        secure_directory(&self.state_root)?;
        let boxes = self.boxes_directory();
        secure_directory(&boxes)?;

        loop {
            let box_id = BoxId::new();
            let destination = self.box_directory(box_id);
            if destination.symlink_metadata().is_ok() {
                continue;
            }
            return self.create_with_id(box_id, request, source_disk);
        }
    }

    pub fn get(&self, box_id: BoxId) -> Result<BoxMetadata, BoxStoreError> {
        let paths = self.checked_paths(box_id)?;
        read_metadata(&paths.metadata, box_id)
    }

    pub fn list(&self) -> Result<Vec<BoxMetadata>, BoxStoreError> {
        self.ensure_root()?;
        let directory = self.boxes_directory();
        if !directory.exists() {
            return Ok(Vec::new());
        }
        validate_directory(&directory, "box store")?;
        let mut values = Vec::new();
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                return Err(BoxStoreError::CorruptStore(
                    "box directory name is not valid UTF-8".into(),
                ));
            };
            if name.starts_with('.') {
                continue;
            }
            let box_id = BoxId::from_str(name).map_err(|_| {
                BoxStoreError::CorruptStore(format!("invalid box directory name: {name}"))
            })?;
            values.push(self.get(box_id)?);
        }
        values.sort_by_key(|metadata| metadata.box_id.to_string());
        Ok(values)
    }

    pub fn try_acquire(&self, box_id: BoxId) -> Result<BoxLease, BoxStoreError> {
        let paths = self.checked_paths(box_id)?;
        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&paths.lock)?;
        set_file_permissions(&paths.lock)?;
        FileExt::try_lock_exclusive(&lock_file)
            .map_err(|source| BoxStoreError::Busy { box_id, source })?;
        let metadata = read_metadata(&paths.metadata, box_id)?;
        if metadata.state == BoxState::NeedsRepair {
            return Err(BoxStoreError::NeedsRepair(box_id));
        }
        validate_regular_file(&paths.disk, "box root disk")?;
        Ok(BoxLease {
            lock_file,
            directory: paths.directory,
            disk_path: paths.disk,
            metadata,
        })
    }

    pub fn set_state(&self, lease: &mut BoxLease, state: BoxState) -> Result<(), BoxStoreError> {
        let expected = self.box_directory(lease.id());
        if lease.directory != expected {
            return Err(BoxStoreError::CorruptStore(
                "box lease belongs to another store".into(),
            ));
        }
        lease.metadata.state = state;
        lease.metadata.updated_at_unix_ms = now_unix_millis()?;
        let metadata = lease.directory.join(METADATA_FILE);
        if state == BoxState::NeedsRepair {
            write_json_atomic(&metadata, &lease.metadata)
        } else {
            write_json_atomic_transient(&metadata, &lease.metadata)
        }
    }

    pub fn delete(&self, box_id: BoxId) -> Result<BoxMetadata, BoxStoreError> {
        let lease = self.try_acquire(box_id)?;
        let metadata = lease.metadata.clone();
        let tombstone = self.temporary_path("deleted", box_id);
        fs::rename(&lease.directory, &tombstone)?;
        drop(lease);
        fs::remove_dir_all(tombstone)?;
        Ok(metadata)
    }

    pub fn reset(&self, box_id: BoxId, source_disk: &Path) -> Result<BoxMetadata, BoxStoreError> {
        validate_regular_file(source_disk, "source root disk")?;
        let mut lease = self.try_acquire(box_id)?;
        let source_size = fs::metadata(source_disk)?.len();
        if source_size != lease.metadata.virtual_size_bytes {
            return Err(BoxStoreError::InvalidMetadata(format!(
                "source root disk size {source_size} does not match box virtual size {}",
                lease.metadata.virtual_size_bytes
            )));
        }
        let replacement = lease.directory.join(format!(
            ".{ROOT_DISK_FILE}.{}.tmp",
            TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        copy_disk(source_disk, &replacement)?;
        fs::rename(&replacement, &lease.disk_path)?;
        lease.metadata.generation = lease.metadata.generation.saturating_add(1);
        lease.metadata.state = BoxState::Ready;
        lease.metadata.updated_at_unix_ms = now_unix_millis()?;
        write_json_atomic(&lease.directory.join(METADATA_FILE), &lease.metadata)?;
        Ok(lease.metadata.clone())
    }

    pub fn clone_box(&self, source_id: BoxId) -> Result<BoxMetadata, BoxStoreError> {
        let source = self.try_acquire(source_id)?;
        let request = CreateBox::new(
            source.metadata.manifest_digest.clone(),
            source.metadata.platform.clone(),
            source.metadata.virtual_size_bytes,
        );
        self.create(&request, source.disk_path())
    }

    fn create_with_id(
        &self,
        box_id: BoxId,
        request: &CreateBox,
        source_disk: &Path,
    ) -> Result<BoxMetadata, BoxStoreError> {
        let destination = self.box_directory(box_id);
        let staging = self.temporary_path("creating", box_id);
        if staging.symlink_metadata().is_ok() {
            remove_managed_directory(&staging)?;
        }
        secure_directory(&staging)?;
        let disk = staging.join(ROOT_DISK_FILE);
        let result = (|| {
            copy_disk(source_disk, &disk)?;
            let now = now_unix_millis()?;
            let metadata = BoxMetadata {
                schema_version: SCHEMA_VERSION,
                box_id,
                state: BoxState::Ready,
                manifest_digest: request.manifest_digest.clone(),
                platform: request.platform.clone(),
                disk_format: BoxDiskFormat::RawExt4,
                virtual_size_bytes: request.virtual_size_bytes,
                generation: 0,
                created_at_unix_ms: now,
                updated_at_unix_ms: now,
                owner_uid: owner_uid(&staging)?,
            };
            write_json_atomic(&staging.join(METADATA_FILE), &metadata)?;
            let lock = staging.join(LOCK_FILE);
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock)?
                .sync_all()?;
            set_file_permissions(&lock)?;
            fs::rename(&staging, &destination)?;
            sync_parent(&destination)?;
            Ok(metadata)
        })();
        if result.is_err() && staging.symlink_metadata().is_ok() {
            let _ = remove_managed_directory(&staging);
        }
        result
    }

    fn checked_paths(&self, box_id: BoxId) -> Result<BoxPaths, BoxStoreError> {
        self.ensure_root()?;
        let directory = self.box_directory(box_id);
        if !directory.exists() {
            return Err(BoxStoreError::NotFound(box_id));
        }
        validate_directory(&directory, "box directory")?;
        let paths = BoxPaths {
            metadata: directory.join(METADATA_FILE),
            disk: directory.join(ROOT_DISK_FILE),
            lock: directory.join(LOCK_FILE),
            directory,
        };
        validate_regular_file(&paths.metadata, "box metadata")?;
        validate_regular_file(&paths.disk, "box root disk")?;
        if paths.lock.exists() {
            validate_regular_file(&paths.lock, "box lock")?;
        }
        Ok(paths)
    }

    fn box_directory(&self, box_id: BoxId) -> PathBuf {
        self.boxes_directory().join(box_id.to_string())
    }

    fn temporary_path(&self, operation: &str, box_id: BoxId) -> PathBuf {
        self.boxes_directory().join(format!(
            ".{operation}-{box_id}-{}-{}",
            std::process::id(),
            TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn ensure_root(&self) -> Result<(), BoxStoreError> {
        ensure_private_storage_root(&self.state_root)?;
        Ok(())
    }
}

impl Drop for BoxLease {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.lock_file);
    }
}

#[derive(Debug)]
struct BoxPaths {
    directory: PathBuf,
    metadata: PathBuf,
    disk: PathBuf,
    lock: PathBuf,
}

fn read_metadata(path: &Path, expected_id: BoxId) -> Result<BoxMetadata, BoxStoreError> {
    let bytes = fs::read(path)?;
    let value: BoxMetadata = serde_json::from_slice(&bytes).map_err(|source| {
        BoxStoreError::InvalidMetadata(format!("{}: {source}", path.display()))
    })?;
    if value.schema_version != SCHEMA_VERSION {
        return Err(BoxStoreError::UnsupportedSchema {
            expected: SCHEMA_VERSION,
            actual: value.schema_version,
        });
    }
    if value.box_id != expected_id {
        return Err(BoxStoreError::CorruptStore(format!(
            "metadata at {} belongs to box {} instead of {expected_id}",
            path.display(),
            value.box_id
        )));
    }
    if value.manifest_digest.trim().is_empty()
        || value.platform.trim().is_empty()
        || value.virtual_size_bytes == 0
    {
        return Err(BoxStoreError::InvalidMetadata(format!(
            "required fields are missing in {}",
            path.display()
        )));
    }
    Ok(value)
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<(), BoxStoreError> {
    write_json_atomic_with_durability(path, value, true)
}

fn write_json_atomic_transient(path: &Path, value: &impl Serialize) -> Result<(), BoxStoreError> {
    // Rename makes state transitions atomic for process and parent-loss recovery. Structural
    // metadata writes remain fsync-backed; per-run state avoids a storage flush on the hot path.
    write_json_atomic_with_durability(path, value, false)
}

fn write_json_atomic_with_durability(
    path: &Path,
    value: &impl Serialize,
    durable: bool,
) -> Result<(), BoxStoreError> {
    let parent = path
        .parent()
        .ok_or_else(|| BoxStoreError::InvalidPath(path.into()))?;
    validate_directory(parent, "metadata parent")?;
    let temporary = parent.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("metadata"),
        std::process::id(),
        TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let bytes = serde_json::to_vec_pretty(value)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    if durable {
        file.sync_all()?;
    }
    drop(file);
    set_file_permissions(&temporary)?;
    fs::rename(&temporary, path)?;
    if durable {
        sync_parent(path)?;
    }
    Ok(())
}

fn copy_disk(source: &Path, destination: &Path) -> Result<(), BoxStoreError> {
    validate_regular_file(source, "source root disk")?;
    if destination.symlink_metadata().is_ok() {
        return Err(BoxStoreError::InvalidPath(destination.into()));
    }
    fs::copy(source, destination)?;
    set_file_permissions(destination)?;
    File::open(destination)?.sync_all()?;
    Ok(())
}

fn secure_directory(path: &Path) -> Result<(), BoxStoreError> {
    fs::create_dir_all(path)?;
    validate_directory(path, "managed directory")?;
    set_directory_permissions(path)?;
    Ok(())
}

fn validate_directory(path: &Path, label: &str) -> Result<(), BoxStoreError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(BoxStoreError::UnsafeFileType {
            label: label.into(),
            path: path.into(),
        });
    }
    Ok(())
}

fn validate_regular_file(path: &Path, label: &str) -> Result<(), BoxStoreError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(BoxStoreError::UnsafeFileType {
            label: label.into(),
            path: path.into(),
        });
    }
    Ok(())
}

fn remove_managed_directory(path: &Path) -> Result<(), BoxStoreError> {
    validate_directory(path, "managed directory")?;
    fs::remove_dir_all(path)?;
    Ok(())
}

#[cfg(unix)]
fn set_directory_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_directory_permissions(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_file_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_file_permissions(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn owner_uid(path: &Path) -> Result<Option<u32>, BoxStoreError> {
    use std::os::unix::fs::MetadataExt;
    Ok(Some(fs::metadata(path)?.uid()))
}

#[cfg(not(unix))]
fn owner_uid(_path: &Path) -> Result<Option<u32>, BoxStoreError> {
    Ok(None)
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn now_unix_millis() -> Result<u64, BoxStoreError> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| BoxStoreError::ClockBeforeUnixEpoch)?;
    u64::try_from(elapsed.as_millis()).map_err(|_| BoxStoreError::ClockOverflow)
}

#[derive(Debug, Error)]
pub enum BoxStoreError {
    #[error("box not found: {0}")]
    NotFound(BoxId),
    #[error("box is already in use: {box_id}")]
    Busy {
        box_id: BoxId,
        #[source]
        source: io::Error,
    },
    #[error("box requires repair before it can run: {0}")]
    NeedsRepair(BoxId),
    #[error("base disk preparation is busy at {}: {source}", .path.display())]
    BaseDiskBusy {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("mke2fs failed with status {status:?}: {stderr}")]
    Mke2fs { status: Option<i32>, stderr: String },
    #[error("copy-on-write cloning is unavailable: {detail}")]
    CowCloneUnavailable { detail: String },
    #[error("ephemeral disk already exists for session {0}")]
    EphemeralExists(moraebox_core::SessionId),
    #[error("unsupported box metadata schema: expected {expected}, got {actual}")]
    UnsupportedSchema { expected: u32, actual: u32 },
    #[error("invalid box metadata: {0}")]
    InvalidMetadata(String),
    #[error("corrupt box store: {0}")]
    CorruptStore(String),
    #[error("unsafe {label} file type at {}", path.display())]
    UnsafeFileType { label: String, path: PathBuf },
    #[error("invalid managed path: {}", .0.display())]
    InvalidPath(PathBuf),
    #[error(transparent)]
    StorageRoot(#[from] StorageRootError),
    #[error("system clock is before the Unix epoch")]
    ClockBeforeUnixEpoch,
    #[error("system clock value does not fit in u64 milliseconds")]
    ClockOverflow,
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    const DISK_BYTES: u64 = 1024 * 1024;

    struct Fixture {
        temporary: tempfile::TempDir,
        store: BoxStore,
        base: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let temporary = tempfile::tempdir().unwrap();
            let base = temporary.path().join("base.ext4");
            let file = File::create(&base).unwrap();
            file.set_len(DISK_BYTES).unwrap();
            let store = BoxStore::new(temporary.path().join("state"));
            Self {
                temporary,
                store,
                base,
            }
        }

        fn create(&self) -> BoxMetadata {
            self.store
                .create(
                    &CreateBox::new("sha256:abc", "linux/arm64", DISK_BYTES),
                    &self.base,
                )
                .unwrap()
        }
    }

    #[test]
    fn creates_and_reads_a_versioned_box() {
        let fixture = Fixture::new();
        let created = fixture.create();
        let loaded = fixture.store.get(created.box_id).unwrap();

        assert_eq!(loaded, created);
        assert_eq!(loaded.state, BoxState::Ready);
        assert_eq!(loaded.disk_format, BoxDiskFormat::RawExt4);
        assert_eq!(loaded.generation, 0);
        assert_eq!(
            fs::metadata(
                fixture
                    .store
                    .box_directory(created.box_id)
                    .join(ROOT_DISK_FILE)
            )
            .unwrap()
            .len(),
            DISK_BYTES
        );
    }

    #[cfg(unix)]
    #[test]
    fn applies_private_directory_and_file_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = Fixture::new();
        let created = fixture.create();
        let directory = fixture.store.box_directory(created.box_id);

        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(directory.join(ROOT_DISK_FILE))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn list_tightens_state_root_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let state = temporary.path().join("state");
        fs::create_dir(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o777)).unwrap();
        let store = BoxStore::new(&state);

        assert!(store.list().unwrap().is_empty());
        assert_eq!(
            fs::metadata(state).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[cfg(unix)]
    #[test]
    fn list_rejects_symlink_state_root() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let target = temporary.path().join("target");
        let state = temporary.path().join("state");
        fs::create_dir(&target).unwrap();
        symlink(target, &state).unwrap();
        let store = BoxStore::new(&state);

        assert!(matches!(
            store.list(),
            Err(BoxStoreError::StorageRoot(StorageRootError::UnsafeFileType(path)))
                if path == state
        ));
    }

    #[test]
    fn rejects_a_second_writer_without_waiting() {
        let fixture = Fixture::new();
        let created = fixture.create();
        let _lease = fixture.store.try_acquire(created.box_id).unwrap();

        assert!(matches!(
            fixture.store.try_acquire(created.box_id),
            Err(BoxStoreError::Busy { box_id, .. }) if box_id == created.box_id
        ));
    }

    #[test]
    fn records_state_changes_while_the_lease_is_held() {
        let fixture = Fixture::new();
        let created = fixture.create();
        let mut lease = fixture.store.try_acquire(created.box_id).unwrap();

        fixture
            .store
            .set_state(&mut lease, BoxState::Dirty)
            .unwrap();
        drop(lease);

        assert_eq!(
            fixture.store.get(created.box_id).unwrap().state,
            BoxState::Dirty
        );
    }

    #[test]
    fn needs_repair_boxes_cannot_be_acquired() {
        let fixture = Fixture::new();
        let created = fixture.create();
        let mut lease = fixture.store.try_acquire(created.box_id).unwrap();
        fixture
            .store
            .set_state(&mut lease, BoxState::NeedsRepair)
            .unwrap();
        drop(lease);

        assert!(matches!(
            fixture.store.try_acquire(created.box_id),
            Err(BoxStoreError::NeedsRepair(box_id)) if box_id == created.box_id
        ));
    }

    #[test]
    fn clones_into_an_independent_box() {
        let fixture = Fixture::new();
        let source = fixture.create();
        let cloned = fixture.store.clone_box(source.box_id).unwrap();

        assert_ne!(source.box_id, cloned.box_id);
        assert_eq!(source.manifest_digest, cloned.manifest_digest);
        assert_eq!(source.virtual_size_bytes, cloned.virtual_size_bytes);
        assert_eq!(fixture.store.list().unwrap().len(), 2);
    }

    #[test]
    fn reset_replaces_the_disk_and_advances_generation() {
        let fixture = Fixture::new();
        let created = fixture.create();
        let replacement = fixture.temporary.path().join("replacement.ext4");
        let mut bytes = vec![0_u8; usize::try_from(DISK_BYTES).unwrap()];
        bytes[0] = 42;
        fs::write(&replacement, bytes).unwrap();

        let reset = fixture.store.reset(created.box_id, &replacement).unwrap();

        assert_eq!(reset.generation, 1);
        let disk = fixture
            .store
            .box_directory(created.box_id)
            .join(ROOT_DISK_FILE);
        assert_eq!(fs::read(disk).unwrap()[0], 42);
    }

    #[test]
    fn delete_removes_only_the_selected_box() {
        let fixture = Fixture::new();
        let first = fixture.create();
        let second = fixture.create();

        fixture.store.delete(first.box_id).unwrap();

        assert!(matches!(
            fixture.store.get(first.box_id),
            Err(BoxStoreError::NotFound(_))
        ));
        assert_eq!(fixture.store.get(second.box_id).unwrap(), second);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_source_disks() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let link = fixture.temporary.path().join("root-link.ext4");
        symlink(&fixture.base, &link).unwrap();

        assert!(matches!(
            fixture.store.create(
                &CreateBox::new("sha256:abc", "linux/arm64", DISK_BYTES),
                &link
            ),
            Err(BoxStoreError::UnsafeFileType { .. })
        ));
    }
}
