use std::fmt::Display;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::TaskScope;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceState {
    Empty,
    Refreshing,
    Ready,
    Failed(Arc<str>),
}

#[derive(Debug)]
pub struct SourceSnapshot<T> {
    pub data: Option<Arc<T>>,
    pub state: SourceState,
    /// Last successful fetch, retained when refresh fails.
    pub updated_at: Option<Instant>,
    pub revision: u64,
}

impl<T> Default for SourceSnapshot<T> {
    fn default() -> Self {
        Self {
            data: None,
            state: SourceState::Empty,
            updated_at: None,
            revision: 0,
        }
    }
}

impl<T> Clone for SourceSnapshot<T> {
    fn clone(&self) -> Self {
        Self {
            data: self.data.clone(),
            state: self.state.clone(),
            updated_at: self.updated_at,
            revision: self.revision,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RefreshPolicy {
    /// Measured from the preceding fetch's start, so slow fetches consume it.
    pub minimum_interval: Duration,
    /// Coalesce the initial burst without extending it for every later event.
    pub debounce: Duration,
}

pub struct SourceHandle<T> {
    requests: mpsc::Sender<()>,
    snapshots: watch::Receiver<SourceSnapshot<T>>,
}

impl<T> Clone for SourceHandle<T> {
    fn clone(&self) -> Self {
        Self {
            requests: self.requests.clone(),
            snapshots: self.snapshots.clone(),
        }
    }
}

impl<T> SourceHandle<T> {
    /// Mark the source dirty. Full means a refresh is already queued, not an
    /// error. This queue carries invalidations, never commands or writes.
    pub fn refresh(&self) -> bool {
        match self.requests.try_send(()) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(())) => true,
            Err(mpsc::error::TrySendError::Closed(())) => false,
        }
    }

    pub fn snapshot(&self) -> SourceSnapshot<T> {
        self.snapshots.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<SourceSnapshot<T>> {
        self.snapshots.clone()
    }
}

/// A single projection owner publishes prepared data and receives explicit
/// refresh requests. Publishing never schedules work by itself, so composing
/// sources cannot create a fetch/redraw feedback loop.
pub struct SourcePublisher<T> {
    publish: watch::Sender<SourceSnapshot<T>>,
    requests: mpsc::Receiver<()>,
}

impl<T> SourcePublisher<T> {
    pub fn publish(&self, snapshot: SourceSnapshot<T>) {
        self.publish.send_modify(|current| {
            let revision = current.revision.wrapping_add(1);
            *current = snapshot;
            current.revision = revision;
        });
    }

    pub async fn requested(&mut self) -> Option<()> {
        self.requests.recv().await
    }
}

pub fn source_channel<T>() -> (SourceHandle<T>, SourcePublisher<T>) {
    let (requests, pending) = mpsc::channel(1);
    let (publish, snapshots) = watch::channel(SourceSnapshot {
        data: None,
        state: SourceState::Empty,
        updated_at: None,
        revision: 0,
    });
    (
        SourceHandle {
            requests,
            snapshots,
        },
        SourcePublisher {
            publish,
            requests: pending,
        },
    )
}

/// One owner serializes fetches for this source. Requests during a fetch produce
/// at most one trailing refresh. The fetch owns cancellation cleanup and must
/// finish promptly when its token is cancelled; it is not silently detached.
pub fn start_source<T, E, F, Fut>(
    scope: &TaskScope,
    policy: RefreshPolicy,
    mut fetch: F,
) -> SourceHandle<T>
where
    T: Send + Sync + 'static,
    E: Display + Send + 'static,
    F: FnMut(CancellationToken) -> Fut + Send + 'static,
    Fut: Future<Output = Result<T, E>> + Send + 'static,
{
    let (requests, mut pending) = mpsc::channel(1);
    let snapshot = SourceSnapshot {
        data: None,
        state: SourceState::Empty,
        updated_at: None,
        revision: 0,
    };
    let (publish, snapshots) = watch::channel(snapshot);
    let token = scope.token();
    scope.spawn(async move {
        let mut last_start = None::<Instant>;
        loop {
            tokio::select! {
                biased;
                _ = token.cancelled() => break,
                request = pending.recv() => if request.is_none() { break; },
            }
            let now = Instant::now();
            let due = (now + policy.debounce)
                .max(last_start.map_or(now, |at| at + policy.minimum_interval));
            tokio::select! {
                biased;
                _ = token.cancelled() => break,
                _ = tokio::time::sleep_until(due) => {},
            }
            // Events before the fetch are included in it. Do not carry them
            // forward as a spurious extra refresh.
            while pending.try_recv().is_ok() {}
            last_start = Some(Instant::now());
            publish.send_modify(|snapshot| {
                snapshot.state = SourceState::Refreshing;
                snapshot.revision += 1;
            });
            // Include the factory call inside the protected future: it can
            // panic before producing its future. Keep this lane alive and its
            // last good data visible so the next invalidation can recover.
            let result = AssertUnwindSafe(async { fetch(token.child_token()).await })
                .catch_unwind()
                .await;
            let result = match result {
                Ok(result) => result.map_err(|error| error.to_string()),
                Err(payload) => {
                    let message = payload
                        .downcast_ref::<String>()
                        .map(String::as_str)
                        .or_else(|| payload.downcast_ref::<&str>().copied())
                        .unwrap_or("non-string panic payload");
                    tracing::error!(message, "background source panicked");
                    Err(format!("background source panicked: {message}"))
                }
            };
            if token.is_cancelled() {
                break;
            }
            publish.send_modify(|snapshot| {
                match result {
                    Ok(data) => {
                        snapshot.data = Some(Arc::new(data));
                        snapshot.updated_at = Some(Instant::now());
                        snapshot.state = SourceState::Ready;
                    }
                    Err(error) => snapshot.state = SourceState::Failed(error.to_string().into()),
                }
                snapshot.revision += 1;
            });
        }
    });
    SourceHandle {
        requests,
        snapshots,
    }
}
