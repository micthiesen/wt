//! Prepared source data with its provenance. Fields are private: a snapshot
//! only becomes live through a leaf publisher, and only becomes restored
//! through the disk cache. Derived snapshots inherit what their inputs say.
use std::collections::BTreeMap;
use std::ops::Deref;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::{Domain, Stamp};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceState {
    Empty,
    Refreshing,
    Ready,
    Failed(Arc<str>),
}

impl SourceState {
    /// Failed over Refreshing over Empty over Ready.
    pub fn worst(self, other: SourceState) -> SourceState {
        fn rank(state: &SourceState) -> u8 {
            match state {
                SourceState::Ready => 0,
                SourceState::Empty => 1,
                SourceState::Refreshing => 2,
                SourceState::Failed(_) => 3,
            }
        }
        if rank(&other) > rank(&self) {
            other
        } else {
            self
        }
    }
}

/// Where displayed data came from. Only `Live` data was fetched by the host
/// that publishes it, during this process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum Provenance {
    Live,
    /// Loaded from the disposable cache; `saved_at_ms` is wall-clock epoch ms.
    Restored {
        saved_at_ms: u64,
    },
}

/// One domain's provenance, fetch-start stamp and lane state.
///
/// Equality ignores `observed`: a fetch that returns the same data must not
/// count as a visible change, so idle boards stay still. Pending mutations
/// read the stamp directly.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DomainMeta {
    provenance: Provenance,
    observed: Stamp,
    state: SourceState,
}

impl PartialEq for DomainMeta {
    fn eq(&self, other: &Self) -> bool {
        self.provenance == other.provenance && self.state == other.state
    }
}

impl Eq for DomainMeta {}

impl DomainMeta {
    pub(crate) fn live(observed: Stamp, state: SourceState) -> Self {
        Self {
            provenance: Provenance::Live,
            observed,
            state,
        }
    }

    pub(crate) fn restored(saved_at_ms: u64) -> Self {
        Self {
            provenance: Provenance::Restored { saved_at_ms },
            observed: Stamp::unordered(),
            state: SourceState::Refreshing,
        }
    }

    pub(crate) fn with_state(&self, state: SourceState) -> Self {
        Self {
            state,
            ..self.clone()
        }
    }

    pub fn provenance(&self) -> Provenance {
        self.provenance
    }

    pub fn observed(&self) -> Stamp {
        self.observed
    }

    pub fn state(&self) -> &SourceState {
        &self.state
    }

    pub fn is_live(&self) -> bool {
        self.provenance == Provenance::Live
    }

    /// Restored is sticky, the stamp is the earliest, the state the worst.
    fn merge(&mut self, other: &DomainMeta) {
        self.provenance = match (self.provenance, other.provenance) {
            (Provenance::Live, Provenance::Live) => Provenance::Live,
            (Provenance::Restored { saved_at_ms: a }, Provenance::Restored { saved_at_ms: b }) => {
                Provenance::Restored {
                    saved_at_ms: a.min(b),
                }
            }
            (Provenance::Restored { saved_at_ms }, _)
            | (_, Provenance::Restored { saved_at_ms }) => Provenance::Restored { saved_at_ms },
        };
        self.observed = self.observed.earliest(other.observed);
        self.state = self.state.clone().worst(other.state.clone());
    }
}

pub type DomainMap = BTreeMap<Domain, DomainMeta>;

/// Merge `other` into `into` per domain.
pub fn merge_domains(into: &mut DomainMap, other: &DomainMap) {
    for (domain, meta) in other {
        into.entry(*domain)
            .and_modify(|current| current.merge(meta))
            .or_insert_with(|| meta.clone());
    }
}

#[derive(Debug)]
pub struct SourceSnapshot<T> {
    data: Option<Arc<T>>,
    state: SourceState,
    domains: Arc<DomainMap>,
    revision: u64,
}

impl<T> Default for SourceSnapshot<T> {
    fn default() -> Self {
        Self {
            data: None,
            state: SourceState::Empty,
            domains: Arc::default(),
            revision: 0,
        }
    }
}

impl<T> Clone for SourceSnapshot<T> {
    fn clone(&self) -> Self {
        Self {
            data: self.data.clone(),
            state: self.state.clone(),
            domains: self.domains.clone(),
            revision: self.revision,
        }
    }
}

impl<T> SourceSnapshot<T> {
    #[cfg(any(test, feature = "testing"))]
    fn from_parts(data: Option<Arc<T>>, state: SourceState, domains: Arc<DomainMap>) -> Self {
        Self {
            data,
            state,
            domains,
            revision: 0,
        }
    }

    /// Display data. Safety decisions use `LeafHandle::confirmed` instead.
    pub fn data(&self) -> Option<&Arc<T>> {
        self.data.as_ref()
    }

    pub fn state(&self) -> &SourceState {
        &self.state
    }

    pub fn domains(&self) -> &DomainMap {
        &self.domains
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub(crate) fn set_revision(&mut self, revision: u64) {
        self.revision = revision;
    }

    pub(crate) fn set_data(&mut self, data: Option<Arc<T>>) {
        self.data = data;
    }

    pub(crate) fn set_meta(&mut self, domain: Domain, meta: DomainMeta) {
        Arc::make_mut(&mut self.domains).insert(domain, meta);
    }

    /// True when any contributing domain was restored from the cache.
    pub fn is_restored(&self) -> bool {
        self.domains.values().any(|meta| !meta.is_live())
    }

    /// A derived snapshot carrying new data with this snapshot's provenance
    /// and state. This is the only way to give derived data a provenance.
    pub fn derive<U>(&self, data: Option<Arc<U>>) -> SourceSnapshot<U> {
        SourceSnapshot {
            data,
            state: self.state.clone(),
            domains: self.domains.clone(),
            revision: 0,
        }
    }

    pub fn map<U>(&self, f: impl FnOnce(&T) -> U) -> SourceSnapshot<U> {
        self.derive(self.data.as_deref().map(|data| Arc::new(f(data))))
    }

    /// Replace the data, keeping provenance. The data must be derived from the
    /// inputs whose provenance this snapshot carries.
    pub fn with_data(mut self, data: Option<Arc<T>>) -> Self {
        self.data = data;
        self
    }

    pub fn with_state(mut self, state: SourceState) -> Self {
        self.state = state;
        self
    }

    /// Add another input's provenance. The combined state is left to the
    /// caller, which knows whether that input's failure masks this one.
    pub fn with_input<U>(mut self, input: &SourceSnapshot<U>) -> Self {
        if !input.domains.is_empty() {
            merge_domains(Arc::make_mut(&mut self.domains), &input.domains);
        }
        self
    }

    /// Provenance of a whole other host, merged in. Used by the fleet board.
    pub fn with_domains(mut self, domains: &DomainMap) -> Self {
        if !domains.is_empty() {
            merge_domains(Arc::make_mut(&mut self.domains), domains);
        }
        self
    }

    /// Test fixtures only: a live, ready observation of `domain`.
    #[cfg(any(test, feature = "testing"))]
    pub fn test_live(data: Option<T>, state: SourceState, domain: Domain) -> Self {
        let mut snapshot = Self::from_parts(data.map(Arc::new), state.clone(), Arc::default());
        snapshot.set_meta(domain, DomainMeta::live(Stamp::now(), state));
        snapshot
    }

    /// Test fixtures only: edit the data in place.
    #[cfg(any(test, feature = "testing"))]
    pub fn test_data_mut(&mut self) -> &mut T
    where
        T: Clone,
    {
        Arc::make_mut(self.data.as_mut().expect("fixture snapshot has data"))
    }

    /// Test fixtures only: a snapshot restored from the cache for `domain`.
    #[cfg(any(test, feature = "testing"))]
    pub fn test_restored(data: T, domain: Domain, saved_at_ms: u64) -> Self {
        let mut snapshot = Self::from_parts(
            Some(Arc::new(data)),
            SourceState::Refreshing,
            Arc::default(),
        );
        snapshot.set_meta(domain, DomainMeta::restored(saved_at_ms));
        snapshot
    }

    /// Whether a republish would change what anyone sees: data, state and
    /// provenance, ignoring revision. Stamps alone are not a visible change,
    /// except while a recent write waits for confirmation (`hold_for_settlement`):
    /// then a newer fetch-start stamp must reach the pending engine even when
    /// the data is identical, or the change could only settle at its deadline.
    pub fn same_view(&self, other: &Self) -> bool
    where
        T: PartialEq,
    {
        self.data == other.data
            && self.state == other.state
            && self.domains == other.domains
            && !(settling() && stamps_advanced(&self.domains, &other.domains))
    }
}

/// Data that this process fetched live, with no optimistic projection and no
/// cache restore behind it. Only a leaf handle can produce one, so a function
/// that takes `Confirmed<T>` cannot be fed displayed or restored data.
#[derive(Debug)]
pub struct Confirmed<T> {
    data: Arc<T>,
    observed: Stamp,
}

impl<T> Clone for Confirmed<T> {
    fn clone(&self) -> Self {
        Self {
            data: self.data.clone(),
            observed: self.observed,
        }
    }
}

impl<T> Deref for Confirmed<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.data
    }
}

impl<T> Confirmed<T> {
    pub(crate) fn new(data: Arc<T>, observed: Stamp) -> Self {
        Self { data, observed }
    }

    /// Start of the fetch that produced this data.
    pub fn observed(&self) -> Stamp {
        self.observed
    }

    /// Time since the fetch started; `None` for observations whose start is
    /// unknown, which callers must treat as too old.
    pub fn age(&self) -> Option<std::time::Duration> {
        self.observed
            .local_instant()
            .map(|at| tokio::time::Instant::now().saturating_duration_since(at))
    }

    /// Fresh enough for a decision that needs a bounded age.
    pub fn fresh_within(&self, limit: std::time::Duration) -> bool {
        self.age().is_some_and(|age| age <= limit)
    }

    pub fn arc(&self) -> &Arc<T> {
        &self.data
    }

    /// A derived confirmed value, e.g. a narrowed view of the same fetch.
    pub fn map<U>(&self, f: impl FnOnce(&T) -> U) -> Confirmed<U> {
        Confirmed {
            data: Arc::new(f(&self.data)),
            observed: self.observed,
        }
    }

    /// Test fixtures only: production code obtains confirmations from leaves.
    #[cfg(any(test, feature = "testing"))]
    pub fn for_test(data: T) -> Self {
        Self {
            data: Arc::new(data),
            observed: Stamp::now(),
        }
    }
}

static SETTLE_UNTIL_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn now_ms() -> u64 {
    // Process-local monotonic milliseconds; only compared within this process.
    use std::sync::OnceLock;
    static ORIGIN: OnceLock<std::time::Instant> = OnceLock::new();
    ORIGIN
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64
}

/// Keep stamp-only changes flowing for `window`, because a write this process
/// accepted (or relayed) is waiting for a fetch that started after it. Outside
/// such windows, an identical refetch does not republish or redraw.
pub fn hold_for_settlement(window: std::time::Duration) {
    let until = now_ms().saturating_add(window.as_millis() as u64);
    SETTLE_UNTIL_MS.fetch_max(until, std::sync::atomic::Ordering::Relaxed);
}

/// Whether a write is waiting for confirmation in this process.
pub fn settling() -> bool {
    now_ms() < SETTLE_UNTIL_MS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Whether any domain's fetch-start stamp differs between two maps.
pub fn stamps_advanced(old: &DomainMap, new: &DomainMap) -> bool {
    new.iter().any(|(domain, meta)| {
        old.get(domain)
            .is_some_and(|previous| previous.observed != meta.observed)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn confirmed_data_ages_out_of_bounded_decisions() {
        let read = Confirmed::for_test(1u8);
        assert!(read.fresh_within(Duration::from_secs(600)));
        tokio::time::advance(Duration::from_secs(601)).await;
        assert!(!read.fresh_within(Duration::from_secs(600)));
        // An observation with no known start is never fresh.
        let unordered = Confirmed::new(Arc::new(1u8), Stamp::unordered());
        assert!(!unordered.fresh_within(Duration::from_secs(600)));
    }
}
