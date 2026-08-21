use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    str::FromStr,
    sync::atomic::{AtomicU64, Ordering},
};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    Cas, Credentials, Digest, ImageManifest, ImageReference, Platform, RegistryClient,
    RegistryError, durability::sync_directory, lock::AdvisoryLock, reference::Selector,
};

const SCHEMA_VERSION: u32 = 1;
const CURRENT_COMPLETE_MARKER: &str = ".moraebox-rootfs-complete";
const LEGACY_COMPLETE_MARKER: &str = ".fastmvm-rootfs-complete";
pub const BUILTIN_DEFAULT_IMAGE: &str = "docker.io/library/python:3.12";
static TEMPORARY_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub struct ImageCache {
    root: PathBuf,
}

#[derive(Debug)]
pub struct CacheLock {
    file: File,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CachedImage {
    pub reference: Option<String>,
    pub manifest_digest: String,
    pub platform: Option<Platform>,
    pub rootfs: PathBuf,
    pub ready: bool,
    pub default: bool,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PreparedImage {
    pub reference: String,
    pub manifest_digest: String,
    pub rootfs: PathBuf,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RemoveReport {
    pub target: String,
    pub references_removed: Vec<String>,
    pub rootfs_removed: Vec<PathBuf>,
    pub reclaimed_bytes: u64,
    pub applied: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CacheUsage {
    pub references: usize,
    pub images: usize,
    pub rootfs_bytes: u64,
    pub base_disks: usize,
    pub base_disk_bytes: u64,
    pub oci_blobs: usize,
    pub oci_bytes: u64,
    pub workspaces: usize,
    pub workspace_bytes: u64,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PruneReport {
    pub oci_blobs_removed: usize,
    pub incomplete_rootfs_removed: usize,
    pub stale_records_removed: usize,
    pub reclaimed_bytes: u64,
    pub applied: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CleanReport {
    pub entries_removed: usize,
    pub reclaimed_bytes: u64,
    pub applied: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ImageRecord {
    schema_version: u32,
    reference: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_manifest_digest: Option<String>,
    manifest_digest: String,
    platform: Platform,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DefaultImage {
    schema_version: u32,
    reference: String,
}

#[derive(Debug)]
struct StoredRecord {
    path: PathBuf,
    value: ImageRecord,
}

#[derive(Debug)]
struct RootfsEntry {
    digest: Digest,
    path: PathBuf,
    ready: bool,
    size_bytes: u64,
}

#[derive(Debug)]
struct StagedRootfs {
    directory: PathBuf,
    rootfs: PathBuf,
}

impl StagedRootfs {
    fn new(cache_root: &Path, digest: &Digest) -> Self {
        let sequence = TEMPORARY_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let directory = cache_root.join("tmp/rootfs").join(format!(
            "{}.{}.{}",
            digest.hex(),
            std::process::id(),
            sequence
        ));
        let rootfs = directory.join("rootfs");
        Self { directory, rootfs }
    }

    fn publish(&self, destination: &Path) -> Result<(), ImageCacheError> {
        fs::rename(&self.rootfs, destination)?;
        sync_directory(
            destination
                .parent()
                .ok_or_else(|| ImageCacheError::InvalidManagedPath(destination.into()))?,
        )?;
        Ok(())
    }
}

impl Drop for StagedRootfs {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[derive(Debug)]
enum ImageTarget {
    Digest(Digest),
    Reference(String),
}

impl ImageCache {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn lock_exclusive(&self) -> Result<CacheLock, ImageCacheError> {
        self.lock(true)
    }

    pub fn lock_shared(&self) -> Result<CacheLock, ImageCacheError> {
        self.lock(false)
    }

    pub fn record_image(
        &self,
        _lock: &CacheLock,
        reference: &str,
        manifest_digest: &Digest,
        platform: &Platform,
    ) -> Result<(), ImageCacheError> {
        self.write_image_record(reference, None, manifest_digest, platform)
    }

    fn record_pulled_image(
        &self,
        reference: &str,
        source_manifest_digest: &Digest,
        manifest_digest: &Digest,
        platform: &Platform,
    ) -> Result<(), ImageCacheError> {
        self.write_image_record(
            reference,
            Some(source_manifest_digest),
            manifest_digest,
            platform,
        )
    }

    fn write_image_record(
        &self,
        reference: &str,
        source_manifest_digest: Option<&Digest>,
        manifest_digest: &Digest,
        platform: &Platform,
    ) -> Result<(), ImageCacheError> {
        let record = ImageRecord {
            schema_version: SCHEMA_VERSION,
            reference: canonical_reference(reference)?,
            source_manifest_digest: source_manifest_digest.map(ToString::to_string),
            manifest_digest: manifest_digest.to_string(),
            platform: platform.clone(),
        };
        let path = self.record_path(&record);
        write_json_atomic(&path, &record)
    }

    pub fn list(&self) -> Result<Vec<CachedImage>, ImageCacheError> {
        let _lock = self.lock_shared()?;
        self.list_unlocked()
    }

    pub fn default_reference(&self) -> Result<String, ImageCacheError> {
        let _lock = self.lock_shared()?;
        self.default_reference_unlocked()
    }

    pub fn default_rootfs(&self, platform: &Platform) -> Result<Option<PathBuf>, ImageCacheError> {
        let reference = self.default_reference()?;
        self.resolve_reference(&reference, platform)
            .map(|image| image.map(|image| image.rootfs))
    }

    pub fn resolve_reference(
        &self,
        reference: &str,
        platform: &Platform,
    ) -> Result<Option<CachedImage>, ImageCacheError> {
        let _lock = self.lock_shared()?;
        let reference = canonical_reference(reference)?;
        self.resolve_reference_unlocked(&reference, platform)
    }

    pub async fn resolve_or_pull(
        &self,
        reference: &str,
        platform: &Platform,
        credentials: Option<Credentials>,
    ) -> Result<PreparedImage, ImageCacheError> {
        let canonical = canonical_reference(reference)?;
        if let Some(image) = self.resolve_reference(&canonical, platform)? {
            return Ok(prepared_image(image));
        }
        let _activity = self.lock_activity(false)?;
        let _reference =
            AdvisoryLock::acquire(&self.reference_lock_path(&canonical, platform)).await?;
        if let Some(image) = self.resolve_reference(&canonical, platform)? {
            return Ok(prepared_image(image));
        }
        self.pull_unlocked(&canonical, platform, credentials).await
    }

    pub async fn pull(
        &self,
        reference: &str,
        platform: &Platform,
        credentials: Option<Credentials>,
    ) -> Result<PreparedImage, ImageCacheError> {
        let canonical = canonical_reference(reference)?;
        let _activity = self.lock_activity(false)?;
        let _reference =
            AdvisoryLock::acquire(&self.reference_lock_path(&canonical, platform)).await?;
        self.pull_unlocked(&canonical, platform, credentials).await
    }

    fn resolve_reference_unlocked(
        &self,
        canonical_reference: &str,
        platform: &Platform,
    ) -> Result<Option<CachedImage>, ImageCacheError> {
        let path = self.record_path_for(canonical_reference, platform);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let record: ImageRecord = serde_json::from_slice(&bytes)?;
        validate_record(&record, &path)?;
        if record.reference != canonical_reference || &record.platform != platform {
            return Err(ImageCacheError::InvalidRecord(path));
        }
        if let Some(expected) = pinned_source_digest(canonical_reference)? {
            let Some(actual) = record
                .source_manifest_digest
                .as_deref()
                .map(parse_digest)
                .transpose()?
            else {
                return Ok(None);
            };
            if actual != expected {
                return Ok(None);
            }
        }
        let digest = parse_digest(&record.manifest_digest)?;
        let rootfs = self.rootfs_path(&digest);
        if !is_complete_rootfs(&rootfs, &digest) {
            return Ok(None);
        }
        let default = self.default_reference_unlocked()?;
        Ok(Some(CachedImage {
            reference: Some(record.reference.clone()),
            manifest_digest: record.manifest_digest,
            platform: Some(record.platform),
            rootfs,
            ready: true,
            default: record.reference == default,
            // Size calculation recursively walks the rootfs and intentionally belongs only to
            // management operations such as `list` and `usage`, never the run hot path.
            size_bytes: 0,
        }))
    }

    async fn pull_unlocked(
        &self,
        reference: &str,
        platform: &Platform,
        credentials: Option<Credentials>,
    ) -> Result<PreparedImage, ImageCacheError> {
        let parsed = ImageReference::from_str(reference)
            .map_err(|error| ImageCacheError::InvalidTarget(error.to_string()))?;
        let ImageReference::Registry(reference) = parsed else {
            return Err(ImageCacheError::InvalidTarget(reference.into()));
        };
        let cas = Cas::new(self.root.join("oci"));
        let image = RegistryClient::new(credentials)?
            .pull(reference, platform, &cas)
            .await?;
        let staging = StagedRootfs::new(&self.root, &image.manifest_digest);
        let materialize_image = image.clone();
        let materialize_cas = cas.clone();
        let materialize_rootfs = staging.rootfs.clone();
        let staging = tokio::task::spawn_blocking(move || {
            materialize_image.materialize_rootfs(&materialize_cas, &materialize_rootfs)?;
            Ok::<_, RegistryError>(staging)
        })
        .await
        .map_err(|error| ImageCacheError::Task(error.to_string()))??;
        self.publish_staged_rootfs(
            staging,
            &image.reference.to_string(),
            &image.source_manifest_digest,
            &image.manifest_digest,
            platform,
        )
        .await
    }

    async fn publish_staged_rootfs(
        &self,
        staging: StagedRootfs,
        reference: &str,
        source_manifest_digest: &Digest,
        manifest_digest: &Digest,
        platform: &Platform,
    ) -> Result<PreparedImage, ImageCacheError> {
        let rootfs = self.rootfs_path(manifest_digest);
        let _digest = AdvisoryLock::acquire(&self.rootfs_lock_path(manifest_digest)).await?;
        let _metadata = AdvisoryLock::acquire(&self.metadata_lock_path()).await?;
        if !is_complete_rootfs(&rootfs, manifest_digest) {
            if rootfs.exists() {
                return Err(RegistryError::RootfsExists(rootfs.clone()).into());
            }
            fs::create_dir_all(self.rootfs_directory())?;
            staging.publish(&rootfs)?;
        }
        self.record_pulled_image(reference, source_manifest_digest, manifest_digest, platform)?;
        Ok(PreparedImage {
            reference: reference.into(),
            manifest_digest: manifest_digest.to_string(),
            rootfs,
        })
    }

    pub fn set_default(&self, reference: &str) -> Result<String, ImageCacheError> {
        let _lock = self.lock_exclusive()?;
        let reference = canonical_reference(reference)?;
        write_json_atomic(
            &self.default_path(),
            &DefaultImage {
                schema_version: SCHEMA_VERSION,
                reference: reference.clone(),
            },
        )?;
        Ok(reference)
    }

    pub fn clear_default(&self) -> Result<bool, ImageCacheError> {
        let _lock = self.lock_exclusive()?;
        remove_file_if_exists(&self.default_path())
    }

    pub fn remove(&self, target: &str, apply: bool) -> Result<RemoveReport, ImageCacheError> {
        let _activity = apply.then(|| self.lock_activity(true)).transpose()?;
        let _lock = self.lock_exclusive()?;
        let target_kind = parse_target(target)?;
        let records = self.read_records()?;
        let roots = self.read_rootfs_entries()?;
        let mut records_to_remove = Vec::new();
        let mut affected = BTreeSet::new();

        match &target_kind {
            ImageTarget::Digest(digest) => {
                let exists = roots.iter().any(|entry| &entry.digest == digest)
                    || records.iter().any(|record| {
                        parse_digest(&record.value.manifest_digest)
                            .is_ok_and(|parsed| &parsed == digest)
                    });
                if !exists {
                    return Err(ImageCacheError::TargetNotFound(target.into()));
                }
                affected.insert(digest.clone());
                records_to_remove.extend(records.iter().filter(|record| {
                    parse_digest(&record.value.manifest_digest)
                        .is_ok_and(|parsed| &parsed == digest)
                }));
            }
            ImageTarget::Reference(reference) => {
                records_to_remove.extend(
                    records
                        .iter()
                        .filter(|record| &record.value.reference == reference),
                );
                if records_to_remove.is_empty() {
                    return Err(ImageCacheError::TargetNotFound(target.into()));
                }
                for record in &records_to_remove {
                    affected.insert(parse_digest(&record.value.manifest_digest)?);
                }
            }
        }

        let removed_paths = records_to_remove
            .iter()
            .map(|record| record.path.clone())
            .collect::<BTreeSet<_>>();
        let mut roots_to_remove = Vec::new();
        for digest in &affected {
            let has_remaining_reference = records.iter().any(|record| {
                !removed_paths.contains(&record.path)
                    && parse_digest(&record.value.manifest_digest)
                        .is_ok_and(|parsed| &parsed == digest)
            });
            if matches!(target_kind, ImageTarget::Digest(_)) || !has_remaining_reference {
                if let Some(root) = roots.iter().find(|entry| &entry.digest == digest) {
                    roots_to_remove.push(root);
                }
            }
        }

        let reclaimed_bytes = roots_to_remove.iter().map(|entry| entry.size_bytes).sum();
        let mut references_removed = records_to_remove
            .iter()
            .map(|record| record.value.reference.clone())
            .collect::<Vec<_>>();
        references_removed.sort();
        references_removed.dedup();
        let rootfs_removed = roots_to_remove
            .iter()
            .map(|entry| entry.path.clone())
            .collect::<Vec<_>>();

        if apply {
            for record in &records_to_remove {
                remove_file_if_exists(&record.path)?;
            }
            for root in &roots_to_remove {
                remove_managed_directory(&root.path)?;
            }
        }

        Ok(RemoveReport {
            target: target.into(),
            references_removed,
            rootfs_removed,
            reclaimed_bytes,
            applied: apply,
        })
    }

    pub fn usage(&self) -> Result<CacheUsage, ImageCacheError> {
        let _lock = self.lock_shared()?;
        self.usage_unlocked()
    }

    pub fn prune(&self, apply: bool) -> Result<PruneReport, ImageCacheError> {
        let _activity = apply.then(|| self.lock_activity(true)).transpose()?;
        let _lock = self.lock_exclusive()?;
        let roots = self.read_rootfs_entries()?;
        let complete = roots
            .iter()
            .filter(|entry| entry.ready)
            .map(|entry| entry.digest.clone())
            .collect::<BTreeSet<_>>();
        let reachable = self.reachable_blobs(&complete)?;
        let blob_entries = read_entries(&self.blob_directory())?;
        let mut blobs_to_remove = Vec::new();
        for entry in blob_entries {
            if !entry.file_type()?.is_file() {
                continue;
            }
            let path = entry.path();
            let keep = entry
                .file_name()
                .to_str()
                .and_then(|name| parse_digest_hex(name).ok())
                .is_some_and(|digest| reachable.contains(&digest));
            if !keep {
                blobs_to_remove.push((path, entry.metadata()?.len()));
            }
        }

        let incomplete_roots = roots
            .iter()
            .filter(|entry| !entry.ready)
            .collect::<Vec<_>>();
        let records = self.read_records()?;
        let stale_records = records
            .iter()
            .filter(|record| {
                parse_digest(&record.value.manifest_digest)
                    .map_or(true, |digest| !complete.contains(&digest))
            })
            .collect::<Vec<_>>();
        let reclaimed_bytes = blobs_to_remove.iter().map(|(_, bytes)| bytes).sum::<u64>()
            + incomplete_roots
                .iter()
                .map(|entry| entry.size_bytes)
                .sum::<u64>()
            + stale_records
                .iter()
                .map(|record| file_size(&record.path).unwrap_or(0))
                .sum::<u64>();

        if apply {
            for (path, _) in &blobs_to_remove {
                remove_file_if_exists(path)?;
            }
            for root in &incomplete_roots {
                remove_managed_directory(&root.path)?;
            }
            for record in &stale_records {
                remove_file_if_exists(&record.path)?;
            }
        }

        Ok(PruneReport {
            oci_blobs_removed: blobs_to_remove.len(),
            incomplete_rootfs_removed: incomplete_roots.len(),
            stale_records_removed: stale_records.len(),
            reclaimed_bytes,
            applied: apply,
        })
    }

    pub fn clean(&self, apply: bool) -> Result<CleanReport, ImageCacheError> {
        let _activity = apply.then(|| self.lock_activity(true)).transpose()?;
        let _lock = self.lock_exclusive()?;
        let paths = [
            self.root.join("images"),
            self.root.join("oci"),
            self.root.join("rootfs"),
            self.root.join("box-bases"),
            self.root.join("workspaces"),
            self.default_path(),
        ];
        let existing = paths
            .iter()
            .filter(|path| path.symlink_metadata().is_ok())
            .collect::<Vec<_>>();
        let reclaimed_bytes = existing
            .iter()
            .map(|path| path_size(path).unwrap_or(0))
            .sum();
        if apply {
            for path in &existing {
                remove_managed_path(path)?;
            }
        }
        Ok(CleanReport {
            entries_removed: existing.len(),
            reclaimed_bytes,
            applied: apply,
        })
    }

    fn lock(&self, exclusive: bool) -> Result<CacheLock, ImageCacheError> {
        self.lock_path(&self.metadata_lock_path(), exclusive)
    }

    fn lock_activity(&self, exclusive: bool) -> Result<CacheLock, ImageCacheError> {
        self.lock_path(&self.activity_lock_path(), exclusive)
    }

    fn lock_path(&self, path: &Path, exclusive: bool) -> Result<CacheLock, ImageCacheError> {
        fs::create_dir_all(&self.root)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let result = if exclusive {
            FileExt::try_lock_exclusive(&file)
        } else {
            FileExt::try_lock_shared(&file)
        };
        result.map_err(|error| ImageCacheError::Busy {
            path: path.into(),
            source: error,
        })?;
        Ok(CacheLock { file })
    }

    fn list_unlocked(&self) -> Result<Vec<CachedImage>, ImageCacheError> {
        let records = self.read_records()?;
        let roots = self.read_rootfs_entries()?;
        let default = self.default_reference_unlocked()?;
        let roots_by_digest = roots
            .iter()
            .map(|entry| (entry.digest.clone(), entry))
            .collect::<BTreeMap<_, _>>();
        let mut indexed = BTreeSet::new();
        let mut images = Vec::new();

        for record in records {
            let digest = parse_digest(&record.value.manifest_digest)?;
            indexed.insert(digest.clone());
            let rootfs_path = self.rootfs_path(&digest);
            let root = roots_by_digest.get(&digest).copied();
            let is_default = record.value.reference == default;
            images.push(CachedImage {
                reference: Some(record.value.reference),
                manifest_digest: digest.to_string(),
                platform: Some(record.value.platform),
                rootfs: rootfs_path,
                ready: root.is_some_and(|entry| entry.ready),
                default: is_default,
                size_bytes: root.map_or(0, |entry| entry.size_bytes),
            });
        }

        for root in roots {
            if !indexed.contains(&root.digest) {
                images.push(CachedImage {
                    reference: None,
                    manifest_digest: root.digest.to_string(),
                    platform: None,
                    rootfs: root.path,
                    ready: root.ready,
                    default: false,
                    size_bytes: root.size_bytes,
                });
            }
        }
        images.sort_by(|left, right| {
            left.reference
                .cmp(&right.reference)
                .then(left.manifest_digest.cmp(&right.manifest_digest))
        });
        Ok(images)
    }

    fn usage_unlocked(&self) -> Result<CacheUsage, ImageCacheError> {
        let references = self.read_records()?.len();
        let roots = self.read_rootfs_entries()?;
        let images = roots.iter().filter(|entry| entry.ready).count();
        let rootfs_bytes = roots.iter().map(|entry| entry.size_bytes).sum();
        let (base_disks, base_disk_bytes) = count_base_disks(&self.root.join("box-bases"))?;
        let (oci_blobs, oci_bytes) = count_regular_files(&self.blob_directory())?;
        let (workspaces, workspace_bytes) =
            count_regular_files(&self.root.join("workspaces/sha256"))?;
        Ok(CacheUsage {
            references,
            images,
            rootfs_bytes,
            base_disks,
            base_disk_bytes,
            oci_blobs,
            oci_bytes,
            workspaces,
            workspace_bytes,
            total_bytes: rootfs_bytes + base_disk_bytes + oci_bytes + workspace_bytes,
        })
    }

    fn reachable_blobs(
        &self,
        manifests: &BTreeSet<Digest>,
    ) -> Result<BTreeSet<Digest>, ImageCacheError> {
        let mut reachable = BTreeSet::new();
        for digest in manifests {
            let path = self.blob_path(digest);
            let metadata =
                fs::symlink_metadata(&path).map_err(|error| ImageCacheError::MissingManifest {
                    digest: digest.to_string(),
                    path: path.clone(),
                    source: error,
                })?;
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(ImageCacheError::InvalidManifest(digest.to_string()));
            }
            let bytes = fs::read(&path)?;
            if Digest::from_bytes(&bytes) != *digest {
                return Err(ImageCacheError::InvalidManifest(digest.to_string()));
            }
            let manifest: ImageManifest = serde_json::from_slice(&bytes)?;
            reachable.insert(digest.clone());
            reachable.insert(parse_digest(&manifest.config.digest)?);
            for layer in manifest.layers {
                reachable.insert(parse_digest(&layer.digest)?);
            }
        }
        Ok(reachable)
    }

    fn read_records(&self) -> Result<Vec<StoredRecord>, ImageCacheError> {
        let mut records = Vec::new();
        for entry in read_entries(&self.record_directory())? {
            let file_type = entry.file_type()?;
            if !file_type.is_file()
                || entry.path().extension().and_then(|value| value.to_str()) != Some("json")
            {
                continue;
            }
            let path = entry.path();
            let record: ImageRecord = serde_json::from_slice(&fs::read(&path)?)?;
            validate_record(&record, &path)?;
            records.push(StoredRecord {
                path,
                value: record,
            });
        }
        records.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(records)
    }

    fn read_rootfs_entries(&self) -> Result<Vec<RootfsEntry>, ImageCacheError> {
        let mut roots = Vec::new();
        for entry in read_entries(&self.rootfs_directory())? {
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Ok(digest) = parse_digest_hex(&name) else {
                let path = entry.path();
                roots.push(RootfsEntry {
                    digest: Digest::from_bytes(path.as_os_str().as_encoded_bytes()),
                    size_bytes: path_size(&path)?,
                    path,
                    ready: false,
                });
                continue;
            };
            let path = entry.path();
            roots.push(RootfsEntry {
                ready: is_complete_rootfs(&path, &digest),
                size_bytes: path_size(&path)?,
                path,
                digest,
            });
        }
        roots.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(roots)
    }

    fn read_default(&self) -> Result<Option<DefaultImage>, ImageCacheError> {
        let path = self.default_path();
        match fs::symlink_metadata(&path) {
            Ok(metadata) if !metadata.is_file() || metadata.file_type().is_symlink() => {
                return Err(ImageCacheError::InvalidManagedPath(path));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        let bytes = fs::read(&path)?;
        let mut value: DefaultImage = serde_json::from_slice(&bytes)?;
        if value.schema_version != SCHEMA_VERSION {
            return Err(ImageCacheError::UnsupportedSchema {
                path,
                schema: value.schema_version,
            });
        }
        value.reference = canonical_reference(&value.reference)?;
        Ok(Some(value))
    }

    fn default_reference_unlocked(&self) -> Result<String, ImageCacheError> {
        Ok(self
            .read_default()?
            .map_or_else(|| BUILTIN_DEFAULT_IMAGE.into(), |value| value.reference))
    }

    fn record_path(&self, record: &ImageRecord) -> PathBuf {
        self.record_path_for(&record.reference, &record.platform)
    }

    fn record_path_for(&self, reference: &str, platform: &Platform) -> PathBuf {
        let key = format!(
            "{}\0{}\0{}\0{}",
            reference,
            platform.os,
            platform.architecture,
            platform.variant.as_deref().unwrap_or_default()
        );
        self.record_directory()
            .join(format!("{}.json", Digest::from_bytes(key.as_bytes()).hex()))
    }

    fn default_path(&self) -> PathBuf {
        self.root.join("default-image.json")
    }

    fn metadata_lock_path(&self) -> PathBuf {
        self.root.join(".moraebox-cache.lock")
    }

    fn activity_lock_path(&self) -> PathBuf {
        self.root.join(".moraebox-cache.activity.lock")
    }

    fn reference_lock_path(&self, reference: &str, platform: &Platform) -> PathBuf {
        let key = format!(
            "{reference}\0{}\0{}\0{}",
            platform.os,
            platform.architecture,
            platform.variant.as_deref().unwrap_or_default()
        );
        self.root
            .join("locks/references")
            .join(format!("{}.lock", Digest::from_bytes(key.as_bytes()).hex()))
    }

    fn rootfs_lock_path(&self, digest: &Digest) -> PathBuf {
        self.root
            .join("locks/rootfs")
            .join(format!("{}.lock", digest.hex()))
    }

    fn record_directory(&self) -> PathBuf {
        self.root.join("images/sha256")
    }

    fn rootfs_directory(&self) -> PathBuf {
        self.root.join("rootfs/sha256")
    }

    fn rootfs_path(&self, digest: &Digest) -> PathBuf {
        self.rootfs_directory().join(digest.hex())
    }

    fn blob_directory(&self) -> PathBuf {
        self.root.join("oci/blobs/sha256")
    }

    fn blob_path(&self, digest: &Digest) -> PathBuf {
        self.blob_directory().join(digest.hex())
    }
}

impl Drop for CacheLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

fn prepared_image(image: CachedImage) -> PreparedImage {
    PreparedImage {
        reference: image
            .reference
            .expect("resolved cache entries always have a reference"),
        manifest_digest: image.manifest_digest,
        rootfs: image.rootfs,
    }
}

fn validate_record(record: &ImageRecord, path: &Path) -> Result<(), ImageCacheError> {
    if record.schema_version != SCHEMA_VERSION {
        return Err(ImageCacheError::UnsupportedSchema {
            path: path.into(),
            schema: record.schema_version,
        });
    }
    if canonical_reference(&record.reference)? != record.reference {
        return Err(ImageCacheError::InvalidRecord(path.into()));
    }
    parse_digest(&record.manifest_digest)?;
    if let Some(source_manifest_digest) = &record.source_manifest_digest {
        parse_digest(source_manifest_digest)?;
    }
    if record.platform.os.is_empty() || record.platform.architecture.is_empty() {
        return Err(ImageCacheError::InvalidRecord(path.into()));
    }
    Ok(())
}

fn canonical_reference(value: &str) -> Result<String, ImageCacheError> {
    let reference = ImageReference::from_str(value)
        .map_err(|error| ImageCacheError::InvalidTarget(error.to_string()))?;
    let ImageReference::Registry(reference) = reference else {
        return Err(ImageCacheError::InvalidTarget(value.into()));
    };
    Ok(reference.to_string())
}

fn pinned_source_digest(value: &str) -> Result<Option<Digest>, ImageCacheError> {
    let reference = ImageReference::from_str(value)
        .map_err(|error| ImageCacheError::InvalidTarget(error.to_string()))?;
    let ImageReference::Registry(reference) = reference else {
        return Err(ImageCacheError::InvalidTarget(value.into()));
    };
    match reference.selector {
        Selector::Digest(digest) => parse_digest(&digest).map(Some),
        Selector::Tag(_) => Ok(None),
    }
}

fn parse_target(value: &str) -> Result<ImageTarget, ImageCacheError> {
    if value.starts_with("sha256:")
        || (value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        return parse_digest(value).map(ImageTarget::Digest);
    }
    canonical_reference(value).map(ImageTarget::Reference)
}

fn parse_digest(value: &str) -> Result<Digest, ImageCacheError> {
    let normalized = if value.starts_with("sha256:") {
        value.to_owned()
    } else {
        format!("sha256:{value}")
    };
    Digest::from_str(&normalized).map_err(|_| ImageCacheError::InvalidDigest(value.into()))
}

fn parse_digest_hex(value: &str) -> Result<Digest, ImageCacheError> {
    parse_digest(value)
}

fn is_complete_rootfs(path: &Path, digest: &Digest) -> bool {
    [CURRENT_COMPLETE_MARKER, LEGACY_COMPLETE_MARKER]
        .iter()
        .any(|marker| {
            let marker = path.join(marker);
            fs::symlink_metadata(&marker).is_ok_and(|metadata| {
                metadata.is_file()
                    && !metadata.file_type().is_symlink()
                    && fs::read_to_string(marker).is_ok_and(|value| value == digest.to_string())
            })
        })
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), ImageCacheError> {
    let parent = path
        .parent()
        .ok_or_else(|| ImageCacheError::InvalidManagedPath(path.into()))?;
    fs::create_dir_all(parent)?;
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| ImageCacheError::InvalidManagedPath(path.into()))?;
    let sequence = TEMPORARY_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(".{name}.{}.{}.tmp", std::process::id(), sequence));
    let temporary_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    serde_json::to_writer_pretty(&temporary_file, value)?;
    temporary_file.sync_all()?;
    drop(temporary_file);
    match fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(_) => match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                fs::remove_file(path)?;
                fs::rename(temporary, path)?;
                Ok(())
            }
            Ok(_) => {
                let _ = fs::remove_file(temporary);
                Err(ImageCacheError::InvalidManagedPath(path.into()))
            }
            Err(error) => {
                let _ = fs::remove_file(temporary);
                Err(error.into())
            }
        },
    }
}

fn read_entries(path: &Path) -> Result<Vec<fs::DirEntry>, ImageCacheError> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    entries
        .collect::<Result<Vec<_>, _>>()
        .map_err(ImageCacheError::Io)
}

fn path_size(path: &Path) -> Result<u64, ImageCacheError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || metadata.is_file() {
        return Ok(metadata.len());
    }
    if !metadata.is_dir() {
        return Ok(0);
    }
    let mut bytes = 0_u64;
    for entry in fs::read_dir(path)? {
        bytes = bytes.saturating_add(path_size(&entry?.path())?);
    }
    Ok(bytes)
}

fn file_size(path: &Path) -> Result<u64, ImageCacheError> {
    Ok(fs::symlink_metadata(path)?.len())
}

fn count_regular_files(path: &Path) -> Result<(usize, u64), ImageCacheError> {
    let mut count = 0;
    let mut bytes = 0;
    for entry in read_entries(path)? {
        if entry.file_type()?.is_file() {
            count += 1;
            bytes += entry.metadata()?.len();
        }
    }
    Ok((count, bytes))
}

fn count_base_disks(path: &Path) -> Result<(usize, u64), ImageCacheError> {
    let mut count = 0;
    let mut bytes = 0_u64;
    for entry in read_entries(path)? {
        let metadata = entry.metadata()?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            let (child_count, child_bytes) = count_base_disks(&entry.path())?;
            count += child_count;
            bytes = bytes.saturating_add(child_bytes);
        } else if metadata.is_file() && entry.file_name() == "root.ext4" {
            count += 1;
            bytes = bytes.saturating_add(metadata.len());
        }
    }
    Ok((count, bytes))
}

fn remove_file_if_exists(path: &Path) -> Result<bool, ImageCacheError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn remove_managed_directory(path: &Path) -> Result<(), ImageCacheError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(ImageCacheError::InvalidManagedPath(path.into()));
    }
    fs::remove_dir_all(path)?;
    Ok(())
}

fn remove_managed_path(path: &Path) -> Result<(), ImageCacheError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || metadata.is_file() {
        fs::remove_file(path)?;
    } else if metadata.is_dir() {
        fs::remove_dir_all(path)?;
    } else {
        return Err(ImageCacheError::InvalidManagedPath(path.into()));
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum ImageCacheError {
    #[error("image cache is busy at {}; wait for the other operation to finish: {source}", .path.display())]
    Busy {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("image target was not found in the cache: {0}")]
    TargetNotFound(String),
    #[error("invalid image target: {0}")]
    InvalidTarget(String),
    #[error("invalid sha256 digest: {0}")]
    InvalidDigest(String),
    #[error("invalid image cache record: {}", .0.display())]
    InvalidRecord(PathBuf),
    #[error("unsupported image cache schema {schema} in {}", .path.display())]
    UnsupportedSchema { path: PathBuf, schema: u32 },
    #[error("missing cached manifest {digest} at {}: {source}", .path.display())]
    MissingManifest {
        digest: String,
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("cached manifest failed digest verification: {0}")]
    InvalidManifest(String),
    #[error("refusing to operate on unmanaged cache path: {}", .0.display())]
    InvalidManagedPath(PathBuf),
    #[error("image cache JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("image cache I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("image cache background task failed: {0}")]
    Task(String),
    #[error(transparent)]
    Registry(#[from] RegistryError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::Descriptor;

    struct Fixture {
        _directory: tempfile::TempDir,
        cache: ImageCache,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let cache = ImageCache::new(directory.path());
            Self {
                _directory: directory,
                cache,
            }
        }

        fn platform() -> Platform {
            Platform {
                os: "linux".into(),
                architecture: "amd64".into(),
                variant: None,
            }
        }

        fn add_image(&self, reference: Option<&str>) -> Digest {
            let config = b"config";
            let layer = b"layer";
            let config_digest = Digest::from_bytes(config);
            let layer_digest = Digest::from_bytes(layer);
            let manifest = ImageManifest {
                schema_version: 2,
                media_type: Some("application/vnd.oci.image.manifest.v1+json".into()),
                config: Descriptor {
                    media_type: "application/vnd.oci.image.config.v1+json".into(),
                    digest: config_digest.to_string(),
                    size: u64::try_from(config.len()).unwrap(),
                    platform: None,
                },
                layers: vec![Descriptor {
                    media_type: "application/vnd.oci.image.layer.v1.tar".into(),
                    digest: layer_digest.to_string(),
                    size: u64::try_from(layer.len()).unwrap(),
                    platform: None,
                }],
            };
            let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
            let manifest_digest = Digest::from_bytes(&manifest_bytes);
            for (digest, bytes) in [
                (&manifest_digest, manifest_bytes.as_slice()),
                (&config_digest, config.as_slice()),
                (&layer_digest, layer.as_slice()),
            ] {
                let path = self.cache.blob_path(digest);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, bytes).unwrap();
            }
            let rootfs = self.cache.rootfs_path(&manifest_digest);
            fs::create_dir_all(&rootfs).unwrap();
            fs::write(
                rootfs.join(CURRENT_COMPLETE_MARKER),
                manifest_digest.to_string(),
            )
            .unwrap();
            fs::write(rootfs.join("payload"), b"rootfs").unwrap();
            if let Some(reference) = reference {
                let lock = self.cache.lock_exclusive().unwrap();
                self.cache
                    .record_image(&lock, reference, &manifest_digest, &Self::platform())
                    .unwrap();
            }
            manifest_digest
        }
    }

    #[test]
    fn uses_python_312_as_the_builtin_default_and_resets_to_it() {
        let fixture = Fixture::new();
        assert_eq!(
            fixture.cache.default_reference().unwrap(),
            BUILTIN_DEFAULT_IMAGE
        );
        assert_eq!(
            fixture.cache.set_default("debian:bookworm").unwrap(),
            "docker.io/library/debian:bookworm"
        );
        assert_eq!(
            fixture.cache.default_reference().unwrap(),
            "docker.io/library/debian:bookworm"
        );
        assert!(fixture.cache.clear_default().unwrap());
        assert_eq!(
            fixture.cache.default_reference().unwrap(),
            BUILTIN_DEFAULT_IMAGE
        );
    }

    #[test]
    fn records_lists_and_resolves_a_ready_reference() {
        let fixture = Fixture::new();
        let digest = fixture.add_image(Some("python:3.12"));
        let images = fixture.cache.list().unwrap();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].reference.as_deref(), Some(BUILTIN_DEFAULT_IMAGE));
        assert_eq!(images[0].manifest_digest, digest.to_string());
        assert!(images[0].ready);
        assert!(images[0].default);
        assert!(
            fixture
                .cache
                .resolve_reference("python:3.12", &Fixture::platform())
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn digest_pinned_cache_hit_requires_matching_source_manifest_proof() {
        let fixture = Fixture::new();
        let selected_digest = fixture.add_image(None);
        let source_digest = Digest::from_bytes(b"top-level-index");
        let reference = format!("example.com/a/image@{source_digest}");
        let lock = fixture.cache.lock_exclusive().unwrap();
        fixture
            .cache
            .record_image(&lock, &reference, &selected_digest, &Fixture::platform())
            .unwrap();
        drop(lock);
        assert!(
            fixture
                .cache
                .resolve_reference(&reference, &Fixture::platform())
                .unwrap()
                .is_none(),
            "legacy pinned records without source proof must be re-pulled"
        );

        let lock = fixture.cache.lock_exclusive().unwrap();
        fixture
            .cache
            .record_pulled_image(
                &reference,
                &source_digest,
                &selected_digest,
                &Fixture::platform(),
            )
            .unwrap();
        drop(lock);
        let resolved = fixture
            .cache
            .resolve_reference(&reference, &Fixture::platform())
            .unwrap()
            .unwrap();
        assert_eq!(resolved.manifest_digest, selected_digest.to_string());

        let lock = fixture.cache.lock_exclusive().unwrap();
        fixture
            .cache
            .record_pulled_image(
                &reference,
                &Digest::from_bytes(b"wrong-source"),
                &selected_digest,
                &Fixture::platform(),
            )
            .unwrap();
        drop(lock);
        assert!(
            fixture
                .cache
                .resolve_reference(&reference, &Fixture::platform())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn resolving_a_reference_does_not_enumerate_unrelated_records() {
        let fixture = Fixture::new();
        fixture.add_image(Some("python:3.12"));
        let unrelated = fixture.cache.record_directory().join("unrelated.json");
        fs::write(&unrelated, b"not json").unwrap();

        let resolved = fixture
            .cache
            .resolve_reference("python:3.12", &Fixture::platform())
            .unwrap()
            .unwrap();

        assert_eq!(resolved.reference.as_deref(), Some(BUILTIN_DEFAULT_IMAGE));
        assert_eq!(resolved.size_bytes, 0);
        assert!(fixture.cache.list().is_err());
    }

    #[test]
    fn removing_one_alias_preserves_a_shared_rootfs() {
        let fixture = Fixture::new();
        let digest = fixture.add_image(Some("python:3.12"));
        let lock = fixture.cache.lock_exclusive().unwrap();
        fixture
            .cache
            .record_image(&lock, "python:latest", &digest, &Fixture::platform())
            .unwrap();
        drop(lock);

        let first = fixture.cache.remove("python:3.12", true).unwrap();
        assert!(first.rootfs_removed.is_empty());
        assert!(fixture.cache.rootfs_path(&digest).is_dir());
        assert_eq!(fixture.cache.list().unwrap().len(), 1);

        let preview = fixture.cache.remove("python:latest", false).unwrap();
        assert_eq!(preview.rootfs_removed.len(), 1);
        assert!(fixture.cache.rootfs_path(&digest).is_dir());
        fixture.cache.remove("python:latest", true).unwrap();
        assert!(!fixture.cache.rootfs_path(&digest).exists());
    }

    #[test]
    fn lists_an_unindexed_legacy_rootfs_by_digest() {
        let fixture = Fixture::new();
        let digest = fixture.add_image(None);
        let current = fixture
            .cache
            .rootfs_path(&digest)
            .join(CURRENT_COMPLETE_MARKER);
        fs::rename(
            current,
            fixture
                .cache
                .rootfs_path(&digest)
                .join(LEGACY_COMPLETE_MARKER),
        )
        .unwrap();
        let images = fixture.cache.list().unwrap();
        assert_eq!(images.len(), 1);
        assert!(images[0].reference.is_none());
        assert_eq!(images[0].manifest_digest, digest.to_string());
    }

    #[test]
    fn prune_keeps_reachable_blobs_and_removes_orphans() {
        let fixture = Fixture::new();
        let digest = fixture.add_image(Some("python:3.12"));
        let orphan = Digest::from_bytes(b"orphan");
        fs::write(fixture.cache.blob_path(&orphan), b"orphan").unwrap();
        let incomplete = fixture.cache.rootfs_directory().join(".partial");
        fs::create_dir_all(&incomplete).unwrap();
        fs::write(incomplete.join("file"), b"partial").unwrap();

        let preview = fixture.cache.prune(false).unwrap();
        assert_eq!(preview.oci_blobs_removed, 1);
        assert_eq!(preview.incomplete_rootfs_removed, 1);
        assert!(fixture.cache.blob_path(&orphan).is_file());
        assert!(fixture.cache.rootfs_path(&digest).is_dir());

        let applied = fixture.cache.prune(true).unwrap();
        assert!(applied.applied);
        assert!(!fixture.cache.blob_path(&orphan).exists());
        assert!(!incomplete.exists());
        assert!(fixture.cache.blob_path(&digest).is_file());
    }

    #[test]
    fn clean_removes_managed_entries_and_resets_the_default() {
        let fixture = Fixture::new();
        fixture.add_image(Some("python:3.12"));
        fixture.cache.set_default("debian:bookworm").unwrap();
        let workspace = fixture.cache.root.join("workspaces/sha256/workspace.ext4");
        fs::create_dir_all(workspace.parent().unwrap()).unwrap();
        fs::write(&workspace, b"workspace").unwrap();
        let base_disk = fixture
            .cache
            .root
            .join("box-bases/v1/sha256/base/root.ext4");
        fs::create_dir_all(base_disk.parent().unwrap()).unwrap();
        fs::write(&base_disk, b"base").unwrap();

        let usage = fixture.cache.usage().unwrap();
        assert_eq!(usage.base_disks, 1);
        assert_eq!(usage.base_disk_bytes, 4);

        let preview = fixture.cache.clean(false).unwrap();
        assert!(!preview.applied);
        assert!(workspace.is_file());
        assert!(base_disk.is_file());
        let applied = fixture.cache.clean(true).unwrap();
        assert!(applied.applied);
        assert!(!workspace.exists());
        assert!(!base_disk.exists());
        assert_eq!(
            fixture.cache.default_reference().unwrap(),
            BUILTIN_DEFAULT_IMAGE
        );
    }

    #[test]
    fn rejects_a_second_writer_while_the_cache_is_locked() {
        let fixture = Fixture::new();
        let _lock = fixture.cache.lock_exclusive().unwrap();
        assert!(matches!(
            fixture.cache.lock_exclusive(),
            Err(ImageCacheError::Busy { .. })
        ));
    }

    #[tokio::test]
    async fn reference_locks_do_not_depend_on_the_global_metadata_lock() {
        let fixture = Fixture::new();
        let _metadata = fixture.cache.lock_exclusive().unwrap();
        let path = fixture
            .cache
            .reference_lock_path(BUILTIN_DEFAULT_IMAGE, &Fixture::platform());

        let lock = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            AdvisoryLock::acquire(&path),
        )
        .await
        .unwrap()
        .unwrap();

        drop(lock);
    }

    #[test]
    fn destructive_management_rejects_an_active_pull_lease() {
        let fixture = Fixture::new();
        let activity = fixture.cache.lock_activity(false).unwrap();

        assert!(matches!(
            fixture.cache.clean(true),
            Err(ImageCacheError::Busy { .. })
        ));

        drop(activity);
        assert!(fixture.cache.clean(true).unwrap().applied);
    }

    #[tokio::test]
    async fn same_digest_rootfs_publish_is_double_checked() {
        let fixture = Fixture::new();
        let digest = Digest::from_bytes(b"shared-rootfs");
        let source = Digest::from_bytes(b"source-index");
        let first = staged_rootfs(&fixture.cache, &digest, b"first");
        let second = staged_rootfs(&fixture.cache, &digest, b"second");
        let first_directory = first.directory.clone();
        let second_directory = second.directory.clone();
        let platform = Fixture::platform();

        let (first_result, second_result) = tokio::join!(
            fixture.cache.publish_staged_rootfs(
                first,
                "example.com/a:first",
                &source,
                &digest,
                &platform,
            ),
            fixture.cache.publish_staged_rootfs(
                second,
                "example.com/a:second",
                &source,
                &digest,
                &platform,
            )
        );

        assert_eq!(
            first_result.unwrap().rootfs,
            fixture.cache.rootfs_path(&digest)
        );
        assert_eq!(
            second_result.unwrap().rootfs,
            fixture.cache.rootfs_path(&digest)
        );
        assert!(is_complete_rootfs(
            &fixture.cache.rootfs_path(&digest),
            &digest
        ));
        assert!(!first_directory.exists());
        assert!(!second_directory.exists());
        assert!(
            fixture
                .cache
                .resolve_reference("example.com/a:first", &Fixture::platform())
                .unwrap()
                .is_some()
        );
        assert!(
            fixture
                .cache
                .resolve_reference("example.com/a:second", &Fixture::platform())
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn cache_readiness_requires_the_expected_digest_marker() {
        let fixture = Fixture::new();
        let digest = Digest::from_bytes(b"manifest");
        let rootfs = fixture.cache.rootfs_path(&digest);
        fs::create_dir_all(&rootfs).unwrap();
        let marker = rootfs.join(CURRENT_COMPLETE_MARKER);
        fs::write(&marker, "wrong").unwrap();
        assert!(!is_complete_rootfs(&rootfs, &digest));
        fs::write(&marker, digest.to_string()).unwrap();
        assert!(is_complete_rootfs(&rootfs, &digest));

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            fs::remove_file(&marker).unwrap();
            let target = rootfs.join("target");
            fs::write(&target, digest.to_string()).unwrap();
            symlink(target, marker).unwrap();
            assert!(!is_complete_rootfs(&rootfs, &digest));
        }
    }

    fn staged_rootfs(cache: &ImageCache, digest: &Digest, payload: &[u8]) -> StagedRootfs {
        let staging = StagedRootfs::new(cache.root(), digest);
        fs::create_dir_all(&staging.rootfs).unwrap();
        fs::write(
            staging.rootfs.join(CURRENT_COMPLETE_MARKER),
            digest.to_string(),
        )
        .unwrap();
        fs::write(staging.rootfs.join("payload"), payload).unwrap();
        staging
    }
}
