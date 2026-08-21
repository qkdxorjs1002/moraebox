use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    process::Command,
    str::FromStr,
    sync::atomic::{AtomicU64, Ordering},
};

use fs2::FileExt;
use moraebox_core::{SessionId, ensure_private_storage_root};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::{
    BoxDiskFormat, BoxStoreError, now_unix_millis, remove_managed_directory, secure_directory,
    set_file_permissions, sync_parent, validate_directory, validate_regular_file,
    write_json_atomic,
};

pub const BASE_DISK_LAYOUT_VERSION: u32 = 1;
pub const DEFAULT_BOX_DISK_SIZE_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const BASES_DIRECTORY: &str = "box-bases/v1/sha256";
const EPHEMERAL_DIRECTORY: &str = "ephemeral-boxes";
const ROOT_DISK_FILE: &str = "root.ext4";
const METADATA_FILE: &str = "metadata.json";
const LOCK_FILE: &str = ".lock";
static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseDiskSpec {
    pub manifest_digest: String,
    pub platform: String,
    pub virtual_size_bytes: u64,
}

impl BaseDiskSpec {
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

    pub fn key(&self) -> Result<String, BoxStoreError> {
        self.validate()?;
        let mut hasher = Sha256::new();
        for value in [
            BASE_DISK_LAYOUT_VERSION.to_string(),
            self.manifest_digest.clone(),
            self.platform.clone(),
            self.virtual_size_bytes.to_string(),
            "raw_ext4".into(),
        ] {
            hasher.update(value.as_bytes());
            hasher.update([0]);
        }
        Ok(to_hex(&hasher.finalize()))
    }

    fn validate(&self) -> Result<(), BoxStoreError> {
        if self.manifest_digest.trim().is_empty() {
            return Err(BoxStoreError::InvalidMetadata(
                "base disk manifest digest must not be empty".into(),
            ));
        }
        if self.platform.trim().is_empty() {
            return Err(BoxStoreError::InvalidMetadata(
                "base disk platform must not be empty".into(),
            ));
        }
        if self.virtual_size_bytes == 0 {
            return Err(BoxStoreError::InvalidMetadata(
                "base disk virtual size must be greater than zero".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaseDiskMetadata {
    pub schema_version: u32,
    pub layout_version: u32,
    pub key: String,
    pub manifest_digest: String,
    pub platform: String,
    pub disk_format: BoxDiskFormat,
    pub virtual_size_bytes: u64,
    pub created_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseDisk {
    metadata: BaseDiskMetadata,
    disk_path: PathBuf,
}

impl BaseDisk {
    pub fn metadata(&self) -> &BaseDiskMetadata {
        &self.metadata
    }

    pub fn disk_path(&self) -> &Path {
        &self.disk_path
    }
}

#[derive(Debug, Clone)]
pub struct BaseDiskStore {
    cache_root: PathBuf,
}

impl BaseDiskStore {
    pub fn new(cache_root: impl Into<PathBuf>) -> Self {
        Self {
            cache_root: cache_root.into(),
        }
    }

    pub fn get(&self, spec: &BaseDiskSpec) -> Result<Option<BaseDisk>, BoxStoreError> {
        ensure_private_storage_root(&self.cache_root)?;
        let key = spec.key()?;
        let directory = self.base_directory(&key);
        if !directory.exists() {
            return Ok(None);
        }
        validate_directory(&directory, "base disk directory")?;
        let metadata_path = directory.join(METADATA_FILE);
        let disk_path = directory.join(ROOT_DISK_FILE);
        validate_regular_file(&metadata_path, "base disk metadata")?;
        validate_regular_file(&disk_path, "base root disk")?;
        let metadata: BaseDiskMetadata = serde_json::from_slice(&fs::read(&metadata_path)?)?;
        validate_base_metadata(&metadata, spec, &key)?;
        if fs::metadata(&disk_path)?.len() != spec.virtual_size_bytes {
            return Err(BoxStoreError::CorruptStore(format!(
                "base disk size does not match metadata at {}",
                disk_path.display()
            )));
        }
        Ok(Some(BaseDisk {
            metadata,
            disk_path,
        }))
    }

    pub fn prepare(
        &self,
        spec: &BaseDiskSpec,
        rootfs: &Path,
        mke2fs: &Path,
    ) -> Result<BaseDisk, BoxStoreError> {
        ensure_private_storage_root(&self.cache_root)?;
        spec.validate()?;
        validate_directory(rootfs, "materialized rootfs")?;
        if let Some(base) = self.get(spec)? {
            return Ok(base);
        }

        let bases = self.bases_directory();
        secure_directory(&bases)?;
        let lock_path = bases.join(".prepare.lock");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;
        set_file_permissions(&lock_path)?;
        FileExt::try_lock_exclusive(&lock).map_err(|source| BoxStoreError::BaseDiskBusy {
            path: lock_path,
            source,
        })?;
        if let Some(base) = self.get(spec)? {
            return Ok(base);
        }

        let key = spec.key()?;
        let destination = self.base_directory(&key);
        let staging = bases.join(format!(
            ".creating-{key}-{}-{}",
            std::process::id(),
            TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        if staging.exists() {
            remove_managed_directory(&staging)?;
        }
        secure_directory(&staging)?;
        let disk_path = staging.join(ROOT_DISK_FILE);
        let result = (|| {
            let disk = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&disk_path)?;
            disk.set_len(spec.virtual_size_bytes)?;
            disk.sync_all()?;
            drop(disk);
            let output = Command::new(mke2fs)
                .args(["-q", "-t", "ext4", "-F", "-m", "0", "-d"])
                .arg(rootfs)
                .arg(&disk_path)
                .output()?;
            if !output.status.success() {
                return Err(BoxStoreError::Mke2fs {
                    status: output.status.code(),
                    stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
                });
            }
            if fs::metadata(&disk_path)?.len() != spec.virtual_size_bytes {
                return Err(BoxStoreError::CorruptStore(format!(
                    "mke2fs changed the virtual disk size at {}",
                    disk_path.display()
                )));
            }
            set_read_only(&disk_path)?;
            let metadata = BaseDiskMetadata {
                schema_version: 1,
                layout_version: BASE_DISK_LAYOUT_VERSION,
                key: key.clone(),
                manifest_digest: spec.manifest_digest.clone(),
                platform: spec.platform.clone(),
                disk_format: BoxDiskFormat::RawExt4,
                virtual_size_bytes: spec.virtual_size_bytes,
                created_at_unix_ms: now_unix_millis()?,
            };
            write_json_atomic(&staging.join(METADATA_FILE), &metadata)?;
            fs::rename(&staging, &destination)?;
            sync_parent(&destination)?;
            Ok(BaseDisk {
                metadata,
                disk_path: destination.join(ROOT_DISK_FILE),
            })
        })();
        if result.is_err() && staging.exists() {
            let _ = remove_managed_directory(&staging);
        }
        result
    }

    fn bases_directory(&self) -> PathBuf {
        self.cache_root.join(BASES_DIRECTORY)
    }

    fn base_directory(&self, key: &str) -> PathBuf {
        self.bases_directory().join(key)
    }
}

#[derive(Debug, Clone)]
pub struct EphemeralDiskStore {
    runtime_root: PathBuf,
}

#[derive(Debug)]
pub struct EphemeralDisk {
    lock_file: File,
    directory: PathBuf,
    disk_path: PathBuf,
    session_id: SessionId,
}

impl EphemeralDisk {
    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn disk_path(&self) -> &Path {
        &self.disk_path
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EphemeralGcReport {
    pub removed: usize,
    pub skipped_busy: usize,
}

impl EphemeralDiskStore {
    pub fn new(runtime_root: impl Into<PathBuf>) -> Self {
        Self {
            runtime_root: runtime_root.into(),
        }
    }

    pub fn clone_for_session(
        &self,
        base: &BaseDisk,
        session_id: SessionId,
    ) -> Result<EphemeralDisk, BoxStoreError> {
        ensure_private_storage_root(&self.runtime_root)?;
        let root = self.directory();
        secure_directory(&root)?;
        let destination = root.join(session_id.to_string());
        if destination.exists() {
            return Err(BoxStoreError::EphemeralExists(session_id));
        }
        let staging = root.join(format!(
            ".creating-{session_id}-{}-{}",
            std::process::id(),
            TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        secure_directory(&staging)?;
        let result = (|| {
            let lock_path = staging.join(LOCK_FILE);
            let lock_file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&lock_path)?;
            set_file_permissions(&lock_path)?;
            FileExt::try_lock_exclusive(&lock_file).map_err(|source| {
                BoxStoreError::BaseDiskBusy {
                    path: lock_path,
                    source,
                }
            })?;
            let disk_path = staging.join(ROOT_DISK_FILE);
            strict_cow_clone(base.disk_path(), &disk_path)?;
            set_file_permissions(&disk_path)?;
            fs::rename(&staging, &destination)?;
            sync_parent(&destination)?;
            Ok(EphemeralDisk {
                lock_file,
                disk_path: destination.join(ROOT_DISK_FILE),
                directory: destination,
                session_id,
            })
        })();
        if result.is_err() && staging.exists() {
            let _ = remove_managed_directory(&staging);
        }
        result
    }

    pub fn garbage_collect(&self) -> Result<EphemeralGcReport, BoxStoreError> {
        ensure_private_storage_root(&self.runtime_root)?;
        let root = self.directory();
        if !root.exists() {
            return Ok(EphemeralGcReport::default());
        }
        validate_directory(&root, "ephemeral disk directory")?;
        let mut report = EphemeralGcReport::default();
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if name.starts_with('.') || SessionId::from_str(&name).is_err() {
                continue;
            }
            let directory = entry.path();
            validate_directory(&directory, "ephemeral session directory")?;
            let lock_path = directory.join(LOCK_FILE);
            let lock_file = match OpenOptions::new().read(true).write(true).open(&lock_path) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    remove_managed_directory(&directory)?;
                    report.removed += 1;
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            if FileExt::try_lock_exclusive(&lock_file).is_err() {
                report.skipped_busy += 1;
                continue;
            }
            remove_managed_directory(&directory)?;
            report.removed += 1;
        }
        Ok(report)
    }

    fn directory(&self) -> PathBuf {
        self.runtime_root.join(EPHEMERAL_DIRECTORY)
    }
}

impl Drop for EphemeralDisk {
    fn drop(&mut self) {
        let tombstone = self.directory.with_file_name(format!(
            ".deleting-{}-{}-{}",
            self.session_id,
            std::process::id(),
            TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let target = if fs::rename(&self.directory, &tombstone).is_ok() {
            &tombstone
        } else {
            &self.directory
        };
        let _ = FileExt::unlock(&self.lock_file);
        let _ = remove_managed_directory(target);
    }
}

fn validate_base_metadata(
    metadata: &BaseDiskMetadata,
    spec: &BaseDiskSpec,
    expected_key: &str,
) -> Result<(), BoxStoreError> {
    if metadata.schema_version != 1
        || metadata.layout_version != BASE_DISK_LAYOUT_VERSION
        || metadata.key != expected_key
        || metadata.manifest_digest != spec.manifest_digest
        || metadata.platform != spec.platform
        || metadata.disk_format != BoxDiskFormat::RawExt4
        || metadata.virtual_size_bytes != spec.virtual_size_bytes
    {
        return Err(BoxStoreError::InvalidMetadata(format!(
            "base disk metadata does not match key {expected_key}"
        )));
    }
    Ok(())
}

fn strict_cow_clone(source: &Path, destination: &Path) -> Result<(), BoxStoreError> {
    validate_regular_file(source, "immutable base disk")?;
    if destination.exists() {
        return Err(BoxStoreError::InvalidPath(destination.into()));
    }

    #[cfg(target_os = "macos")]
    let output = Command::new("/bin/cp")
        .arg("-c")
        .arg(source)
        .arg(destination)
        .output()?;

    #[cfg(target_os = "linux")]
    let output = Command::new("cp")
        .args(["--reflink=always", "--sparse=always", "--"])
        .arg(source)
        .arg(destination)
        .output()?;

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    return Err(BoxStoreError::CowCloneUnavailable {
        detail: "strict CoW cloning is supported only on macOS and Linux".into(),
    });

    if !output.status.success() {
        let _ = fs::remove_file(destination);
        return Err(BoxStoreError::CowCloneUnavailable {
            detail: format!(
                "clone command exited with status {:?}: {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }
    if fs::metadata(source)?.len() != fs::metadata(destination)?.len() {
        let _ = fs::remove_file(destination);
        return Err(BoxStoreError::CorruptStore(
            "CoW clone size does not match immutable base disk".into(),
        ));
    }
    File::open(destination)?.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn set_read_only(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o400))
}

#[cfg(not(unix))]
fn set_read_only(path: &Path) -> std::io::Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_readonly(true);
    fs::set_permissions(path, permissions)
}

fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        value.push(char::from(DIGITS[usize::from(byte >> 4)]));
        value.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    value
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    const DISK_BYTES: u64 = 8 * 1024 * 1024;

    #[cfg(unix)]
    fn fake_mke2fs(directory: &Path) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;

        let executable = directory.join("mke2fs");
        let calls = directory.join("calls");
        let mut file = File::create(&executable).unwrap();
        writeln!(file, "#!/bin/sh").unwrap();
        writeln!(file, "printf x >> '{}'", calls.display()).unwrap();
        writeln!(file, "exit 0").unwrap();
        drop(file);
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        (executable, calls)
    }

    #[cfg(unix)]
    #[test]
    fn prepares_a_base_once_and_uses_direct_lookup_afterward() {
        let temporary = tempfile::tempdir().unwrap();
        let rootfs = temporary.path().join("rootfs");
        fs::create_dir(&rootfs).unwrap();
        fs::write(rootfs.join("payload"), b"rootfs").unwrap();
        let (mke2fs, calls) = fake_mke2fs(temporary.path());
        let store = BaseDiskStore::new(temporary.path().join("cache"));
        let spec = BaseDiskSpec::new("sha256:abc", "linux/arm64", DISK_BYTES);

        let first = store.prepare(&spec, &rootfs, &mke2fs).unwrap();
        let second = store.prepare(&spec, &rootfs, &mke2fs).unwrap();

        assert_eq!(first, second);
        assert_eq!(fs::read(calls).unwrap(), b"x");
        assert_eq!(first.metadata().key, spec.key().unwrap());
        assert_eq!(fs::metadata(first.disk_path()).unwrap().len(), DISK_BYTES);
    }

    #[test]
    fn base_key_changes_with_every_disk_input() {
        let base = BaseDiskSpec::new("sha256:abc", "linux/arm64", DISK_BYTES);
        let other_digest = BaseDiskSpec::new("sha256:def", "linux/arm64", DISK_BYTES);
        let other_platform = BaseDiskSpec::new("sha256:abc", "linux/amd64", DISK_BYTES);
        let other_size = BaseDiskSpec::new("sha256:abc", "linux/arm64", DISK_BYTES * 2);

        assert_ne!(base.key().unwrap(), other_digest.key().unwrap());
        assert_ne!(base.key().unwrap(), other_platform.key().unwrap());
        assert_ne!(base.key().unwrap(), other_size.key().unwrap());
    }

    #[test]
    fn garbage_collection_removes_an_unlocked_orphan() {
        let temporary = tempfile::tempdir().unwrap();
        let store = EphemeralDiskStore::new(temporary.path());
        let session_id = SessionId::new();
        let orphan = store.directory().join(session_id.to_string());
        secure_directory(&orphan).unwrap();
        File::create(orphan.join(LOCK_FILE)).unwrap();

        let report = store.garbage_collect().unwrap();

        assert_eq!(report.removed, 1);
        assert_eq!(report.skipped_busy, 0);
        assert!(!orphan.exists());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn ephemeral_clone_is_strict_and_is_removed_on_drop() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("base.ext4");
        let file = File::create(&source).unwrap();
        file.set_len(DISK_BYTES).unwrap();
        drop(file);
        let metadata = BaseDiskMetadata {
            schema_version: 1,
            layout_version: BASE_DISK_LAYOUT_VERSION,
            key: "test".into(),
            manifest_digest: "sha256:abc".into(),
            platform: "linux/arm64".into(),
            disk_format: BoxDiskFormat::RawExt4,
            virtual_size_bytes: DISK_BYTES,
            created_at_unix_ms: 0,
        };
        let base = BaseDisk {
            metadata,
            disk_path: source,
        };
        let store = EphemeralDiskStore::new(temporary.path().join("runtime"));
        let session_id = SessionId::new();

        match store.clone_for_session(&base, session_id) {
            Ok(clone) => {
                let directory = clone.disk_path().parent().unwrap().to_path_buf();
                assert_eq!(fs::metadata(clone.disk_path()).unwrap().len(), DISK_BYTES);
                drop(clone);
                assert!(!directory.exists());
            }
            Err(BoxStoreError::CowCloneUnavailable { .. }) => {
                assert!(!store.directory().join(session_id.to_string()).exists());
            }
            Err(error) => panic!("unexpected clone error: {error}"),
        }
    }

    #[test]
    fn disk_copy_helper_never_accepts_a_preexisting_destination() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        let destination = temporary.path().join("destination");
        fs::write(&source, b"source").unwrap();
        fs::write(&destination, b"existing").unwrap();

        assert!(crate::copy_disk(&source, &destination).is_err());
        assert_eq!(fs::read(destination).unwrap(), b"existing");
    }
}
