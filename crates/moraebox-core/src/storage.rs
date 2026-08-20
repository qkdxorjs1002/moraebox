use std::{
    env,
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

#[derive(Debug, Error, PartialEq, Eq)]
pub enum StoragePathError {
    #[error(
        "cannot determine the user home directory; set HOME (or USERPROFILE on Windows) or provide an explicit storage path"
    )]
    HomeDirectoryUnavailable,
    #[error("user home directory must be an absolute path: {}", .0.display())]
    InvalidHomeDirectory(PathBuf),
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
    let mut home = std::ffi::OsString::from(drive);
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
}
