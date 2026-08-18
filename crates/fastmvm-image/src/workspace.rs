use std::{
    fs::{self, File},
    io::{self, Read},
    path::{Component, Path, PathBuf},
    process::Command,
};

use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::Digest;

const MIN_IMAGE_BYTES: u64 = 64 * 1024 * 1024;
const IMAGE_HEADROOM_BYTES: u64 = 32 * 1024 * 1024;

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
        let (source_digest, content_bytes) = digest_tree_with_size(&source)?;
        let directory = cache_root.join("workspaces/sha256");
        fs::create_dir_all(&directory)?;
        let image_path = directory.join(format!("{}.ext4", source_digest.hex()));
        if !image_path.exists() {
            let temporary = directory.join(format!(
                ".{}.{}.tmp",
                source_digest.hex(),
                std::process::id()
            ));
            let size = image_size(content_bytes);
            let file = File::create(&temporary)?;
            file.set_len(size)?;
            drop(file);
            let output = Command::new(mke2fs)
                .args(["-q", "-t", "ext4", "-F", "-d"])
                .arg(&source)
                .arg(&temporary)
                .output()?;
            if !output.status.success() {
                let _ = fs::remove_file(&temporary);
                return Err(WorkspaceError::Mke2fs {
                    status: output.status.code(),
                    stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
                });
            }
            set_read_only(&temporary)?;
            match fs::rename(&temporary, &image_path) {
                Ok(()) => {}
                Err(_error) if image_path.exists() => {
                    let _ = fs::remove_file(&temporary);
                }
                Err(error) => return Err(error.into()),
            }
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

pub fn digest_tree(root: &Path) -> Result<Digest, WorkspaceError> {
    digest_tree_with_size(root).map(|(digest, _)| digest)
}

fn digest_tree_with_size(root: &Path) -> Result<(Digest, u64), WorkspaceError> {
    let mut hasher = Sha256::new();
    let mut content_bytes = 0_u64;
    walk(root, root, &mut |relative, metadata| {
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
    visitor: &mut impl FnMut(&Path, &fs::Metadata) -> Result<(), WorkspaceError>,
) -> Result<(), WorkspaceError> {
    let mut children = fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
    children.sort_by_key(fs::DirEntry::file_name);
    for child in children {
        let path = child.path();
        let relative = path
            .strip_prefix(root)
            .map_err(|_| WorkspaceError::UnsafePath(path.clone()))?;
        validate_relative(relative)?;
        let metadata = fs::symlink_metadata(&path)?;
        visitor(relative, &metadata)?;
        if metadata.is_dir() {
            walk(root, &path, visitor)?;
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
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    io::copy(&mut file, &mut HashWriter(&mut hasher))?;
    Ok(Digest::from_sha256(hasher.finalize().into()))
}

struct HashWriter<'a>(&'a mut Sha256);

impl io::Write for HashWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0.update(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
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
}
