//! Priority admission for one process's subprocess budget.
//!
//! One budget replaces separate pools: a person's request is served before
//! reads that confirm a write, those before periodic reads, and long bulk
//! work has a small cap of its own. Admission is FIFO within a class.

use std::{
    collections::VecDeque,
    num::NonZeroUsize,
    sync::{Arc, Mutex, OnceLock, PoisonError},
    time::Duration,
};

use tokio::{sync::oneshot, time::Instant};

use crate::lock::MachineSlots;

/// Who is waiting for a subprocess. Declared in service order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProcessClass {
    /// A person's request.
    Interactive,
    /// A source read serving an accepted write or an explicit refresh.
    Confirming,
    /// Periodic source reads and everything not classified otherwise.
    Background,
    /// Origin fetch, creation checkout, dependency install, large deletes.
    Bulk,
}

impl ProcessClass {
    pub(crate) const ALL: [Self; 4] = [
        Self::Interactive,
        Self::Confirming,
        Self::Background,
        Self::Bulk,
    ];

    pub(crate) const fn index(self) -> usize {
        self as usize
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Confirming => "confirming",
            Self::Background => "background",
            Self::Bulk => "bulk",
        }
    }

    /// The default bound on waiting for admission. Bulk work legitimately
    /// queues behind minutes-long installs; the caller's cancellation still
    /// ends the wait.
    pub(crate) const fn admission_timeout(self) -> Duration {
        match self {
            Self::Bulk => Duration::from_secs(15 * 60),
            _ => Duration::from_secs(60),
        }
    }
}

/// How one process's concurrent subprocesses are shared between classes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessBudget {
    /// Subprocesses of every class together.
    pub total: NonZeroUsize,
    /// Slots only `Interactive` may occupy. The other classes together never
    /// hold more than `total - interactive_reserve`.
    pub interactive_reserve: usize,
    pub background: NonZeroUsize,
    pub bulk: NonZeroUsize,
    /// A waiter this old is served before higher classes, within its own
    /// caps, so sustained higher-class demand cannot starve it.
    pub starvation_bound: Duration,
}

impl ProcessBudget {
    /// One undivided FIFO pool: every class may use every slot.
    pub const fn flat(total: NonZeroUsize) -> Self {
        Self {
            total,
            interactive_reserve: 0,
            background: total,
            bulk: total,
            starvation_bound: Duration::from_secs(5),
        }
    }

    /// The application budget. The total equals the two eight-permit pools
    /// it replaces. Ten slots are shared by the non-interactive classes, of
    /// which periodic reads take at most six and bulk work two, so a
    /// confirming read always has two that neither can hold.
    pub const fn application() -> Self {
        Self {
            total: NonZeroUsize::new(16).expect("nonzero"),
            interactive_reserve: 6,
            background: NonZeroUsize::new(6).expect("nonzero"),
            bulk: NonZeroUsize::new(2).expect("nonzero"),
            starvation_bound: Duration::from_secs(5),
        }
    }

    fn shared(&self) -> usize {
        // A reserve as large as the pool would leave the other classes no
        // slot at all; they keep one.
        self.total
            .get()
            .saturating_sub(self.interactive_reserve)
            .max(1)
    }

    fn admits(&self, class: ProcessClass, active: &[usize; 4]) -> bool {
        let total: usize = active.iter().sum();
        if total >= self.total.get() {
            return false;
        }
        if class == ProcessClass::Interactive {
            return true;
        }
        if total - active[ProcessClass::Interactive.index()] >= self.shared() {
            return false;
        }
        match class {
            ProcessClass::Background => active[class.index()] < self.background.get(),
            ProcessClass::Bulk => active[class.index()] < self.bulk.get(),
            ProcessClass::Interactive | ProcessClass::Confirming => true,
        }
    }
}

struct Waiter {
    id: u64,
    since: Instant,
    grant: oneshot::Sender<Permit>,
}

#[derive(Default)]
struct State {
    active: [usize; 4],
    waiting: [VecDeque<Waiter>; 4],
    next_id: u64,
}

pub(crate) struct Admission {
    budget: ProcessBudget,
    state: Mutex<State>,
    /// Cross-process slots for heavy commands. `ProcessRunner::admit` tries
    /// one only while it holds the local permit and waits for one only with
    /// no permit in hand, so neither is held idle behind the other.
    heavy: OnceLock<MachineSlots>,
    /// Cross-process slots held for the length of one whole-fleet read.
    fleet_scans: OnceLock<MachineSlots>,
}

/// One admitted subprocess. Dropping it admits the next waiter.
pub(crate) struct Permit {
    admission: Arc<Admission>,
    class: ProcessClass,
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut state = self.admission.lock();
        state.active[self.class.index()] -= 1;
        self.admission.dispatch(&mut state);
    }
}

/// Removes its queue entry when the caller stops waiting. A permit granted
/// but never received is released when the receiver drops after this body.
struct Queued {
    admission: Arc<Admission>,
    class: ProcessClass,
    id: u64,
    grant: oneshot::Receiver<Permit>,
}

impl Drop for Queued {
    fn drop(&mut self) {
        let mut state = self.admission.lock();
        state.waiting[self.class.index()].retain(|waiter| waiter.id != self.id);
    }
}

impl Admission {
    pub(crate) fn new(budget: ProcessBudget) -> Arc<Self> {
        Arc::new(Self {
            budget,
            state: Mutex::default(),
            heavy: OnceLock::new(),
            fleet_scans: OnceLock::new(),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn set_heavy(&self, slots: MachineSlots) {
        let _ = self.heavy.set(slots);
    }

    pub(crate) fn heavy(&self) -> Option<&MachineSlots> {
        self.heavy.get()
    }

    pub(crate) fn set_fleet_scans(&self, slots: MachineSlots) {
        let _ = self.fleet_scans.set(slots);
    }

    pub(crate) fn fleet_scans(&self) -> Option<&MachineSlots> {
        self.fleet_scans.get()
    }

    /// A permit only if the budget admits `class` now and nobody of that
    /// class is queued ahead.
    pub(crate) fn try_acquire(self: &Arc<Self>, class: ProcessClass) -> Option<Permit> {
        let mut state = self.lock();
        if !state.waiting[class.index()].is_empty() || !self.budget.admits(class, &state.active) {
            return None;
        }
        state.active[class.index()] += 1;
        Some(Permit {
            admission: Arc::clone(self),
            class,
        })
    }

    pub(crate) async fn acquire(self: &Arc<Self>, class: ProcessClass) -> Permit {
        let (grant, granted) = oneshot::channel();
        let id = {
            let mut state = self.lock();
            let id = state.next_id;
            state.next_id += 1;
            state.waiting[class.index()].push_back(Waiter {
                id,
                since: Instant::now(),
                grant,
            });
            self.dispatch(&mut state);
            id
        };
        let mut queued = Queued {
            admission: Arc::clone(self),
            class,
            id,
            grant: granted,
        };
        (&mut queued.grant)
            .await
            .expect("a queued waiter is granted or removes itself")
    }

    /// Grant every waiter the budget admits now. Only the head of a class is
    /// ever granted. Among admissible heads, one that has waited past the
    /// starvation bound goes first (the oldest such), then the highest class.
    fn dispatch(self: &Arc<Self>, state: &mut State) {
        let now = Instant::now();
        loop {
            let mut highest = None;
            let mut starved: Option<(Instant, ProcessClass)> = None;
            for class in ProcessClass::ALL {
                let Some(head) = state.waiting[class.index()].front() else {
                    continue;
                };
                if !self.budget.admits(class, &state.active) {
                    continue;
                }
                if highest.is_none() {
                    highest = Some(class);
                }
                if now.saturating_duration_since(head.since) >= self.budget.starvation_bound
                    && starved.is_none_or(|(since, _)| head.since < since)
                {
                    starved = Some((head.since, class));
                }
            }
            let Some(class) = starved.map(|(_, class)| class).or(highest) else {
                return;
            };
            let waiter = state.waiting[class.index()]
                .pop_front()
                .expect("an admissible head was just observed");
            state.active[class.index()] += 1;
            let permit = Permit {
                admission: Arc::clone(self),
                class,
            };
            if let Err(permit) = waiter.grant.send(permit) {
                // Dropping the permit here would lock the state this call
                // already holds.
                std::mem::forget(permit);
                state.active[class.index()] -= 1;
            }
        }
    }

    #[cfg(test)]
    fn active(&self) -> [usize; 4] {
        self.lock().active
    }

    #[cfg(test)]
    fn waiting(&self) -> [usize; 4] {
        let state = self.lock();
        std::array::from_fn(|index| state.waiting[index].len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ProcessClass::{Background, Bulk, Confirming, Interactive};
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll, Waker};

    fn budget(total: usize, reserve: usize, background: usize, bulk: usize) -> ProcessBudget {
        ProcessBudget {
            total: NonZeroUsize::new(total).unwrap(),
            interactive_reserve: reserve,
            background: NonZeroUsize::new(background).unwrap(),
            bulk: NonZeroUsize::new(bulk).unwrap(),
            starvation_bound: Duration::from_secs(5),
        }
    }

    type Request = Pin<Box<dyn Future<Output = Permit>>>;

    fn request(admission: &Arc<Admission>, class: ProcessClass) -> Request {
        let admission = Arc::clone(admission);
        Box::pin(async move { admission.acquire(class).await })
    }

    /// A request joins its queue at its first poll.
    fn poll(request: &mut Request) -> Option<Permit> {
        match request
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            Poll::Ready(permit) => Some(permit),
            Poll::Pending => None,
        }
    }

    fn admitted(admission: &Arc<Admission>, class: ProcessClass) -> Permit {
        poll(&mut request(admission, class)).expect("admitted at once")
    }

    #[tokio::test(start_paused = true)]
    async fn the_interactive_reserve_is_never_taken_by_other_classes() {
        let admission = Admission::new(budget(4, 2, 2, 2));
        let _shared = [admitted(&admission, Confirming), admitted(&admission, Bulk)];
        for class in [Confirming, Background, Bulk] {
            let mut waiting = request(&admission, class);
            assert!(poll(&mut waiting).is_none(), "{class:?} took the reserve");
            // Not even a waiter past the starvation bound may take it.
            tokio::time::advance(Duration::from_secs(60)).await;
            assert!(poll(&mut waiting).is_none());
        }
        let _person = [
            admitted(&admission, Interactive),
            admitted(&admission, Interactive),
        ];
        assert_eq!(admission.active(), [2, 1, 0, 1]);
    }

    #[tokio::test(start_paused = true)]
    async fn class_caps_leave_room_for_a_confirming_read() {
        let admission = Admission::new(budget(8, 2, 3, 1));
        let _background = [
            admitted(&admission, Background),
            admitted(&admission, Background),
            admitted(&admission, Background),
        ];
        let _bulk = admitted(&admission, Bulk);
        assert!(poll(&mut request(&admission, Background)).is_none());
        assert!(poll(&mut request(&admission, Bulk)).is_none());
        let _confirming = [
            admitted(&admission, Confirming),
            admitted(&admission, Confirming),
        ];
        assert!(poll(&mut request(&admission, Confirming)).is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_freed_slot_goes_to_the_highest_class_and_in_arrival_order_within_it() {
        let admission = Admission::new(ProcessBudget::flat(NonZeroUsize::new(1).unwrap()));
        let held = admitted(&admission, Background);
        // Arrival order is the reverse of the order they must be served in,
        // except within the one class that has two waiters.
        let mut waiting = [
            ("bulk", request(&admission, Bulk)),
            ("first", request(&admission, Background)),
            ("second", request(&admission, Background)),
            ("confirming", request(&admission, Confirming)),
            ("person", request(&admission, Interactive)),
        ];
        for (name, request) in &mut waiting {
            assert!(poll(request).is_none(), "{name}");
        }
        drop(held);
        let mut order = Vec::new();
        while order.len() < waiting.len() {
            let before = order.len();
            for (name, request) in &mut waiting {
                if !order.contains(name) && poll(request).is_some() {
                    // The permit is dropped here, which admits the next one.
                    order.push(*name);
                    break;
                }
            }
            assert!(order.len() > before, "nobody was admitted after {order:?}");
        }
        assert_eq!(order, ["person", "confirming", "first", "second", "bulk"]);
    }

    #[tokio::test(start_paused = true)]
    async fn sustained_higher_demand_cannot_starve_a_lower_class() {
        let admission = Admission::new(ProcessBudget::flat(NonZeroUsize::new(1).unwrap()));
        let mut held = admitted(&admission, Confirming);
        let mut bulk = request(&admission, Bulk);
        assert!(poll(&mut bulk).is_none());
        let mut served_at = None;
        // A confirming read is always queued before the slot frees.
        for second in 1..=20 {
            let mut next = request(&admission, Confirming);
            assert!(poll(&mut next).is_none());
            tokio::time::advance(Duration::from_secs(1)).await;
            drop(held);
            if let Some(permit) = poll(&mut bulk) {
                served_at = Some(second);
                drop(permit);
                break;
            }
            held = poll(&mut next).expect("the higher class is served before the bound");
        }
        assert_eq!(served_at, Some(5));
    }

    #[tokio::test(start_paused = true)]
    async fn an_immediate_permit_never_jumps_its_own_class_queue() {
        let admission = Admission::new(ProcessBudget::flat(NonZeroUsize::new(1).unwrap()));
        let held = admission.try_acquire(Bulk).expect("a free budget admits");
        assert!(admission.try_acquire(Bulk).is_none());
        let mut queued = request(&admission, Bulk);
        assert!(poll(&mut queued).is_none());
        drop(held);
        assert!(admission.try_acquire(Bulk).is_none());
        let permit = poll(&mut queued).expect("the queued waiter got the slot");
        drop(permit);
        assert_eq!(admission.active(), [0; 4]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_waiter_that_stops_waiting_leaves_the_queue_and_the_slot() {
        let admission = Admission::new(ProcessBudget::flat(NonZeroUsize::new(1).unwrap()));
        let held = admitted(&admission, Background);
        let mut abandoned = request(&admission, Background);
        let mut patient = request(&admission, Background);
        assert!(poll(&mut abandoned).is_none());
        assert!(poll(&mut patient).is_none());
        drop(abandoned);
        assert_eq!(admission.waiting(), [0, 0, 1, 0]);
        drop(held);
        let permit = poll(&mut patient).expect("the next waiter is served");
        assert_eq!(admission.active(), [0, 0, 1, 0]);
        drop(permit);

        // Granted but never received: the slot still comes back.
        let held = admitted(&admission, Background);
        let mut granted_unseen = request(&admission, Background);
        assert!(poll(&mut granted_unseen).is_none());
        drop(held);
        assert_eq!(admission.active(), [0, 0, 1, 0]);
        drop(granted_unseen);
        assert_eq!(admission.active(), [0; 4]);
        assert_eq!(admission.waiting(), [0; 4]);
    }

    /// Random arrivals, releases and abandoned waits against the budget's
    /// invariants. Deterministic: the generator is seeded.
    #[tokio::test(start_paused = true)]
    async fn random_schedules_keep_every_cap_and_never_idle_an_admissible_waiter() {
        let limits = budget(6, 2, 2, 1);
        for seed in 1..=40_u64 {
            let admission = Admission::new(limits);
            let mut random = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15);
            let mut next = move |bound: usize| {
                random ^= random << 13;
                random ^= random >> 7;
                random ^= random << 17;
                (random % bound as u64) as usize
            };
            let mut waiting: Vec<(ProcessClass, Request)> = Vec::new();
            let mut held: Vec<Permit> = Vec::new();
            for _ in 0..400 {
                match next(4) {
                    0 | 1 => {
                        let class = ProcessClass::ALL[next(4)];
                        waiting.push((class, request(&admission, class)));
                    }
                    2 if !held.is_empty() => drop(held.swap_remove(next(held.len()))),
                    3 if !waiting.is_empty() => drop(waiting.swap_remove(next(waiting.len()))),
                    _ => {}
                }
                tokio::time::advance(Duration::from_millis(700)).await;
                let mut index = 0;
                while index < waiting.len() {
                    match poll(&mut waiting[index].1) {
                        Some(permit) => {
                            held.push(permit);
                            drop(waiting.swap_remove(index));
                        }
                        None => index += 1,
                    }
                }
                let active = admission.active();
                let total: usize = active.iter().sum();
                assert_eq!(total, held.len(), "seed {seed}");
                assert!(total <= 6, "seed {seed}");
                assert!(total - active[Interactive.index()] <= 4, "seed {seed}");
                assert!(active[Background.index()] <= 2, "seed {seed}");
                assert!(active[Bulk.index()] <= 1, "seed {seed}");
                for (class, _) in &waiting {
                    assert!(
                        !limits.admits(*class, &active),
                        "seed {seed}: {class:?} waits although the budget admits it"
                    );
                }
            }
        }
    }
}
