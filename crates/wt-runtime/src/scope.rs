use std::future::Future;
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
}

#[derive(Debug, Error)]
#[error("background shutdown exceeded {timeout:?}; {remaining} tasks still need cleanup")]
pub struct ShutdownError {
    pub timeout: Duration,
    pub remaining: usize,
}

impl TaskScope {
    pub fn new() -> Self {
        Self {
            token: CancellationToken::new(),
            tracker: TaskTracker::new(),
        }
    }

    pub fn token(&self) -> CancellationToken {
        self.token.child_token()
    }

    /// The tracker retains lifetime accounting even when the caller has no need
    /// for the task's return value. Work must observe the supplied scope token.
    pub fn spawn<F>(&self, work: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.tracker.spawn(work)
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
            })
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
