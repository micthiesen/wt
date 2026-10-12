//! Push-driven sources. A leaf source fetches one domain and is the only kind
//! that can publish live data or produce a `Confirmed` value. A derived source
//! combines other sources and inherits their provenance.
use std::fmt::Display;
use std::future::Future;
use std::ops::Deref;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::FutureExt;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::cache::{CacheSlot, DiskCache};
use crate::snapshot::DomainMeta;
use crate::{Confirmed, Domain, SourceSnapshot, SourceState, Stamp, TaskScope};

#[derive(Clone, Copy, Debug, Default)]
pub struct RefreshPolicy {
    /// Measured from the preceding fetch's start, so slow fetches consume it.
    pub minimum_interval: Duration,
    /// Coalesce the initial burst without extending it for every later event.
    pub debounce: Duration,
}

/// Every leaf states whether its last good data survives a restart.
pub enum Persistence<T> {
    /// Restored at start as `Provenance::Restored`, saved after live fetches.
    Disk(DiskCache<T>),
    /// Never restored; the reason is part of the declaration.
    Never(&'static str),
}

/// Required to construct a leaf source.
pub struct SourceSpec<T> {
    pub name: &'static str,
    pub domain: Domain,
    pub persistence: Persistence<T>,
    pub policy: RefreshPolicy,
}

/// What a scope's sources declared, for the registry test and diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceInfo {
    pub name: &'static str,
    pub kind: SourceKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceKind {
    Leaf {
        domain: Domain,
        persisted: bool,
        never_reason: Option<&'static str>,
    },
    Derived,
}

#[derive(Clone, Default)]
pub(crate) struct Registry(Arc<Mutex<Vec<SourceInfo>>>);

impl Registry {
    fn record(&self, info: SourceInfo) {
        if let Ok(mut entries) = self.0.lock() {
            entries.push(info);
        }
    }

    pub(crate) fn entries(&self) -> Vec<SourceInfo> {
        self.0.lock().map(|e| e.clone()).unwrap_or_default()
    }
}

/// The one place board data gets a watch channel; everything else goes
/// through the constructors below.
#[expect(
    clippy::disallowed_methods,
    reason = "wt-runtime owns the source channels the lint protects"
)]
fn snapshot_channel<T>(
    initial: SourceSnapshot<T>,
) -> (
    watch::Sender<SourceSnapshot<T>>,
    watch::Receiver<SourceSnapshot<T>>,
) {
    watch::channel(initial)
}

/// Display access to a source. It has no way to produce a `Confirmed` value.
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

    /// A source that never changes, for disabled features and fixtures.
    pub fn constant(snapshot: SourceSnapshot<T>) -> Self {
        let (requests, _) = mpsc::channel(1);
        let (_, snapshots) = snapshot_channel(snapshot);
        Self {
            requests,
            snapshots,
        }
    }
}

/// A leaf source: fetched data for one domain.
pub struct LeafHandle<T> {
    handle: SourceHandle<T>,
    domain: Domain,
}

impl<T> Clone for LeafHandle<T> {
    fn clone(&self) -> Self {
        Self {
            handle: self.handle.clone(),
            domain: self.domain,
        }
    }
}

impl<T> Deref for LeafHandle<T> {
    type Target = SourceHandle<T>;
    fn deref(&self) -> &SourceHandle<T> {
        &self.handle
    }
}

impl<T> LeafHandle<T> {
    pub fn domain(&self) -> Domain {
        self.domain
    }

    /// Display-only handle for derived sources.
    pub fn view(&self) -> SourceHandle<T> {
        self.handle.clone()
    }

    /// The last data this process fetched live. `None` while the source is
    /// empty or still showing restored cache data.
    pub fn confirmed(&self) -> Option<Confirmed<T>> {
        confirm(&self.handle.snapshots.borrow(), self.domain)
    }

    /// Confirm a snapshot previously read from this leaf.
    pub fn confirm(&self, snapshot: &SourceSnapshot<T>) -> Option<Confirmed<T>> {
        confirm(snapshot, self.domain)
    }
}

fn confirm<T>(snapshot: &SourceSnapshot<T>, domain: Domain) -> Option<Confirmed<T>> {
    let meta = snapshot.domains().get(&domain)?;
    if !meta.is_live() || snapshot.domains().len() != 1 {
        return None;
    }
    snapshot
        .data()
        .map(|data| Confirmed::new(data.clone(), meta.observed()))
}

/// Publishes a leaf's own fetches. Live data enters the system only here.
pub struct LeafPublisher<T> {
    sender: Arc<watch::Sender<SourceSnapshot<T>>>,
    requests: mpsc::Receiver<()>,
    domain: Domain,
}

impl<T> LeafPublisher<T> {
    /// Data fetched by this process; `observed` is when the fetch started.
    pub fn publish_live(&self, data: T, observed: Stamp) {
        self.publish_live_with_state(data, observed, SourceState::Ready);
    }

    /// Live data whose lane reports a non-ready state, such as a remote host
    /// that is reachable but still refreshing.
    pub fn publish_live_with_state(&self, data: T, observed: Stamp, state: SourceState) {
        publish_live(&self.sender, self.domain, Arc::new(data), observed, state);
    }

    /// Change the lane state while keeping the last data and its provenance.
    pub fn set_state(&self, state: SourceState) {
        set_state(&self.sender, self.domain, state);
    }

    pub async fn requested(&mut self) -> Option<()> {
        self.requests.recv().await
    }
}

fn publish_live<T>(
    sender: &watch::Sender<SourceSnapshot<T>>,
    domain: Domain,
    data: Arc<T>,
    observed: Stamp,
    state: SourceState,
) {
    sender.send_modify(|snapshot| {
        let revision = snapshot.revision().wrapping_add(1);
        snapshot.set_data(Some(data));
        snapshot.set_meta(domain, DomainMeta::live(observed, state.clone()));
        *snapshot = std::mem::take(snapshot).with_state(state);
        snapshot.set_revision(revision);
    });
}

fn set_state<T>(sender: &watch::Sender<SourceSnapshot<T>>, domain: Domain, state: SourceState) {
    sender.send_if_modified(|snapshot| {
        if *snapshot.state() == state
            && snapshot
                .domains()
                .get(&domain)
                .is_some_and(|meta| *meta.state() == state)
        {
            return false;
        }
        let revision = snapshot.revision().wrapping_add(1);
        let meta = match snapshot.domains().get(&domain) {
            Some(meta) => meta.with_state(state.clone()),
            // A lane with no data yet still reports its state per domain, so
            // a first-fetch failure is visible. It cannot confirm anything.
            None => DomainMeta::live(Stamp::unordered(), state.clone()),
        };
        snapshot.set_meta(domain, meta);
        *snapshot = std::mem::take(snapshot).with_state(state);
        snapshot.set_revision(revision);
        true
    });
}

/// Publishes combinations of other sources.
pub struct DerivedPublisher<T> {
    sender: watch::Sender<SourceSnapshot<T>>,
    requests: mpsc::Receiver<()>,
}

impl<T> DerivedPublisher<T> {
    pub fn publish(&self, snapshot: SourceSnapshot<T>) {
        self.sender.send_modify(|current| {
            let revision = current.revision().wrapping_add(1);
            *current = snapshot;
            current.set_revision(revision);
        });
    }

    pub async fn requested(&mut self) -> Option<()> {
        self.requests.recv().await
    }
}

/// A derived source. Its data must come from snapshots it read, so its
/// provenance is theirs (`SourceSnapshot::derive` / `with_input`).
pub fn derived_channel<T>(
    scope: &TaskScope,
    name: &'static str,
) -> (SourceHandle<T>, DerivedPublisher<T>) {
    scope.registry().record(SourceInfo {
        name,
        kind: SourceKind::Derived,
    });
    let (requests, pending) = mpsc::channel(1);
    let (sender, snapshots) = snapshot_channel(SourceSnapshot::default());
    (
        SourceHandle {
            requests,
            snapshots,
        },
        DerivedPublisher {
            sender,
            requests: pending,
        },
    )
}

/// A hand-driven leaf. Disk persistence restores and saves automatically.
pub fn leaf_channel<T: Send + Sync + 'static>(
    scope: &TaskScope,
    spec: SourceSpec<T>,
) -> (LeafHandle<T>, LeafPublisher<T>) {
    let (requests, pending) = mpsc::channel(1);
    let (sender, snapshots) = snapshot_channel(SourceSnapshot::default());
    let sender = Arc::new(sender);
    let domain = spec.domain;
    register_leaf(scope, &spec);
    if let Persistence::Disk(cache) = spec.persistence {
        attach_cache(scope, spec.name, domain, cache.slot(), sender.clone());
    }
    (
        LeafHandle {
            handle: SourceHandle {
                requests,
                snapshots,
            },
            domain,
        },
        LeafPublisher {
            sender,
            requests: pending,
            domain,
        },
    )
}

fn register_leaf<T>(scope: &TaskScope, spec: &SourceSpec<T>) {
    let (persisted, never_reason) = match &spec.persistence {
        Persistence::Disk(_) => (true, None),
        Persistence::Never(reason) => (false, Some(*reason)),
    };
    scope.registry().record(SourceInfo {
        name: spec.name,
        kind: SourceKind::Leaf {
            domain: spec.domain,
            persisted,
            never_reason,
        },
    });
}

/// A fetch running longer than this shows its lane as refreshing.
const REFRESHING_AFTER: Duration = Duration::from_millis(100);

/// Minimum spacing between cache writes of one source.
const WRITE_INTERVAL: Duration = Duration::from_secs(10);
/// Bound on the final write at shutdown. A skipped write loses only a cache.
const FLUSH_BUDGET: Duration = Duration::from_secs(1);

fn attach_cache<T: Send + Sync + 'static>(
    scope: &TaskScope,
    name: &'static str,
    domain: Domain,
    cache: Arc<dyn CacheSlot<T>>,
    sender: Arc<watch::Sender<SourceSnapshot<T>>>,
) {
    // Restore off the input path. A live publish that lands first wins.
    {
        let cache = cache.clone();
        let sender = sender.clone();
        let cancel = scope.token();
        scope.spawn(async move {
            let read = tokio::task::spawn_blocking(move || cache.read(name));
            let result = tokio::select! {
                biased;
                _ = cancel.cancelled() => return,
                result = read => result,
            };
            let Ok(Ok((data, saved_at_ms))) = result else {
                return;
            };
            let data = Arc::new(data);
            sender.send_if_modified(|snapshot| {
                // Live data that landed first wins. A lane that has only
                // reported a state (a fetch started, or failed) has no data
                // yet, so the restore still applies and keeps that state.
                if snapshot.data().is_some() {
                    return false;
                }
                let state = match snapshot.domains().get(&domain).map(|meta| meta.state()) {
                    Some(state @ (SourceState::Refreshing | SourceState::Failed(_))) => {
                        state.clone()
                    }
                    _ => SourceState::Refreshing,
                };
                let revision = snapshot.revision().wrapping_add(1);
                snapshot.set_data(Some(data));
                snapshot.set_meta(
                    domain,
                    DomainMeta::restored(saved_at_ms).with_state(state.clone()),
                );
                *snapshot = std::mem::take(snapshot).with_state(state);
                snapshot.set_revision(revision);
                true
            });
        });
    }
    // Save live data, throttled; flush the latest once at shutdown.
    let cancel = scope.token();
    let mut updates = sender.subscribe();
    scope.spawn(async move {
        let mut pending: Option<Arc<T>> = None;
        let mut saved: Option<Arc<T>> = None;
        let mut last_write: Option<Instant> = None;
        loop {
            // The first save happens at once; later ones are spaced.
            let due = last_write.map_or_else(Instant::now, |at| at + WRITE_INTERVAL);
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                changed = updates.changed() => {
                    if changed.is_err() { break; }
                    let data = live_data(&updates.borrow_and_update(), domain);
                    if let Some(data) = data
                        && !saved.as_ref().is_some_and(|s| Arc::ptr_eq(s, &data))
                    {
                        pending = Some(data);
                    }
                }
                _ = tokio::time::sleep_until(due), if pending.is_some() => {
                    if let Some(data) = pending.take() {
                        write(cache.clone(), name, data.clone(), None).await;
                        saved = Some(data);
                        last_write = Some(Instant::now());
                    }
                }
            }
        }
        if let Some(data) = pending.take() {
            write(cache, name, data, Some(FLUSH_BUDGET)).await;
        }
    });
}

fn live_data<T>(snapshot: &SourceSnapshot<T>, domain: Domain) -> Option<Arc<T>> {
    let meta = snapshot.domains().get(&domain)?;
    (meta.is_live() && *meta.state() == SourceState::Ready && meta.observed().is_ordered())
        .then(|| snapshot.data().cloned())
        .flatten()
}

async fn write<T: Send + Sync + 'static>(
    cache: Arc<dyn CacheSlot<T>>,
    name: &'static str,
    data: Arc<T>,
    budget: Option<Duration>,
) {
    let job = tokio::task::spawn_blocking(move || cache.write(name, &data));
    let result = match budget {
        Some(budget) => match tokio::time::timeout(budget, job).await {
            Ok(result) => result,
            Err(_) => {
                tracing::warn!(source = name, "skipped source cache flush at shutdown");
                return;
            }
        },
        None => job.await,
    };
    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(source = name, %error, "could not save source cache"),
        Err(error) => tracing::warn!(source = name, %error, "source cache writer panicked"),
    }
}

/// One owner serializes fetches for this source. Requests during a fetch
/// produce at most one trailing refresh. Data is stamped with the fetch start.
pub fn start_source<T, E, F, Fut>(
    scope: &TaskScope,
    spec: SourceSpec<T>,
    mut fetch: F,
) -> LeafHandle<T>
where
    T: Send + Sync + 'static,
    E: Display + Send + 'static,
    F: FnMut(CancellationToken) -> Fut + Send + 'static,
    Fut: Future<Output = Result<T, E>> + Send + 'static,
{
    start_observed_source(scope, spec, move |token| {
        let started = Stamp::now();
        let future = fetch(token);
        async move { future.await.map(|data| Fetched::new(data, started)) }
    })
}

/// A fetch result with the moment its observation started. Most sources use
/// `start_source`, which stamps the fetch start; a source that reads another
/// process's observation passes that observation's own start.
pub struct Fetched<T> {
    pub data: T,
    pub observed: Stamp,
}

impl<T> Fetched<T> {
    pub fn new(data: T, observed: Stamp) -> Self {
        Self { data, observed }
    }
}

/// Like `start_source`, but the fetch reports its own observation stamp.
pub fn start_observed_source<T, E, F, Fut>(
    scope: &TaskScope,
    spec: SourceSpec<T>,
    mut fetch: F,
) -> LeafHandle<T>
where
    T: Send + Sync + 'static,
    E: Display + Send + 'static,
    F: FnMut(CancellationToken) -> Fut + Send + 'static,
    Fut: Future<Output = Result<Fetched<T>, E>> + Send + 'static,
{
    start_skippable_source(scope, spec, move |token| {
        let future = fetch(token);
        async move { future.await.map(Some) }
    })
}

/// A source whose fetch may decline to observe anything, e.g. because its key
/// set comes from another source that is not yet confirmed. `Ok(None)` keeps
/// the current data, provenance and state; a later invalidation retries.
pub fn start_skippable_source<T, E, F, Fut>(
    scope: &TaskScope,
    spec: SourceSpec<T>,
    mut fetch: F,
) -> LeafHandle<T>
where
    T: Send + Sync + 'static,
    E: Display + Send + 'static,
    F: FnMut(CancellationToken) -> Fut + Send + 'static,
    Fut: Future<Output = Result<Option<Fetched<T>>, E>> + Send + 'static,
{
    let policy = spec.policy;
    let (handle, mut publisher) = leaf_channel(scope, spec);
    let token = scope.token();
    scope.spawn(async move {
        let mut last_start = None::<Instant>;
        loop {
            tokio::select! {
                biased;
                _ = token.cancelled() => break,
                request = publisher.requested() => if request.is_none() { break; },
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
            while publisher.requests.try_recv().is_ok() {}
            let previous = publisher.sender.borrow().state().clone();
            let started = Instant::now();
            // Include the factory call inside the protected future: it can
            // panic before producing its future. Keep this lane alive and its
            // last good data visible so the next invalidation can recover.
            let work = AssertUnwindSafe(async { fetch(token.child_token()).await }).catch_unwind();
            tokio::pin!(work);
            // Only a fetch that takes a while reads as refreshing; a quick
            // fetch or a skip publishes nothing until it has a result.
            let mut announced = false;
            let result = tokio::select! {
                result = &mut work => result,
                _ = tokio::time::sleep(REFRESHING_AFTER) => {
                    announced = true;
                    publisher.set_state(SourceState::Refreshing);
                    work.await
                }
            };
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
            match result {
                Ok(Some(fetched)) => {
                    last_start = Some(started);
                    publisher.publish_live(fetched.data, fetched.observed)
                }
                // Nothing was fetched, so the rate-limit floor is untouched.
                // Restored data that arrived meanwhile keeps reading as stale.
                Ok(None) if !announced => {}
                Ok(None) => {
                    let has_data = publisher.sender.borrow().data().is_some();
                    publisher.set_state(match previous {
                        SourceState::Empty if has_data => SourceState::Refreshing,
                        state => state,
                    })
                }
                Err(error) => {
                    last_start = Some(started);
                    publisher.set_state(SourceState::Failed(error.into()))
                }
            }
        }
    });
    handle
}
