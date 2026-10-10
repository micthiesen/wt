use std::future::Future;
use std::panic::Location;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;

use thiserror::Error;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

/// The application owns one scope and explicitly joins it before releasing
/// terminal state or exiting. Dropping the scope also requests cancellation.
pub struct TaskScope {
    token: CancellationToken,
    tracker: TaskTracker,
    task_origins: Arc<Mutex<std::collections::BTreeMap<u64, String>>>,
    next_task_id: AtomicU64,
}

#[derive(Debug, Error)]
#[error(
    "background shutdown exceeded {timeout:?}; {remaining} tasks still need cleanup ({task_origins:?})"
)]
pub struct ShutdownError {
    pub timeout: Duration,
    pub remaining: usize,
    pub task_origins: Vec<String>,
}

impl TaskScope {
    pub fn new() -> Self {
        Self {
            token: CancellationToken::new(),
            tracker: TaskTracker::new(),
            task_origins: Arc::default(),
            next_task_id: AtomicU64::new(0),
        }
    }

    pub fn token(&self) -> CancellationToken {
        self.token.child_token()
    }

    /// The tracker retains lifetime accounting even when the caller has no need
    /// for the task's return value. Work must observe the supplied scope token.
    #[track_caller]
    pub fn spawn<F>(&self, work: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let id = self.next_task_id.fetch_add(1, Ordering::Relaxed);
        let origin = Location::caller();
        let label = format!("{}:{}", origin.file(), origin.line());
        if let Ok(mut origins) = self.task_origins.lock() {
            origins.insert(id, label);
        }
        let task_origins = self.task_origins.clone();
        self.tracker.spawn(async move {
            let _origin = TaskOriginGuard {
                id,
                origins: task_origins,
            };
            work.await
        })
    }

    pub fn active_tasks(&self) -> usize {
        self.tracker.len()
    }

    pub fn cancel(&self) {
        self.token.cancel();
    }

    pub async fn shutdown(&self, grace: Duration) -> Result<(), ShutdownError> {
        self.token.cancel();
        self.tracker.close();
        tokio::time::timeout(grace, self.tracker.wait())
            .await
            .map_err(|_| ShutdownError {
                timeout: grace,
                remaining: self.tracker.len(),
                task_origins: self
                    .task_origins
                    .lock()
                    .map(|origins| origins.values().cloned().collect())
                    .unwrap_or_default(),
            })
    }
}

struct TaskOriginGuard {
    id: u64,
    origins: Arc<Mutex<std::collections::BTreeMap<u64, String>>>,
}

impl Drop for TaskOriginGuard {
    fn drop(&mut self) {
        if let Ok(mut origins) = self.origins.lock() {
            origins.remove(&self.id);
        }
    }
}

impl Default for TaskScope {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TaskScope {
    fn drop(&mut self) {
        self.token.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::TaskScope;
    use std::time::Duration;

    #[tokio::test]
    async fn shutdown_error_identifies_the_remaining_spawn_site() {
        let scope = TaskScope::new();
        let (release, wait) = tokio::sync::oneshot::channel::<()>();
        scope.spawn(async move {
            let _ = wait.await;
        });

        let error = scope
            .shutdown(Duration::from_millis(1))
            .await
            .expect_err("the task is deliberately held open");
        assert_eq!(error.remaining, 1);
        assert_eq!(error.task_origins.len(), 1);
        assert!(error.task_origins[0].contains("scope.rs:"));

        let _ = release.send(());
        scope
            .shutdown(Duration::from_secs(1))
            .await
            .expect("the task releases and is joined");
    }
}
