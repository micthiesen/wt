//! Cancellable advisory locks shared by lifecycle and stack operations.

use std::{
    fs::{File, OpenOptions},
    io,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    time::Duration,
};

use thiserror::Error;
use tokio::fs;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Error)]
pub enum LockError {
    #[error("unsafe lock key {0:?}")]
    InvalidKey(String),
    #[error("{operation} at {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("blocking lock operation failed: {0}")]
    Join(String),
    #[error("lock acquisition cancelled")]
    Cancelled,
}

/// An advisory lock at `<lock_dir>/<key>.lock`. Different services must use
/// this type so destroy, create and stack rewrites share one lock namespace.
pub struct FileLock {
    file: File,
}

impl Drop for FileLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        // SAFETY: the descriptor is owned by this guard and remains open here.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

impl FileLock {
    /// Attempt one non-blocking acquisition. `None` means another process owns
    /// the lock. The lock file is retained after release to avoid inode races.
    pub async fn try_acquire(
        lock_dir: &Path,
        key: &str,
        operation: &'static str,
    ) -> Result<Option<Self>, LockError> {
        validate_key(key)?;
        fs::create_dir_all(lock_dir)
            .await
            .map_err(|source| LockError::Io {
                operation: "create lock directory",
                path: lock_dir.to_path_buf(),
                source,
            })?;
        let path = lock_dir.join(format!("{key}.lock"));
        let open_path = path.clone();
        let file = tokio::task::spawn_blocking(move || {
            OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(open_path)
        })
        .await
        .map_err(|error| LockError::Join(error.to_string()))?
        .map_err(|source| LockError::Io {
            operation: "open operation lock",
            path: path.clone(),
            source,
        })?;
        let probe = file.try_clone().map_err(|source| LockError::Io {
            operation: "clone operation lock",
            path: path.clone(),
            source,
        })?;
        let probe_path = path.clone();
        let acquired =
            tokio::task::spawn_blocking(move || flock_try(&probe, &probe_path, operation))
                .await
                .map_err(|error| LockError::Join(error.to_string()))??;
        Ok(acquired.then_some(Self { file }))
    }

    /// Wait for a single lock, checking cancellation while waiting.
    pub async fn acquire(
        lock_dir: &Path,
        key: &str,
        operation: &'static str,
        cancellation: &CancellationToken,
    ) -> Result<Self, LockError> {
        loop {
            if cancellation.is_cancelled() {
                return Err(LockError::Cancelled);
            }
            if let Some(lock) = Self::try_acquire(lock_dir, key, operation).await? {
                return Ok(lock);
            }
            tokio::select! {
                _ = cancellation.cancelled() => return Err(LockError::Cancelled),
                _ = tokio::time::sleep(Duration::from_millis(80)) => {}
            }
        }
    }
}

fn validate_key(key: &str) -> Result<(), LockError> {
    if key.is_empty()
        || key == "."
        || key == ".."
        || key.contains('/')
        || key.contains('\\')
        || key.chars().any(char::is_control)
    {
        return Err(LockError::InvalidKey(key.to_owned()));
    }
    Ok(())
}

fn flock_try(file: &File, path: &Path, operation: &'static str) -> Result<bool, LockError> {
    #[cfg(unix)]
    {
        // SAFETY: the descriptor is open for the duration of the call.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            return Ok(false);
        }
        Err(LockError::Io {
            operation,
            path: path.to_path_buf(),
            source: error,
        })
    }
    #[cfg(not(unix))]
    {
        let _ = file;
        Err(LockError::Io {
            operation,
            path: path.to_path_buf(),
            source: io::Error::new(io::ErrorKind::Unsupported, "flock is unavailable"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn try_acquire_is_exclusive_and_releases_for_later_owner() {
        let directory = tempfile::tempdir().unwrap();
        let first = FileLock::try_acquire(directory.path(), "worktree", "test")
            .await
            .unwrap()
            .unwrap();
        assert!(
            FileLock::try_acquire(directory.path(), "worktree", "test")
                .await
                .unwrap()
                .is_none()
        );
        drop(first);
        assert!(
            FileLock::try_acquire(directory.path(), "worktree", "test")
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn acquire_wait_obeys_cancellation() {
        let directory = tempfile::tempdir().unwrap();
        let _held = FileLock::try_acquire(directory.path(), "worktree", "test")
            .await
            .unwrap()
            .unwrap();
        let cancel = CancellationToken::new();
        let waiting = tokio::spawn({
            let path = directory.path().to_path_buf();
            let cancel = cancel.clone();
            async move { FileLock::acquire(&path, "worktree", "test", &cancel).await }
        });
        tokio::task::yield_now().await;
        cancel.cancel();
        assert!(matches!(waiting.await.unwrap(), Err(LockError::Cancelled)));
    }
}
