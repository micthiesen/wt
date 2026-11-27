//! A leaf whose domain is a set of rows that can each be read alone, such as
//! one Git status per checkout. It keeps the last read of every row and reads
//! again only what was asked for: a named row, the listing of keys, or all of
//! it. Every row carries the start of its own read (`KeyedStamps`), so a
//! change to one row can settle on a read of that row while the domain stamp
//! stays the oldest row's.
//!
//! Requests are served by class. A user-caused read (`Request::Refresh` or
//! `Direct` for named keys, `Direct` for the whole domain) goes before an
//! event-caused one (`Recheck` for named keys), which goes before a sweep
//! (any other whole request). One read lane is kept for user-caused reads, so
//! a sweep in flight never makes one wait for more than the listing. A sweep
//! also waits for the reader's turn (`KeyedReader::sweep`), which is how
//! whole-fleet reads are rationed across processes; nothing else does.
//!
//! Nothing a sweep has read is thrown away when a newer request arrives: each
//! row's read stands on its own stamp. The one read that is cancelled is a
//! background read of a row the user has just asked about, because it started
//! before the request and so cannot answer it. The read that replaces a
//! cancelled one always runs to completion, and event-caused requests never
//! cancel anything, so a row under constant invalidation still gets read and
//! published.
use std::collections::BTreeMap;
use std::future::Future;
use std::num::NonZeroUsize;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures_util::stream::FuturesUnordered;
use futures_util::{FutureExt, StreamExt};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::snapshot::KeyedStamps;
use crate::source::{Demand, LeafPublisher, REFRESHING_AFTER, leaf_channel};
use crate::{LeafHandle, Request, SourceSpec, SourceState, Stamp, TaskScope};

/// How a keyed leaf reads its domain.
pub trait KeyedReader: Send + Sync + 'static {
    /// What the listing says about one key.
    type Entry: Clone + PartialEq + Send + Sync + 'static;
    /// What a read of one key returns. A row-level failure is a value, so one
    /// broken row never hides the others.
    type Value: Send + Sync + 'static;
    /// The published row.
    type Row: PartialEq + Send + Sync + 'static;
    /// Held while a sweep reads every row; see `sweep`.
    type Sweep: Send + 'static;

    /// List the keys in publication order. A failure keeps the last data and
    /// shows the lane as failed until a later listing succeeds. `urgent`
    /// marks a user-caused read here and in `read`, for a reader whose
    /// resources are shared with background work.
    fn index(
        &self,
        urgent: bool,
        cancel: CancellationToken,
    ) -> impl Future<Output = Result<Vec<(String, Self::Entry)>, String>> + Send;

    /// Read one listed key.
    fn read(
        &self,
        entry: Self::Entry,
        urgent: bool,
        cancel: CancellationToken,
    ) -> impl Future<Output = Self::Value> + Send;

    /// Wait for this leaf's turn to read every row, where such sweeps are
    /// rationed (across processes, say). A sweep's reads start once this
    /// resolves, and what it returns is dropped when the last of them ends.
    /// Named reads and user-caused ones never wait for it.
    fn sweep(&self, cancel: CancellationToken) -> impl Future<Output = Self::Sweep> + Send;

    /// Whether a value read for `read` still describes `listed`. A key that
    /// now names something else (another path) is read again from nothing.
    fn same_subject(read: &Self::Entry, listed: &Self::Entry) -> bool;

    /// The row for a value and the entry it was read against.
    fn row(entry: &Self::Entry, value: &Self::Value) -> Self::Row;

    /// The key a row's stamp is published under and may be asked for by.
    /// Rows that share one are confirmed together: the stamp is the oldest
    /// of theirs and a request for it reads them all. For a reader whose
    /// listing can hold two rows that claim the same outward key.
    fn stamp_key(key: &str) -> &str {
        key
    }
}

/// Bounds of a keyed leaf beyond its `RefreshPolicy`, whose debounce
/// coalesces event-caused and sweep requests and whose minimum interval
/// spaces listings. Neither delays a user-caused read.
#[derive(Clone, Copy, Debug)]
pub struct KeyedPolicy {
    /// Rows read at once. With more than one, one lane is kept for
    /// user-caused reads.
    pub concurrency: NonZeroUsize,
    /// Spacing between event-caused or sweep reads of one row, measured from
    /// the preceding read's start.
    pub key_interval: Duration,
}

/// A sweep read that has waited this long is served like an event-caused
/// one, oldest first. Without it, a few rows that change continuously would
/// take every background lane for good, and the rows only the sweep reads
/// (with the turn it holds across processes) would never be read.
const SWEEP_PATIENCE: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Class {
    Sweep,
    Event,
    User,
}

/// An unserved request: any read that starts after it was made serves it.
#[derive(Clone, Copy, Debug)]
struct Want {
    class: Class,
    /// When it was first asked for; the debounce counts from here, so later
    /// requests cannot postpone the read.
    first: Instant,
}

fn raise(want: &mut Option<Want>, class: Class, now: Instant) {
    match want {
        Some(want) => want.class = want.class.max(class),
        None => *want = Some(Want { class, first: now }),
    }
}

struct Flight<E> {
    id: u64,
    started: Instant,
    /// The requests this read serves.
    want: Want,
    entry: E,
    /// Start of the listing that showed `entry`, as of the read's start.
    listed: Instant,
    cancel: CancellationToken,
    /// A user-caused request arrived after this read started.
    superseded: bool,
    /// Started in place of a superseded read; never superseded itself.
    replacement: bool,
}

struct RowState<R: KeyedReader> {
    entry: R::Entry,
    /// Start of the listing that last showed `entry`.
    listed: Instant,
    value: Option<Read<R>>,
    last_start: Option<Instant>,
    want: Option<Want>,
    flight: Option<Flight<R::Entry>>,
    replace_next: bool,
}

/// The last value read for a row. It is published with the entry it was read
/// against, never joined to a newer listing's entry: what a row shows was
/// true at one moment.
struct Read<R: KeyedReader> {
    value: R::Value,
    started: Instant,
    entry: R::Entry,
    /// Start of the latest listing that showed `entry`.
    listed: Instant,
}

enum Done<R: KeyedReader> {
    Index {
        started: Instant,
        want: Want,
        result: Result<Vec<(String, R::Entry)>, String>,
    },
    Read {
        key: String,
        id: u64,
        value: Result<R::Value, String>,
    },
    /// `None` when the reader's `sweep` panicked: the sweep then runs
    /// without a turn rather than never.
    Turn(Option<R::Sweep>),
}

/// This leaf's turn to sweep.
enum Turn<S> {
    None,
    Asked,
    Held(Option<S>),
}

type Flights<R> = FuturesUnordered<Pin<Box<dyn Future<Output = Done<R>> + Send>>>;

struct Engine<R: KeyedReader> {
    reader: Arc<R>,
    token: CancellationToken,
    debounce: Duration,
    index_interval: Duration,
    key_interval: Duration,
    lanes: usize,
    rows: BTreeMap<String, RowState<R>>,
    order: Vec<String>,
    index_want: Option<Want>,
    index_flight: Option<Want>,
    index_last_start: Option<Instant>,
    /// Start of the last listing that succeeded.
    index_at: Option<Instant>,
    failed: Option<Arc<str>>,
    reading: usize,
    next_id: u64,
    /// Work finished since the last publish: the strongest class, and the
    /// earliest-asked request among the work of that class.
    unpublished: Option<Want>,
    published: Option<Arc<Vec<R::Row>>>,
    turn: Turn<R::Sweep>,
    busy_since: Option<Instant>,
    announced: bool,
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    panic_message_ref(&payload).to_owned()
}

fn panic_message_ref(payload: &Box<dyn std::any::Any + Send>) -> &str {
    payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .unwrap_or("non-string panic payload")
}

impl<R: KeyedReader> Engine<R> {
    fn absorb(&mut self, demand: Demand, now: Instant) {
        if let Some(request) = demand.whole {
            let class = if request == Request::Direct {
                Class::User
            } else {
                Class::Sweep
            };
            raise(&mut self.index_want, class, now);
            for row in self.rows.values_mut() {
                Self::want_row(row, class, now);
            }
        }
        let keyed = |request| match request {
            Request::Recheck => Class::Event,
            Request::Refresh | Request::Direct => Class::User,
        };
        if let Some(request) = demand.index {
            raise(&mut self.index_want, keyed(request), now);
        }
        for (key, request) in demand.keys {
            // A key that is not listed yet is read when a listing shows it.
            for (_, row) in self
                .rows
                .iter_mut()
                .filter(|(listed, _)| **listed == key || R::stamp_key(listed) == key)
            {
                Self::want_row(row, keyed(request), now);
            }
        }
    }

    fn want_row(row: &mut RowState<R>, class: Class, now: Instant) {
        if let Some(flight) = &mut row.flight {
            if flight.started >= now {
                return;
            }
            if class == Class::User && !flight.superseded && !flight.replacement {
                flight.superseded = true;
                flight.cancel.cancel();
            }
        }
        raise(&mut row.want, class, now);
    }

    fn due(&self, want: Want, last_start: Option<Instant>, floor: Duration) -> Instant {
        if want.class == Class::User {
            return want.first;
        }
        (want.first + self.debounce).max(last_start.map_or(want.first, |at| at + floor))
    }

    /// Start every read that is due and has a lane. Returns when the next
    /// waiting one becomes due.
    fn pump(&mut self, flights: &mut Flights<R>, now: Instant) -> Option<Instant> {
        let mut wake = None::<Instant>;
        let mut later = |at: Instant| wake = Some(wake.map_or(at, |wake: Instant| wake.min(at)));
        if self.index_flight.is_none()
            && let Some(want) = self.index_want
        {
            let due = self.due(want, self.index_last_start, self.index_interval);
            if due <= now {
                self.index_want = None;
                self.index_flight = Some(want);
                self.index_last_start = Some(now);
                let reader = self.reader.clone();
                let cancel = self.token.child_token();
                let urgent = want.class == Class::User;
                flights.push(Box::pin(async move {
                    let result = AssertUnwindSafe(reader.index(urgent, cancel))
                        .catch_unwind()
                        .await
                        .unwrap_or_else(|payload| {
                            let message = panic_message(payload);
                            tracing::error!(message, "keyed source listing panicked");
                            Err(format!("background source panicked: {message}"))
                        });
                    Done::Index {
                        started: now,
                        want,
                        result,
                    }
                }));
            } else {
                later(due);
            }
        }
        let mut ready = Vec::new();
        for (key, row) in &self.rows {
            if row.flight.is_some() {
                continue;
            }
            let Some(want) = row.want else { continue };
            let due = self.due(want, row.last_start, self.key_interval);
            if due <= now {
                ready.push((want, key.clone()));
            } else {
                later(due);
            }
        }
        let rank = |want: &Want| match want.class {
            Class::Sweep if now.duration_since(want.first) >= SWEEP_PATIENCE => Class::Event,
            class => class,
        };
        ready.sort_by(|(a, a_key), (b, b_key)| {
            (rank(b), a.first, a_key).cmp(&(rank(a), b.first, b_key))
        });
        let sweeping = |row: &RowState<R>| {
            row.want.is_some_and(|want| want.class == Class::Sweep)
                || row
                    .flight
                    .as_ref()
                    .is_some_and(|flight| flight.want.class == Class::Sweep)
        };
        if matches!(self.turn, Turn::Held(_)) && !self.rows.values().any(sweeping) {
            self.turn = Turn::None;
        }
        let background = self.lanes.saturating_sub(1).max(1);
        for (want, key) in ready {
            let limit = if want.class == Class::User {
                self.lanes
            } else {
                background
            };
            if self.reading >= limit {
                // A finished read pumps again.
                continue;
            }
            if want.class == Class::Sweep && !matches!(self.turn, Turn::Held(_)) {
                if matches!(self.turn, Turn::None) {
                    self.turn = Turn::Asked;
                    let reader = self.reader.clone();
                    let cancel = self.token.child_token();
                    flights.push(Box::pin(async move {
                        let turn = AssertUnwindSafe(reader.sweep(cancel)).catch_unwind().await;
                        if let Err(payload) = &turn {
                            let message = panic_message_ref(payload);
                            tracing::error!(message, "keyed source sweep turn panicked");
                        }
                        Done::Turn(turn.ok())
                    }));
                }
                continue;
            }
            let Some(row) = self.rows.get_mut(&key) else {
                continue;
            };
            self.next_id += 1;
            let id = self.next_id;
            let cancel = self.token.child_token();
            row.want = None;
            row.last_start = Some(now);
            row.flight = Some(Flight {
                id,
                started: now,
                want,
                entry: row.entry.clone(),
                listed: row.listed,
                cancel: cancel.clone(),
                superseded: false,
                replacement: std::mem::take(&mut row.replace_next),
            });
            self.reading += 1;
            let reader = self.reader.clone();
            let entry = row.entry.clone();
            let urgent = want.class == Class::User;
            flights.push(Box::pin(async move {
                let value = AssertUnwindSafe(reader.read(entry, urgent, cancel))
                    .catch_unwind()
                    .await
                    .map_err(panic_message);
                Done::Read { key, id, value }
            }));
        }
        wake
    }

    fn finished(&mut self, done: Done<R>, publisher: &LeafPublisher<Vec<R::Row>>) {
        match done {
            Done::Turn(turn) => self.turn = Turn::Held(turn),
            Done::Index {
                started,
                want,
                result,
            } => {
                self.index_flight = None;
                match result {
                    Ok(listing) => self.listed(listing, started, want),
                    Err(error) => {
                        // The requests this listing served are dropped, as a
                        // failed whole fetch drops its own: the next request
                        // or backstop tries again.
                        let error: Arc<str> = error.into();
                        self.failed = Some(error.clone());
                        publisher.set_state(SourceState::Failed(error));
                    }
                }
            }
            Done::Read { key, id, value } => {
                self.reading -= 1;
                let Some(row) = self.rows.get_mut(&key) else {
                    return;
                };
                let Some(flight) = row.flight.take_if(|flight| flight.id == id) else {
                    return;
                };
                if flight.superseded {
                    row.replace_next = true;
                    raise(&mut row.want, flight.want.class, flight.want.first);
                    return;
                }
                if !R::same_subject(&flight.entry, &row.entry) {
                    // The key names something else now and has no value for
                    // it, which holds back every publish: its read goes
                    // first, as a newly listed row's does.
                    raise(&mut row.want, Class::User, flight.want.first);
                    if let Some(want) = &mut row.want {
                        want.class = Class::User;
                    }
                    return;
                }
                match value {
                    Ok(value) => {
                        let listed = if flight.entry == row.entry {
                            row.listed
                        } else {
                            flight.listed
                        };
                        row.value = Some(Read {
                            value,
                            started: flight.started,
                            entry: flight.entry,
                            listed,
                        });
                        Self::finish(&mut self.unpublished, flight.want);
                    }
                    Err(message) => {
                        tracing::error!(message, key, "keyed source read panicked");
                        let error: Arc<str> =
                            format!("background source panicked: {message}").into();
                        self.failed = Some(error.clone());
                        publisher.set_state(SourceState::Failed(error));
                    }
                }
            }
        }
    }

    fn listed(&mut self, listing: Vec<(String, R::Entry)>, started: Instant, want: Want) {
        let mut previous = std::mem::take(&mut self.rows);
        let mut order = Vec::with_capacity(listing.len());
        for (key, entry) in listing {
            if self.rows.contains_key(&key) {
                continue;
            }
            let row = match previous.remove(&key) {
                Some(mut row) => {
                    if !R::same_subject(&row.entry, &entry) {
                        row.value = None;
                    }
                    let reading = row.flight.as_ref().is_some_and(|flight| {
                        !flight.superseded && flight.entry == entry && flight.started >= started
                    });
                    let read = row
                        .value
                        .as_ref()
                        .is_some_and(|read| read.entry == entry && read.started >= started);
                    if let Some(read) = &mut row.value
                        && read.entry == entry
                    {
                        read.listed = started;
                    }
                    // The listing is a change detector of its own: an entry
                    // that moved means the row did, even if no event said so.
                    if row.entry != entry && !reading && !read {
                        raise(&mut row.want, want.class, want.first);
                    }
                    row.entry = entry;
                    row.listed = started;
                    row
                }
                None => RowState {
                    entry,
                    listed: started,
                    value: None,
                    last_start: None,
                    want: None,
                    flight: None,
                    replace_next: false,
                },
            };
            order.push(key.clone());
            self.rows.insert(key, row);
        }
        for row in self.rows.values_mut() {
            // A row without a value holds back every publish, so its first
            // read goes before anything else.
            if row.value.is_none() && row.flight.is_none() {
                raise(&mut row.want, Class::User, started);
            }
        }
        for row in previous.values() {
            if let Some(flight) = &row.flight {
                flight.cancel.cancel();
            }
        }
        self.order = order;
        self.index_at = Some(started);
        self.failed = None;
        Self::finish(&mut self.unpublished, want);
    }

    fn finish(unpublished: &mut Option<Want>, want: Want) {
        *unpublished = Some(match *unpublished {
            Some(finished) if finished.class > want.class => finished,
            Some(finished) if finished.class == want.class => Want {
                class: want.class,
                first: finished.first.min(want.first),
            },
            _ => want,
        });
    }

    /// Requests still wanted or in flight.
    fn outstanding(&self) -> impl Iterator<Item = Want> + '_ {
        self.rows
            .values()
            .flat_map(|row| {
                row.want
                    .into_iter()
                    .chain(row.flight.as_ref().map(|flight| flight.want))
            })
            .chain(self.index_want)
            .chain(self.index_flight)
    }

    /// Publish once the batch of the most urgent finished work is done:
    /// nothing at least as urgent that was asked for no later is still
    /// waiting. So a named read publishes without waiting for a sweep, a
    /// sweep publishes once at its end rather than once per row, and requests
    /// that keep arriving never hold back what has already been read.
    fn publish(&mut self, publisher: &LeafPublisher<Vec<R::Row>>) {
        let Some(finished) = self.unpublished else {
            return;
        };
        let Some(index_at) = self.index_at else {
            return;
        };
        if self.failed.is_some()
            || self
                .outstanding()
                .any(|want| want.class >= finished.class && want.first <= finished.first)
        {
            return;
        }
        let mut rows = Vec::with_capacity(self.order.len());
        let mut stamps = BTreeMap::new();
        for key in &self.order {
            let Some(row) = self.rows.get(key) else {
                return;
            };
            let Some(read) = &row.value else {
                return;
            };
            rows.push(R::row(&read.entry, &read.value));
            let stamp = Stamp::at(read.started.min(read.listed));
            stamps
                .entry(R::stamp_key(key).to_owned())
                .and_modify(|shared: &mut Stamp| *shared = shared.earliest(stamp))
                .or_insert(stamp);
        }
        // Reusing the published allocation keeps the cache writer and
        // pointer comparisons downstream from seeing an identical refetch
        // as new data.
        let data = match &self.published {
            Some(published) if **published == rows => published.clone(),
            _ => Arc::new(rows),
        };
        self.published = Some(data.clone());
        self.unpublished = None;
        let state = if self.announced && self.outstanding().any(|want| want.class != Class::Event) {
            SourceState::Refreshing
        } else {
            SourceState::Ready
        };
        publisher.publish_live_keyed(data, KeyedStamps::new(Stamp::at(index_at), stamps), state);
    }

    /// A sweep or a user-caused read that takes a while reads as refreshing;
    /// the lane returns to its resting state once nothing is left. An
    /// event-caused recheck of a row or two does not: it would redraw every
    /// pane twice for a read that usually changes nothing. Returns when to
    /// look again.
    fn announce(
        &mut self,
        publisher: &LeafPublisher<Vec<R::Row>>,
        now: Instant,
    ) -> Option<Instant> {
        if !self.outstanding().any(|want| want.class != Class::Event) {
            self.busy_since = None;
            if std::mem::take(&mut self.announced) {
                match &self.failed {
                    Some(error) => publisher.set_state(SourceState::Failed(error.clone())),
                    None if self.published.is_some() => publisher.set_state(SourceState::Ready),
                    None => {}
                }
            }
            return None;
        }
        let since = *self.busy_since.get_or_insert(now);
        if self.announced {
            return None;
        }
        if now >= since + REFRESHING_AFTER {
            self.announced = true;
            publisher.set_state(SourceState::Refreshing);
            return None;
        }
        Some(since + REFRESHING_AFTER)
    }
}

/// Start a leaf that lists its keys and reads each row on its own. See the
/// module documentation for how requests are ordered and stamped.
pub fn start_keyed_source<R: KeyedReader>(
    scope: &TaskScope,
    spec: SourceSpec<Vec<R::Row>>,
    policy: KeyedPolicy,
    reader: R,
) -> LeafHandle<Vec<R::Row>> {
    let refresh = spec.policy;
    let (handle, mut publisher) = leaf_channel(scope, spec);
    let token = scope.token();
    scope.spawn(async move {
        let mut engine = Engine {
            reader: Arc::new(reader),
            token: token.child_token(),
            debounce: refresh.debounce,
            index_interval: refresh.minimum_interval,
            key_interval: policy.key_interval,
            lanes: policy.concurrency.get(),
            rows: BTreeMap::new(),
            order: Vec::new(),
            index_want: None,
            index_flight: None,
            index_last_start: None,
            index_at: None,
            failed: None,
            reading: 0,
            next_id: 0,
            unpublished: None,
            published: None,
            turn: Turn::None,
            busy_since: None,
            announced: false,
        };
        let mut flights: Flights<R> = FuturesUnordered::new();
        loop {
            let now = Instant::now();
            engine.publish(&publisher);
            let due = engine.pump(&mut flights, now);
            let announce = engine.announce(&publisher, now);
            let wake = match (due, announce) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            tokio::select! {
                biased;
                _ = token.cancelled() => break,
                request = publisher.requested() => {
                    if request.is_none() {
                        break;
                    }
                    let demand = publisher.take_demand();
                    engine.absorb(demand, Instant::now());
                }
                Some(done) = flights.next(), if !flights.is_empty() => {
                    engine.finished(done, &publisher);
                }
                _ = tokio::time::sleep_until(wake.unwrap_or(now)), if wake.is_some() => {}
            }
        }
        // Reads observe cancellation through their child tokens; let their
        // cleanup finish before the task ends.
        engine.token.cancel();
        while flights.next().await.is_some() {}
    });
    handle
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Persistence, RefreshPolicy};
    use std::sync::Mutex;
    use wt_core::Domain;

    #[derive(Default)]
    struct State {
        /// Key and the path-like identity the listing gives it.
        listing: Vec<(String, u32)>,
        listing_error: Option<String>,
        /// What else the listing says about a key, shown in its row as
        /// thousands so a test can tell which entry a row was built from.
        labels: BTreeMap<String, u32>,
        values: BTreeMap<String, u32>,
        reads: Vec<String>,
        /// Reads made for a user-caused request.
        urgent: Vec<String>,
        urgent_listings: usize,
        cancelled: Vec<String>,
        listings: usize,
        /// Sweep turns taken, and how many are held now.
        turns: usize,
        turns_held: usize,
    }

    #[derive(Clone)]
    struct Fake {
        state: Arc<Mutex<State>>,
        read_time: Duration,
        turn_wait: Duration,
    }

    struct TurnGuard(Arc<Mutex<State>>);

    impl Drop for TurnGuard {
        fn drop(&mut self) {
            self.0.lock().unwrap().turns_held -= 1;
        }
    }

    impl Fake {
        fn new(keys: &[&str]) -> Self {
            let state = State {
                listing: keys.iter().map(|key| ((*key).to_owned(), 0)).collect(),
                ..State::default()
            };
            Self {
                state: Arc::new(Mutex::new(state)),
                read_time: Duration::from_millis(100),
                turn_wait: Duration::ZERO,
            }
        }

        fn with<T>(&self, f: impl FnOnce(&mut State) -> T) -> T {
            f(&mut self.state.lock().unwrap())
        }

        fn reads(&self) -> Vec<String> {
            self.with(|state| state.reads.clone())
        }
    }

    impl KeyedReader for Fake {
        type Entry = (String, u32, u32);
        type Value = u32;
        type Row = (String, u32);
        type Sweep = TurnGuard;

        fn stamp_key(key: &str) -> &str {
            key.split('#').next().unwrap_or(key)
        }

        async fn sweep(&self, _cancel: CancellationToken) -> TurnGuard {
            tokio::time::sleep(self.turn_wait).await;
            self.with(|state| {
                state.turns += 1;
                state.turns_held += 1;
            });
            TurnGuard(self.state.clone())
        }

        async fn index(
            &self,
            urgent: bool,
            _cancel: CancellationToken,
        ) -> Result<Vec<(String, Self::Entry)>, String> {
            self.with(|state| state.urgent_listings += usize::from(urgent));
            let result = self.with(|state| {
                state.listings += 1;
                match &state.listing_error {
                    Some(error) => Err(error.clone()),
                    None => Ok(state
                        .listing
                        .iter()
                        .map(|(key, path)| {
                            let label = state.labels.get(key).copied().unwrap_or_default();
                            (key.clone(), (key.clone(), *path, label))
                        })
                        .collect()),
                }
            });
            tokio::time::sleep(Duration::from_millis(10)).await;
            result
        }

        async fn read(&self, entry: Self::Entry, urgent: bool, cancel: CancellationToken) -> u32 {
            let value = self.with(|state| {
                if urgent {
                    state.urgent.push(entry.0.clone());
                }
                state.reads.push(entry.0.clone());
                state.values.get(&entry.0).copied().unwrap_or_default()
            });
            tokio::select! {
                _ = tokio::time::sleep(self.read_time) => value,
                _ = cancel.cancelled() => {
                    self.with(|state| state.cancelled.push(entry.0.clone()));
                    u32::MAX
                }
            }
        }

        fn same_subject(read: &Self::Entry, listed: &Self::Entry) -> bool {
            read.1 == listed.1
        }

        fn row(entry: &Self::Entry, value: &u32) -> Self::Row {
            (entry.0.clone(), entry.2 * 1000 + *value)
        }
    }

    fn start(scope: &TaskScope, fake: &Fake) -> LeafHandle<Vec<(String, u32)>> {
        start_keyed_source(
            scope,
            SourceSpec {
                name: "rows",
                domain: Domain::Git,
                persistence: Persistence::Never("test fixture"),
                policy: RefreshPolicy::default(),
            },
            KeyedPolicy {
                concurrency: NonZeroUsize::new(4).unwrap(),
                key_interval: Duration::ZERO,
            },
            fake.clone(),
        )
    }

    async fn loaded(source: &LeafHandle<Vec<(String, u32)>>) {
        source.refresh();
        source
            .subscribe()
            .wait_for(|snapshot| *snapshot.state() == SourceState::Ready)
            .await
            .unwrap();
    }

    fn value(source: &LeafHandle<Vec<(String, u32)>>, key: &str) -> Option<u32> {
        source
            .snapshot()
            .data()
            .and_then(|rows| rows.iter().find(|row| row.0 == key).map(|row| row.1))
    }

    fn value_in(snapshot: &crate::SourceSnapshot<Vec<(String, u32)>>, key: &str) -> Option<u32> {
        snapshot
            .data()
            .and_then(|rows| rows.iter().find(|row| row.0 == key).map(|row| row.1))
    }

    fn stamp(source: &LeafHandle<Vec<(String, u32)>>, key: &str) -> Stamp {
        source.snapshot().domains()[&Domain::Git].observed_for(key)
    }

    #[tokio::test(start_paused = true)]
    async fn a_named_row_is_read_alone_and_carries_its_own_stamp() {
        let scope = TaskScope::new();
        let fake = Fake::new(&["a", "b", "c", "d", "e"]);
        let source = start(&scope, &fake);
        assert!(source.confirmed().is_none());
        let mut updates = source.subscribe();
        source.refresh();
        // Nothing is published before every listed row has been read.
        let first = updates
            .wait_for(|snapshot| snapshot.data().is_some())
            .await
            .unwrap()
            .clone();
        assert_eq!(first.data().unwrap().len(), 5);
        assert_eq!(fake.reads().len(), 5);

        tokio::time::sleep(Duration::from_secs(1)).await;
        let accepted = Stamp::now();
        fake.with(|state| state.values.insert("c".into(), 7));
        source.request_keys(Request::Direct, ["c"]);
        updates
            .wait_for(|snapshot| {
                snapshot
                    .data()
                    .is_some_and(|rows| rows.contains(&("c".into(), 7)))
            })
            .await
            .unwrap();
        assert_eq!(fake.reads().len(), 6, "only the named row is read again");
        assert_eq!(fake.reads().last().map(String::as_str), Some("c"));
        assert_eq!(fake.with(|state| state.listings), 2);
        assert_eq!(stamp(&source, "c").at_or_after(&accepted), Some(true));
        assert_eq!(stamp(&source, "a").at_or_after(&accepted), Some(false));
        // The domain, and so anything confirmed from it, is only as fresh as
        // its oldest row.
        let meta = source.snapshot().domains()[&Domain::Git].clone();
        assert_eq!(meta.observed().at_or_after(&accepted), Some(false));
        assert_eq!(meta.observed(), stamp(&source, "a"));
        let confirmed = source.confirmed().unwrap();
        assert_eq!(confirmed.observed(), meta.observed());
        assert_eq!(confirmed.len(), 5);
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_user_read_overtakes_a_sweep_and_publishes_before_it_ends() {
        let scope = TaskScope::new();
        let keys: Vec<String> = (0..12).map(|index| format!("k{index:02}")).collect();
        let fake = Fake::new(&keys.iter().map(String::as_str).collect::<Vec<_>>());
        let source = start(&scope, &fake);
        loaded(&source).await;
        tokio::time::sleep(Duration::from_secs(1)).await;

        let swept = Stamp::now();
        let before = fake.reads().len();
        source.refresh();
        tokio::time::sleep(Duration::from_millis(50)).await;
        // The sweep holds every background lane; one is left for this.
        assert_eq!(fake.reads().len() - before, 3);
        fake.with(|state| state.values.insert("k11".into(), 9));
        source.request_keys(Request::Direct, ["k11"]);
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert_eq!(fake.reads().last().map(String::as_str), Some("k11"));
        let mut updates = source.subscribe();
        updates
            .wait_for(|snapshot| {
                snapshot
                    .data()
                    .is_some_and(|rows| rows.contains(&("k11".into(), 9)))
            })
            .await
            .unwrap();
        assert_eq!(stamp(&source, "k11").at_or_after(&swept), Some(true));
        assert_eq!(
            stamp(&source, "k10").at_or_after(&swept),
            Some(false),
            "the sweep has not reached this row yet"
        );
        // The sweep still finishes, reading every row once.
        updates
            .wait_for(|snapshot| {
                *snapshot.state() == SourceState::Ready
                    && snapshot.domains()[&Domain::Git]
                        .observed()
                        .at_or_after(&swept)
                        == Some(true)
            })
            .await
            .unwrap();
        // The user's read of k11 started after the sweep was asked for, so
        // it is that row's sweep read too.
        assert_eq!(fake.reads().len() - before, 12);
        assert!(fake.with(|state| state.cancelled.is_empty()));
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_refresh_during_a_sweep_repeats_only_reads_that_had_started() {
        let scope = TaskScope::new();
        let fake = Fake::new(&["a", "b", "c", "d", "e", "f", "g", "h"]);
        let source = start(&scope, &fake);
        source.refresh();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(fake.reads().len(), 4);
        let second = Stamp::now();
        // Two callers ask for the first load a moment apart, as at start.
        source.refresh();
        source
            .subscribe()
            .wait_for(|snapshot| {
                *snapshot.state() == SourceState::Ready
                    && snapshot.domains()[&Domain::Git]
                        .observed()
                        .at_or_after(&second)
                        == Some(true)
            })
            .await
            .unwrap();
        assert_eq!(
            fake.reads().len(),
            12,
            "the four reads in flight are repeated, the four not yet started are not"
        );
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn the_listing_adds_drops_and_replaces_rows() {
        let scope = TaskScope::new();
        let fake = Fake::new(&["a", "b"]);
        let source = start(&scope, &fake);
        loaded(&source).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        let accepted = Stamp::now();
        fake.with(|state| {
            state.listing = vec![("a".into(), 0), ("c".into(), 0)];
            state.values.insert("c".into(), 3);
        });
        // No key named: only the key set may have changed.
        source.request_keys(Request::Direct, Vec::<String>::new());
        let mut updates = source.subscribe();
        updates
            .wait_for(|snapshot| {
                snapshot
                    .data()
                    .is_some_and(|rows| rows.len() == 2 && rows[1].0 == "c")
            })
            .await
            .unwrap();
        assert_eq!(fake.reads(), ["a", "b", "c"], "only the new row is read");
        assert_eq!(value(&source, "c"), Some(3));
        // A dropped key answers with the listing that no longer had it.
        let meta = source.snapshot().domains()[&Domain::Git].clone();
        assert_eq!(meta.observed_for("b"), meta.keyed().unwrap().index());
        assert_eq!(meta.observed_for("b").at_or_after(&accepted), Some(true));
        assert_eq!(meta.observed_for("a").at_or_after(&accepted), Some(false));

        // The same key at another path is another row: its old value is not
        // shown for it.
        fake.with(|state| {
            state.listing = vec![("a".into(), 1), ("c".into(), 0)];
            state.values.insert("a".into(), 5);
        });
        source.request_keys(Request::Recheck, Vec::<String>::new());
        updates
            .wait_for(|snapshot| {
                snapshot
                    .data()
                    .is_some_and(|rows| rows[0] == ("a".into(), 5))
            })
            .await
            .unwrap();
        assert_eq!(fake.reads(), ["a", "b", "c", "a"]);
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_key_relisted_elsewhere_during_its_read_is_read_first() {
        let scope = TaskScope::new();
        let fake = Fake::new(&["a", "b"]);
        let source = start(&scope, &fake);
        loaded(&source).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        let urgent = fake.with(|state| state.urgent.len());
        // A background read of "a" is in flight when a listing shows the key
        // at another path.
        source.request_keys(Request::Recheck, ["a"]);
        tokio::time::sleep(Duration::from_millis(20)).await;
        fake.with(|state| {
            state.listing = vec![("a".into(), 1), ("b".into(), 0)];
            state.values.insert("a".into(), 5);
        });
        source.request_keys(Request::Recheck, Vec::<String>::new());
        source
            .subscribe()
            .wait_for(|snapshot| snapshot.data().is_some_and(|rows| rows[0].1 == 5))
            .await
            .unwrap();
        // The row had no value, which holds back every publish: its read is
        // user-urgent and waits for no sweep turn, whatever the read it
        // replaces was for.
        assert_eq!(fake.with(|state| state.urgent[urgent..].to_vec()), ["a"]);
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_row_is_published_with_the_entry_its_value_was_read_against() {
        let scope = TaskScope::new();
        let fake = Fake::new(&["a", "b"]);
        fake.with(|state| state.values.insert("a".into(), 7));
        let source = start_keyed_source(
            &scope,
            SourceSpec {
                name: "rows",
                domain: Domain::Git,
                persistence: Persistence::Never("test fixture"),
                policy: RefreshPolicy::default(),
            },
            KeyedPolicy {
                concurrency: NonZeroUsize::new(4).unwrap(),
                key_interval: Duration::from_millis(400),
            },
            fake.clone(),
        );
        loaded(&source).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        source.request_keys(Request::Recheck, ["a"]);
        tokio::time::sleep(Duration::from_millis(150)).await;
        // The listing now says something new about "a" (a new branch). Its
        // background re-read is spaced out; meanwhile a user-caused read of
        // another row publishes.
        fake.with(|state| {
            state.labels.insert("a".into(), 1);
            state.values.insert("a".into(), 9);
            state.values.insert("b".into(), 2);
        });
        source.request_keys(Request::Recheck, Vec::<String>::new());
        tokio::time::sleep(Duration::from_millis(20)).await;
        source.request_keys(Request::Direct, ["b"]);
        let mut updates = source.subscribe();
        updates
            .wait_for(|snapshot| value_in(snapshot, "b") == Some(2))
            .await
            .unwrap();
        assert_eq!(
            value(&source, "a"),
            Some(7),
            "the new entry must not be shown with the status read before it"
        );
        updates
            .wait_for(|snapshot| value_in(snapshot, "a") == Some(1009))
            .await
            .unwrap();
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn rows_that_share_a_stamp_key_are_asked_for_and_confirmed_together() {
        let scope = TaskScope::new();
        let fake = Fake::new(&["a", "a#2", "b"]);
        let source = start(&scope, &fake);
        loaded(&source).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        let reads = fake.reads().len();
        let accepted = Stamp::now();
        source.request_keys(Request::Direct, ["a"]);
        let mut updates = source.subscribe();
        updates
            .wait_for(|snapshot| {
                snapshot.domains()[&Domain::Git]
                    .observed_for("a")
                    .at_or_after(&accepted)
                    == Some(true)
            })
            .await
            .unwrap();
        assert_eq!(fake.reads()[reads..], ["a", "a#2"]);
        let keys: Vec<String> = source.snapshot().domains()[&Domain::Git]
            .keyed()
            .unwrap()
            .keys()
            .map(str::to_owned)
            .collect();
        assert_eq!(keys, ["a", "b"]);
        // A newer read of one of them alone does not move the shared stamp:
        // the other is as old as it was.
        tokio::time::sleep(Duration::from_secs(1)).await;
        let later = Stamp::now();
        let revision = source.snapshot().revision();
        source.request_keys(Request::Recheck, ["a#2"]);
        updates
            .wait_for(|snapshot| snapshot.revision() > revision)
            .await
            .unwrap();
        assert_eq!(fake.reads()[reads..], ["a", "a#2", "a#2"]);
        assert_eq!(stamp(&source, "a").at_or_after(&later), Some(false));
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_sweep_is_not_starved_by_rows_that_change_continuously() {
        let scope = TaskScope::new();
        let fake = Fake::new(&["a", "b", "c", "d", "e", "f"]);
        let source = start(&scope, &fake);
        loaded(&source).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        // Three rows change faster than they can be read, which takes every
        // background lane.
        let busy = source.clone();
        let storm = tokio::spawn(async move {
            loop {
                busy.request_keys(Request::Recheck, ["a", "b", "c"]);
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        let swept = Stamp::now();
        source.refresh();
        let mut updates = source.subscribe();
        // Only the outcome is kept: the snapshot it borrows would block every
        // later publish.
        let done = tokio::time::timeout(
            SWEEP_PATIENCE + Duration::from_secs(2),
            updates.wait_for(|snapshot| {
                snapshot.domains()[&Domain::Git]
                    .observed()
                    .at_or_after(&swept)
                    == Some(true)
            }),
        )
        .await
        .is_ok();
        assert!(
            done,
            "the quiet rows were never swept: {:?}",
            fake.reads().iter().rev().take(12).collect::<Vec<_>>()
        );
        // Its turn, held across processes, is given back once it is done.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(fake.with(|state| state.turns_held), 0);
        storm.abort();
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_listing_keeps_the_rows_and_a_later_one_recovers() {
        let scope = TaskScope::new();
        let fake = Fake::new(&["a", "b"]);
        let source = start(&scope, &fake);
        loaded(&source).await;
        let live = source.snapshot().domains()[&Domain::Git].observed();
        fake.with(|state| state.listing_error = Some("offline".into()));
        source.refresh();
        let mut updates = source.subscribe();
        updates
            .wait_for(|snapshot| matches!(snapshot.state(), SourceState::Failed(_)))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        let failed = source.snapshot();
        assert!(matches!(failed.state(), SourceState::Failed(_)));
        assert_eq!(failed.data().unwrap().len(), 2);
        assert_eq!(failed.domains()[&Domain::Git].observed(), live);
        fake.with(|state| state.listing_error = None);
        source.refresh();
        updates
            .wait_for(|snapshot| *snapshot.state() == SourceState::Ready)
            .await
            .unwrap();
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_row_under_constant_invalidation_is_still_read_and_published() {
        let scope = TaskScope::new();
        let fake = Fake::new(&["a", "b"]);
        let source = start(&scope, &fake);
        loaded(&source).await;
        let before = source.snapshot().revision();
        for round in 0..20 {
            fake.with(|state| state.values.insert("a".into(), round));
            source.request_keys(Request::Recheck, ["a"]);
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        assert!(
            source.snapshot().revision() >= before + 3,
            "events arriving faster than a read finishes must not starve it"
        );
        assert!(
            fake.with(|state| state.cancelled.is_empty()),
            "an event never cancels a read"
        );
        source
            .subscribe()
            .wait_for(|snapshot| snapshot.data().is_some_and(|rows| rows[0].1 == 19))
            .await
            .unwrap();
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn only_a_sweep_waits_for_its_turn_and_holds_it_to_its_last_read() {
        let scope = TaskScope::new();
        let mut fake = Fake::new(&["a", "b", "c", "d", "e", "f"]);
        // Another process is sweeping: the turn takes a while to come.
        fake.turn_wait = Duration::from_millis(500);
        let source = start(&scope, &fake);
        // The first load reads rows that have no value yet; it is not held.
        loaded(&source).await;
        assert_eq!(fake.with(|state| state.turns), 0);
        assert_eq!(fake.with(|state| state.urgent.len()), 6);
        tokio::time::sleep(Duration::from_secs(1)).await;

        let reads = fake.reads().len();
        let swept = Stamp::now();
        source.refresh();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(fake.reads().len(), reads, "the sweep waits for its turn");
        // A user-caused read and an event-caused one do not.
        source.request_keys(Request::Direct, ["a"]);
        source.request_keys(Request::Recheck, ["b"]);
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(fake.reads()[reads..], ["a", "b"]);
        assert_eq!(
            fake.with(|state| state.urgent.last().cloned()).as_deref(),
            Some("a")
        );
        assert_eq!(fake.with(|state| state.urgent_listings), 1);
        assert_eq!(fake.with(|state| state.turns_held), 0);
        // Its turn comes: it reads the rest, holding the turn throughout.
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(fake.with(|state| (state.turns, state.turns_held)), (1, 1));
        source
            .subscribe()
            .wait_for(|snapshot| {
                *snapshot.state() == SourceState::Ready
                    && snapshot.domains()[&Domain::Git]
                        .observed()
                        .at_or_after(&swept)
                        == Some(true)
            })
            .await
            .unwrap();
        // The two rows read meanwhile were read after the sweep was asked
        // for, so the sweep does not repeat them.
        assert_eq!(fake.reads().len(), reads + 6);
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert_eq!(fake.with(|state| (state.turns, state.turns_held)), (1, 0));
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn background_pacing_holds_events_and_never_a_user_read() {
        let scope = TaskScope::new();
        let fake = Fake::new(&["a", "b"]);
        let interval = Duration::from_millis(400);
        let source = start_keyed_source(
            &scope,
            SourceSpec {
                name: "rows",
                domain: Domain::Git,
                persistence: Persistence::Never("test fixture"),
                policy: RefreshPolicy {
                    debounce: Duration::from_millis(25),
                    minimum_interval: interval,
                },
            },
            KeyedPolicy {
                concurrency: NonZeroUsize::new(4).unwrap(),
                key_interval: interval,
            },
            fake.clone(),
        );
        loaded(&source).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        let (reads, listings) = (fake.reads().len(), fake.with(|state| state.listings));
        // An isolated event waits only for the debounce.
        source.request_keys(Request::Recheck, ["a"]);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(fake.reads().len(), reads + 1);
        // One that follows closely waits out the spacing, row and listing.
        tokio::time::sleep(Duration::from_millis(120)).await;
        source.request_keys(Request::Recheck, ["a"]);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(fake.reads().len(), reads + 1);
        assert_eq!(fake.with(|state| state.listings), listings + 1);
        // A user-caused read of the same row does not.
        source.request_keys(Request::Direct, ["a"]);
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert_eq!(fake.reads().len(), reads + 2);
        assert_eq!(fake.with(|state| state.listings), listings + 2);
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_user_request_supersedes_a_background_read_of_its_row_once() {
        let scope = TaskScope::new();
        let fake = Fake::new(&["a", "b"]);
        let source = start(&scope, &fake);
        loaded(&source).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        let reads = fake.reads().len();
        source.request_keys(Request::Recheck, ["a"]);
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(fake.reads().len(), reads + 1);
        // The read in flight started before this request, so it cannot
        // answer it.
        let accepted = Stamp::now();
        fake.with(|state| state.values.insert("a".into(), 4));
        source.request_keys(Request::Direct, ["a"]);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(fake.with(|state| state.cancelled.clone()), ["a"]);
        assert_eq!(fake.reads().len(), reads + 2);
        // Its replacement runs to completion whatever arrives meanwhile.
        source.request_keys(Request::Direct, ["a"]);
        tokio::time::sleep(Duration::from_millis(20)).await;
        source.request_keys(Request::Direct, ["a"]);
        source
            .subscribe()
            .wait_for(|snapshot| snapshot.data().is_some_and(|rows| rows[0].1 == 4))
            .await
            .unwrap();
        assert_eq!(fake.with(|state| state.cancelled.clone()), ["a"]);
        assert_eq!(stamp(&source, "a").at_or_after(&accepted), Some(true));
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
    }
}
