//! Cancellable advisory locks shared by lifecycle and stack operations.

use std::{
    fs::{File, OpenOptions},
    io,
    num::NonZeroUsize,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use thiserror::Error;
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
        let lock_dir = lock_dir.to_path_buf();
        let path = lock_dir.join(format!("{key}.lock"));
        // One hop: a waiter polls this, and each hop queues behind whatever
        // else the blocking pool is running.
        tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&lock_dir).map_err(|source| LockError::Io {
                operation: "create lock directory",
                path: lock_dir.clone(),
                source,
            })?;
            let file = open_lock_file(&path)?;
            Ok(flock_try(&file, &path, operation)?.then_some(Self { file }))
        })
        .await
        .map_err(|error| LockError::Join(error.to_string()))?
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

/// The pool of [`MachineSlots`] every dependency install on the machine takes
/// a slot from, whichever crate starts it, opened in the configured lock
/// directory. Measured on a 12-core machine: an install took 92 s alone,
/// 169 s with four at once and 350 s with nine.
pub const DEPENDENCY_INSTALL_POOL: &str = "dependency-install";
pub const DEPENDENCY_INSTALL_SLOTS: NonZeroUsize = NonZeroUsize::new(3).expect("nonzero");

/// A counted limit shared by every process using one lock directory: at most
/// `count` holders at a time, each owning one advisory lock at
/// `<lock_dir>/__<name>-<n>.lock`. The prefix keeps the files apart from the
/// per-slug locks in the same directory. A holder that dies releases its slot
/// with its descriptor, so no slot is ever stale and no file is ever deleted.
#[derive(Clone)]
pub struct MachineSlots {
    files: Arc<SlotFiles>,
}

struct SlotFiles {
    files: Vec<(File, PathBuf)>,
    /// flock belongs to the open file, so a second task of this process
    /// would "acquire" a slot the first still holds. Locking and unlocking
    /// happen under this mutex.
    held: Mutex<Vec<bool>>,
}

/// One held slot, released on drop.
pub struct MachineSlot {
    files: Arc<SlotFiles>,
    index: usize,
}

impl Drop for MachineSlot {
    fn drop(&mut self) {
        let mut held = self
            .files
            .held
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        #[cfg(unix)]
        // SAFETY: the descriptor is owned by the shared slot files, which
        // this guard keeps alive.
        unsafe {
            libc::flock(self.files.files[self.index].0.as_raw_fd(), libc::LOCK_UN);
        }
        held[self.index] = false;
    }
}

impl MachineSlots {
    /// Open (creating as needed) the slot files once, so that taking a slot
    /// later is a non-blocking system call and never touches a thread pool.
    pub async fn open(lock_dir: &Path, name: &str, count: NonZeroUsize) -> Result<Self, LockError> {
        validate_key(name)?;
        let lock_dir = lock_dir.to_path_buf();
        let name = name.to_owned();
        tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&lock_dir).map_err(|source| LockError::Io {
                operation: "create lock directory",
                path: lock_dir.clone(),
                source,
            })?;
            let files = (0..count.get())
                .map(|index| {
                    let path = lock_dir.join(format!("__{name}-{index}.lock"));
                    open_lock_file(&path).map(|file| (file, path))
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Self {
                files: Arc::new(SlotFiles {
                    held: Mutex::new(vec![false; files.len()]),
                    files,
                }),
            })
        })
        .await
        .map_err(|error| LockError::Join(error.to_string()))?
    }

    /// Attempt every slot once without blocking. `None` means all are held.
    pub fn try_acquire(&self) -> Result<Option<MachineSlot>, LockError> {
        let mut held = self
            .files
            .held
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let count = held.len();
        // Processes start at different slots so a lightly used limit costs
        // one system call instead of a walk past everyone else's slots.
        let first = std::process::id() as usize % count;
        for index in (0..count).map(|offset| (first + offset) % count) {
            if held[index] {
                continue;
            }
            let (file, path) = &self.files.files[index];
            if flock_try(file, path, "take machine slot")? {
                held[index] = true;
                return Ok(Some(MachineSlot {
                    files: Arc::clone(&self.files),
                    index,
                }));
            }
        }
        Ok(None)
    }

    /// Wait for a slot, checking cancellation while waiting. Waiters in
    /// different processes are served in no particular order.
    pub async fn acquire(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<MachineSlot, LockError> {
        loop {
            if cancellation.is_cancelled() {
                return Err(LockError::Cancelled);
            }
            if let Some(slot) = self.try_acquire()? {
                return Ok(slot);
            }
            tokio::select! {
                _ = cancellation.cancelled() => return Err(LockError::Cancelled),
                _ = tokio::time::sleep(MACHINE_SLOT_POLL) => {}
            }
        }
    }
}

impl MachineSlots {
    /// [`Self::acquire`] for at most `bound`. `None` means every slot stayed
    /// held: the limit sheds load and guards no data, so a caller then
    /// proceeds without a slot instead of waiting on a holder that may be
    /// suspended or on a crowd it cannot outwait.
    pub async fn acquire_within(
        &self,
        bound: Duration,
        cancellation: &CancellationToken,
    ) -> Result<Option<MachineSlot>, LockError> {
        tokio::select! {
            biased;
            slot = self.acquire(cancellation) => slot.map(Some),
            _ = tokio::time::sleep(bound) => Ok(None),
        }
    }
}

/// Short against the subprocesses these slots admit (a Git read is some
/// hundred milliseconds), and a poll is a few non-blocking system calls.
const MACHINE_SLOT_POLL: Duration = Duration::from_millis(25);

fn open_lock_file(path: &Path) -> Result<File, LockError> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .map_err(|source| LockError::Io {
            operation: "open operation lock",
            path: path.to_path_buf(),
            source,
        })
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
    #[tokio::test]
    async fn machine_slots_bound_holders_across_openers_and_within_one() {
        let directory = tempfile::tempdir().unwrap();
        let two = NonZeroUsize::new(2).unwrap();
        // Separate opens stand in for separate processes: each has its own
        // open files, which is what flock tells apart.
        let ours = MachineSlots::open(directory.path(), "scan", two)
            .await
            .unwrap();
        let theirs = MachineSlots::open(directory.path(), "scan", two)
            .await
            .unwrap();
        let first = ours.try_acquire().unwrap().unwrap();
        let second = theirs.try_acquire().unwrap().unwrap();
        assert!(ours.try_acquire().unwrap().is_none());
        assert!(theirs.try_acquire().unwrap().is_none());
        drop(second);
        // Both of one opener's slots: the second must not be the one it
        // already holds.
        let third = ours.try_acquire().unwrap().unwrap();
        assert!(ours.try_acquire().unwrap().is_none());
        assert!(theirs.try_acquire().unwrap().is_none());
        drop(first);
        assert!(theirs.try_acquire().unwrap().is_some());
        drop(third);
    }

    #[tokio::test(start_paused = true)]
    async fn a_bounded_slot_wait_gives_up_and_slot_files_cannot_be_a_slug_lock() {
        let directory = tempfile::tempdir().unwrap();
        let one = NonZeroUsize::new(1).unwrap();
        let slots = MachineSlots::open(directory.path(), "fleet-scan", one)
            .await
            .unwrap();
        // A worktree may be named like a slot; its lock is another file.
        let _slug = FileLock::try_acquire(directory.path(), "fleet-scan-0", "test")
            .await
            .unwrap()
            .unwrap();
        let cancel = CancellationToken::new();
        let held = slots
            .acquire_within(Duration::from_secs(10), &cancel)
            .await
            .unwrap()
            .expect("the slug lock does not hold the slot");
        assert!(directory.path().join("__fleet-scan-0.lock").exists());
        let other = MachineSlots::open(directory.path(), "fleet-scan", one)
            .await
            .unwrap();
        let started = tokio::time::Instant::now();
        assert!(
            other
                .acquire_within(Duration::from_secs(10), &cancel)
                .await
                .unwrap()
                .is_none()
        );
        assert!(started.elapsed() >= Duration::from_secs(10));
        drop(held);
        assert!(
            other
                .acquire_within(Duration::from_secs(10), &cancel)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn machine_slot_wait_ends_on_release_and_on_cancellation() {
        let directory = tempfile::tempdir().unwrap();
        let one = NonZeroUsize::new(1).unwrap();
        let slots = MachineSlots::open(directory.path(), "bulk", one)
            .await
            .unwrap();
        let other = MachineSlots::open(directory.path(), "bulk", one)
            .await
            .unwrap();
        let held = other.try_acquire().unwrap().unwrap();
        let cancel = CancellationToken::new();
        let waiting = tokio::spawn({
            let (slots, cancel) = (slots.clone(), cancel.clone());
            async move { slots.acquire(&cancel).await.map(|_| ()) }
        });
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(!waiting.is_finished());
        drop(held);
        waiting.await.unwrap().unwrap();

        let _held = other.try_acquire().unwrap().unwrap();
        let waiting = tokio::spawn({
            let cancel = cancel.clone();
            async move { slots.acquire(&cancel).await.map(|_| ()) }
        });
        tokio::time::sleep(Duration::from_millis(60)).await;
        cancel.cancel();
        assert!(matches!(waiting.await.unwrap(), Err(LockError::Cancelled)));
    }
}
