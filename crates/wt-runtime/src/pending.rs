//! The one optimistic-update engine. It never performs writes: it only
//! remembers what a caller asked for and projects it over confirmed data
//! until a fetch that started after the write shows it, the write fails, or a
//! bounded deadline passes. Because it cannot write, it cannot retry.
use std::marker::PhantomData;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use crate::Stamp;
use wt_core::Domain;

/// What a pending entry is about, for markers and per-host observation.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Target {
    /// `None` is the local host.
    pub host: Option<String>,
    /// Row identity on the board; `None` for board-wide state.
    pub key: Option<String>,
}

impl Target {
    pub fn row(host: Option<String>, key: impl Into<String>) -> Self {
        Self {
            host,
            key: Some(key.into()),
        }
    }

    pub fn board(host: Option<String>) -> Self {
        Self { host, key: None }
    }
}

/// A requested change that can be shown before it is observed.
pub trait Mutation<T>: Clone + Send + Sync + 'static {
    /// Entries with the same key replace each other; the newest wins.
    fn key(&self) -> String;
    fn target(&self) -> Target;
    /// Short present-tense text for the marker, e.g. "arming merge".
    fn label(&self) -> String;
    /// The domains a confirming fetch must come from. Also what the executing
    /// host refreshes after the write.
    fn domains(&self) -> &'static [Domain];
    /// Show the requested result over confirmed data.
    fn apply(&self, view: &mut T);
    /// Whether confirmed data already shows the requested result.
    fn observed(&self, confirmed: &T) -> bool;
    /// The rows whose reads confirm the change: every one of them must have
    /// been read by a fetch that started at or after the write. A change to
    /// several rows names each, so it need not wait for rows it never
    /// touched; a target without a row waits for the whole domain.
    fn confirming(&self) -> Vec<Target> {
        vec![self.target()]
    }
    /// Bounds for work that legitimately runs long (a checkout, a queued
    /// removal). `None` uses the engine's limits. Still bounded: a change is
    /// never shown forever.
    fn deadlines(&self) -> Option<Deadlines> {
        None
    }
}

/// Per-mutation bounds for the in-flight and accepted phases.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Deadlines {
    pub in_flight: Duration,
    pub accepted: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PendingPhase {
    /// Sent; the host has not answered.
    InFlight,
    /// The host reports the write succeeded; waiting for a fetch to show it.
    Accepted,
    /// The write may or may not have happened. Shown until its deadline;
    /// never resent.
    Unconfirmed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingView {
    pub target: Target,
    pub label: String,
    pub phase: PendingPhase,
}

#[derive(Clone, Copy, Debug)]
pub struct PendingLimits {
    pub in_flight: Duration,
    /// For GitHub-backed domains, whose reads lag writes and are rate limited.
    pub accepted_remote: Duration,
    /// How far the accepted deadline of a GitHub change stretches while no
    /// read that started after the write has finished: the GitHub lane's rate
    /// floor plus a slow, retried read.
    pub accepted_github_cap: Duration,
    pub accepted_local: Duration,
    /// How far the accepted deadline of a local change stretches while no
    /// read that started after the write has finished. A local read takes
    /// well under a second, but it can sit behind a loaded machine or an
    /// SSH hop; a change is reported as not shown only once a read that
    /// could have shown it has come back without it.
    pub accepted_local_cap: Duration,
    pub unconfirmed: Duration,
}

impl Default for PendingLimits {
    fn default() -> Self {
        Self {
            in_flight: Duration::from_secs(60),
            accepted_remote: Duration::from_secs(30),
            // 10s GitHub rate floor + the client's 100s retry budget + slack.
            accepted_github_cap: Duration::from_secs(120),
            accepted_local: Duration::from_secs(12),
            accepted_local_cap: Duration::from_secs(60),
            unconfirmed: Duration::from_secs(30),
        }
    }
}

impl PendingLimits {
    /// The accepted deadline and its cap for these domains.
    fn accepted(&self, domains: &[Domain]) -> (Duration, Duration) {
        let deadline = if domains.iter().any(|domain| domain.slow_reads()) {
            self.accepted_remote
        } else {
            self.accepted_local
        };
        let cap = if domains.contains(&Domain::Github) {
            self.accepted_github_cap.max(deadline)
        } else if domains.iter().any(|domain| domain.slow_reads()) {
            deadline
        } else {
            self.accepted_local_cap.max(deadline)
        };
        (deadline, cap)
    }
}

#[derive(Clone, Debug)]
enum Phase {
    InFlight {
        deadline: Instant,
    },
    /// `deadline` applies once a read that started at or after `at` has
    /// finished. Until then the entry may wait up to `cap`, so a confirming
    /// read delayed by the rate floor or a slow reply can still settle it.
    Accepted {
        at: Stamp,
        deadline: Instant,
        cap: Instant,
        /// A read that started at or after `at` has been seen.
        read_after: bool,
    },
    Unconfirmed {
        deadline: Instant,
    },
}

impl Phase {
    /// When this entry next needs checking. An accepted entry still waiting
    /// for its first confirming read is checked at `deadline` and, if that
    /// read has not arrived, kept until `cap`.
    fn deadline(&self, now: Instant) -> Instant {
        match self {
            Self::Accepted {
                deadline,
                cap,
                read_after: false,
                ..
            } if now >= *deadline => *cap,
            Self::InFlight { deadline }
            | Self::Accepted { deadline, .. }
            | Self::Unconfirmed { deadline } => *deadline,
        }
    }

    fn view(&self) -> PendingPhase {
        match self {
            Self::InFlight { .. } => PendingPhase::InFlight,
            Self::Accepted { .. } => PendingPhase::Accepted,
            Self::Unconfirmed { .. } => PendingPhase::Unconfirmed,
        }
    }
}

#[derive(Clone, Debug)]
struct Entry<M> {
    op: u64,
    mutation: M,
    phase: Phase,
}

/// A deadline passed before any confirming fetch showed the change.
#[derive(Clone, Debug)]
pub struct Expired<M> {
    pub mutation: M,
    pub phase: PendingPhase,
    /// For an accepted write: when the host performed it.
    pub accepted_at: Option<Stamp>,
    /// For an accepted write: the latest read start seen per domain at
    /// expiry, to compare against `accepted_at`.
    pub observed: Vec<(Domain, Option<Stamp>)>,
}

#[derive(Clone, Debug)]
pub struct Pending<T, M> {
    entries: Vec<Entry<M>>,
    limits: PendingLimits,
    _data: PhantomData<fn(&T)>,
}

impl<T, M: Mutation<T>> Default for Pending<T, M> {
    fn default() -> Self {
        Self::new(PendingLimits::default())
    }
}

impl<T, M: Mutation<T>> Pending<T, M> {
    pub fn new(limits: PendingLimits) -> Self {
        Self {
            entries: Vec::new(),
            limits,
            _data: PhantomData,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn contains(&self, op: u64) -> bool {
        self.entries.iter().any(|entry| entry.op == op)
    }

    pub fn mutation(&self, op: u64) -> Option<&M> {
        self.entries
            .iter()
            .find(|entry| entry.op == op)
            .map(|entry| &entry.mutation)
    }

    /// Refine an operation's display identity without accepting it or renewing
    /// its lifetime. Intermediate host replies must not extend the deadline.
    pub fn refine(&mut self, op: u64, refine: impl FnOnce(&mut M)) -> bool {
        let Some(entry) = self.entries.iter_mut().find(|entry| entry.op == op) else {
            return false;
        };
        refine(&mut entry.mutation);
        true
    }

    pub fn mutations(&self) -> impl Iterator<Item = &M> {
        self.entries.iter().map(|entry| &entry.mutation)
    }

    /// Show `mutation` now. A newer request for the same key replaces an
    /// older one, so only the latest intent is projected.
    pub fn begin(&mut self, op: u64, mutation: M, now: Instant) {
        let key = mutation.key();
        self.entries.retain(|entry| entry.mutation.key() != key);
        let in_flight = mutation
            .deadlines()
            .map_or(self.limits.in_flight, |bounds| bounds.in_flight);
        self.entries.push(Entry {
            op,
            mutation,
            phase: Phase::InFlight {
                deadline: now + in_flight,
            },
        });
    }

    /// Record a write the host performed at `at` (its own clock). Only a fetch
    /// that started at or after `at` can settle the entry.
    pub fn accept(&mut self, op: u64, at: Stamp, now: Instant) -> bool {
        self.accept_with(op, at, now, |_| {})
    }

    /// Like `accept`, letting the reply refine the mutation (e.g. creation
    /// learns the real row key).
    pub fn accept_with(
        &mut self,
        op: u64,
        at: Stamp,
        now: Instant,
        refine: impl FnOnce(&mut M),
    ) -> bool {
        let limits = self.limits;
        let Some(entry) = self.entries.iter_mut().find(|entry| entry.op == op) else {
            return false;
        };
        refine(&mut entry.mutation);
        let (accepted, cap) = entry.mutation.deadlines().map_or_else(
            || limits.accepted(entry.mutation.domains()),
            |bounds| (bounds.accepted, bounds.accepted),
        );
        entry.phase = Phase::Accepted {
            at,
            deadline: now + accepted,
            cap: now + cap,
            read_after: false,
        };
        true
    }

    /// The outcome is unknown. Keep showing it, marked, until the deadline.
    pub fn uncertain(&mut self, op: u64, now: Instant) -> bool {
        let deadline = now + self.limits.unconfirmed;
        let Some(entry) = self.entries.iter_mut().find(|entry| entry.op == op) else {
            return false;
        };
        entry.phase = Phase::Unconfirmed { deadline };
        true
    }

    /// The write failed or turned out to be unnecessary. Stop showing it.
    pub fn remove(&mut self, op: u64) -> Option<M> {
        let index = self.entries.iter().position(|entry| entry.op == op)?;
        Some(self.entries.remove(index).mutation)
    }

    /// Drop entries a confirming fetch shows, and entries past their deadline.
    /// `observed(target, domain)` is the latest fetch-start stamp for that
    /// target on its host: the stamp of the target's own row where the domain
    /// is read row by row, else the domain's. Returns the entries that expired
    /// while confirmed data still disagreed, so the caller can say the change
    /// was not reflected.
    pub fn settle(
        &mut self,
        confirmed: &T,
        observed: impl Fn(&Target, Domain) -> Option<Stamp>,
        now: Instant,
    ) -> Vec<Expired<M>> {
        // The read every confirming row shares: the oldest of theirs.
        let oldest = |mutation: &M, domain: Domain| {
            mutation
                .confirming()
                .iter()
                .map(|target| observed(target, domain))
                .reduce(|a, b| Some(a?.earliest(b?)))
                .flatten()
        };
        let mut expired = Vec::new();
        self.entries.retain_mut(|entry| {
            let shown = entry.mutation.observed(confirmed);
            if let Phase::Accepted { at, read_after, .. } = &mut entry.phase {
                let fresh = entry.mutation.domains().iter().all(|domain| {
                    oldest(&entry.mutation, *domain).and_then(|stamp| stamp.at_or_after(at))
                        == Some(true)
                });
                if fresh && shown {
                    return false;
                }
                *read_after |= fresh;
            }
            if now >= entry.phase.deadline(now) {
                if !shown {
                    let (accepted_at, observed) = match &entry.phase {
                        Phase::Accepted { at, .. } => {
                            let observed = entry
                                .mutation
                                .domains()
                                .iter()
                                .map(|domain| (*domain, oldest(&entry.mutation, *domain)))
                                .collect();
                            (Some(*at), observed)
                        }
                        _ => (None, Vec::new()),
                    };
                    if let Phase::Accepted { read_after, .. } = &entry.phase {
                        tracing::info!(
                            change = %entry.mutation.key(),
                            accepted_at = ?accepted_at,
                            observed = ?observed,
                            confirming_read_finished = read_after,
                            "accepted change expired before a read showed it"
                        );
                    }
                    expired.push(Expired {
                        mutation: entry.mutation.clone(),
                        phase: entry.phase.view(),
                        accepted_at,
                        observed,
                    });
                }
                return false;
            }
            true
        });
        expired
    }

    /// Confirmed data with every pending change applied in request order.
    pub fn project(&self, confirmed: &T) -> T
    where
        T: Clone,
    {
        let mut view = confirmed.clone();
        for entry in &self.entries {
            entry.mutation.apply(&mut view);
        }
        view
    }

    pub fn views(&self) -> Vec<PendingView> {
        self.entries
            .iter()
            .map(|entry| PendingView {
                target: entry.mutation.target(),
                label: entry.mutation.label(),
                phase: entry.phase.view(),
            })
            .collect()
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        let now = Instant::now();
        self.entries
            .iter()
            .map(|entry| entry.phase.deadline(now))
            .min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, PartialEq)]
    struct Set(&'static str, bool);

    impl Mutation<std::collections::BTreeMap<&'static str, bool>> for Set {
        fn key(&self) -> String {
            self.0.into()
        }
        fn target(&self) -> Target {
            Target::row(None, self.0)
        }
        fn label(&self) -> String {
            format!("setting {}", self.0)
        }
        fn domains(&self) -> &'static [Domain] {
            &[Domain::Github]
        }
        fn apply(&self, view: &mut std::collections::BTreeMap<&'static str, bool>) {
            view.insert(self.0, self.1);
        }
        fn observed(&self, confirmed: &std::collections::BTreeMap<&'static str, bool>) -> bool {
            confirmed.get(self.0) == Some(&self.1)
        }
    }

    #[tokio::test(start_paused = true)]
    async fn fetch_started_before_accept_cannot_settle_a_reversal() {
        let mut pending = Pending::default();
        let before = Stamp::now();
        let confirmed = [("pr", false)].into_iter().collect();
        // Arm, accepted; then disarm (same key) before any fetch.
        pending.begin(1, Set("pr", true), Instant::now());
        tokio::time::advance(Duration::from_millis(10)).await;
        pending.accept(1, Stamp::now(), Instant::now());
        pending.begin(2, Set("pr", false), Instant::now());
        tokio::time::advance(Duration::from_millis(10)).await;
        let accepted = Stamp::now();
        pending.accept(2, accepted, Instant::now());
        // A fetch from before either write shows "false": it must not settle.
        assert!(
            pending
                .settle(&confirmed, |_, _| Some(before), Instant::now())
                .is_empty()
        );
        assert_eq!(pending.len(), 1);
        assert_eq!(pending.project(&confirmed).get("pr"), Some(&false));
        // A fetch that started after the disarm and shows it settles it.
        tokio::time::advance(Duration::from_millis(10)).await;
        let after = Stamp::now();
        assert!(
            pending
                .settle(&confirmed, |_, _| Some(after), Instant::now())
                .is_empty()
        );
        assert!(pending.is_empty());
    }

    /// Removes several rows at once, as a cleanup does.
    #[derive(Clone, Debug, PartialEq)]
    struct Drop(&'static [&'static str]);

    impl Mutation<std::collections::BTreeMap<&'static str, bool>> for Drop {
        fn key(&self) -> String {
            "drop".into()
        }
        fn target(&self) -> Target {
            Target::board(None)
        }
        fn label(&self) -> String {
            "dropping".into()
        }
        fn domains(&self) -> &'static [Domain] {
            &[Domain::Git]
        }
        fn apply(&self, _view: &mut std::collections::BTreeMap<&'static str, bool>) {}
        fn observed(&self, confirmed: &std::collections::BTreeMap<&'static str, bool>) -> bool {
            self.0.iter().all(|key| !confirmed.contains_key(key))
        }
        fn confirming(&self) -> Vec<Target> {
            self.0.iter().map(|key| Target::row(None, *key)).collect()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_change_settles_on_reads_of_its_own_rows_and_on_all_of_them() {
        let mut pending = Pending::default();
        let gone = [("kept", true)].into_iter().collect();
        let stale = Stamp::now();
        tokio::time::advance(Duration::from_millis(10)).await;
        pending.begin(1, Drop(&["a", "b"]), Instant::now());
        let accepted = Stamp::now();
        pending.accept(1, accepted, Instant::now());
        tokio::time::advance(Duration::from_millis(10)).await;
        let fresh = Stamp::now();
        let row = |target: &Target| target.key.clone().unwrap_or_default();
        // A newer read of a row the change never touched proves nothing.
        pending.settle(
            &gone,
            |target, _| Some(if row(target) == "kept" { fresh } else { stale }),
            Instant::now(),
        );
        assert_eq!(pending.len(), 1);
        // Nor does a read of only one of its rows.
        pending.settle(
            &gone,
            |target, _| Some(if row(target) == "a" { fresh } else { stale }),
            Instant::now(),
        );
        assert_eq!(pending.len(), 1);
        // A row with no live read at all never confirms.
        pending.settle(
            &gone,
            |target, _| (row(target) == "a").then_some(fresh),
            Instant::now(),
        );
        assert_eq!(pending.len(), 1);
        // Reads of both rows settle it while the rest of the domain is older.
        pending.settle(
            &gone,
            |target, _| Some(if row(target) == "kept" { stale } else { fresh }),
            Instant::now(),
        );
        assert!(pending.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn no_revert_before_observed_failed_or_expired() {
        let mut pending = Pending::default();
        let stale = [("pr", false)].into_iter().collect();
        pending.begin(1, Set("pr", true), Instant::now());
        let accepted = Stamp::now();
        pending.accept(1, accepted, Instant::now());
        for _ in 0..29 {
            tokio::time::advance(Duration::from_secs(1)).await;
            // Fresh fetches that still show the old value keep the projection.
            assert!(
                pending
                    .settle(&stale, |_, _| Some(Stamp::now()), Instant::now())
                    .is_empty()
            );
            assert_eq!(pending.project(&stale).get("pr"), Some(&true));
        }
        tokio::time::advance(Duration::from_secs(1)).await;
        let expired = pending.settle(&stale, |_, _| Some(Stamp::now()), Instant::now());
        assert_eq!(expired.len(), 1);
        assert!(pending.is_empty());
        // A failure stops showing it at once.
        pending.begin(2, Set("pr", true), Instant::now());
        assert_eq!(pending.remove(2), Some(Set("pr", true)));
        assert_eq!(pending.project(&stale).get("pr"), Some(&false));
    }

    #[tokio::test(start_paused = true)]
    async fn unconfirmed_and_in_flight_never_settle_by_observation() {
        let mut pending = Pending::default();
        let shown = [("pr", true)].into_iter().collect();
        pending.begin(1, Set("pr", true), Instant::now());
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(
            pending
                .settle(&shown, |_, _| Some(Stamp::now()), Instant::now())
                .is_empty()
        );
        assert_eq!(pending.len(), 1, "in flight waits for the reply");
        pending.uncertain(1, Instant::now());
        tokio::time::advance(Duration::from_secs(1)).await;
        pending.settle(&shown, |_, _| Some(Stamp::now()), Instant::now());
        assert_eq!(pending.views()[0].phase, PendingPhase::Unconfirmed);
        tokio::time::advance(Duration::from_secs(30)).await;
        // Expiry while the data agrees is silent.
        assert!(
            pending
                .settle(&shown, |_, _| None, Instant::now())
                .is_empty()
        );
        assert!(pending.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn refining_identity_preserves_phase_and_original_deadline() {
        let mut pending = Pending::default();
        pending.begin(1, Set("provisional", true), Instant::now());
        let deadline = pending.next_deadline();
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(pending.refine(1, |change| change.0 = "canonical"));
        assert_eq!(pending.next_deadline(), deadline);
        assert_eq!(pending.views()[0].phase, PendingPhase::InFlight);
        assert_eq!(pending.mutation(1), Some(&Set("canonical", true)));
        pending.accept(1, Stamp::now(), Instant::now());
        let deadline = pending.next_deadline();
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(pending.refine(1, |change| change.1 = false));
        assert_eq!(pending.next_deadline(), deadline);
        assert_eq!(pending.views()[0].phase, PendingPhase::Accepted);
        assert!(!pending.refine(2, |_| panic!("missing operations must not run refinement")));
    }

    /// The rate floor plus a slow reply can push the first read that started
    /// after the write past the accepted deadline. That read still settles
    /// the change instead of the deadline reporting it as not shown.
    #[tokio::test(start_paused = true)]
    async fn a_slow_confirming_read_settles_instead_of_expiring() {
        let mut pending = Pending::default();
        let stale = [("pr", false)].into_iter().collect();
        let shown = [("pr", true)].into_iter().collect();
        let before = Stamp::now();
        tokio::time::advance(Duration::from_millis(10)).await;
        pending.begin(1, Set("pr", true), Instant::now());
        let accepted = Stamp::now();
        pending.accept(1, accepted, Instant::now());
        // The floor defers the confirming read 10s; it then takes 25s.
        tokio::time::advance(Duration::from_secs(10)).await;
        let confirming = Stamp::now();
        tokio::time::advance(Duration::from_secs(20)).await;
        // Soft deadline: only the pre-write read has finished. Keep waiting.
        assert!(
            pending
                .settle(&stale, |_, _| Some(before), Instant::now())
                .is_empty()
        );
        assert_eq!(pending.len(), 1);
        assert!(pending.next_deadline().unwrap() > Instant::now());
        tokio::time::advance(Duration::from_secs(5)).await;
        assert!(
            pending
                .settle(&shown, |_, _| Some(confirming), Instant::now())
                .is_empty()
        );
        assert!(pending.is_empty(), "the slow read settled it");
    }

    #[tokio::test(start_paused = true)]
    async fn waiting_for_a_confirming_read_is_still_bounded() {
        let mut pending = Pending::default();
        let stale = [("pr", false)].into_iter().collect();
        let before = Stamp::now();
        tokio::time::advance(Duration::from_millis(10)).await;
        pending.begin(1, Set("pr", true), Instant::now());
        pending.accept(1, Stamp::now(), Instant::now());
        tokio::time::advance(Duration::from_secs(31)).await;
        assert!(
            pending
                .settle(&stale, |_, _| Some(before), Instant::now())
                .is_empty()
        );
        // A finished read after the write that disagrees expires it at once.
        let mut disagreed = pending.clone();
        let expired = disagreed.settle(&stale, |_, _| Some(Stamp::now()), Instant::now());
        assert_eq!(expired.len(), 1);
        assert!(expired[0].accepted_at.is_some());
        assert_eq!(expired[0].observed.len(), 1);
        // Without one, the cap still ends it.
        tokio::time::advance(Duration::from_secs(89)).await;
        let expired = pending.settle(&stale, |_, _| Some(before), Instant::now());
        assert_eq!(expired.len(), 1);
        assert!(pending.is_empty());
    }

    /// A local read is quick, but one held up past the accepted deadline (a
    /// loaded machine, a slow hop to another host) says nothing about the
    /// change. Only a read that could have shown it, or the cap, ends it.
    #[tokio::test(start_paused = true)]
    async fn a_local_change_outlives_its_deadline_until_a_read_could_have_shown_it() {
        let mut pending = Pending::default();
        let rows: std::collections::BTreeMap<_, _> = [("a", true)].into_iter().collect();
        let gone = std::collections::BTreeMap::new();
        let before = Stamp::now();
        tokio::time::advance(Duration::from_millis(10)).await;
        pending.begin(1, Drop(&["a"]), Instant::now());
        pending.accept(1, Stamp::now(), Instant::now());
        tokio::time::advance(Duration::from_secs(20)).await;
        assert!(
            pending
                .settle(&rows, |_, _| Some(before), Instant::now())
                .is_empty(),
            "no read that started after the write has finished yet"
        );
        assert_eq!(pending.len(), 1);
        let mut shown = pending.clone();
        assert!(
            shown
                .settle(&gone, |_, _| Some(Stamp::now()), Instant::now())
                .is_empty()
        );
        assert!(shown.is_empty(), "the late read settled it");
        // A read after the write that disagrees still ends it at once.
        let mut disagreed = pending.clone();
        assert_eq!(
            disagreed
                .settle(&rows, |_, _| Some(Stamp::now()), Instant::now())
                .len(),
            1
        );
        tokio::time::advance(Duration::from_secs(41)).await;
        assert_eq!(
            pending
                .settle(&rows, |_, _| Some(before), Instant::now())
                .len(),
            1,
            "the cap still bounds it"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn foreign_or_unordered_stamps_never_settle() {
        let mut pending = Pending::default();
        let shown = [("pr", true)].into_iter().collect();
        pending.begin(1, Set("pr", true), Instant::now());
        pending.accept(1, Stamp::now(), Instant::now());
        tokio::time::advance(Duration::from_secs(1)).await;
        pending.settle(&shown, |_, _| Some(Stamp::unordered()), Instant::now());
        assert_eq!(pending.len(), 1);
        pending.settle(&shown, |_, _| None, Instant::now());
        assert_eq!(pending.len(), 1);
    }
}
