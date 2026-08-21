use std::{
    ffi::OsStr,
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
};

use flate2::read::GzDecoder;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerCompression {
    None,
    Gzip,
    Zstd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayerLimits {
    pub max_expanded_bytes: u64,
    pub max_file_bytes: u64,
    pub max_entries: u64,
}

impl Default for LayerLimits {
    fn default() -> Self {
        Self {
            max_expanded_bytes: 8 * 1024 * 1024 * 1024,
            max_file_bytes: 2 * 1024 * 1024 * 1024,
            max_entries: 1_000_000,
        }
    }
}

#[derive(Debug)]
pub(crate) struct LayerBudget {
    limits: LayerLimits,
    expanded_bytes: u64,
    entries: u64,
}

impl LayerBudget {
    pub(crate) fn new(limits: LayerLimits) -> Self {
        Self {
            limits,
            expanded_bytes: 0,
            entries: 0,
        }
    }

    fn charge<R: Read>(
        &mut self,
        entry: &tar::Entry<'_, R>,
        path: &Path,
    ) -> Result<(), LayerError> {
        self.entries = self
            .entries
            .checked_add(1)
            .ok_or(LayerError::EntryLimitExceeded {
                maximum: self.limits.max_entries,
            })?;
        if self.entries > self.limits.max_entries {
            return Err(LayerError::EntryLimitExceeded {
                maximum: self.limits.max_entries,
            });
        }

        if !entry.header().entry_type().is_file() {
            return Ok(());
        }
        let size = entry.header().size()?;
        if size > self.limits.max_file_bytes {
            return Err(LayerError::FileSizeLimitExceeded {
                path: path.to_path_buf(),
                size,
                maximum: self.limits.max_file_bytes,
            });
        }
        let attempted = self.expanded_bytes.saturating_add(size);
        if attempted > self.limits.max_expanded_bytes {
            return Err(LayerError::ExpandedSizeLimitExceeded {
                attempted,
                maximum: self.limits.max_expanded_bytes,
            });
        }
        self.expanded_bytes = attempted;
        Ok(())
    }
}

impl LayerCompression {
    pub fn from_media_type(media_type: &str) -> Result<Self, LayerError> {
        if media_type.strip_suffix("+gzip").is_some() || media_type.strip_suffix(".gzip").is_some()
        {
            Ok(Self::Gzip)
        } else if media_type.ends_with("+zstd") {
            Ok(Self::Zstd)
        } else if media_type.strip_suffix(".tar").is_some() || media_type.contains("layer.v1.tar") {
            Ok(Self::None)
        } else {
            Err(LayerError::UnsupportedMediaType(media_type.into()))
        }
    }
}

pub fn apply_layer(
    reader: impl Read,
    compression: LayerCompression,
    root: &Path,
) -> Result<(), LayerError> {
    let mut budget = LayerBudget::new(LayerLimits::default());
    apply_layer_with_budget(reader, compression, root, &mut budget)
}

pub(crate) fn apply_layer_with_budget(
    reader: impl Read,
    compression: LayerCompression,
    root: &Path,
    budget: &mut LayerBudget,
) -> Result<(), LayerError> {
    fs::create_dir_all(root)?;
    match compression {
        LayerCompression::None => apply_tar(reader, root, budget),
        LayerCompression::Gzip => apply_tar(GzDecoder::new(reader), root, budget),
        LayerCompression::Zstd => {
            apply_tar(zstd::stream::read::Decoder::new(reader)?, root, budget)
        }
    }
}

fn apply_tar(reader: impl Read, root: &Path, budget: &mut LayerBudget) -> Result<(), LayerError> {
    let mut archive = tar::Archive::new(reader);
    archive.set_preserve_permissions(true);
    archive.set_preserve_mtime(true);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let relative = validated_path(&entry.path()?)?;
        budget.charge(&entry, &relative)?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        if apply_whiteout(root, &relative)? {
            continue;
        }
        validate_entry_type(&entry)?;
        ensure_safe_parents(root, relative.parent().unwrap_or_else(|| Path::new("")))?;
        validate_link(&entry, &relative)?;

        let destination = root.join(&relative);
        if entry.header().entry_type().is_hard_link() {
            let target = entry
                .link_name()?
                .ok_or_else(|| LayerError::MissingLinkTarget(relative.clone()))?;
            let target = validated_path(&target)?;
            ensure_safe_parents(root, target.parent().unwrap_or_else(|| Path::new("")))?;
            remove_existing(&destination)?;
            fs::hard_link(root.join(target), destination)?;
            continue;
        }
        if !entry.header().entry_type().is_dir() {
            remove_existing(&destination)?;
        }
        entry.unpack(&destination)?;
    }
    Ok(())
}

fn validated_path(path: &Path) -> Result<PathBuf, LayerError> {
    let mut safe = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => safe.push(value),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(LayerError::UnsafePath(path.to_path_buf()));
            }
        }
    }
    Ok(safe)
}

fn validate_entry_type<R: Read>(entry: &tar::Entry<'_, R>) -> Result<(), LayerError> {
    let kind = entry.header().entry_type();
    if kind.is_file() || kind.is_dir() || kind.is_symlink() || kind.is_hard_link() {
        Ok(())
    } else {
        Err(LayerError::UnsupportedEntryType(kind.as_byte()))
    }
}

fn validate_link<R: Read>(entry: &tar::Entry<'_, R>, relative: &Path) -> Result<(), LayerError> {
    let kind = entry.header().entry_type();
    if !kind.is_symlink() && !kind.is_hard_link() {
        return Ok(());
    }
    let target = entry
        .link_name()?
        .ok_or_else(|| LayerError::MissingLinkTarget(relative.into()))?;
    if kind.is_hard_link() && target.is_absolute() {
        return Err(LayerError::UnsafeLink {
            path: relative.into(),
            target: target.into_owned(),
        });
    }
    let base = if target.is_absolute() || kind.is_hard_link() {
        Path::new("")
    } else {
        relative.parent().unwrap_or_else(|| Path::new(""))
    };
    let mut depth = base.components().count();
    for component in target.components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir if depth > 0 => depth -= 1,
            Component::RootDir if kind.is_symlink() => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(LayerError::UnsafeLink {
                    path: relative.into(),
                    target: target.into_owned(),
                });
            }
        }
    }
    Ok(())
}

fn ensure_safe_parents(root: &Path, relative: &Path) -> Result<(), LayerError> {
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(value) = component else {
            return Err(LayerError::UnsafePath(relative.into()));
        };
        current.push(value);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(LayerError::SymlinkParent(current));
            }
            Ok(metadata) if !metadata.is_dir() => {
                remove_existing(&current)?;
                fs::create_dir(&current)?;
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::create_dir(&current)?,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn apply_whiteout(root: &Path, relative: &Path) -> Result<bool, LayerError> {
    let Some(name) = relative.file_name().and_then(OsStr::to_str) else {
        return Ok(false);
    };
    let Some(whiteout) = name.strip_prefix(".wh.") else {
        return Ok(false);
    };
    let parent = relative.parent().unwrap_or_else(|| Path::new(""));
    ensure_safe_parents(root, parent)?;
    if whiteout == ".wh..opq" || name == ".wh..wh..opq" {
        let directory = root.join(parent);
        for child in fs::read_dir(directory)? {
            remove_existing(&child?.path())?;
        }
    } else if !whiteout.is_empty() {
        remove_existing(&root.join(parent).join(whiteout))?;
    } else {
        return Err(LayerError::UnsafePath(relative.into()));
    }
    Ok(true)
}

fn remove_existing(path: &Path) -> Result<(), LayerError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir_all(path)?;
        }
        Ok(_) => fs::remove_file(path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum LayerError {
    #[error("unsupported OCI layer media type: {0}")]
    UnsupportedMediaType(String),
    #[error("unsafe path in OCI layer: {}", .0.display())]
    UnsafePath(PathBuf),
    #[error("OCI layer path traverses an existing symlink: {}", .0.display())]
    SymlinkParent(PathBuf),
    #[error("unsafe link in OCI layer: {} -> {}", path.display(), target.display())]
    UnsafeLink { path: PathBuf, target: PathBuf },
    #[error("link has no target: {}", .0.display())]
    MissingLinkTarget(PathBuf),
    #[error("unsupported tar entry type {0:#x}")]
    UnsupportedEntryType(u8),
    #[error("OCI layer exceeds the maximum entry count of {maximum}")]
    EntryLimitExceeded { maximum: u64 },
    #[error(
        "OCI layer file {} is {size} bytes, exceeding the {maximum}-byte limit",
        path.display()
    )]
    FileSizeLimitExceeded {
        path: PathBuf,
        size: u64,
        maximum: u64,
    },
    #[error("OCI layers would expand to {attempted} bytes, exceeding the {maximum}-byte limit")]
    ExpandedSizeLimitExceeded { attempted: u64, maximum: u64 },
    #[error("OCI layer I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use flate2::{Compression, write::GzEncoder};

    use super::*;

    fn tar_with_files(files: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut bytes);
            for (path, contents) in files {
                let mut header = tar::Header::new_gnu();
                header.set_size(u64::try_from(contents.len()).unwrap());
                header.set_mode(0o644);
                header.set_cksum();
                builder
                    .append_data(&mut header, path, Cursor::new(contents.as_slice()))
                    .unwrap();
            }
            builder.finish().unwrap();
        }
        bytes
    }

    #[test]
    fn rejects_traversal_and_absolute_paths() {
        assert!(validated_path(Path::new("../escape")).is_err());
        assert!(validated_path(Path::new("/escape")).is_err());
    }

    #[test]
    fn applies_whiteout_without_touching_siblings() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("removed"), b"old").unwrap();
        fs::write(root.path().join("kept"), b"old").unwrap();
        let mut bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut bytes);
            let mut header = tar::Header::new_gnu();
            header.set_size(0);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, ".wh.removed", Cursor::new([]))
                .unwrap();
            builder.finish().unwrap();
        }
        apply_layer(Cursor::new(bytes), LayerCompression::None, root.path()).unwrap();
        assert!(!root.path().join("removed").exists());
        assert!(root.path().join("kept").exists());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_existing_symlink_parent() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), root.path().join("link")).unwrap();
        let error = ensure_safe_parents(root.path(), Path::new("link/child")).unwrap_err();
        assert!(matches!(error, LayerError::SymlinkParent(_)));
    }

    #[test]
    fn shares_expanded_byte_budget_across_layers() {
        let root = tempfile::tempdir().unwrap();
        let limits = LayerLimits {
            max_expanded_bytes: 5,
            max_file_bytes: 4,
            max_entries: 10,
        };
        let mut budget = LayerBudget::new(limits);
        let first = tar_with_files(&[("first", b"abc".to_vec())]);
        apply_layer_with_budget(
            Cursor::new(first),
            LayerCompression::None,
            root.path(),
            &mut budget,
        )
        .unwrap();
        let second = tar_with_files(&[("second", b"def".to_vec())]);
        let error = apply_layer_with_budget(
            Cursor::new(second),
            LayerCompression::None,
            root.path(),
            &mut budget,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            LayerError::ExpandedSizeLimitExceeded {
                attempted: 6,
                maximum: 5
            }
        ));
        assert!(!root.path().join("second").exists());
    }

    #[test]
    fn rejects_too_many_archive_entries() {
        let root = tempfile::tempdir().unwrap();
        let mut budget = LayerBudget::new(LayerLimits {
            max_entries: 1,
            ..LayerLimits::default()
        });
        let archive = tar_with_files(&[("first", Vec::new()), ("second", Vec::new())]);

        let error = apply_layer_with_budget(
            Cursor::new(archive),
            LayerCompression::None,
            root.path(),
            &mut budget,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            LayerError::EntryLimitExceeded { maximum: 1 }
        ));
    }

    #[test]
    fn rejects_gzip_bomb_before_unpacking_oversized_file() {
        let root = tempfile::tempdir().unwrap();
        let archive = tar_with_files(&[("large", vec![0; 128 * 1024])]);
        let mut encoder = GzEncoder::new(Vec::new(), Compression::best());
        encoder.write_all(&archive).unwrap();
        let compressed = encoder.finish().unwrap();
        assert!(compressed.len() < archive.len() / 4);
        let mut budget = LayerBudget::new(LayerLimits {
            max_expanded_bytes: 1024,
            max_file_bytes: 1024,
            max_entries: 10,
        });

        let error = apply_layer_with_budget(
            Cursor::new(compressed),
            LayerCompression::Gzip,
            root.path(),
            &mut budget,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            LayerError::FileSizeLimitExceeded {
                size: 131_072,
                maximum: 1024,
                ..
            }
        ));
        assert!(!root.path().join("large").exists());
    }
}
