use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

use thiserror::Error;

const STORAGE_DIRECTORY: &str = ".moraebox";
const CACHE_DIRECTORY: &str = "cache";
const STATE_DIRECTORY: &str = "state";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoragePaths {
    root: PathBuf,
    cache: PathBuf,
    state: PathBuf,
}

impl StoragePaths {
    pub fn for_current_user() -> Result<Self, StoragePathError> {
        let home = user_home_directory().ok_or(StoragePathError::HomeDirectoryUnavailable)?;
        Self::from_home(home)
    }

    pub fn from_home(home: impl Into<PathBuf>) -> Result<Self, StoragePathError> {
        let home = home.into();
        if home.as_os_str().is_empty() || !home.is_absolute() {
            return Err(StoragePathError::InvalidHomeDirectory(home));
        }
        let root = home.join(STORAGE_DIRECTORY);
        Ok(Self {
            cache: root.join(CACHE_DIRECTORY),
            state: root.join(STATE_DIRECTORY),
            root,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn cache(&self) -> &Path {
        &self.cache
    }

    pub fn state(&self) -> &Path {
        &self.state
    }
}

pub fn resolve_cache_dir(explicit: Option<&Path>) -> Result<PathBuf, StoragePathError> {
    explicit.map(Path::to_path_buf).map_or_else(
        || StoragePaths::for_current_user().map(|paths| paths.cache),
        Ok,
    )
}

pub fn resolve_state_dir(explicit: Option<&Path>) -> Result<PathBuf, StoragePathError> {
    explicit.map(Path::to_path_buf).map_or_else(
        || StoragePaths::for_current_user().map(|paths| paths.state),
        Ok,
    )
}

/// Creates a managed storage root and restricts it to the current user.
///
/// On Unix the final path component is opened without following symlinks, checked against the
/// effective user id, and changed to mode `0700` through the open directory descriptor. Parent
/// components are intentionally not rejected because platform-standard paths may contain
/// symlinks (for example `/tmp` on macOS).
pub fn ensure_private_storage_root(path: &Path) -> Result<(), StorageRootError> {
    if path.as_os_str().is_empty() {
        return Err(StorageRootError::EmptyPath);
    }

    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_root_file_type(path, &metadata)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => create_private_directory(path)?,
        Err(source) => {
            return Err(StorageRootError::Io {
                path: path.into(),
                source,
            });
        }
    }

    validate_private_directory(path)
}

fn validate_root_file_type(path: &Path, metadata: &fs::Metadata) -> Result<(), StorageRootError> {
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(StorageRootError::UnsafeFileType(path.into()));
    }
    Ok(())
}

#[cfg(unix)]
fn create_private_directory(path: &Path) -> Result<(), StorageRootError> {
    use std::os::unix::fs::DirBuilderExt;

    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(path).map_err(|source| StorageRootError::Io {
        path: path.into(),
        source,
    })
}

#[cfg(not(unix))]
fn create_private_directory(path: &Path) -> Result<(), StorageRootError> {
    fs::create_dir_all(path).map_err(|source| StorageRootError::Io {
        path: path.into(),
        source,
    })
}

#[cfg(unix)]
fn validate_private_directory(path: &Path) -> Result<(), StorageRootError> {
    use std::fs::OpenOptions;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    let metadata = fs::symlink_metadata(path).map_err(|source| StorageRootError::Io {
        path: path.into(),
        source,
    })?;
    validate_root_file_type(path, &metadata)?;

    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|source| StorageRootError::Io {
            path: path.into(),
            source,
        })?;
    let metadata = directory
        .metadata()
        .map_err(|source| StorageRootError::Io {
            path: path.into(),
            source,
        })?;
    validate_root_file_type(path, &metadata)?;
    validate_owner_uid(path, metadata.uid(), nix::unistd::geteuid().as_raw())?;
    directory
        .set_permissions(fs::Permissions::from_mode(0o700))
        .map_err(|source| StorageRootError::Io {
            path: path.into(),
            source,
        })?;
    Ok(())
}

#[cfg(not(unix))]
fn validate_private_directory(path: &Path) -> Result<(), StorageRootError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| StorageRootError::Io {
        path: path.into(),
        source,
    })?;
    validate_root_file_type(path, &metadata)
}

#[cfg(unix)]
fn validate_owner_uid(
    path: &Path,
    actual_uid: u32,
    expected_uid: u32,
) -> Result<(), StorageRootError> {
    if actual_uid != expected_uid {
        return Err(StorageRootError::OwnerMismatch {
            path: path.into(),
            expected_uid,
            actual_uid,
        });
    }
    Ok(())
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum StoragePathError {
    #[error(
        "cannot determine the user home directory; set HOME (or USERPROFILE on Windows) or provide an explicit storage path"
    )]
    HomeDirectoryUnavailable,
    #[error("user home directory must be an absolute path: {}", .0.display())]
    InvalidHomeDirectory(PathBuf),
}

#[derive(Debug, Error)]
pub enum StorageRootError {
    #[error("storage root path must not be empty")]
    EmptyPath,
    #[error("storage root must be a real directory, not a symlink: {}", .0.display())]
    UnsafeFileType(PathBuf),
    #[cfg(unix)]
    #[error(
        "storage root {} is owned by uid {actual_uid}, but the current effective uid is {expected_uid}",
        .path.display()
    )]
    OwnerMismatch {
        path: PathBuf,
        expected_uid: u32,
        actual_uid: u32,
    },
    #[error("cannot prepare storage root {}: {source}", .path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

fn user_home_directory() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        environment_path("USERPROFILE")
            .or_else(windows_home_directory)
            .or_else(|| environment_path("HOME"))
    }

    #[cfg(not(windows))]
    {
        environment_path("HOME")
    }
}

fn environment_path(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(windows)]
fn windows_home_directory() -> Option<PathBuf> {
    let drive = env::var_os("HOMEDRIVE")?;
    let path = env::var_os("HOMEPATH")?;
    if drive.is_empty() || path.is_empty() {
        return None;
    }
    let mut home = drive;
    home.push(path);
    Some(PathBuf::from(home))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn absolute_home() -> PathBuf {
        #[cfg(windows)]
        {
            PathBuf::from(r"C:\Users\moraebox-test")
        }
        #[cfg(not(windows))]
        {
            PathBuf::from("/Users/moraebox-test")
        }
    }

    #[test]
    fn derives_user_global_storage_paths() {
        let home = absolute_home();
        let paths = StoragePaths::from_home(&home).unwrap();

        assert_eq!(paths.root(), home.join(".moraebox"));
        assert_eq!(paths.cache(), home.join(".moraebox/cache"));
        assert_eq!(paths.state(), home.join(".moraebox/state"));
    }

    #[test]
    fn rejects_relative_or_empty_home_directories() {
        assert_eq!(
            StoragePaths::from_home("relative/home").unwrap_err(),
            StoragePathError::InvalidHomeDirectory("relative/home".into())
        );
        assert_eq!(
            StoragePaths::from_home("").unwrap_err(),
            StoragePathError::InvalidHomeDirectory(PathBuf::new())
        );
    }

    #[test]
    fn explicit_storage_paths_are_used_verbatim() {
        assert_eq!(
            resolve_cache_dir(Some(Path::new("relative/cache"))).unwrap(),
            PathBuf::from("relative/cache")
        );
        assert_eq!(
            resolve_state_dir(Some(Path::new("relative/state"))).unwrap(),
            PathBuf::from("relative/state")
        );
    }

    #[test]
    fn creates_private_storage_root() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("nested/cache");

        ensure_private_storage_root(&root).unwrap();

        assert!(root.is_dir());
        assert!(
            !fs::symlink_metadata(&root)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(root).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn tightens_existing_storage_root_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("cache");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o777)).unwrap();

        ensure_private_storage_root(&root).unwrap();

        assert_eq!(
            fs::metadata(root).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_storage_root() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let target = temporary.path().join("target");
        let root = temporary.path().join("cache");
        fs::create_dir(&target).unwrap();
        symlink(&target, &root).unwrap();

        assert!(matches!(
            ensure_private_storage_root(&root),
            Err(StorageRootError::UnsafeFileType(path)) if path == root
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_storage_root_owned_by_another_uid() {
        let path = Path::new("/managed/cache");
        let error = validate_owner_uid(path, 42, 43).unwrap_err();

        assert!(matches!(
            error,
            StorageRootError::OwnerMismatch {
                path: actual_path,
                actual_uid: 42,
                expected_uid: 43,
            } if actual_path == path
        ));
    }
}
