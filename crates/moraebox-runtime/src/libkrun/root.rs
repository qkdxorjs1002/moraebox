use std::path::{Path, PathBuf};

use moraebox_box::{BoxLease, BoxStore, BoxStoreError, EphemeralDisk};
use tokio::process::Command;

use crate::BackendError;

pub(super) enum PreparedRoot {
    Directory,
    StaticDisk(PathBuf),
    Managed(ManagedRootLease),
}

impl PreparedRoot {
    pub(super) fn disk_path(&self) -> Option<&Path> {
        match self {
            Self::Directory => None,
            Self::StaticDisk(path) => Some(path),
            Self::Managed(lease) => Some(lease.disk_path()),
        }
    }

    pub(super) fn into_lease(self) -> Option<ManagedRootLease> {
        match self {
            Self::Managed(lease) => Some(lease),
            Self::Directory | Self::StaticDisk(_) => None,
        }
    }
}

pub(super) enum ManagedRootLease {
    Persistent {
        store: BoxStore,
        lease: BoxLease,
        e2fsck_path: PathBuf,
    },
    Ephemeral(EphemeralDisk),
}

impl ManagedRootLease {
    pub(super) fn disk_path(&self) -> &Path {
        match self {
            Self::Persistent { lease, .. } => lease.disk_path(),
            Self::Ephemeral(disk) => disk.disk_path(),
        }
    }

    pub(super) async fn finish_clean(&mut self) -> Result<(), BackendError> {
        if let Self::Persistent {
            store,
            lease,
            e2fsck_path,
        } = self
        {
            repair_dirty_box(store, lease, e2fsck_path).await?;
        }
        Ok(())
    }
}

pub(super) async fn repair_dirty_box(
    store: &BoxStore,
    lease: &mut BoxLease,
    e2fsck: &Path,
) -> Result<(), BackendError> {
    if !e2fsck.is_file() {
        return Err(BackendError::Control(format!(
            "dirty Box {} requires e2fsck, but it was not found at {}",
            lease.id(),
            e2fsck.display()
        )));
    }
    let mut command = Command::new(e2fsck);
    command
        .arg("-p")
        .arg(lease.disk_path())
        .env_clear()
        .kill_on_drop(true);
    let output = command.output().await?;
    if matches!(output.status.code(), Some(0 | 1)) {
        store.finish_repair(lease).map_err(box_backend_error)?;
        return Ok(());
    }
    store.mark_needs_repair(lease).map_err(box_backend_error)?;
    Err(BackendError::Control(format!(
        "e2fsck could not repair Box {} (status {:?}): {}",
        lease.id(),
        output.status.code(),
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "this function is a map_err adapter and owns the source error"
)]
pub(super) fn box_backend_error(error: BoxStoreError) -> BackendError {
    BackendError::Control(error.to_string())
}
