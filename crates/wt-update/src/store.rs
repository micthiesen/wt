use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use thiserror::Error;

use crate::{InstallPaths, InstallState, StateError};

#[derive(Clone, Debug)]
pub struct StateStore {
    paths: InstallPaths,
}

impl StateStore {
    pub fn new(paths: InstallPaths) -> Self {
        Self { paths }
    }

    pub fn paths(&self) -> &InstallPaths {
        &self.paths
    }

    pub fn load(&self) -> Result<InstallState, StoreError> {
        let path = self.paths.state_file();
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(InstallState::new(crate::Channel::default()));
            }
            Err(source) => return Err(StoreError::Io { path, source }),
        };
        let mut bytes = Vec::new();
        file.take((MAX_STATE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|source| StoreError::Io {
                path: path.clone(),
                source,
            })?;
        if bytes.len() > MAX_STATE_BYTES {
            return Err(StoreError::Io {
                path,
                source: io::Error::new(
                    io::ErrorKind::InvalidData,
                    "update state exceeds size limit",
                ),
            });
        }
        InstallState::parse(&bytes).map_err(StoreError::State)
    }

    pub fn save(&self, state: &InstallState) -> Result<(), StoreError> {
        let path = self.paths.state_file();
        let bytes = state.to_json().map_err(StoreError::State)?;
        atomic_write(&path, &bytes).map_err(|source| StoreError::Io { path, source })
    }

    pub fn lock(&self) -> Result<StateLock, StoreError> {
        let lock_path = self.paths.lock_file();
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent).map_err(|source| StoreError::Io {
                path: parent.to_owned(),
                source,
            })?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|source| StoreError::Io {
                path: lock_path.clone(),
                source,
            })?;
        // OS file locks are released on process death, so no stale PID marker
        // or platform-specific lock cleanup is needed.
        if let Err(error) = file.try_lock() {
            return match error {
                std::fs::TryLockError::WouldBlock => Err(StoreError::Busy),
                std::fs::TryLockError::Error(source) => Err(StoreError::Io {
                    path: lock_path,
                    source,
                }),
            };
        }
        Ok(StateLock { _file: file })
    }

    /// Retry a contended transition lock for a short, explicit bound. The
    /// launcher uses this only to avoid observing the tiny state/rename window.
    pub fn lock_wait(&self, timeout: Duration) -> Result<StateLock, StoreError> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.lock() {
                Ok(lock) => return Ok(lock),
                Err(StoreError::Busy) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error),
            }
        }
    }
}

pub struct StateLock {
    _file: File,
}

const MAX_STATE_BYTES: usize = 2 * 1024 * 1024;

/// Replace one file through a same-directory temporary, file sync, and rename.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file has no parent"))?;
    fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file has no name"))?
        .to_string_lossy();
    static NEXT_TEMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let (temp, mut file) = loop {
        let sequence = NEXT_TEMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let temp = parent.join(format!(
            ".{file_name}.{}-{sequence}.tmp",
            std::process::id()
        ));
        match OpenOptions::new().write(true).create_new(true).open(&temp) {
            Ok(file) => break (temp, file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    };
    let write_result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)?;
        sync_directory(parent)?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    write_result
}

fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("another update or rollback is already in progress")]
    Busy,
    #[error("update state error: {0}")]
    State(#[source] StateError),
    #[error("updater state I/O at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn state_lock_is_exclusive_and_released_on_drop() {
        let temp = tempdir().unwrap();
        let store = StateStore::new(InstallPaths::new(temp.path().join("install")).unwrap());
        let lock = store.lock().unwrap();
        assert!(matches!(store.lock(), Err(StoreError::Busy)));
        drop(lock);
        assert!(store.lock().is_ok());
    }

    #[test]
    fn state_writes_replace_atomically_and_preserve_parseable_state() {
        let temp = tempdir().unwrap();
        let store = StateStore::new(InstallPaths::new(temp.path().join("install")).unwrap());
        let state = InstallState::new(crate::Channel::Preview);
        store.save(&state).unwrap();
        assert_eq!(store.load().unwrap(), state);
    }
}
