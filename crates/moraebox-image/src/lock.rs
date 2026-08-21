use std::{
    fs::{File, OpenOptions},
    io,
    path::Path,
    time::Duration,
};

use fs2::FileExt;

const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Debug)]
pub(crate) struct AdvisoryLock {
    file: File,
}

impl AdvisoryLock {
    pub(crate) async fn acquire(path: &Path) -> io::Result<Self> {
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "lock has no parent"))?;
        tokio::fs::create_dir_all(parent).await?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        loop {
            match FileExt::try_lock_exclusive(&file) {
                Ok(()) => return Ok(Self { file }),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    tokio::time::sleep(LOCK_RETRY_INTERVAL).await;
                }
                Err(error) => return Err(error),
            }
        }
    }
}

impl Drop for AdvisoryLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn serializes_only_the_same_key() {
        let directory = tempfile::tempdir().unwrap();
        let first_path = directory.path().join("first.lock");
        let second_path = directory.path().join("second.lock");
        let first = AdvisoryLock::acquire(&first_path).await.unwrap();
        let other = tokio::time::timeout(
            Duration::from_millis(100),
            AdvisoryLock::acquire(&second_path),
        )
        .await
        .unwrap()
        .unwrap();
        let blocked = tokio::time::timeout(
            Duration::from_millis(30),
            AdvisoryLock::acquire(&first_path),
        )
        .await;
        assert!(blocked.is_err());

        drop(first);
        let same = tokio::time::timeout(
            Duration::from_millis(100),
            AdvisoryLock::acquire(&first_path),
        )
        .await
        .unwrap()
        .unwrap();
        drop((same, other));
    }
}
