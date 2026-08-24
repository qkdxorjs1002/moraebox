//! Persistent root filesystem state for otherwise disposable moraebox runs.

#![forbid(unsafe_code)]

use std::{
    cmp::Ordering as CmpOrdering,
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    str::FromStr,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use fs2::FileExt;
use moraebox_core::{BoxId, StorageRootError, ensure_private_storage_root};
use serde::{Deserialize, Serialize};
use thiserror::Error;

mod bundle;
mod disk;

use disk::copy_disk;

pub use bundle::{BOX_BUNDLE_SCHEMA_VERSION, BoxBundleReport};
pub use disk::{
    BASE_DISK_LAYOUT_VERSION, BaseDisk, BaseDiskMetadata, BaseDiskSpec, BaseDiskStore,
    DEFAULT_BOX_DISK_SIZE_BYTES, EphemeralDisk, EphemeralDiskStore, EphemeralGcReport,
};

const SCHEMA_VERSION: u32 = 2;
const LEGACY_SCHEMA_VERSION: u32 = 1;
const BOXES_DIRECTORY: &str = "boxes";
const METADATA_FILE: &str = "metadata.json";
const ROOT_DISK_FILE: &str = "root.ext4";
const LOCK_FILE: &str = ".lock";
const METADATA_LOCK_FILE: &str = ".metadata.lock";
const MAX_NAME_CHARS: usize = 64;
const MAX_LABEL_KEY_CHARS: usize = 63;
const MAX_LABEL_VALUE_CHARS: usize = 256;
const MAX_TAG_CHARS: usize = 63;
static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);
pub const DEFAULT_GC_MIN_AGE: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GarbageCollectionReport {
    pub removed: usize,
    pub skipped_busy: usize,
    pub skipped_young: usize,
}

pub(crate) enum GarbageCollectionLock {
    Acquired(Option<File>),
    Busy,
}

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
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub tags: BTreeSet<String>,
    #[serde(default)]
    pub last_used_at_unix_ms: Option<u64>,
    #[serde(default)]
    pub physical_size_bytes: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoxSortBy {
    Name,
    Created,
    Updated,
    LastUsed,
    PhysicalSize,
    VirtualSize,
    #[default]
    Id,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BoxQuery {
    pub name: Option<String>,
    pub labels: BTreeMap<String, Option<String>>,
    pub tags: BTreeSet<String>,
    pub state: Option<BoxState>,
    pub sort_by: BoxSortBy,
    pub descending: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpdateBox {
    pub name: Option<String>,
    pub clear_name: bool,
    pub set_labels: BTreeMap<String, String>,
    pub remove_labels: BTreeSet<String>,
    pub add_tags: BTreeSet<String>,
    pub remove_tags: BTreeSet<String>,
}

impl BoxQuery {
    fn matches(&self, metadata: &BoxMetadata) -> bool {
        self.name.as_deref().is_none_or(|name| {
            metadata
                .name
                .as_deref()
                .is_some_and(|candidate| candidate.eq_ignore_ascii_case(name))
        }) && self.state.is_none_or(|state| metadata.state == state)
            && self.labels.iter().all(|(key, value)| {
                metadata
                    .labels
                    .get(key)
                    .is_some_and(|candidate| value.as_ref().is_none_or(|value| candidate == value))
            })
            && self.tags.iter().all(|tag| metadata.tags.contains(tag))
    }
}

impl BoxSortBy {
    fn compare(self, left: &BoxMetadata, right: &BoxMetadata) -> CmpOrdering {
        match self {
            Self::Name => left.name.cmp(&right.name),
            Self::Created => left.created_at_unix_ms.cmp(&right.created_at_unix_ms),
            Self::Updated => left.updated_at_unix_ms.cmp(&right.updated_at_unix_ms),
            Self::LastUsed => left.last_used_at_unix_ms.cmp(&right.last_used_at_unix_ms),
            Self::PhysicalSize => left.physical_size_bytes.cmp(&right.physical_size_bytes),
            Self::VirtualSize => left.virtual_size_bytes.cmp(&right.virtual_size_bytes),
            Self::Id => CmpOrdering::Equal,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoxEntryErrorCode {
    InvalidName,
    InvalidMetadata,
    UnsupportedSchema,
    UnsafeFileType,
    MissingData,
    Busy,
    Io,
    CorruptStore,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoxEntryError {
    pub entry_name: String,
    pub box_id: Option<BoxId>,
    pub code: BoxEntryErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoxListReport {
    pub boxes: Vec<BoxMetadata>,
    pub errors: Vec<BoxEntryError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuarantinedBoxEntry {
    pub entry_name: String,
    pub box_id: Option<BoxId>,
    pub destination: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoxRepairReport {
    pub applied: bool,
    pub detected: Vec<BoxEntryError>,
    pub quarantined: Vec<QuarantinedBoxEntry>,
    pub failures: Vec<BoxEntryError>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateBox {
    pub manifest_digest: String,
    pub platform: String,
    pub virtual_size_bytes: u64,
    pub name: Option<String>,
    pub labels: BTreeMap<String, String>,
    pub tags: BTreeSet<String>,
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
            name: None,
            labels: BTreeMap::new(),
            tags: BTreeSet::new(),
        }
    }

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
        validate_optional_name(self.name.as_deref())?;
        validate_labels(&self.labels)?;
        validate_tags(&self.tags)?;
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
    metadata: Box<BoxMetadata>,
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
        let _metadata_lock = self.lock_metadata_index()?;
        self.ensure_unique_name(request.name.as_deref(), None)?;

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
        let (metadata, migrated) = read_metadata(&paths.metadata, box_id)?;
        if migrated {
            return Self::migrate_metadata_if_idle(&paths, metadata);
        }
        Ok(metadata)
    }

    pub fn list(&self) -> Result<BoxListReport, BoxStoreError> {
        self.list_with(&BoxQuery::default())
    }

    pub fn list_with(&self, query: &BoxQuery) -> Result<BoxListReport, BoxStoreError> {
        validate_query(query)?;
        let mut report = self.scan()?.report;
        report.boxes.retain(|metadata| query.matches(metadata));
        report.boxes.sort_by(|left, right| {
            query
                .sort_by
                .compare(left, right)
                .then_with(|| left.box_id.to_string().cmp(&right.box_id.to_string()))
        });
        if query.descending {
            report.boxes.reverse();
        }
        Ok(report)
    }

    pub fn repair(&self, apply: bool) -> Result<BoxRepairReport, BoxStoreError> {
        let scan = self.scan()?;
        let mut report = BoxRepairReport {
            applied: apply,
            detected: scan.report.errors,
            quarantined: Vec::new(),
            failures: scan.unaddressable,
        };
        if !apply || scan.corrupt.is_empty() {
            return Ok(report);
        }

        let quarantine_root = self.state_root.join("quarantine");
        secure_directory(&quarantine_root)?;
        let batch = quarantine_root.join(format!(
            "boxes-{}-{}-{}",
            now_unix_millis()?,
            std::process::id(),
            TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        secure_directory(&batch)?;

        for entry in scan.corrupt {
            match Self::quarantine_entry(&entry, &batch) {
                Ok(destination) => report.quarantined.push(QuarantinedBoxEntry {
                    entry_name: entry.error.entry_name,
                    box_id: entry.error.box_id,
                    destination,
                }),
                Err(error) => report.failures.push(BoxEntryError::from_store_error(
                    entry.error.entry_name,
                    entry.error.box_id,
                    &error,
                )),
            }
        }
        Ok(report)
    }

    pub fn garbage_collect(&self) -> Result<GarbageCollectionReport, BoxStoreError> {
        self.garbage_collect_older_than(DEFAULT_GC_MIN_AGE)
    }

    pub fn garbage_collect_older_than(
        &self,
        minimum_age: Duration,
    ) -> Result<GarbageCollectionReport, BoxStoreError> {
        self.ensure_root()?;
        let boxes = self.boxes_directory();
        if !boxes.exists() {
            return Ok(GarbageCollectionReport::default());
        }
        validate_directory(&boxes, "box store")?;
        let mut report = GarbageCollectionReport::default();
        for entry in fs::read_dir(&boxes)? {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let path = entry.path();
            if parse_box_artifact_name(&name, ".creating-").is_some()
                || parse_box_artifact_name(&name, ".deleted-").is_some()
            {
                collect_stale_directory(&path, minimum_age, &mut report)?;
                continue;
            }
            if BoxId::from_str(&name).is_ok() {
                Self::collect_box_temporary_files(&path, minimum_age, &mut report)?;
            }
        }
        Ok(report)
    }

    fn collect_box_temporary_files(
        box_directory: &Path,
        minimum_age: Duration,
        report: &mut GarbageCollectionReport,
    ) -> Result<(), BoxStoreError> {
        validate_directory(box_directory, "box directory")?;
        let mut stale = Vec::new();
        for entry in fs::read_dir(box_directory)? {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !is_reset_temporary_disk_name(&name) && !is_atomic_metadata_temporary_name(&name) {
                continue;
            }
            let path = entry.path();
            validate_regular_file(&path, "box temporary file")?;
            if garbage_collection_candidate_is_old(&path, minimum_age)? {
                stale.push(path);
            } else {
                report.skipped_young += 1;
            }
        }
        if stale.is_empty() {
            return Ok(());
        }
        match garbage_collection_lock(box_directory)? {
            GarbageCollectionLock::Acquired(_lock) => {
                for path in stale {
                    fs::remove_file(&path)?;
                    sync_parent(&path)?;
                    report.removed += 1;
                }
            }
            GarbageCollectionLock::Busy => report.skipped_busy += stale.len(),
        }
        Ok(())
    }

    fn scan(&self) -> Result<BoxScan, BoxStoreError> {
        self.ensure_root()?;
        let directory = self.boxes_directory();
        if !directory.exists() {
            return Ok(BoxScan::default());
        }
        validate_directory(&directory, "box store")?;
        let mut scan = BoxScan::default();
        for entry in fs::read_dir(&directory)? {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    scan.push_unaddressable(BoxEntryError {
                        entry_name: "<unreadable>".into(),
                        box_id: None,
                        code: BoxEntryErrorCode::Io,
                        message: error.to_string(),
                    });
                    continue;
                }
            };
            let name = entry.file_name();
            if name.to_string_lossy().starts_with('.') {
                continue;
            }
            let Some(name) = name.to_str() else {
                scan.push_corrupt(
                    entry.path(),
                    entry.file_name(),
                    BoxEntryError {
                        entry_name: entry.file_name().to_string_lossy().into_owned(),
                        box_id: None,
                        code: BoxEntryErrorCode::InvalidName,
                        message: "box directory name is not valid UTF-8".into(),
                    },
                );
                continue;
            };
            let Ok(box_id) = BoxId::from_str(name) else {
                scan.push_corrupt(
                    entry.path(),
                    entry.file_name(),
                    BoxEntryError {
                        entry_name: name.into(),
                        box_id: None,
                        code: BoxEntryErrorCode::InvalidName,
                        message: format!("invalid box directory name: {name}"),
                    },
                );
                continue;
            };
            match self.get(box_id) {
                Ok(metadata) => scan.report.boxes.push(metadata),
                Err(error) => scan.push_corrupt(
                    entry.path(),
                    entry.file_name(),
                    BoxEntryError::from_store_error(name.into(), Some(box_id), &error),
                ),
            }
        }
        scan.report
            .boxes
            .sort_by_key(|metadata| metadata.box_id.to_string());
        scan.report
            .errors
            .sort_by(|left, right| left.entry_name.cmp(&right.entry_name));
        Ok(scan)
    }

    fn quarantine_entry(entry: &CorruptBoxEntry, batch: &Path) -> Result<PathBuf, BoxStoreError> {
        let _lock = if let Some(box_id) = entry.error.box_id {
            lock_for_quarantine(&entry.source, box_id)?
        } else {
            None
        };
        let destination = batch.join(&entry.name);
        if destination.symlink_metadata().is_ok() {
            return Err(BoxStoreError::InvalidPath(destination));
        }
        fs::rename(&entry.source, &destination)?;
        sync_parent(&entry.source)?;
        sync_parent(&destination)?;
        Ok(destination)
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
        let (metadata, migrated) = read_metadata(&paths.metadata, box_id)?;
        if migrated {
            write_json_atomic(&paths.metadata, &metadata)?;
        }
        if metadata.state == BoxState::NeedsRepair {
            return Err(BoxStoreError::NeedsRepair(box_id));
        }
        validate_regular_file(&paths.disk, "box root disk")?;
        Ok(BoxLease {
            lock_file,
            directory: paths.directory,
            disk_path: paths.disk,
            metadata: Box::new(metadata),
        })
    }

    /// Durably records that a writable Box is about to be exposed to a guest.
    pub fn begin_writable_use(&self, lease: &mut BoxLease) -> Result<(), BoxStoreError> {
        self.transition_state(lease, BoxState::Ready, BoxState::Dirty, true)
    }

    /// Durably records that the helper completed a clean writable Box run.
    pub fn finish_clean_use(&self, lease: &mut BoxLease) -> Result<(), BoxStoreError> {
        self.transition_state(lease, BoxState::Dirty, BoxState::Ready, false)
    }

    /// Durably records that `e2fsck` successfully recovered a dirty Box.
    pub fn finish_repair(&self, lease: &mut BoxLease) -> Result<(), BoxStoreError> {
        self.transition_state(lease, BoxState::Dirty, BoxState::Ready, false)
    }

    /// Durably blocks a dirty Box after `e2fsck` could not repair it.
    pub fn mark_needs_repair(&self, lease: &mut BoxLease) -> Result<(), BoxStoreError> {
        self.transition_state(lease, BoxState::Dirty, BoxState::NeedsRepair, false)
    }

    fn transition_state(
        &self,
        lease: &mut BoxLease,
        expected: BoxState,
        state: BoxState,
        mark_used: bool,
    ) -> Result<(), BoxStoreError> {
        let expected_directory = self.box_directory(lease.id());
        if lease.directory != expected_directory {
            return Err(BoxStoreError::CorruptStore(
                "box lease belongs to another store".into(),
            ));
        }
        if lease.metadata.state != expected {
            return Err(BoxStoreError::InvalidStateTransition {
                box_id: lease.id(),
                expected,
                actual: lease.metadata.state,
                next: state,
            });
        }
        let mut next_metadata = (*lease.metadata).clone();
        let now = now_unix_millis()?;
        next_metadata.state = state;
        next_metadata.updated_at_unix_ms = now;
        if mark_used {
            next_metadata.last_used_at_unix_ms = Some(now);
        }
        let metadata = lease.directory.join(METADATA_FILE);
        write_json_atomic(&metadata, &next_metadata)?;
        *lease.metadata = next_metadata;
        Ok(())
    }

    pub fn delete(&self, box_id: BoxId) -> Result<BoxMetadata, BoxStoreError> {
        let lease = self.try_acquire(box_id)?;
        let metadata = (*lease.metadata).clone();
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
        lease.metadata.physical_size_bytes = allocated_size_bytes(&lease.disk_path)?;
        lease.metadata.updated_at_unix_ms = now_unix_millis()?;
        write_json_atomic(&lease.directory.join(METADATA_FILE), &lease.metadata)?;
        Ok((*lease.metadata).clone())
    }

    pub fn clone_box(&self, source_id: BoxId) -> Result<BoxMetadata, BoxStoreError> {
        let source = self.try_acquire(source_id)?;
        let request = CreateBox::new(
            source.metadata.manifest_digest.clone(),
            source.metadata.platform.clone(),
            source.metadata.virtual_size_bytes,
        )
        .with_labels(source.metadata.labels.clone())
        .with_tags(source.metadata.tags.clone());
        self.create(&request, source.disk_path())
    }

    pub fn rename(
        &self,
        box_id: BoxId,
        name: impl Into<String>,
    ) -> Result<BoxMetadata, BoxStoreError> {
        self.update(
            box_id,
            &UpdateBox {
                name: Some(name.into()),
                ..UpdateBox::default()
            },
        )
    }

    pub fn update(&self, box_id: BoxId, update: &UpdateBox) -> Result<BoxMetadata, BoxStoreError> {
        validate_update(update)?;
        self.ensure_root()?;
        secure_directory(&self.boxes_directory())?;
        let _metadata_lock = self.lock_metadata_index()?;
        self.ensure_unique_name(update.name.as_deref(), Some(box_id))?;
        let mut lease = self.try_acquire(box_id)?;
        if update.clear_name {
            lease.metadata.name = None;
        } else if let Some(name) = &update.name {
            lease.metadata.name = Some(name.clone());
        }
        for key in &update.remove_labels {
            lease.metadata.labels.remove(key);
        }
        lease.metadata.labels.extend(update.set_labels.clone());
        for tag in &update.remove_tags {
            lease.metadata.tags.remove(tag);
        }
        lease.metadata.tags.extend(update.add_tags.iter().cloned());
        lease.metadata.updated_at_unix_ms = now_unix_millis()?;
        write_json_atomic(&lease.directory.join(METADATA_FILE), &lease.metadata)?;
        Ok((*lease.metadata).clone())
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
            let lock = staging.join(LOCK_FILE);
            let lock_file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&lock)?;
            set_file_permissions(&lock)?;
            FileExt::lock_exclusive(&lock_file)?;
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
                name: request.name.clone(),
                labels: request.labels.clone(),
                tags: request.tags.clone(),
                last_used_at_unix_ms: None,
                physical_size_bytes: allocated_size_bytes(&disk)?,
            };
            write_json_atomic(&staging.join(METADATA_FILE), &metadata)?;
            lock_file.sync_all()?;
            fs::rename(&staging, &destination)?;
            sync_parent(&destination)?;
            // Do not rely only on descriptor drop here. Callers may acquire the newly published
            // Box immediately after create returns, and explicit unlock avoids transient
            // self-contention observed on Linux CI filesystems.
            let _ = FileExt::unlock(&lock_file);
            drop(lock_file);
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

    fn lock_metadata_index(&self) -> Result<File, BoxStoreError> {
        let path = self.boxes_directory().join(METADATA_LOCK_FILE);
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

    fn ensure_unique_name(
        &self,
        name: Option<&str>,
        except: Option<BoxId>,
    ) -> Result<(), BoxStoreError> {
        let Some(name) = name else {
            return Ok(());
        };
        for entry in fs::read_dir(self.boxes_directory())? {
            let entry = entry?;
            let Some(entry_name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Ok(box_id) = BoxId::from_str(&entry_name) else {
                continue;
            };
            if Some(box_id) == except {
                continue;
            }
            let path = entry.path().join(METADATA_FILE);
            let Ok((metadata, _)) = read_metadata(&path, box_id) else {
                continue;
            };
            if metadata
                .name
                .as_deref()
                .is_some_and(|existing| existing.eq_ignore_ascii_case(name))
            {
                return Err(BoxStoreError::NameConflict(name.into()));
            }
        }
        Ok(())
    }

    fn migrate_metadata_if_idle(
        paths: &BoxPaths,
        fallback: BoxMetadata,
    ) -> Result<BoxMetadata, BoxStoreError> {
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&paths.lock)?;
        set_file_permissions(&paths.lock)?;
        match FileExt::try_lock_exclusive(&lock) {
            Ok(()) => {
                let (metadata, migrated) = read_metadata(&paths.metadata, fallback.box_id)?;
                if migrated {
                    write_json_atomic(&paths.metadata, &metadata)?;
                }
                Ok(metadata)
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(fallback),
            Err(source) => Err(BoxStoreError::Busy {
                box_id: fallback.box_id,
                source,
            }),
        }
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

#[derive(Debug, Default)]
struct BoxScan {
    report: BoxListReport,
    corrupt: Vec<CorruptBoxEntry>,
    unaddressable: Vec<BoxEntryError>,
}

impl BoxScan {
    fn push_corrupt(&mut self, source: PathBuf, name: OsString, error: BoxEntryError) {
        self.report.errors.push(error.clone());
        self.corrupt.push(CorruptBoxEntry {
            source,
            name,
            error,
        });
    }

    fn push_unaddressable(&mut self, error: BoxEntryError) {
        self.report.errors.push(error.clone());
        self.unaddressable.push(error);
    }
}

#[derive(Debug)]
struct CorruptBoxEntry {
    source: PathBuf,
    name: OsString,
    error: BoxEntryError,
}

impl BoxEntryError {
    fn from_store_error(entry_name: String, box_id: Option<BoxId>, error: &BoxStoreError) -> Self {
        let code = match error {
            BoxStoreError::Busy { .. } => BoxEntryErrorCode::Busy,
            BoxStoreError::UnsupportedSchema { .. } => BoxEntryErrorCode::UnsupportedSchema,
            BoxStoreError::InvalidMetadata(_)
            | BoxStoreError::InvalidBundle(_)
            | BoxStoreError::NameConflict(_) => BoxEntryErrorCode::InvalidMetadata,
            BoxStoreError::UnsafeFileType { .. } | BoxStoreError::InvalidPath(_) => {
                BoxEntryErrorCode::UnsafeFileType
            }
            BoxStoreError::NotFound(_) => BoxEntryErrorCode::MissingData,
            BoxStoreError::Io(source) if source.kind() == io::ErrorKind::NotFound => {
                BoxEntryErrorCode::MissingData
            }
            BoxStoreError::CorruptStore(_) | BoxStoreError::InvalidStateTransition { .. } => {
                BoxEntryErrorCode::CorruptStore
            }
            BoxStoreError::NeedsRepair(_)
            | BoxStoreError::BaseDiskBusy { .. }
            | BoxStoreError::Mke2fs { .. }
            | BoxStoreError::CowCloneUnavailable { .. }
            | BoxStoreError::EphemeralExists(_)
            | BoxStoreError::StorageRoot(_)
            | BoxStoreError::ClockBeforeUnixEpoch
            | BoxStoreError::ClockOverflow
            | BoxStoreError::Io(_)
            | BoxStoreError::Json(_) => BoxEntryErrorCode::Io,
        };
        Self {
            entry_name,
            box_id,
            code,
            message: error.to_string(),
        }
    }
}

fn lock_for_quarantine(directory: &Path, box_id: BoxId) -> Result<Option<File>, BoxStoreError> {
    let metadata = fs::symlink_metadata(directory)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Ok(None);
    }
    let path = directory.join(LOCK_FILE);
    if let Ok(metadata) = fs::symlink_metadata(&path) {
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(BoxStoreError::UnsafeFileType {
                label: "box lock".into(),
                path,
            });
        }
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    set_file_permissions(&path)?;
    FileExt::try_lock_exclusive(&lock).map_err(|source| BoxStoreError::Busy { box_id, source })?;
    Ok(Some(lock))
}

pub(crate) fn garbage_collection_lock(
    directory: &Path,
) -> Result<GarbageCollectionLock, BoxStoreError> {
    validate_directory(directory, "garbage collection candidate")?;
    let lock_path = directory.join(LOCK_FILE);
    let lock = match OpenOptions::new().read(true).write(true).open(&lock_path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(GarbageCollectionLock::Acquired(None));
        }
        Err(error) => return Err(error.into()),
    };
    validate_regular_file(&lock_path, "garbage collection lock")?;
    match FileExt::try_lock_exclusive(&lock) {
        Ok(()) => Ok(GarbageCollectionLock::Acquired(Some(lock))),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(GarbageCollectionLock::Busy),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn garbage_collection_candidate_is_old(
    path: &Path,
    minimum_age: Duration,
) -> Result<bool, BoxStoreError> {
    let modified = path.symlink_metadata()?.modified()?;
    Ok(SystemTime::now()
        .duration_since(modified)
        .is_ok_and(|age| age >= minimum_age))
}

fn collect_stale_directory(
    path: &Path,
    minimum_age: Duration,
    report: &mut GarbageCollectionReport,
) -> Result<(), BoxStoreError> {
    validate_directory(path, "box garbage collection candidate")?;
    if !garbage_collection_candidate_is_old(path, minimum_age)? {
        report.skipped_young += 1;
        return Ok(());
    }
    match garbage_collection_lock(path)? {
        GarbageCollectionLock::Acquired(_lock) => {
            remove_managed_directory(path)?;
            sync_parent(path)?;
            report.removed += 1;
        }
        GarbageCollectionLock::Busy => report.skipped_busy += 1,
    }
    Ok(())
}

fn parse_box_artifact_name(name: &str, prefix: &str) -> Option<BoxId> {
    let value = name.strip_prefix(prefix)?;
    let mut parts = value.rsplitn(3, '-');
    let sequence = parts.next()?;
    let process = parts.next()?;
    let box_id = parts.next()?;
    if process.is_empty()
        || !process.bytes().all(|byte| byte.is_ascii_digit())
        || sequence.is_empty()
        || !sequence.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    BoxId::from_str(box_id).ok()
}

fn is_reset_temporary_disk_name(name: &str) -> bool {
    name.strip_prefix(&format!(".{ROOT_DISK_FILE}."))
        .and_then(|value| value.strip_suffix(".tmp"))
        .is_some_and(|sequence| {
            !sequence.is_empty() && sequence.bytes().all(|byte| byte.is_ascii_digit())
        })
}

fn is_atomic_metadata_temporary_name(name: &str) -> bool {
    let Some(value) = name
        .strip_prefix(&format!(".{METADATA_FILE}."))
        .and_then(|value| value.strip_suffix(".tmp"))
    else {
        return false;
    };
    let mut parts = value.split('.');
    let Some(process_id) = parts.next() else {
        return false;
    };
    let Some(sequence) = parts.next() else {
        return false;
    };
    parts.next().is_none() && process_id.parse::<u32>().is_ok() && sequence.parse::<u64>().is_ok()
}

fn read_metadata(path: &Path, expected_id: BoxId) -> Result<(BoxMetadata, bool), BoxStoreError> {
    let bytes = fs::read(path)?;
    let mut value: BoxMetadata = serde_json::from_slice(&bytes).map_err(|source| {
        BoxStoreError::InvalidMetadata(format!("{}: {source}", path.display()))
    })?;
    let migrated = match value.schema_version {
        SCHEMA_VERSION => false,
        LEGACY_SCHEMA_VERSION => {
            value.schema_version = SCHEMA_VERSION;
            true
        }
        actual => {
            return Err(BoxStoreError::UnsupportedSchema {
                expected: SCHEMA_VERSION,
                actual,
            });
        }
    };
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
    validate_optional_name(value.name.as_deref())?;
    validate_labels(&value.labels)?;
    validate_tags(&value.tags)?;
    let disk = path
        .parent()
        .ok_or_else(|| BoxStoreError::InvalidPath(path.into()))?
        .join(ROOT_DISK_FILE);
    let disk_size = fs::metadata(&disk)?.len();
    if disk_size != value.virtual_size_bytes {
        return Err(BoxStoreError::CorruptStore(format!(
            "root disk size {disk_size} does not match metadata virtual size {} for Box {expected_id}",
            value.virtual_size_bytes
        )));
    }
    value.physical_size_bytes = allocated_size_bytes(&disk)?;
    Ok((value, migrated))
}

fn validate_query(query: &BoxQuery) -> Result<(), BoxStoreError> {
    validate_optional_name(query.name.as_deref())?;
    for (key, value) in &query.labels {
        validate_label_key(key)?;
        if let Some(value) = value {
            validate_label_value(value)?;
        }
    }
    validate_tags(&query.tags)
}

fn validate_update(update: &UpdateBox) -> Result<(), BoxStoreError> {
    if update.name.is_some() && update.clear_name {
        return Err(BoxStoreError::InvalidMetadata(
            "name and clear_name cannot be used together".into(),
        ));
    }
    validate_optional_name(update.name.as_deref())?;
    validate_labels(&update.set_labels)?;
    for key in &update.remove_labels {
        validate_label_key(key)?;
        if update.set_labels.contains_key(key) {
            return Err(BoxStoreError::InvalidMetadata(format!(
                "label {key} cannot be set and removed in one update"
            )));
        }
    }
    validate_tags(&update.add_tags)?;
    validate_tags(&update.remove_tags)?;
    if let Some(tag) = update.add_tags.intersection(&update.remove_tags).next() {
        return Err(BoxStoreError::InvalidMetadata(format!(
            "tag {tag} cannot be added and removed in one update"
        )));
    }
    Ok(())
}

fn validate_optional_name(name: Option<&str>) -> Result<(), BoxStoreError> {
    let Some(name) = name else {
        return Ok(());
    };
    validate_identifier("box name", name, MAX_NAME_CHARS, false)
}

fn validate_labels(labels: &BTreeMap<String, String>) -> Result<(), BoxStoreError> {
    for (key, value) in labels {
        validate_label_key(key)?;
        validate_label_value(value)?;
    }
    Ok(())
}

fn validate_label_key(key: &str) -> Result<(), BoxStoreError> {
    validate_identifier("label key", key, MAX_LABEL_KEY_CHARS, true)
}

fn validate_label_value(value: &str) -> Result<(), BoxStoreError> {
    if value.chars().count() > MAX_LABEL_VALUE_CHARS
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(BoxStoreError::InvalidMetadata(format!(
            "label value must contain at most {MAX_LABEL_VALUE_CHARS} non-control characters without surrounding whitespace"
        )));
    }
    Ok(())
}

fn validate_tags(tags: &BTreeSet<String>) -> Result<(), BoxStoreError> {
    for tag in tags {
        validate_identifier("tag", tag, MAX_TAG_CHARS, false)?;
    }
    Ok(())
}

fn validate_identifier(
    label: &str,
    value: &str,
    max_chars: usize,
    allow_slash: bool,
) -> Result<(), BoxStoreError> {
    let valid = !value.is_empty()
        && value.chars().count() <= max_chars
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'.' | b'_' | b'-')
                || (allow_slash && byte == b'/')
        });
    if !valid {
        return Err(BoxStoreError::InvalidMetadata(format!(
            "{label} must be 1-{max_chars} ASCII alphanumeric, '.', '_', '-'{} characters",
            if allow_slash { ", or '/'" } else { "" }
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn allocated_size_bytes(path: &Path) -> Result<u64, BoxStoreError> {
    use std::os::unix::fs::MetadataExt;
    Ok(fs::metadata(path)?.blocks().saturating_mul(512))
}

#[cfg(not(unix))]
fn allocated_size_bytes(path: &Path) -> Result<u64, BoxStoreError> {
    Ok(fs::metadata(path)?.len())
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<(), BoxStoreError> {
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
    set_file_permissions(&temporary)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary, path)?;
    sync_parent(path)?;
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
// These no-op adapters preserve one fallible API at platform-neutral call sites.
#[allow(clippy::unnecessary_wraps)]
fn set_directory_permissions(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_file_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)]
fn set_file_permissions(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn owner_uid(path: &Path) -> Result<Option<u32>, BoxStoreError> {
    use std::os::unix::fs::MetadataExt;
    Ok(Some(fs::metadata(path)?.uid()))
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)]
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
#[allow(clippy::unnecessary_wraps)]
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
    #[error("box name is already in use: {0}")]
    NameConflict(String),
    #[error(
        "invalid state transition for Box {box_id}: expected {expected:?}, found {actual:?}, cannot transition to {next:?}"
    )]
    InvalidStateTransition {
        box_id: BoxId,
        expected: BoxState,
        actual: BoxState,
        next: BoxState,
    },
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
    #[error("invalid Box bundle: {0}")]
    InvalidBundle(String),
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
    use proptest::prelude::*;

    const DISK_BYTES: u64 = 1024 * 1024;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(32))]

        #[test]
        fn property_v1_metadata_migration_preserves_identity_fields(
            digest_suffix in "[a-f0-9]{1,32}",
            architecture in "[a-z0-9_]{1,12}",
            generation in any::<u16>(),
        ) {
            let temporary = tempfile::tempdir().unwrap();
            let box_id = BoxId::new();
            let directory = temporary.path().join(box_id.to_string());
            fs::create_dir(&directory).unwrap();
            File::create(directory.join(ROOT_DISK_FILE))
                .unwrap()
                .set_len(4096)
                .unwrap();
            let metadata_path = directory.join(METADATA_FILE);
            let manifest_digest = format!("sha256:{digest_suffix}");
            let platform = format!("linux/{architecture}");
            fs::write(
                &metadata_path,
                serde_json::to_vec(&serde_json::json!({
                    "schema_version": 1,
                    "box_id": box_id,
                    "state": "ready",
                    "manifest_digest": manifest_digest,
                    "platform": platform,
                    "disk_format": "raw_ext4",
                    "virtual_size_bytes": 4096,
                    "generation": generation,
                    "created_at_unix_ms": 1,
                    "updated_at_unix_ms": 2,
                    "owner_uid": null
                })).unwrap(),
            ).unwrap();

            let (migrated, changed) = read_metadata(&metadata_path, box_id).unwrap();
            prop_assert!(changed);
            prop_assert_eq!(migrated.schema_version, SCHEMA_VERSION);
            prop_assert_eq!(migrated.manifest_digest, manifest_digest);
            prop_assert_eq!(migrated.platform, platform);
            prop_assert_eq!(migrated.generation, u64::from(generation));
        }
    }

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
        assert_eq!(loaded.schema_version, 2);
        assert!(loaded.physical_size_bytes <= loaded.virtual_size_bytes);
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

    #[test]
    fn lazily_migrates_v1_metadata_without_losing_fields() {
        let fixture = Fixture::new();
        let created = fixture.create();
        let path = fixture
            .store
            .box_directory(created.box_id)
            .join(METADATA_FILE);
        let mut legacy = serde_json::to_value(&created).unwrap();
        let object = legacy.as_object_mut().unwrap();
        object.insert("schema_version".into(), serde_json::json!(1));
        for field in [
            "name",
            "labels",
            "tags",
            "last_used_at_unix_ms",
            "physical_size_bytes",
        ] {
            object.remove(field);
        }
        fs::write(&path, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();

        let migrated = fixture.store.get(created.box_id).unwrap();
        let persisted: serde_json::Value =
            serde_json::from_slice(&fs::read(path).unwrap()).unwrap();

        assert_eq!(migrated.schema_version, 2);
        assert_eq!(migrated.manifest_digest, created.manifest_digest);
        assert_eq!(persisted["schema_version"], 2);
        assert_eq!(persisted["labels"], serde_json::json!({}));
        assert_eq!(persisted["tags"], serde_json::json!([]));
    }

    #[test]
    fn updates_names_labels_and_tags_atomically_and_rejects_collisions() {
        let fixture = Fixture::new();
        let first = fixture
            .store
            .create(
                &CreateBox::new("sha256:abc", "linux/arm64", DISK_BYTES).with_name("alpha"),
                &fixture.base,
            )
            .unwrap();
        let second = fixture.create();
        assert!(matches!(
            fixture.store.rename(second.box_id, "ALPHA"),
            Err(BoxStoreError::NameConflict(name)) if name == "ALPHA"
        ));

        let updated = fixture
            .store
            .update(
                first.box_id,
                &UpdateBox {
                    name: Some("renamed".into()),
                    set_labels: BTreeMap::from([("team/name".into(), "runtime".into())]),
                    add_tags: BTreeSet::from(["warm".into()]),
                    ..UpdateBox::default()
                },
            )
            .unwrap();
        assert_eq!(updated.name.as_deref(), Some("renamed"));
        assert_eq!(updated.labels["team/name"], "runtime");
        assert!(updated.tags.contains("warm"));

        let _lease = fixture.store.try_acquire(first.box_id).unwrap();
        assert!(matches!(
            fixture.store.rename(first.box_id, "busy"),
            Err(BoxStoreError::Busy { box_id, .. }) if box_id == first.box_id
        ));
        assert_eq!(
            fixture.store.get(first.box_id).unwrap().name.as_deref(),
            Some("renamed")
        );
    }

    #[test]
    fn rejects_invalid_labels_and_conflicting_updates() {
        let fixture = Fixture::new();
        let request = CreateBox::new("sha256:abc", "linux/arm64", DISK_BYTES)
            .with_labels(BTreeMap::from([("bad label".into(), "value".into())]));
        assert!(matches!(
            fixture.store.create(&request, &fixture.base),
            Err(BoxStoreError::InvalidMetadata(_))
        ));

        let created = fixture.create();
        assert!(matches!(
            fixture.store.update(
                created.box_id,
                &UpdateBox {
                    set_labels: BTreeMap::from([("team".into(), "one".into())]),
                    remove_labels: BTreeSet::from(["team".into()]),
                    ..UpdateBox::default()
                }
            ),
            Err(BoxStoreError::InvalidMetadata(_))
        ));
    }

    #[test]
    fn filters_and_sorts_boxes_deterministically() {
        let fixture = Fixture::new();
        let zebra = fixture
            .store
            .create(
                &CreateBox::new("sha256:z", "linux/arm64", DISK_BYTES)
                    .with_name("zebra")
                    .with_labels(BTreeMap::from([("team".into(), "core".into())]))
                    .with_tags(BTreeSet::from(["hot".into()])),
                &fixture.base,
            )
            .unwrap();
        let alpha = fixture
            .store
            .create(
                &CreateBox::new("sha256:a", "linux/arm64", DISK_BYTES)
                    .with_name("alpha")
                    .with_labels(BTreeMap::from([("team".into(), "core".into())]))
                    .with_tags(BTreeSet::from(["hot".into()])),
                &fixture.base,
            )
            .unwrap();
        fixture.create();

        let report = fixture
            .store
            .list_with(&BoxQuery {
                labels: BTreeMap::from([("team".into(), Some("core".into()))]),
                tags: BTreeSet::from(["hot".into()]),
                sort_by: BoxSortBy::Name,
                ..BoxQuery::default()
            })
            .unwrap();

        assert_eq!(
            report
                .boxes
                .iter()
                .map(|metadata| metadata.box_id)
                .collect::<Vec<_>>(),
            [alpha.box_id, zebra.box_id]
        );
    }

    #[test]
    fn writable_use_records_last_used_and_reports_sparse_allocation() {
        let fixture = Fixture::new();
        let created = fixture.create();
        assert!(created.last_used_at_unix_ms.is_none());
        assert!(created.physical_size_bytes <= created.virtual_size_bytes);

        let mut lease = fixture.store.try_acquire(created.box_id).unwrap();
        fixture.store.begin_writable_use(&mut lease).unwrap();
        assert!(lease.metadata().last_used_at_unix_ms.is_some());
        fixture.store.finish_clean_use(&mut lease).unwrap();
        drop(lease);

        assert!(
            fixture
                .store
                .get(created.box_id)
                .unwrap()
                .last_used_at_unix_ms
                .is_some()
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

        let report = store.list().unwrap();
        assert!(report.boxes.is_empty());
        assert!(report.errors.is_empty());
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
    fn list_preserves_healthy_boxes_and_reports_each_corrupt_entry() {
        let fixture = Fixture::new();
        let healthy = fixture.create();
        let corrupt = fixture.create();
        fs::write(
            fixture
                .store
                .box_directory(corrupt.box_id)
                .join(METADATA_FILE),
            b"not-json",
        )
        .unwrap();
        fs::create_dir(fixture.store.boxes_directory().join("not-a-box")).unwrap();

        let report = fixture.store.list().unwrap();

        assert_eq!(report.boxes, [healthy]);
        assert_eq!(report.errors.len(), 2);
        assert!(report.errors.iter().any(|error| {
            error.box_id == Some(corrupt.box_id) && error.code == BoxEntryErrorCode::InvalidMetadata
        }));
        assert!(report.errors.iter().any(|error| {
            error.entry_name == "not-a-box" && error.code == BoxEntryErrorCode::InvalidName
        }));
    }

    #[test]
    fn repair_previews_then_quarantines_without_deleting_data() {
        let fixture = Fixture::new();
        let healthy = fixture.create();
        let corrupt = fixture.create();
        let corrupt_directory = fixture.store.box_directory(corrupt.box_id);
        fs::write(corrupt_directory.join(METADATA_FILE), b"not-json").unwrap();

        let preview = fixture.store.repair(false).unwrap();
        assert!(!preview.applied);
        assert_eq!(preview.detected.len(), 1);
        assert!(preview.quarantined.is_empty());
        assert!(corrupt_directory.exists());

        let applied = fixture.store.repair(true).unwrap();
        assert!(applied.applied);
        assert_eq!(applied.quarantined.len(), 1);
        assert!(applied.failures.is_empty());
        assert!(!corrupt_directory.exists());
        assert!(
            applied.quarantined[0]
                .destination
                .join(ROOT_DISK_FILE)
                .is_file()
        );

        let report = fixture.store.list().unwrap();
        assert_eq!(report.boxes, [healthy]);
        assert!(report.errors.is_empty());
    }

    #[test]
    fn repair_does_not_quarantine_a_busy_corrupt_box() {
        let fixture = Fixture::new();
        let corrupt = fixture.create();
        let directory = fixture.store.box_directory(corrupt.box_id);
        let _lease = fixture.store.try_acquire(corrupt.box_id).unwrap();
        fs::write(directory.join(METADATA_FILE), b"not-json").unwrap();

        let report = fixture.store.repair(true).unwrap();

        assert!(report.quarantined.is_empty());
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].code, BoxEntryErrorCode::Busy);
        assert!(directory.exists());
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
    fn records_writable_state_changes_while_the_lease_is_held() {
        let fixture = Fixture::new();
        let created = fixture.create();
        let mut lease = fixture.store.try_acquire(created.box_id).unwrap();

        fixture.store.begin_writable_use(&mut lease).unwrap();
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
        fixture.store.begin_writable_use(&mut lease).unwrap();
        fixture.store.mark_needs_repair(&mut lease).unwrap();
        drop(lease);

        assert!(matches!(
            fixture.store.try_acquire(created.box_id),
            Err(BoxStoreError::NeedsRepair(box_id)) if box_id == created.box_id
        ));
    }

    #[test]
    fn only_allows_explicit_writable_state_transitions() {
        let fixture = Fixture::new();
        let created = fixture.create();
        let mut lease = fixture.store.try_acquire(created.box_id).unwrap();

        assert!(matches!(
            fixture.store.finish_clean_use(&mut lease),
            Err(BoxStoreError::InvalidStateTransition {
                box_id,
                expected: BoxState::Dirty,
                actual: BoxState::Ready,
                next: BoxState::Ready,
            }) if box_id == created.box_id
        ));

        fixture.store.begin_writable_use(&mut lease).unwrap();
        fixture.store.finish_clean_use(&mut lease).unwrap();
        fixture.store.begin_writable_use(&mut lease).unwrap();
        fixture.store.finish_repair(&mut lease).unwrap();
        assert_eq!(lease.metadata().state, BoxState::Ready);
    }

    #[test]
    fn failed_durable_publish_does_not_change_the_lease_state() {
        let fixture = Fixture::new();
        let created = fixture.create();
        let mut lease = fixture.store.try_acquire(created.box_id).unwrap();
        let metadata = fixture
            .store
            .box_directory(created.box_id)
            .join(METADATA_FILE);
        fs::remove_file(&metadata).unwrap();
        fs::create_dir(&metadata).unwrap();

        assert!(fixture.store.begin_writable_use(&mut lease).is_err());
        assert_eq!(lease.metadata().state, BoxState::Ready);
    }

    #[test]
    fn clones_into_an_independent_box() {
        let fixture = Fixture::new();
        let source = fixture.create();
        let source_disk = fixture
            .store
            .box_directory(source.box_id)
            .join(ROOT_DISK_FILE);
        OpenOptions::new()
            .write(true)
            .open(&source_disk)
            .unwrap()
            .write_all(&[7])
            .unwrap();
        let cloned = fixture.store.clone_box(source.box_id).unwrap();
        let cloned_disk = fixture
            .store
            .box_directory(cloned.box_id)
            .join(ROOT_DISK_FILE);
        OpenOptions::new()
            .write(true)
            .open(&cloned_disk)
            .unwrap()
            .write_all(&[9])
            .unwrap();

        assert_ne!(source.box_id, cloned.box_id);
        assert_eq!(source.manifest_digest, cloned.manifest_digest);
        assert_eq!(source.virtual_size_bytes, cloned.virtual_size_bytes);
        assert_eq!(fixture.store.list().unwrap().boxes.len(), 2);
        assert_eq!(fs::read(source_disk).unwrap()[0], 7);
        assert_eq!(fs::read(cloned_disk).unwrap()[0], 9);
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
        fs::write(&replacement, [7]).unwrap();

        assert_eq!(reset.generation, 1);
        let disk = fixture
            .store
            .box_directory(created.box_id)
            .join(ROOT_DISK_FILE);
        assert_eq!(fs::read(disk).unwrap()[0], 42);
    }

    #[test]
    fn garbage_collection_removes_only_exact_stale_box_artifacts() {
        let fixture = Fixture::new();
        let created = fixture.create();
        let box_directory = fixture.store.box_directory(created.box_id);
        let reset_temporary = box_directory.join(format!(".{ROOT_DISK_FILE}.999.tmp"));
        File::create(&reset_temporary).unwrap();
        let metadata_temporary = box_directory.join(format!(".{METADATA_FILE}.999.1.tmp"));
        fs::write(&metadata_temporary, br#"{"state":"#).unwrap();
        let unmanaged_temporary = box_directory.join(format!(".{METADATA_FILE}.unknown.tmp"));
        File::create(&unmanaged_temporary).unwrap();

        let creating = fixture
            .store
            .boxes_directory()
            .join(format!(".creating-{}-1-1", BoxId::new()));
        secure_directory(&creating).unwrap();
        File::create(creating.join(LOCK_FILE)).unwrap();
        let deleted = fixture
            .store
            .boxes_directory()
            .join(format!(".deleted-{}-1-1", BoxId::new()));
        secure_directory(&deleted).unwrap();
        File::create(deleted.join(LOCK_FILE)).unwrap();
        let unknown = fixture.store.boxes_directory().join(".deleted-not-managed");
        secure_directory(&unknown).unwrap();

        let young = fixture.store.garbage_collect().unwrap();
        assert_eq!(young.skipped_young, 4);
        assert_eq!(fixture.store.get(created.box_id).unwrap(), created);

        let stale = fixture
            .store
            .garbage_collect_older_than(Duration::ZERO)
            .unwrap();
        assert_eq!(stale.removed, 4);
        assert!(!reset_temporary.exists());
        assert!(!metadata_temporary.exists());
        assert!(unmanaged_temporary.exists());
        assert!(!creating.exists());
        assert!(!deleted.exists());
        assert!(unknown.exists());
        assert!(box_directory.exists());
        assert_eq!(fixture.store.get(created.box_id).unwrap(), created);
    }

    #[test]
    fn garbage_collection_skips_a_busy_box_temporary_disk() {
        let fixture = Fixture::new();
        let created = fixture.create();
        let box_directory = fixture.store.box_directory(created.box_id);
        let reset_temporary = box_directory.join(format!(".{ROOT_DISK_FILE}.999.tmp"));
        File::create(&reset_temporary).unwrap();
        let _lease = fixture.store.try_acquire(created.box_id).unwrap();

        let report = fixture
            .store
            .garbage_collect_older_than(Duration::ZERO)
            .unwrap();

        assert_eq!(report.skipped_busy, 1);
        assert!(reset_temporary.exists());
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
