use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read as _, Seek as _, SeekFrom, Write as _},
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
const LOCKS_DIRECTORY: &str = ".locks";
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

        let key = spec.key()?;
        let bases = self.bases_directory();
        secure_directory(&bases)?;
        let locks = bases.join(LOCKS_DIRECTORY);
        secure_directory(&locks)?;
        let lock_path = locks.join(format!("{key}.lock"));
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;
        set_file_permissions(&lock_path)?;
        FileExt::lock_exclusive(&lock).map_err(|source| BoxStoreError::BaseDiskBusy {
            path: lock_path,
            source,
        })?;
        if let Some(base) = self.get(spec)? {
            return Ok(base);
        }

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
    native_cow_clone(source, destination)?;
    finish_disk_copy(source, destination, "immutable base disk")
}

pub(crate) fn copy_disk(source: &Path, destination: &Path) -> Result<(), BoxStoreError> {
    validate_regular_file(source, "source root disk")?;
    ensure_destination_absent(destination)?;

    match native_cow_clone(source, destination) {
        Ok(()) => {}
        Err(BoxStoreError::CowCloneUnavailable { .. }) => {
            sparse_copy(source, destination)?;
        }
        Err(error) => return Err(error),
    }

    let result = finish_disk_copy(source, destination, "source root disk")
        .and_then(|()| set_file_permissions(destination).map_err(BoxStoreError::from));
    if result.is_err() {
        let _ = fs::remove_file(destination);
    }
    result
}

fn ensure_destination_absent(destination: &Path) -> Result<(), BoxStoreError> {
    match destination.symlink_metadata() {
        Ok(_) => Err(BoxStoreError::InvalidPath(destination.into())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(target_os = "macos")]
fn native_cow_clone(source: &Path, destination: &Path) -> Result<(), BoxStoreError> {
    use rustix::fs::{CloneFlags, fclonefileat};

    ensure_destination_absent(destination)?;
    let parent = destination
        .parent()
        .ok_or_else(|| BoxStoreError::InvalidPath(destination.into()))?;
    let name = destination
        .file_name()
        .ok_or_else(|| BoxStoreError::InvalidPath(destination.into()))?;
    let source_file = File::open(source)?;
    let parent_directory = File::open(parent)?;
    fclonefileat(
        &source_file,
        &parent_directory,
        name,
        CloneFlags::NOFOLLOW | CloneFlags::NOOWNERCOPY,
    )
    .map_err(|error| {
        if destination.symlink_metadata().is_ok() {
            BoxStoreError::InvalidPath(destination.into())
        } else {
            BoxStoreError::CowCloneUnavailable {
                detail: format!("clonefile failed for {}: {error}", source.display()),
            }
        }
    })
}

#[cfg(target_os = "linux")]
fn native_cow_clone(source: &Path, destination: &Path) -> Result<(), BoxStoreError> {
    use rustix::fs::ioctl_ficlone;

    ensure_destination_absent(destination)?;
    let source_file = File::open(source)?;
    let destination_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                BoxStoreError::InvalidPath(destination.into())
            } else {
                error.into()
            }
        })?;
    if let Err(error) = ioctl_ficlone(&destination_file, &source_file) {
        drop(destination_file);
        fs::remove_file(destination)?;
        return Err(BoxStoreError::CowCloneUnavailable {
            detail: format!("FICLONE failed for {}: {error}", source.display()),
        });
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn native_cow_clone(_source: &Path, _destination: &Path) -> Result<(), BoxStoreError> {
    Err(BoxStoreError::CowCloneUnavailable {
        detail: "native CoW cloning is supported only on macOS and Linux".into(),
    })
}

fn sparse_copy(source: &Path, destination: &Path) -> Result<(), BoxStoreError> {
    ensure_destination_absent(destination)?;
    let mut source_file = File::open(source)?;
    let mut destination_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                BoxStoreError::InvalidPath(destination.into())
            } else {
                error.into()
            }
        })?;
    let copy_result = (|| {
        let length = source_file.metadata()?.len();
        destination_file.set_len(length)?;
        copy_sparse_extents(&mut source_file, &mut destination_file, length).and_then(|supported| {
            if supported {
                Ok(())
            } else {
                copy_sparse_zero_ranges(&mut source_file, &mut destination_file, length)
            }
        })
    })();
    if let Err(error) = copy_result {
        drop(destination_file);
        let _ = fs::remove_file(destination);
        return Err(error.into());
    }
    Ok(())
}

#[cfg(any(
    target_os = "macos",
    target_os = "linux",
    target_os = "freebsd",
    target_os = "illumos",
    target_os = "solaris"
))]
fn copy_sparse_extents(source: &mut File, destination: &mut File, length: u64) -> io::Result<bool> {
    use rustix::{fs::SeekFrom as RustixSeekFrom, io::Errno};

    if length == 0 {
        return Ok(true);
    }
    let mut data_offset = match rustix::fs::seek(&*source, RustixSeekFrom::Data(0)) {
        Ok(offset) => offset,
        Err(Errno::NXIO) => return Ok(true),
        Err(Errno::INVAL | Errno::NOTSUP) => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    while data_offset < length {
        let hole_offset = rustix::fs::seek(&*source, RustixSeekFrom::Hole(data_offset))
            .map_err(io::Error::from)?
            .min(length);
        copy_exact_range(source, destination, data_offset, hole_offset)?;
        if hole_offset >= length {
            break;
        }
        data_offset = match rustix::fs::seek(&*source, RustixSeekFrom::Data(hole_offset)) {
            Ok(offset) => offset,
            Err(Errno::NXIO) => break,
            Err(error) => return Err(error.into()),
        };
    }
    Ok(true)
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "linux",
    target_os = "freebsd",
    target_os = "illumos",
    target_os = "solaris"
)))]
fn copy_sparse_extents(
    _source: &mut File,
    _destination: &mut File,
    _length: u64,
) -> io::Result<bool> {
    Ok(false)
}

fn copy_exact_range(
    source: &mut File,
    destination: &mut File,
    start: u64,
    end: u64,
) -> io::Result<()> {
    let length = end
        .checked_sub(start)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid sparse extent"))?;
    source.seek(SeekFrom::Start(start))?;
    destination.seek(SeekFrom::Start(start))?;
    let copied = io::copy(&mut source.take(length), destination)?;
    if copied != length {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "source disk changed while copying a sparse extent",
        ));
    }
    Ok(())
}

fn copy_sparse_zero_ranges(
    source: &mut File,
    destination: &mut File,
    length: u64,
) -> io::Result<()> {
    const BUFFER_SIZE: usize = 1024 * 1024;

    source.seek(SeekFrom::Start(0))?;
    destination.seek(SeekFrom::Start(0))?;
    let mut remaining = length;
    let mut buffer = vec![0_u8; BUFFER_SIZE];
    while remaining > 0 {
        let requested = usize::try_from(remaining.min(BUFFER_SIZE as u64))
            .map_err(|_| io::Error::other("copy buffer length overflow"))?;
        source.read_exact(&mut buffer[..requested])?;
        if buffer[..requested].iter().all(|byte| *byte == 0) {
            destination.seek(SeekFrom::Current(
                i64::try_from(requested)
                    .map_err(|_| io::Error::other("copy seek offset overflow"))?,
            ))?;
        } else {
            destination.write_all(&buffer[..requested])?;
        }
        remaining -= u64::try_from(requested)
            .map_err(|_| io::Error::other("copy buffer length overflow"))?;
    }
    Ok(())
}

fn finish_disk_copy(
    source: &Path,
    destination: &Path,
    source_label: &str,
) -> Result<(), BoxStoreError> {
    let source_size = fs::metadata(source)?.len();
    let destination_size = fs::metadata(destination)?.len();
    if source_size != destination_size {
        let _ = fs::remove_file(destination);
        return Err(BoxStoreError::CorruptStore(format!(
            "copied disk size {destination_size} does not match {source_label} size {source_size}"
        )));
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
    use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
    use std::thread;
    use std::time::{Duration, Instant};

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
    fn blocking_mke2fs(directory: &Path) -> (PathBuf, PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;

        let executable = directory.join("blocking-mke2fs");
        let calls = directory.join("blocking-calls");
        let release = directory.join("release-mke2fs");
        let mut file = File::create(&executable).unwrap();
        writeln!(file, "#!/bin/sh").unwrap();
        writeln!(file, "printf x >> '{}'", calls.display()).unwrap();
        writeln!(
            file,
            "while [ ! -f '{}' ]; do sleep 0.01; done",
            release.display()
        )
        .unwrap();
        writeln!(file, "exit 0").unwrap();
        drop(file);
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        (executable, calls, release)
    }

    fn wait_for_call_count(path: &Path, expected: usize) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            let count = fs::read(path).map_or(0, |bytes| bytes.len());
            if count >= expected {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        false
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

    #[cfg(unix)]
    #[test]
    fn same_base_key_waits_and_reuses_one_preparation() {
        let temporary = tempfile::tempdir().unwrap();
        let rootfs = temporary.path().join("rootfs");
        fs::create_dir(&rootfs).unwrap();
        let (mke2fs, calls, release) = blocking_mke2fs(temporary.path());
        let store = BaseDiskStore::new(temporary.path().join("cache"));
        let spec = BaseDiskSpec::new("sha256:same", "linux/arm64", DISK_BYTES);

        let first_store = store.clone();
        let first_spec = spec.clone();
        let first_rootfs = rootfs.clone();
        let first_mke2fs = mke2fs.clone();
        let first =
            thread::spawn(move || first_store.prepare(&first_spec, &first_rootfs, &first_mke2fs));
        assert!(wait_for_call_count(&calls, 1));

        let second_store = store.clone();
        let second_rootfs = rootfs.clone();
        let second_mke2fs = mke2fs.clone();
        let second_spec = spec.clone();
        let second = thread::spawn(move || {
            second_store.prepare(&second_spec, &second_rootfs, &second_mke2fs)
        });
        thread::sleep(Duration::from_millis(100));
        assert_eq!(fs::read(&calls).unwrap(), b"x");
        assert!(!second.is_finished());

        fs::write(release, b"release").unwrap();
        let first = first.join().unwrap().unwrap();
        let second = second.join().unwrap().unwrap();
        assert_eq!(first, second);
        assert_eq!(fs::read(calls).unwrap(), b"x");
    }

    #[cfg(unix)]
    #[test]
    fn different_base_keys_prepare_concurrently() {
        let temporary = tempfile::tempdir().unwrap();
        let rootfs = temporary.path().join("rootfs");
        fs::create_dir(&rootfs).unwrap();
        let (mke2fs, calls, release) = blocking_mke2fs(temporary.path());
        let store = BaseDiskStore::new(temporary.path().join("cache"));

        let first_store = store.clone();
        let first_rootfs = rootfs.clone();
        let first_mke2fs = mke2fs.clone();
        let first = thread::spawn(move || {
            first_store.prepare(
                &BaseDiskSpec::new("sha256:first", "linux/arm64", DISK_BYTES),
                &first_rootfs,
                &first_mke2fs,
            )
        });
        assert!(wait_for_call_count(&calls, 1));

        let second_store = store.clone();
        let second_rootfs = rootfs.clone();
        let second_mke2fs = mke2fs.clone();
        let second = thread::spawn(move || {
            second_store.prepare(
                &BaseDiskSpec::new("sha256:second", "linux/arm64", DISK_BYTES),
                &second_rootfs,
                &second_mke2fs,
            )
        });
        let entered_concurrently = wait_for_call_count(&calls, 2);
        fs::write(release, b"release").unwrap();

        first.join().unwrap().unwrap();
        second.join().unwrap().unwrap();
        assert!(entered_concurrently);
        assert_eq!(fs::read(calls).unwrap(), b"xx");
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

        assert!(copy_disk(&source, &destination).is_err());
        assert_eq!(fs::read(destination).unwrap(), b"existing");
    }

    #[test]
    fn sparse_copy_preserves_content_size_and_holes() {
        const SPARSE_BYTES: u64 = 64 * 1024 * 1024;

        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source.ext4");
        let destination = temporary.path().join("destination.ext4");
        let mut source_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&source)
            .unwrap();
        source_file.set_len(SPARSE_BYTES).unwrap();
        source_file.write_all(b"head").unwrap();
        source_file.seek(SeekFrom::Start(SPARSE_BYTES - 4)).unwrap();
        source_file.write_all(b"tail").unwrap();
        source_file.sync_all().unwrap();
        drop(source_file);

        sparse_copy(&source, &destination).unwrap();
        finish_disk_copy(&source, &destination, "test source").unwrap();

        let mut copied = File::open(&destination).unwrap();
        let mut head = [0_u8; 4];
        copied.read_exact(&mut head).unwrap();
        assert_eq!(&head, b"head");
        copied.seek(SeekFrom::Start(SPARSE_BYTES - 4)).unwrap();
        let mut tail = [0_u8; 4];
        copied.read_exact(&mut tail).unwrap();
        assert_eq!(&tail, b"tail");
        assert_eq!(copied.metadata().unwrap().len(), SPARSE_BYTES);

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;

            let allocated_bytes = copied.metadata().unwrap().blocks().saturating_mul(512);
            assert!(allocated_bytes < SPARSE_BYTES / 4);
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn native_cow_clone_is_content_independent() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source.ext4");
        let destination = temporary.path().join("destination.ext4");
        fs::write(&source, b"source").unwrap();

        match native_cow_clone(&source, &destination) {
            Ok(()) => {
                fs::write(&destination, b"cloned").unwrap();
                assert_eq!(fs::read(&source).unwrap(), b"source");
                assert_eq!(fs::read(&destination).unwrap(), b"cloned");
            }
            Err(BoxStoreError::CowCloneUnavailable { .. }) => {
                assert!(!destination.exists());
            }
            Err(error) => panic!("unexpected clone error: {error}"),
        }
    }
}
