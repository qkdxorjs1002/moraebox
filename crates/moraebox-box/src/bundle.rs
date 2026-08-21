use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
};

use fs2::FileExt;
use moraebox_core::BoxId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tar::{Archive, Builder, Entry, EntryType, Header};

use super::{
    BoxDiskFormat, BoxMetadata, BoxState, BoxStore, BoxStoreError, LOCK_FILE, METADATA_FILE,
    ROOT_DISK_FILE, SCHEMA_VERSION, TEMPORARY_SEQUENCE, allocated_size_bytes, now_unix_millis,
    owner_uid, remove_managed_directory, secure_directory, set_file_permissions, sync_parent,
    validate_directory, validate_labels, validate_optional_name, validate_regular_file,
    validate_tags, write_json_atomic,
};
use std::sync::atomic::Ordering;

pub const BOX_BUNDLE_SCHEMA_VERSION: u32 = 1;
const MANIFEST_FILE: &str = "manifest.json";
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_METADATA_BYTES: u64 = 1024 * 1024;
const MAX_ROOT_DISK_BYTES: u64 = 1024 * 1024 * 1024 * 1024;
const COPY_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoxBundleReport {
    pub box_id: BoxId,
    pub path: PathBuf,
    pub size_bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct BundleManifest {
    schema_version: u32,
    metadata_path: String,
    metadata_size_bytes: u64,
    metadata_sha256: String,
    root_disk_path: String,
    root_disk_size_bytes: u64,
    root_disk_sha256: String,
}

impl BundleManifest {
    fn validate(&self) -> Result<(), BoxStoreError> {
        if self.schema_version != BOX_BUNDLE_SCHEMA_VERSION {
            return Err(BoxStoreError::InvalidBundle(format!(
                "unsupported bundle schema {}; expected {BOX_BUNDLE_SCHEMA_VERSION}",
                self.schema_version
            )));
        }
        if self.metadata_path != METADATA_FILE || self.root_disk_path != ROOT_DISK_FILE {
            return Err(BoxStoreError::InvalidBundle(
                "bundle manifest contains unexpected entry paths".into(),
            ));
        }
        if self.metadata_size_bytes == 0 || self.metadata_size_bytes > MAX_METADATA_BYTES {
            return Err(BoxStoreError::InvalidBundle(
                "bundle metadata size is outside the supported bounds".into(),
            ));
        }
        if self.root_disk_size_bytes == 0 || self.root_disk_size_bytes > MAX_ROOT_DISK_BYTES {
            return Err(BoxStoreError::InvalidBundle(
                "bundle root disk size is outside the supported bounds".into(),
            ));
        }
        validate_digest(&self.metadata_sha256)?;
        validate_digest(&self.root_disk_sha256)?;
        Ok(())
    }
}

impl BoxStore {
    pub fn export_bundle(
        &self,
        box_id: BoxId,
        destination: &Path,
    ) -> Result<BoxBundleReport, BoxStoreError> {
        reject_symlink_components(destination, true)?;
        ensure_absent(destination)?;
        let parent = destination
            .parent()
            .ok_or_else(|| BoxStoreError::InvalidPath(destination.into()))?;
        validate_directory(parent, "bundle destination parent")?;
        let lease = self.try_acquire(box_id)?;
        if lease.metadata().state != BoxState::Ready {
            return Err(BoxStoreError::InvalidBundle(
                "only a ready Box can be exported".into(),
            ));
        }
        let metadata_bytes = serde_json::to_vec_pretty(lease.metadata())?;
        let metadata_size_bytes = u64::try_from(metadata_bytes.len()).map_err(|_| {
            BoxStoreError::InvalidBundle("bundle metadata does not fit in u64".into())
        })?;
        if metadata_size_bytes > MAX_METADATA_BYTES {
            return Err(BoxStoreError::InvalidBundle(
                "bundle metadata exceeds the size limit".into(),
            ));
        }
        let (root_disk_size_bytes, root_disk_sha256) = hash_file(lease.disk_path())?;
        if root_disk_size_bytes > MAX_ROOT_DISK_BYTES {
            return Err(BoxStoreError::InvalidBundle(
                "Box root disk exceeds the bundle size limit".into(),
            ));
        }
        let manifest = BundleManifest {
            schema_version: BOX_BUNDLE_SCHEMA_VERSION,
            metadata_path: METADATA_FILE.into(),
            metadata_size_bytes,
            metadata_sha256: digest_bytes(&metadata_bytes),
            root_disk_path: ROOT_DISK_FILE.into(),
            root_disk_size_bytes,
            root_disk_sha256,
        };
        let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
        let temporary = bundle_temporary_path(destination)?;
        let result = (|| {
            let mut output = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            set_file_permissions(&temporary)?;
            {
                let mut builder = Builder::new(&mut output);
                builder.sparse(true);
                append_bytes(&mut builder, MANIFEST_FILE, &manifest_bytes)?;
                append_bytes(&mut builder, METADATA_FILE, &metadata_bytes)?;
                builder.append_path_with_name(lease.disk_path(), ROOT_DISK_FILE)?;
                builder.finish()?;
            }
            output.sync_all()?;
            drop(output);
            let (size_bytes, sha256) = hash_file(&temporary)?;
            match fs::hard_link(&temporary, destination) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    return Err(BoxStoreError::InvalidPath(destination.into()));
                }
                Err(error) => return Err(error.into()),
            }
            fs::remove_file(&temporary)?;
            sync_parent(destination)?;
            Ok(BoxBundleReport {
                box_id,
                path: destination.into(),
                size_bytes,
                sha256,
            })
        })();
        if result.is_err() && temporary.symlink_metadata().is_ok() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    pub fn import_bundle(&self, source: &Path) -> Result<BoxMetadata, BoxStoreError> {
        reject_symlink_components(source, false)?;
        validate_regular_file(source, "Box bundle")?;
        self.ensure_root()?;
        secure_directory(&self.boxes_directory())?;
        let box_id = BoxId::new();
        let destination = self.box_directory(box_id);
        let staging = self.temporary_path("importing", box_id);
        if staging.symlink_metadata().is_ok() {
            remove_managed_directory(&staging)?;
        }
        secure_directory(&staging)?;
        let result = self.import_bundle_into(source, box_id, &staging, &destination);
        if result.is_err() && staging.symlink_metadata().is_ok() {
            let _ = remove_managed_directory(&staging);
        }
        result
    }

    fn import_bundle_into(
        &self,
        source: &Path,
        box_id: BoxId,
        staging: &Path,
        destination: &Path,
    ) -> Result<BoxMetadata, BoxStoreError> {
        let lock_path = staging.join(LOCK_FILE);
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&lock_path)?;
        set_file_permissions(&lock_path)?;
        FileExt::lock_exclusive(&lock)?;

        let input = open_regular_file(source)?;
        let mut archive = Archive::new(input);
        let mut entries = archive.entries()?;

        let mut manifest_entry = next_entry(&mut entries, MANIFEST_FILE, false)?;
        let manifest_bytes = read_bounded(&mut manifest_entry, MAX_MANIFEST_BYTES)?;
        let manifest: BundleManifest = serde_json::from_slice(&manifest_bytes)
            .map_err(|error| BoxStoreError::InvalidBundle(error.to_string()))?;
        manifest.validate()?;

        let mut metadata_entry = next_entry(&mut entries, METADATA_FILE, false)?;
        if metadata_entry.size() != manifest.metadata_size_bytes {
            return Err(BoxStoreError::InvalidBundle(
                "metadata entry size does not match the manifest".into(),
            ));
        }
        let metadata_bytes = read_bounded(&mut metadata_entry, MAX_METADATA_BYTES)?;
        verify_digest(&metadata_bytes, &manifest.metadata_sha256, "metadata")?;
        let mut metadata: BoxMetadata = serde_json::from_slice(&metadata_bytes)
            .map_err(|error| BoxStoreError::InvalidBundle(error.to_string()))?;
        validate_imported_metadata(&metadata)?;
        if metadata.virtual_size_bytes != manifest.root_disk_size_bytes {
            return Err(BoxStoreError::InvalidBundle(
                "Box metadata virtual size does not match the root disk entry".into(),
            ));
        }

        let mut disk_entry = next_entry(&mut entries, ROOT_DISK_FILE, true)?;
        if disk_entry.size() != manifest.root_disk_size_bytes {
            return Err(BoxStoreError::InvalidBundle(
                "root disk entry size does not match the manifest".into(),
            ));
        }
        let disk_path = staging.join(ROOT_DISK_FILE);
        let digest =
            copy_sparse_and_hash(&mut disk_entry, &disk_path, manifest.root_disk_size_bytes)?;
        if digest != manifest.root_disk_sha256 {
            return Err(BoxStoreError::InvalidBundle(
                "root disk SHA-256 does not match the manifest".into(),
            ));
        }
        if entries.next().transpose()?.is_some() {
            return Err(BoxStoreError::InvalidBundle(
                "bundle contains unexpected extra entries".into(),
            ));
        }

        let _metadata_lock = self.lock_metadata_index()?;
        self.ensure_unique_name(metadata.name.as_deref(), None)?;
        let now = now_unix_millis()?;
        metadata.schema_version = SCHEMA_VERSION;
        metadata.box_id = box_id;
        metadata.state = BoxState::Ready;
        metadata.updated_at_unix_ms = now;
        metadata.owner_uid = owner_uid(staging)?;
        metadata.physical_size_bytes = allocated_size_bytes(&disk_path)?;
        write_json_atomic(&staging.join(METADATA_FILE), &metadata)?;
        lock.sync_all()?;
        if destination.symlink_metadata().is_ok() {
            return Err(BoxStoreError::InvalidPath(destination.into()));
        }
        fs::rename(staging, destination)?;
        sync_parent(destination)?;
        Ok(metadata)
    }
}

fn append_bytes(
    builder: &mut Builder<&mut File>,
    path: &str,
    bytes: &[u8],
) -> Result<(), BoxStoreError> {
    let mut header = Header::new_gnu();
    header.set_entry_type(EntryType::Regular);
    header.set_mode(0o600);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_size(u64::try_from(bytes.len()).map_err(|_| {
        BoxStoreError::InvalidBundle("bundle entry length does not fit in u64".into())
    })?);
    header.set_cksum();
    builder.append_data(&mut header, path, bytes)?;
    Ok(())
}

fn next_entry<'a, R: Read>(
    entries: &mut tar::Entries<'a, R>,
    expected_path: &str,
    allow_sparse: bool,
) -> Result<Entry<'a, R>, BoxStoreError> {
    let entry = entries
        .next()
        .transpose()?
        .ok_or_else(|| BoxStoreError::InvalidBundle(format!("missing {expected_path}")))?;
    if entry.path_bytes().as_ref() != expected_path.as_bytes() {
        return Err(BoxStoreError::InvalidBundle(format!(
            "expected {expected_path}, found {}",
            String::from_utf8_lossy(entry.path_bytes().as_ref())
        )));
    }
    let entry_type = entry.header().entry_type();
    if !(entry_type.is_file() || allow_sparse && entry_type.is_gnu_sparse()) {
        return Err(BoxStoreError::InvalidBundle(format!(
            "{expected_path} must be a regular file"
        )));
    }
    Ok(entry)
}

fn read_bounded(reader: &mut impl Read, limit: u64) -> Result<Vec<u8>, BoxStoreError> {
    let capacity = usize::try_from(limit.min(64 * 1024)).unwrap_or(64 * 1024);
    let mut bytes = Vec::with_capacity(capacity);
    reader
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit {
        return Err(BoxStoreError::InvalidBundle(
            "bundle entry exceeds its size limit".into(),
        ));
    }
    Ok(bytes)
}

fn copy_sparse_and_hash(
    reader: &mut impl Read,
    destination: &Path,
    expected_size: u64,
) -> Result<String, BoxStoreError> {
    let mut output = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(destination)?;
    set_file_permissions(destination)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
    let mut total = 0_u64;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(u64::try_from(read).map_err(|_| {
                BoxStoreError::InvalidBundle("root disk chunk length does not fit in u64".into())
            })?)
            .ok_or_else(|| BoxStoreError::InvalidBundle("root disk size overflow".into()))?;
        if total > expected_size || total > MAX_ROOT_DISK_BYTES {
            return Err(BoxStoreError::InvalidBundle(
                "root disk exceeds its declared size".into(),
            ));
        }
        hasher.update(&buffer[..read]);
        if buffer[..read].iter().all(|byte| *byte == 0) {
            output.seek(SeekFrom::Current(i64::try_from(read).map_err(|_| {
                BoxStoreError::InvalidBundle("sparse seek length does not fit in i64".into())
            })?))?;
        } else {
            output.write_all(&buffer[..read])?;
        }
    }
    if total != expected_size {
        return Err(BoxStoreError::InvalidBundle(format!(
            "root disk size {total} does not match declared size {expected_size}"
        )));
    }
    output.set_len(total)?;
    output.sync_all()?;
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

fn validate_imported_metadata(metadata: &BoxMetadata) -> Result<(), BoxStoreError> {
    if !matches!(metadata.schema_version, 1 | SCHEMA_VERSION) {
        return Err(BoxStoreError::InvalidBundle(format!(
            "unsupported Box metadata schema {}",
            metadata.schema_version
        )));
    }
    if metadata.state != BoxState::Ready {
        return Err(BoxStoreError::InvalidBundle(
            "only ready Box metadata can be imported".into(),
        ));
    }
    if metadata.disk_format != BoxDiskFormat::RawExt4
        || metadata.manifest_digest.trim().is_empty()
        || metadata.platform.trim().is_empty()
        || metadata.virtual_size_bytes == 0
        || metadata.virtual_size_bytes > MAX_ROOT_DISK_BYTES
    {
        return Err(BoxStoreError::InvalidBundle(
            "Box metadata contains invalid required fields".into(),
        ));
    }
    validate_optional_name(metadata.name.as_deref())?;
    validate_labels(&metadata.labels)?;
    validate_tags(&metadata.tags)?;
    Ok(())
}

fn ensure_absent(path: &Path) -> Result<(), BoxStoreError> {
    match path.symlink_metadata() {
        Ok(_) => Err(BoxStoreError::InvalidPath(path.into())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn bundle_temporary_path(destination: &Path) -> Result<PathBuf, BoxStoreError> {
    let parent = destination
        .parent()
        .ok_or_else(|| BoxStoreError::InvalidPath(destination.into()))?;
    let name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| BoxStoreError::InvalidPath(destination.into()))?;
    Ok(parent.join(format!(
        ".{name}.{}-{}.tmp",
        std::process::id(),
        TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )))
}

fn reject_symlink_components(path: &Path, final_may_be_missing: bool) -> Result<(), BoxStoreError> {
    if path
        .components()
        .any(|component| component == Component::ParentDir)
    {
        return Err(BoxStoreError::InvalidPath(path.into()));
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let checked = if final_may_be_missing {
        absolute
            .parent()
            .ok_or_else(|| BoxStoreError::InvalidPath(path.into()))?
    } else {
        &absolute
    };
    let metadata = checked.symlink_metadata()?;
    if metadata.file_type().is_symlink()
        || (final_may_be_missing && !metadata.is_dir())
        || (!final_may_be_missing && !metadata.is_file())
    {
        return Err(BoxStoreError::InvalidPath(checked.into()));
    }
    Ok(())
}

#[cfg(unix)]
fn open_regular_file(path: &Path) -> Result<File, BoxStoreError> {
    let descriptor = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    )
    .map_err(io::Error::from)?;
    let file = File::from(descriptor);
    if !file.metadata()?.is_file() {
        return Err(BoxStoreError::InvalidPath(path.into()));
    }
    Ok(file)
}

#[cfg(not(unix))]
fn open_regular_file(path: &Path) -> Result<File, BoxStoreError> {
    let file = File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(BoxStoreError::InvalidPath(path.into()));
    }
    Ok(file)
}

fn hash_file(path: &Path) -> Result<(u64, String), BoxStoreError> {
    validate_regular_file(path, "bundle content")?;
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
    let mut total = 0_u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
        hasher.update(&buffer[..read]);
    }
    Ok((total, format!("sha256:{:x}", hasher.finalize())))
}

fn digest_bytes(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn verify_digest(bytes: &[u8], expected: &str, label: &str) -> Result<(), BoxStoreError> {
    if digest_bytes(bytes) != expected {
        return Err(BoxStoreError::InvalidBundle(format!(
            "{label} SHA-256 does not match the manifest"
        )));
    }
    Ok(())
}

fn validate_digest(value: &str) -> Result<(), BoxStoreError> {
    let digest = value
        .strip_prefix("sha256:")
        .ok_or_else(|| BoxStoreError::InvalidBundle("bundle digest must use sha256".into()))?;
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(BoxStoreError::InvalidBundle(
            "bundle digest must contain 64 hexadecimal characters".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CreateBox, UpdateBox};
    use proptest::prelude::*;
    use std::collections::{BTreeMap, BTreeSet};

    const DISK_BYTES: u64 = 1024 * 1024;
    const SPARSE_DISK_BYTES: u64 = 64 * 1024 * 1024;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(32))]

        #[test]
        fn property_arbitrary_bundle_bytes_never_publish_or_leave_staging(
            bytes in prop::collection::vec(any::<u8>(), 0..8192)
        ) {
            let temporary = tempfile::tempdir().unwrap();
            let bundle = temporary.path().join("fuzz.tar");
            fs::write(&bundle, bytes).unwrap();
            let store = BoxStore::new(temporary.path().join("state"));
            let before = store.list().unwrap().boxes;
            let _ = store.import_bundle(&bundle);
            prop_assert_eq!(store.list().unwrap().boxes, before);
            let staging_remains = fs::read_dir(store.boxes_directory()).unwrap().any(|entry| {
                entry.unwrap().file_name().to_string_lossy().starts_with(".importing-")
            });
            prop_assert!(!staging_remains);
        }
    }

    fn fixture() -> (tempfile::TempDir, BoxStore, PathBuf) {
        fixture_with_size(DISK_BYTES)
    }

    fn fixture_with_size(disk_bytes: u64) -> (tempfile::TempDir, BoxStore, PathBuf) {
        let temporary = tempfile::tempdir().unwrap();
        let disk = temporary.path().join("base.ext4");
        let mut file = File::create(&disk).unwrap();
        file.set_len(disk_bytes).unwrap();
        file.write_all(b"moraebox").unwrap();
        file.seek(SeekFrom::End(-1)).unwrap();
        file.write_all(&[7]).unwrap();
        let store = BoxStore::new(temporary.path().join("state"));
        (temporary, store, disk)
    }

    #[test]
    fn bundle_round_trip_preserves_metadata_content_and_sparse_disk() {
        let (temporary, store, disk) = fixture_with_size(SPARSE_DISK_BYTES);
        let created = store
            .create(
                &CreateBox::new("sha256:test", "linux/arm64", SPARSE_DISK_BYTES)
                    .with_name("backup")
                    .with_labels(BTreeMap::from([("team".into(), "core".into())]))
                    .with_tags(BTreeSet::from(["warm".into()])),
                &disk,
            )
            .unwrap();
        let bundle = temporary.path().join("box.tar");
        let report = store.export_bundle(created.box_id, &bundle).unwrap();
        assert_eq!(report.sha256, hash_file(&bundle).unwrap().1);

        store
            .update(
                created.box_id,
                &UpdateBox {
                    clear_name: true,
                    ..UpdateBox::default()
                },
            )
            .unwrap();
        let imported = store.import_bundle(&bundle).unwrap();
        let imported_disk = store.box_directory(imported.box_id).join(ROOT_DISK_FILE);

        assert_ne!(imported.box_id, created.box_id);
        assert_eq!(imported.name.as_deref(), Some("backup"));
        assert_eq!(imported.labels["team"], "core");
        assert!(imported.tags.contains("warm"));
        assert_eq!(
            fs::metadata(&imported_disk).unwrap().len(),
            SPARSE_DISK_BYTES
        );
        let mut imported_file = File::open(&imported_disk).unwrap();
        let mut head = [0_u8; 8];
        imported_file.read_exact(&mut head).unwrap();
        assert_eq!(&head, b"moraebox");
        imported_file.seek(SeekFrom::End(-1)).unwrap();
        let mut tail = [0_u8; 1];
        imported_file.read_exact(&mut tail).unwrap();
        assert_eq!(tail, [7]);
        assert!(allocated_size_bytes(&imported_disk).unwrap() < SPARSE_DISK_BYTES / 4);
    }

    #[test]
    fn export_requires_an_idle_box_and_a_new_destination() {
        let (temporary, store, disk) = fixture();
        let created = store
            .create(
                &CreateBox::new("sha256:test", "linux/arm64", DISK_BYTES),
                &disk,
            )
            .unwrap();
        let bundle = temporary.path().join("box.tar");
        let lease = store.try_acquire(created.box_id).unwrap();
        assert!(matches!(
            store.export_bundle(created.box_id, &bundle),
            Err(BoxStoreError::Busy { box_id, .. }) if box_id == created.box_id
        ));
        drop(lease);
        File::create(&bundle).unwrap();
        assert!(matches!(
            store.export_bundle(created.box_id, &bundle),
            Err(BoxStoreError::InvalidPath(path)) if path == bundle
        ));
    }

    #[test]
    fn import_rejects_bad_checksum_size_schema_and_cleans_staging() {
        let (temporary, store, disk) = fixture();
        let created = store
            .create(
                &CreateBox::new("sha256:test", "linux/arm64", DISK_BYTES),
                &disk,
            )
            .unwrap();
        let metadata = serde_json::to_vec(&created).unwrap();
        let root = fs::read(&disk).unwrap();

        for (name, mutate) in [
            ("checksum.tar", 0_u8),
            ("size.tar", 1_u8),
            ("schema.tar", 2_u8),
        ] {
            let bundle = temporary.path().join(name);
            let mut manifest = manifest_for(&metadata, &root);
            match mutate {
                0 => manifest.metadata_sha256 = format!("sha256:{}", "0".repeat(64)),
                1 => manifest.metadata_size_bytes = manifest.metadata_size_bytes.saturating_add(1),
                2 => manifest.schema_version = 99,
                _ => unreachable!(),
            }
            write_test_bundle(&bundle, &manifest, &metadata, &root, None).unwrap();
            assert!(matches!(
                store.import_bundle(&bundle),
                Err(BoxStoreError::InvalidBundle(_))
            ));
        }
        assert!(
            !fs::read_dir(store.boxes_directory()).unwrap().any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".importing-")
            })
        );
    }

    #[test]
    fn import_rejects_traversal_links_devices_duplicates_and_missing_entries() {
        let (temporary, store, disk) = fixture();
        let created = store
            .create(
                &CreateBox::new("sha256:test", "linux/arm64", DISK_BYTES),
                &disk,
            )
            .unwrap();
        let metadata = serde_json::to_vec(&created).unwrap();
        let root = fs::read(&disk).unwrap();
        let manifest = manifest_for(&metadata, &root);

        for (name, extra) in [
            ("traversal.tar", TestEntry::Traversal),
            ("symlink.tar", TestEntry::Symlink),
            ("device.tar", TestEntry::Device),
            ("duplicate.tar", TestEntry::DuplicateMetadata),
            ("missing.tar", TestEntry::MissingRoot),
        ] {
            let bundle = temporary.path().join(name);
            write_test_bundle(&bundle, &manifest, &metadata, &root, Some(extra)).unwrap();
            assert!(
                store.import_bundle(&bundle).is_err(),
                "{name} unexpectedly imported"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn bundle_paths_reject_symlink_components() {
        use std::os::unix::fs::symlink;

        let (temporary, store, disk) = fixture();
        let created = store
            .create(
                &CreateBox::new("sha256:test", "linux/arm64", DISK_BYTES),
                &disk,
            )
            .unwrap();
        let real = temporary.path().join("real");
        fs::create_dir(&real).unwrap();
        let link = temporary.path().join("link");
        symlink(&real, &link).unwrap();
        assert!(matches!(
            store.export_bundle(created.box_id, &link.join("box.tar")),
            Err(BoxStoreError::InvalidPath(_))
        ));
    }

    #[derive(Clone, Copy)]
    enum TestEntry {
        Traversal,
        Symlink,
        Device,
        DuplicateMetadata,
        MissingRoot,
    }

    fn manifest_for(metadata: &[u8], root: &[u8]) -> BundleManifest {
        BundleManifest {
            schema_version: BOX_BUNDLE_SCHEMA_VERSION,
            metadata_path: METADATA_FILE.into(),
            metadata_size_bytes: u64::try_from(metadata.len()).unwrap(),
            metadata_sha256: digest_bytes(metadata),
            root_disk_path: ROOT_DISK_FILE.into(),
            root_disk_size_bytes: u64::try_from(root.len()).unwrap(),
            root_disk_sha256: digest_bytes(root),
        }
    }

    fn write_test_bundle(
        path: &Path,
        manifest: &BundleManifest,
        metadata: &[u8],
        root: &[u8],
        special: Option<TestEntry>,
    ) -> Result<(), BoxStoreError> {
        let mut file = File::create(path)?;
        let mut builder = Builder::new(&mut file);
        let manifest_bytes = serde_json::to_vec(manifest)?;
        match special {
            Some(TestEntry::Traversal) => {
                append_raw_path(
                    &mut builder,
                    "../manifest.json",
                    &manifest_bytes,
                    EntryType::Regular,
                )?;
                append_bytes(&mut builder, METADATA_FILE, metadata)?;
                append_bytes(&mut builder, ROOT_DISK_FILE, root)?;
            }
            Some(TestEntry::Symlink) => {
                append_raw_path(&mut builder, MANIFEST_FILE, &[], EntryType::Symlink)?;
            }
            Some(TestEntry::Device) => {
                append_raw_path(&mut builder, MANIFEST_FILE, &[], EntryType::Char)?;
            }
            Some(TestEntry::DuplicateMetadata) => {
                append_bytes(&mut builder, MANIFEST_FILE, &manifest_bytes)?;
                append_bytes(&mut builder, METADATA_FILE, metadata)?;
                append_bytes(&mut builder, METADATA_FILE, metadata)?;
            }
            Some(TestEntry::MissingRoot) => {
                append_bytes(&mut builder, MANIFEST_FILE, &manifest_bytes)?;
                append_bytes(&mut builder, METADATA_FILE, metadata)?;
            }
            None => {
                append_bytes(&mut builder, MANIFEST_FILE, &manifest_bytes)?;
                append_bytes(&mut builder, METADATA_FILE, metadata)?;
                append_bytes(&mut builder, ROOT_DISK_FILE, root)?;
            }
        }
        builder.finish()?;
        Ok(())
    }

    fn append_raw_path(
        builder: &mut Builder<&mut File>,
        path: &str,
        bytes: &[u8],
        entry_type: EntryType,
    ) -> Result<(), BoxStoreError> {
        let mut header = Header::new_gnu();
        header.set_entry_type(entry_type);
        header.set_mode(0o600);
        header.set_size(u64::try_from(bytes.len()).unwrap());
        let name = path.as_bytes();
        header.as_mut_bytes()[..name.len()].copy_from_slice(name);
        header.set_cksum();
        builder.append(&header, bytes)?;
        Ok(())
    }
}
