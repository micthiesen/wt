//! Machine-wide bounds on the two kinds of work that run for minutes:
//! dependency installs and deletions of removed checkouts. Each has a pool
//! of the platform's machine slots, shared by every wt process using one
//! lock directory, and a waiter other processes can see.
//!
//! These pools are separate from the process runner's heavy slots on
//! purpose. Heavy slots bound checkouts, removals and hooks, which take
//! seconds; a multi-minute install holding one would leave a checkout
//! queued behind it. Work bounded here therefore runs as `bulk()`, not
//! `heavy()`.

use std::{num::NonZeroUsize, path::Path, time::Duration};

use tokio_util::sync::CancellationToken;
use wt_platform::lock::{FileLock, LockError, MachineSlot, MachineSlots};

/// How often a waiter looks again. A slot is held for the length of an
/// install or a recursive delete, so a waiter gains nothing by looking more
/// often, and many of them wait at once.
const RETRY: Duration = Duration::from_millis(500);

/// A fixed number of slots for one kind of work.
#[derive(Clone, Copy, Debug)]
pub struct SlotPool {
    name: &'static str,
    slots: usize,
}

/// Dependency installs. Measured on a 12-core machine: 92 s alone, 169 s
/// with four at once and 350 s with nine, while checkouts beside them took
/// four times as long. Three finish a batch about as soon and leave the
/// machine usable.
pub const DEPENDENCY_INSTALLS: SlotPool = SlotPool::new(
    wt_platform::lock::DEPENDENCY_INSTALL_POOL,
    wt_platform::lock::DEPENDENCY_INSTALL_SLOTS.get(),
);

/// Deletions of checkouts a removal already moved aside. Nothing waits on
/// them, and five at once kept the disk busy for over an hour.
pub const QUARANTINE_DELETES: SlotPool = SlotPool::new("quarantine-delete", 2);

/// One held slot.
pub struct Slot {
    _slot: MachineSlot,
}

impl SlotPool {
    pub const fn new(name: &'static str, slots: usize) -> Self {
        Self { name, slots }
    }

    pub const fn slots(&self) -> usize {
        self.slots
    }

    /// This pool's slot files, opened for one holder. Each open has its own
    /// descriptors, which is what tells two holders in one process apart.
    async fn open(&self, lock_dir: &Path) -> Result<MachineSlots, LockError> {
        let count = NonZeroUsize::new(self.slots).unwrap_or(NonZeroUsize::MIN);
        MachineSlots::open(lock_dir, self.name, count).await
    }

    fn waiter_key(&self, waiter: &str) -> String {
        format!("__{}-wait-{waiter}", self.name)
    }

    /// A free slot, if there is one now.
    pub async fn try_acquire(&self, lock_dir: &Path) -> Result<Option<Slot>, LockError> {
        Ok(self
            .open(lock_dir)
            .await?
            .try_acquire()?
            .map(|slot| Slot { _slot: slot }))
    }

    /// Wait for a slot. `on_wait` runs once, when none is free at first.
    /// While waiting, `waiter` (a worktree slug) is visible to other
    /// processes through [`Self::is_waiting`].
    pub async fn acquire(
        &self,
        lock_dir: &Path,
        waiter: Option<&str>,
        cancellation: &CancellationToken,
        on_wait: impl FnOnce(),
    ) -> Result<Slot, LockError> {
        if cancellation.is_cancelled() {
            return Err(LockError::Cancelled);
        }
        let slots = self.open(lock_dir).await?;
        if let Some(slot) = slots.try_acquire()? {
            return Ok(Slot { _slot: slot });
        }
        on_wait();
        // Held only to be seen, so failing to take it never fails the wait:
        // a second waiter of the same name holds it, a probe had it for a
        // moment (retried below), or the name is not a usable lock key.
        let mut waiting = None;
        loop {
            if waiting.is_none()
                && let Some(waiter) = waiter
            {
                waiting = FileLock::try_acquire(
                    lock_dir,
                    &self.waiter_key(waiter),
                    "wait for a work slot",
                )
                .await
                .ok()
                .flatten();
            }
            tokio::select! {
                _ = cancellation.cancelled() => return Err(LockError::Cancelled),
                _ = tokio::time::sleep(RETRY) => {}
            }
            if let Some(slot) = slots.try_acquire()? {
                return Ok(Slot { _slot: slot });
            }
        }
    }

    /// Whether some process is waiting for a slot under this name. A lock
    /// that cannot be read says nothing, which reads as not waiting.
    pub async fn is_waiting(&self, lock_dir: &Path, waiter: &str) -> bool {
        matches!(
            FileLock::try_acquire(
                lock_dir,
                &self.waiter_key(waiter),
                "probe a work slot waiter"
            )
            .await,
            Ok(None)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_pool_admits_its_count_and_the_next_waits_visibly_until_one_frees() {
        let directory = tempfile::tempdir().unwrap();
        let pool = SlotPool::new("fixture", 2);
        let cancel = CancellationToken::new();
        let first = pool.try_acquire(directory.path()).await.unwrap().unwrap();
        let _second = pool
            .acquire(directory.path(), Some("two"), &cancel, || {
                panic!("a free slot is taken without waiting")
            })
            .await
            .unwrap();
        assert!(pool.try_acquire(directory.path()).await.unwrap().is_none());
        assert!(!pool.is_waiting(directory.path(), "three").await);

        let (waited, told) = tokio::sync::oneshot::channel();
        let waiting = tokio::spawn({
            let path = directory.path().to_path_buf();
            let cancel = cancel.clone();
            async move {
                pool.acquire(&path, Some("three"), &cancel, || {
                    let _ = waited.send(());
                })
                .await
            }
        });
        told.await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !pool.is_waiting(directory.path(), "three").await {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the waiter is visible to other processes");
        assert!(!waiting.is_finished());

        drop(first);
        let third = tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("a freed slot goes to the waiter")
            .unwrap()
            .unwrap();
        assert!(!pool.is_waiting(directory.path(), "three").await);
        assert!(pool.try_acquire(directory.path()).await.unwrap().is_none());
        drop(third);
        assert!(pool.try_acquire(directory.path()).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn a_cancelled_waiter_stops_waiting() {
        let directory = tempfile::tempdir().unwrap();
        let pool = SlotPool::new("fixture", 1);
        let _held = pool.try_acquire(directory.path()).await.unwrap().unwrap();
        let cancel = CancellationToken::new();
        let waiting = tokio::spawn({
            let path = directory.path().to_path_buf();
            let cancel = cancel.clone();
            async move { pool.acquire(&path, None, &cancel, || {}).await }
        });
        tokio::task::yield_now().await;
        cancel.cancel();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(5), waiting)
                .await
                .unwrap()
                .unwrap(),
            Err(LockError::Cancelled)
        ));
    }
}
