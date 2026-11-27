//! Prepared source data with its provenance. Fields are private: a snapshot
//! only becomes live through a leaf publisher, and only becomes restored
//! through the disk cache. Derived snapshots inherit what their inputs say.
use std::collections::BTreeMap;
use std::ops::Deref;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::Stamp;
use wt_core::Domain;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceState {
    Empty,
    Refreshing,
    Ready,
    Failed(Arc<str>),
}

impl SourceState {
    /// The state of something built from several inputs, in the given order:
    /// the first failure wins and is prefixed with its input's label (so the
    /// pane names the source), then any input still refreshing, then ready
    /// when every input is ready, else empty.
    pub fn combine<'a>(
        inputs: impl IntoIterator<Item = (&'a str, &'a SourceState)>,
    ) -> SourceState {
        let mut refreshing = false;
        let mut ready = true;
        for (label, state) in inputs {
            match state {
                SourceState::Failed(error) => {
                    return SourceState::Failed(format!("{label}: {error}").into());
                }
                SourceState::Refreshing => refreshing = true,
                SourceState::Empty => ready = false,
                SourceState::Ready => {}
            }
        }
        if refreshing {
            SourceState::Refreshing
        } else if ready {
            SourceState::Ready
        } else {
            SourceState::Empty
        }
    }

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

/// Fetch-start stamps of a leaf that reads its rows independently: one for
/// the listing of its keys and one per listed key.
///
/// The listing stamp answers for every key the listing did not contain: such
/// a key was absent from a listing that started then. That is what lets a
/// removal settle on the listing alone, and it is why a key from another key
/// space reads as "absent", never as "fresh".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyedStamps {
    index: Stamp,
    rows: BTreeMap<String, Stamp>,
}

impl KeyedStamps {
    pub fn new(index: Stamp, rows: BTreeMap<String, Stamp>) -> Self {
        Self { index, rows }
    }

    /// Start of the listing that produced the key set.
    pub fn index(&self) -> Stamp {
        self.index
    }

    /// Start of the read behind what is known about `key`.
    pub fn get(&self, key: &str) -> Stamp {
        self.rows.get(key).copied().unwrap_or(self.index)
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.rows.keys().map(String::as_str)
    }

    /// The stamp the whole domain can claim: nothing in it was read earlier.
    pub fn oldest(&self) -> Stamp {
        self.rows
            .values()
            .fold(self.index, |oldest, stamp| oldest.earliest(*stamp))
    }
}

/// One domain's provenance, fetch-start stamp and lane state.
///
/// Equality ignores `observed` and `keyed`: a fetch that returns the same
/// data must not count as a visible change, so idle boards stay still.
/// Pending mutations read the stamps directly.
///
/// Invariant: `observed` is never later than any stamp in `keyed`. Everything
/// a snapshot shows for the domain was read by a fetch that started at or
/// after `observed`, whether the leaf reads the domain whole or row by row,
/// so `Confirmed` and every reader of the domain stamp keep their meaning. A
/// keyed stamp only ever says more about one row: a change to that row may
/// settle on it without waiting for the oldest row to be read again.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DomainMeta {
    provenance: Provenance,
    observed: Stamp,
    state: SourceState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    keyed: Option<Arc<KeyedStamps>>,
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
            keyed: None,
        }
    }

    /// Live rows read at different moments. The domain stamp is the oldest.
    pub(crate) fn live_keyed(keyed: KeyedStamps, state: SourceState) -> Self {
        Self {
            provenance: Provenance::Live,
            observed: keyed.oldest(),
            state,
            keyed: Some(Arc::new(keyed)),
        }
    }

    pub(crate) fn restored(saved_at_ms: u64) -> Self {
        Self {
            provenance: Provenance::Restored { saved_at_ms },
            observed: Stamp::unordered(),
            state: SourceState::Refreshing,
            keyed: None,
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

    /// Start of the read behind what the domain shows for `key`. A domain
    /// read whole answers with its one stamp.
    pub fn observed_for(&self, key: &str) -> Stamp {
        self.keyed
            .as_ref()
            .map_or(self.observed, |keyed| keyed.get(key))
    }

    pub fn keyed(&self) -> Option<&KeyedStamps> {
        self.keyed.as_deref()
    }

    pub fn state(&self) -> &SourceState {
        &self.state
    }

    pub fn is_live(&self) -> bool {
        self.provenance == Provenance::Live
    }

    /// Restored is sticky, the stamp is the earliest, the state the worst.
    /// Keyed stamps merge key by key, an unkeyed side answering for every key
    /// with its one stamp, so the merge is never fresher than either input.
    fn merge(&mut self, other: &DomainMeta) {
        let keyed = match (&self.keyed, &other.keyed) {
            (None, None) => None,
            (Some(mine), Some(theirs)) if Arc::ptr_eq(mine, theirs) => Some(mine.clone()),
            (mine, theirs) => {
                let index = |meta: &DomainMeta| {
                    meta.keyed
                        .as_ref()
                        .map_or(meta.observed, |keyed| keyed.index)
                };
                let rows = mine
                    .iter()
                    .chain(theirs)
                    .flat_map(|keyed| keyed.rows.keys())
                    .map(|key| {
                        (
                            key.clone(),
                            self.observed_for(key).earliest(other.observed_for(key)),
                        )
                    })
                    .collect();
                Some(Arc::new(KeyedStamps {
                    index: index(self).earliest(index(other)),
                    rows,
                }))
            }
        };
        self.keyed = keyed;
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

    /// Test fixtures only: also mark `domain` as read live just now.
    #[cfg(any(test, feature = "testing"))]
    pub fn test_also_live(mut self, domain: Domain) -> Self {
        let state = self.state.clone();
        self.set_meta(domain, DomainMeta::live(Stamp::now(), state));
        self
    }

    /// Test fixtures only: `domain` read live row by row, with these stamps.
    #[cfg(any(test, feature = "testing"))]
    pub fn test_keyed(mut self, domain: Domain, stamps: KeyedStamps) -> Self {
        let state = self.state.clone();
        self.set_meta(domain, DomainMeta::live_keyed(stamps, state));
        self
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
    keyed: Option<Arc<KeyedStamps>>,
}

impl<T> Clone for Confirmed<T> {
    fn clone(&self) -> Self {
        Self {
            data: self.data.clone(),
            observed: self.observed,
            keyed: self.keyed.clone(),
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
    #[cfg(test)]
    pub(crate) fn new(data: Arc<T>, observed: Stamp) -> Self {
        Self {
            data,
            observed,
            keyed: None,
        }
    }

    /// Live data with the stamps its leaf published for it.
    pub(crate) fn from_meta(data: Arc<T>, meta: &DomainMeta) -> Self {
        Self {
            data,
            observed: meta.observed,
            keyed: meta.keyed.clone(),
        }
    }

    /// Start of the oldest fetch behind this data.
    pub fn observed(&self) -> Stamp {
        self.observed
    }

    /// Start of the fetch behind what this data says about `key`, never
    /// earlier than `observed`. A domain read whole answers with its one
    /// stamp. A decision about one row may bound its age by this; a decision
    /// about the whole domain uses `observed`.
    pub fn observed_for(&self, key: &str) -> Stamp {
        self.keyed
            .as_ref()
            .map_or(self.observed, |keyed| keyed.get(key))
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
            keyed: self.keyed.clone(),
        }
    }

    /// Test fixtures only: production code obtains confirmations from leaves.
    #[cfg(any(test, feature = "testing"))]
    pub fn for_test(data: T) -> Self {
        Self {
            data: Arc::new(data),
            observed: Stamp::now(),
            keyed: None,
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

/// Whether any domain's fetch-start stamp, whole or for one key, differs
/// between two maps.
pub fn stamps_advanced(old: &DomainMap, new: &DomainMap) -> bool {
    new.iter().any(|(domain, meta)| {
        old.get(domain).is_some_and(|previous| {
            previous.observed != meta.observed || previous.keyed != meta.keyed
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn combined_state_names_the_first_failed_input() {
        let ready = SourceState::Ready;
        let failed = SourceState::Failed("offline".into());
        let refreshing = SourceState::Refreshing;
        let empty = SourceState::Empty;
        assert_eq!(
            SourceState::combine([("Git", &ready), ("State", &failed), ("GitHub", &refreshing)]),
            SourceState::Failed("State: offline".into())
        );
        assert_eq!(
            SourceState::combine([("Git", &ready), ("GitHub", &refreshing)]),
            SourceState::Refreshing
        );
        assert_eq!(
            SourceState::combine([("Git", &ready), ("State", &empty)]),
            SourceState::Empty
        );
        assert_eq!(
            SourceState::combine([("Git", &ready), ("State", &ready)]),
            SourceState::Ready
        );
    }

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

    #[tokio::test(start_paused = true)]
    async fn keyed_stamps_never_make_a_domain_or_a_merge_fresher() {
        let old = Stamp::now();
        tokio::time::advance(Duration::from_millis(10)).await;
        let listed = Stamp::now();
        tokio::time::advance(Duration::from_millis(10)).await;
        let new = Stamp::now();
        let keyed = |a: Stamp, b: Stamp| {
            DomainMeta::live_keyed(
                KeyedStamps::new(listed, [("a".to_owned(), a), ("b".to_owned(), b)].into()),
                SourceState::Ready,
            )
        };
        let meta = keyed(new, old);
        // The domain stamp is the oldest row, so whole-domain readers and
        // `Confirmed` see nothing fresher than before.
        assert_eq!(meta.observed(), old);
        assert_eq!(meta.observed_for("a"), new);
        assert_eq!(meta.observed_for("b"), old);
        // A key that is not listed was absent from the listing.
        assert_eq!(meta.observed_for("gone"), listed);

        // Two versions of the same leaf merge key by key to the earlier.
        let mut merged = keyed(new, old);
        merged.merge(&keyed(old, new));
        assert_eq!(merged.observed_for("a"), old);
        assert_eq!(merged.observed_for("b"), old);
        assert_eq!(merged.observed(), old);

        // An input that read the domain whole answers for every key.
        let mut clamped = keyed(new, new);
        clamped.merge(&DomainMeta::live(old, SourceState::Ready));
        assert_eq!(clamped.observed_for("a"), old);
        assert_eq!(clamped.observed_for("gone"), old);
        let mut whole = DomainMeta::live(new, SourceState::Ready);
        whole.merge(&keyed(old, new));
        assert_eq!(whole.observed_for("a"), old);
        assert_eq!(whole.observed_for("b"), new);
        assert_eq!(whole.observed(), old);

        // Restored or foreign data orders against nothing, per key too.
        let mut restored = keyed(new, new);
        restored.merge(&DomainMeta::restored(1));
        assert!(!restored.is_live());
        assert!(!restored.observed_for("a").is_ordered());

        // A stamp that moved for one row alone still counts as an advance.
        let before = DomainMap::from([(Domain::Git, keyed(old, old))]);
        let after = DomainMap::from([(Domain::Git, keyed(new, old))]);
        assert_eq!(before, after, "stamps are not a visible change");
        assert!(stamps_advanced(&before, &after));
        assert!(!stamps_advanced(&before, &before.clone()));

        // The wire form round-trips, and a peer that sends no keyed stamps
        // reads as a domain read whole.
        let json = serde_json::to_value(&meta).unwrap();
        let back: DomainMeta = serde_json::from_value(json).unwrap();
        assert_eq!(back.observed_for("a"), new);
        let plain = serde_json::to_value(DomainMeta::live(old, SourceState::Ready)).unwrap();
        assert!(plain.get("keyed").is_none());
        let back: DomainMeta = serde_json::from_value(plain).unwrap();
        assert_eq!(back.observed_for("a"), old);
    }
}
