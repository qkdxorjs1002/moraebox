use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    path::Path,
    sync::atomic::Ordering,
};

use serde::Serialize;

use super::{ImageCacheError, TEMPORARY_FILE_SEQUENCE, metadata::StorageUsage};
use crate::durability::sync_directory;

pub(super) fn write_json_atomic<T: Serialize>(
    path: &Path,
    value: &T,
) -> Result<(), ImageCacheError> {
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
    let result = match fs::rename(&temporary, path) {
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
    };
    result?;
    sync_directory(parent)?;
    Ok(())
}

pub(super) fn read_entries(path: &Path) -> Result<Vec<fs::DirEntry>, ImageCacheError> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    entries
        .collect::<Result<Vec<_>, _>>()
        .map_err(ImageCacheError::Io)
}

pub(super) fn path_size(path: &Path) -> Result<u64, ImageCacheError> {
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

pub(super) fn tree_storage_usage(path: &Path) -> Result<StorageUsage, ImageCacheError> {
    let mut seen = BTreeSet::new();
    tree_storage_usage_inner(path, &mut seen)
}

pub(super) fn tree_storage_usage_inner(
    path: &Path,
    seen: &mut BTreeSet<(u64, u64)>,
) -> Result<StorageUsage, ImageCacheError> {
    let metadata = fs::symlink_metadata(path)?;
    let file_type = metadata.file_type();
    let mut usage = StorageUsage {
        logical_bytes: if file_type.is_symlink() || metadata.is_file() {
            metadata.len()
        } else {
            0
        },
        allocated_bytes: allocated_bytes(&metadata),
        entries: 1,
    };

    if !metadata.is_dir() || file_type.is_symlink() {
        if is_duplicate_file(&metadata, seen) {
            return Ok(StorageUsage::default());
        }
        return Ok(usage);
    }

    for entry in fs::read_dir(path)? {
        usage.add(tree_storage_usage_inner(&entry?.path(), seen)?);
    }
    Ok(usage)
}

#[cfg(unix)]
pub(super) fn allocated_bytes(metadata: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;

    metadata.blocks().saturating_mul(512)
}

#[cfg(not(unix))]
pub(super) fn allocated_bytes(metadata: &fs::Metadata) -> u64 {
    metadata.len()
}

#[cfg(unix)]
pub(super) fn is_duplicate_file(metadata: &fs::Metadata, seen: &mut BTreeSet<(u64, u64)>) -> bool {
    use std::os::unix::fs::MetadataExt;

    !seen.insert((metadata.dev(), metadata.ino()))
}

#[cfg(not(unix))]
pub(super) fn is_duplicate_file(
    _metadata: &fs::Metadata,
    _seen: &mut BTreeSet<(u64, u64)>,
) -> bool {
    false
}

pub(super) fn file_size(path: &Path) -> Result<u64, ImageCacheError> {
    Ok(fs::symlink_metadata(path)?.len())
}

pub(super) fn count_regular_files(path: &Path) -> Result<(usize, StorageUsage), ImageCacheError> {
    let mut count = 0;
    let mut usage = StorageUsage::default();
    for entry in read_entries(path)? {
        if entry.file_type()?.is_file() {
            let metadata = entry.metadata()?;
            count += 1;
            usage.logical_bytes = usage.logical_bytes.saturating_add(metadata.len());
            usage.allocated_bytes = usage
                .allocated_bytes
                .saturating_add(allocated_bytes(&metadata));
            usage.entries = usage.entries.saturating_add(1);
        }
    }
    Ok((count, usage))
}

pub(super) fn count_base_disks(path: &Path) -> Result<(usize, StorageUsage), ImageCacheError> {
    let mut count = 0;
    let mut usage = StorageUsage::default();
    for entry in read_entries(path)? {
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            let (child_count, child_usage) = count_base_disks(&entry.path())?;
            count += child_count;
            usage.add(child_usage);
        } else if metadata.is_file() && entry.file_name() == "root.ext4" {
            count += 1;
            usage.logical_bytes = usage.logical_bytes.saturating_add(metadata.len());
            usage.allocated_bytes = usage
                .allocated_bytes
                .saturating_add(allocated_bytes(&metadata));
            usage.entries = usage.entries.saturating_add(1);
        }
    }
    Ok((count, usage))
}

pub(super) fn remove_file_if_exists(path: &Path) -> Result<bool, ImageCacheError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn remove_managed_directory(path: &Path) -> Result<(), ImageCacheError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(ImageCacheError::InvalidManagedPath(path.into()));
    }
    fs::remove_dir_all(path)?;
    Ok(())
}

pub(super) fn remove_managed_path(path: &Path) -> Result<(), ImageCacheError> {
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

pub(super) fn remove_managed_path_if_exists(path: &Path) -> Result<bool, ImageCacheError> {
    match fs::symlink_metadata(path) {
        Ok(_) => {
            remove_managed_path(path)?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}
