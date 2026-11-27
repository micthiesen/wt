//! Which scheduling lane each host request runs in, and which lanes exclude
//! each other. A pure classification of `UiAction`, shared by the controller
//! that schedules requests and by `AppContext::for_action`, which picks the
//! process budget for the same request.

use std::collections::{BTreeSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Lane {
    Host(Option<String>),
    /// Host preferences and controls touch no checkout; share the control budget.
    HostControl(Option<String>),
    /// Commands on one checkout cannot exclude unrelated lifecycle work.
    Target(Option<String>, String),
    /// Dispatch of a configured action for one checkout, or for the manager
    /// or a special slot when there is no checkout. The run it starts is
    /// tracked by the action service under its own locks; this lane covers
    /// only the dispatch, which needs that checkout to stay in place.
    Action(Option<String>, Option<String>),
    /// Session stop owns the slot resource that terminal entry also uses.
    SessionControl(Option<String>, Option<String>),
    /// Create operations serialize with each other, but a durable removal of
    /// an existing target must not hold up creating an unrelated target.
    Creation(Option<String>),
    /// Removal jobs are durable and already own the target's lifecycle lock.
    /// Keep the same target serialized while allowing unrelated work through.
    Removal(Option<String>, String),
    /// Target-scoped metadata writes must not wait for unrelated lifecycle jobs.
    Metadata(Option<String>, String),
    /// Read-only prompts and pickers must not sit behind a long accepted
    /// mutation on the same host. The final mutation still revalidates.
    Preparation(Option<String>),
    /// Background reads that prepare a board menu ahead of its key (palette
    /// prefetch). One runs at a time beside foreground preparations, never
    /// taking their slot or ordinary host capacity.
    Prefetch(Option<String>),
    /// One accepted cleanup confirmation, which may span several hosts. It
    /// owns exactly the checkouts it removes, as `(host, slug)` pairs, so
    /// unrelated checkouts stay available while each teardown runs.
    FleetCleanup(BTreeSet<(Option<String>, String)>),
    /// Layout writes (fold, move, reorder, rename, archive). They serialize
    /// with each other because each one reads the order the last one wrote.
    Presentation,
    /// Effects on this device that own no wt resource: opening a link or an
    /// editor, copying. They exclude nothing, each other included.
    Device,
    /// Bounded, device-local controls must remain responsive while host jobs
    /// or interactive handoffs occupy all ordinary action slots.
    ViewControl,
    /// One device terminal, with host-local checkout identity for removal
    /// conflicts. Special slots have no worktree target.
    Terminal(Option<String>, Option<String>),
}

pub fn lane_for(action: &wt_tui::UiAction) -> Result<Lane> {
    if is_cleanup_action(action) {
        // A single confirmation can contain revisions from several hosts.
        // Serialize it as one accepted operation so shutdown drains the whole
        // fleet mutation and no second cleanup races its prepared revisions.
        // It owns only the checkouts it removes; unrelated work proceeds.
        return cleanup_lane(action, None);
    }
    let (host, action, _) = crate::host_routing::resolve(action.clone())?;
    if action.is_preparation() {
        // Menu ownership stays in UiAction::menu, not in the scheduler.
        return Ok(Lane::Preparation(host));
    }
    use wt_tui::UiAction as A;
    // Exhaustive by design: adding an action requires choosing the resources
    // it actually owns, rather than silently taking a whole-host lock.
    Ok(match action {
        A::OnHost { .. } => unreachable!("routing has been resolved"),
        A::SetAttentionSeen { .. } | A::SetHistoryActive { .. } | A::SetPerf { .. } => {
            Lane::ViewControl
        }
        A::CyclePrimary
        | A::ToggleAutomations { .. }
        | A::CancelAutomations
        | A::ToggleRemovedAutomations { .. }
        | A::DismissReviewRequest { .. }
        | A::KillAction { .. }
        | A::HardRefresh => Lane::HostControl(host),
        A::WarmPalette { .. } | A::DiscoverSessions { .. } => Lane::Prefetch(host),
        A::Session { key, .. } => Lane::Terminal(host, key),
        A::SelectSession { selection } => Lane::Terminal(host, selection.key),
        A::EnterHarness { key, .. } => Lane::Terminal(host, Some(key)),
        A::PerfInvestigate { .. } | A::ManagedSignIn { .. } => Lane::Terminal(host, None),
        A::OpenLink { .. }
        | A::OpenPrLink { .. }
        | A::OpenPrDefault { .. }
        | A::OpenSlotEditor { .. }
        | A::Copy { .. }
        | A::OpenEditor { .. }
        | A::OpenUrl { .. } => Lane::Device,
        A::FoldSection { .. }
        | A::MoveSection { .. }
        | A::Reorder { .. }
        | A::RenameSection { .. }
        | A::DeleteSection { .. }
        | A::ToggleArchive { .. } => Lane::Presentation,
        A::PrepareCreate { .. }
        | A::ResolveCreation { .. }
        | A::PrepareHardRefresh
        | A::PrepareReviewCheckout { .. }
        | A::PrepareReviewers { .. }
        | A::PrepareRestoreRemoved { .. }
        | A::PrepareGithub { .. }
        | A::PrepareActions { .. }
        | A::PrepareAction { .. }
        | A::PrepareSection { .. }
        | A::PrepareDeleteSection { .. }
        | A::PrepareRemove { .. }
        | A::PrepareCleanup
        | A::PrepareStatus { .. }
        | A::PrepareBase { .. }
        | A::PrepareNotify { .. }
        | A::PrepareSessions { .. }
        | A::PrepareHarnesses { .. }
        | A::PrepareStopTerminal { .. }
        | A::GithubFailedChecks { .. } => unreachable!("menu declaration handles preparations"),
        A::Create { .. } | A::CreateResolved { .. } => Lane::Creation(host),
        A::Remove { key, .. } | A::DismissFailedRemoval { key, .. } => Lane::Removal(host, key),
        A::SetTitle { key, .. }
        | A::SetStatus { key, .. }
        | A::SetNotify { key, .. }
        | A::SetIssueOverride { key, .. } => Lane::Metadata(host, key),
        A::SubmitReviewers { key, .. }
        | A::GithubMarkReady { key, .. }
        | A::GithubSetAutoMerge { key, .. }
        | A::GithubShip { key, .. }
        | A::SetBase { key, .. } => Lane::Target(host, key),
        A::StopTerminal { key, .. } => Lane::SessionControl(host, Some(key)),
        A::StopSession { selection } | A::KillSession { selection } => {
            Lane::SessionControl(host, selection.key)
        }
        A::RunAction { surface, .. } => Lane::Action(
            host,
            match surface {
                wt_tui::ActionSurface::Row { key }
                | wt_tui::ActionSurface::Manager { key: Some(key) } => Some(key),
                wt_tui::ActionSurface::Manager { key: None }
                | wt_tui::ActionSurface::Slot { .. } => None,
            },
        ),
        // Stack operations and checkouts made for a review or a restore can
        // span multiple checkouts.
        A::Restack { .. } | A::ReviewCheckout { .. } | A::RestoreRemoved { .. } => Lane::Host(host),
        A::Cleanup { .. } | A::CleanupTargets { .. } => {
            unreachable!("cleanup lanes are resolved before host routing")
        }
    })
}

/// Collects the `(host, slug)` checkouts a cleanup removes, in the same form
/// host routing gives Removal and Terminal lanes. Local keys inside an
/// explicit `OnHost` wrapper belong to that host; on a worker they are local.
pub fn cleanup_lane(action: &wt_tui::UiAction, explicit: Option<&str>) -> Result<Lane> {
    let keys: Vec<&str> = match action {
        wt_tui::UiAction::OnHost { host, action } => {
            if matches!(**action, wt_tui::UiAction::OnHost { .. }) {
                anyhow::bail!("nested host routing is not allowed");
            }
            return cleanup_lane(action, host.as_deref());
        }
        wt_tui::UiAction::CleanupTargets { targets } => {
            targets.iter().map(|target| target.key.as_str()).collect()
        }
        wt_tui::UiAction::Cleanup { revisions } => revisions
            .iter()
            .map(|revision| revision.key.as_str())
            .collect(),
        _ => unreachable!("caller checked is_cleanup_action"),
    };
    let mut targets = BTreeSet::new();
    for key in keys {
        let target = match wt_core::parse_worktree_ledger_key(key)
            .ok_or_else(|| anyhow::anyhow!("invalid worktree identity {key:?}"))?
        {
            wt_core::WorktreeRef::Local { slug } => (explicit.map(str::to_owned), slug),
            wt_core::WorktreeRef::Remote { host, slug } => (Some(host), slug),
        };
        targets.insert(target);
    }
    Ok(Lane::FleetCleanup(targets))
}

fn is_cleanup_action(action: &wt_tui::UiAction) -> bool {
    match action {
        wt_tui::UiAction::Cleanup { .. } | wt_tui::UiAction::CleanupTargets { .. } => true,
        wt_tui::UiAction::OnHost { action, .. } => is_cleanup_action(action),
        _ => false,
    }
}

pub fn lanes_conflict(left: &Lane, right: &Lane) -> bool {
    if matches!(left, Lane::Device) || matches!(right, Lane::Device) {
        return false;
    }
    if left == right {
        return true;
    }
    match (left, right) {
        (Lane::FleetCleanup(_), Lane::FleetCleanup(_)) => return true,
        (Lane::FleetCleanup(targets), other) | (other, Lane::FleetCleanup(targets)) => {
            return cleanup_conflicts(targets, other);
        }
        _ => {}
    }
    match (left, right) {
        (Lane::Terminal(_, _), Lane::Terminal(_, _)) => true,
        (Lane::Terminal(host, Some(key)), Lane::Removal(other_host, other_key))
        | (Lane::Removal(other_host, other_key), Lane::Terminal(host, Some(key))) => {
            host == other_host && key == other_key
        }
        (Lane::Host(left), Lane::Creation(right))
        | (Lane::Creation(left), Lane::Host(right))
        | (Lane::Host(left), Lane::Removal(right, _))
        | (Lane::Removal(left, _), Lane::Host(right)) => left == right,
        (Lane::Removal(left_host, left_key), Lane::Removal(right_host, right_key)) => {
            left_host == right_host && left_key == right_key
        }
        (Lane::Target(host, key), Lane::Removal(other_host, other_key))
        | (Lane::Removal(other_host, other_key), Lane::Target(host, key)) => {
            host == other_host && key == other_key
        }
        (Lane::SessionControl(host, key), Lane::Terminal(other_host, other_key))
        | (Lane::Terminal(other_host, other_key), Lane::SessionControl(host, key)) => {
            host == other_host && key == other_key
        }
        (Lane::SessionControl(host, Some(key)), Lane::Removal(other_host, other_key))
        | (Lane::Removal(other_host, other_key), Lane::SessionControl(host, Some(key))) => {
            host == other_host && key == other_key
        }
        (Lane::Target(host, _), Lane::Host(other_host))
        | (Lane::Host(other_host), Lane::Target(host, _))
        | (Lane::Action(host, _), Lane::Host(other_host))
        | (Lane::Host(other_host), Lane::Action(host, _)) => host == other_host,
        (Lane::Action(host, Some(key)), Lane::Removal(other_host, other_key))
        | (Lane::Removal(other_host, other_key), Lane::Action(host, Some(key)))
        | (Lane::Action(host, Some(key)), Lane::Target(other_host, other_key))
        | (Lane::Target(other_host, other_key), Lane::Action(host, Some(key))) => {
            host == other_host && key == other_key
        }
        (Lane::Metadata(host, key), Lane::Removal(other_host, other_key))
        | (Lane::Removal(other_host, other_key), Lane::Metadata(host, key)) => {
            host == other_host && key == other_key
        }
        _ => false,
    }
}

/// Requests of one kind that may run at once. A budget that names a host is
/// counted per host, so a slow or unreachable remote cannot use up the
/// capacity another host's requests need.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Budget<'a> {
    /// Long jobs: whole-host operations, action dispatch, removal planning.
    Ordinary(&'a Option<String>),
    /// Every removal of a host until its teardown ends, so the number of
    /// teardown workers stays bounded after they stop counting as ordinary.
    Teardown(&'a Option<String>),
    Target(&'a Option<String>),
    SessionControl(&'a Option<String>),
    Creation(&'a Option<String>),
    Metadata(&'a Option<String>),
    Preparation(&'a Option<String>),
    Prefetch(&'a Option<String>),
    HostControl(&'a Option<String>),
    ViewControl,
    Presentation,
    Device,
}

impl Budget<'_> {
    fn limit(self) -> usize {
        match self {
            Self::Ordinary(_) | Self::Teardown(_) | Self::Device => 4,
            Self::Target(_) | Self::SessionControl(_) => 2,
            Self::Creation(_)
            | Self::Metadata(_)
            | Self::Preparation(_)
            | Self::Prefetch(_)
            | Self::HostControl(_)
            | Self::ViewControl
            | Self::Presentation => 1,
        }
    }
}

/// The budgets a lane draws from. `durable` is a removal whose worker owns
/// the rest of the teardown (see [`LaneHold::durable`]).
fn budgets(lane: &Lane, durable: bool) -> [Option<Budget<'_>>; 2] {
    let one = |budget| [Some(budget), None];
    match lane {
        Lane::Removal(host, _) => [
            (!durable).then_some(Budget::Ordinary(host)),
            Some(Budget::Teardown(host)),
        ],
        Lane::Host(host) | Lane::Action(host, _) => one(Budget::Ordinary(host)),
        Lane::Target(host, _) => one(Budget::Target(host)),
        Lane::SessionControl(host, _) => one(Budget::SessionControl(host)),
        Lane::Creation(host) => one(Budget::Creation(host)),
        Lane::Metadata(host, _) => one(Budget::Metadata(host)),
        Lane::Preparation(host) => one(Budget::Preparation(host)),
        Lane::Prefetch(host) => one(Budget::Prefetch(host)),
        Lane::HostControl(host) => one(Budget::HostControl(host)),
        Lane::ViewControl => one(Budget::ViewControl),
        Lane::Presentation => one(Budget::Presentation),
        Lane::Device => one(Budget::Device),
        // Their conflicts already admit one at a time: one device terminal,
        // one cleanup.
        Lane::Terminal(_, _) | Lane::FleetCleanup(_) => [None, None],
    }
}

/// Jobs that can run for a long time and exclude whole hosts or lifecycles.
/// A queued one keeps its turn against later ones (see [`Admission`]).
fn long_job(lane: &Lane) -> bool {
    matches!(
        lane,
        Lane::Host(_)
            | Lane::Action(_, _)
            | Lane::Removal(_, _)
            | Lane::Creation(_)
            | Lane::FleetCleanup(_)
    )
}

/// Lent to a running request by the scheduler that admitted it.
#[derive(Clone, Debug, Default)]
pub struct LaneHold {
    durable: Arc<AtomicBool>,
    changed: Arc<tokio::sync::Notify>,
}

impl LaneHold {
    /// A removal calls this once its durable worker is running. The worker
    /// owns the checkout's lifecycle lock and finishes without this request,
    /// which from here only waits for it. The request therefore stops
    /// counting as a long job and stops excluding whole-host work; it still
    /// excludes everything on its own checkout until it completes.
    pub fn durable(&self) {
        if !self.durable.swap(true, Ordering::SeqCst) {
            self.changed.notify_one();
        }
    }
}

struct Running {
    lane: Lane,
    hold: LaneHold,
}

impl Running {
    fn durable(&self) -> bool {
        matches!(self.lane, Lane::Removal(_, _)) && self.hold.durable.load(Ordering::SeqCst)
    }

    fn excludes(&self, lane: &Lane) -> bool {
        if self.durable() && matches!(lane, Lane::Host(_)) {
            return false;
        }
        lanes_conflict(&self.lane, lane)
    }
}

/// The running requests, and the rule for which queued request starts next:
/// the first one, in arrival order, that has capacity in its budgets, that no
/// running request excludes, and that would not overtake an earlier queued
/// long job it conflicts with. Without the last condition a whole-host
/// request could wait forever while removals and creations that arrived
/// after it keep starting. Short writes and reads may still pass a queued
/// long job, because they finish on their own and a gap always follows.
#[derive(Default)]
pub struct Admission {
    running: Vec<Running>,
    changed: Arc<tokio::sync::Notify>,
}

impl Admission {
    pub fn insert(&mut self, lane: Lane) -> LaneHold {
        let hold = LaneHold {
            durable: Arc::default(),
            changed: self.changed.clone(),
        };
        self.running.push(Running {
            lane,
            hold: hold.clone(),
        });
        hold
    }

    pub fn remove(&mut self, lane: &Lane) {
        if let Some(position) = self
            .running
            .iter()
            .position(|running| running.lane == *lane)
        {
            self.running.remove(position);
        }
    }

    /// Resolves when a running request gave something back, so queued
    /// requests should be considered again.
    pub async fn changed(&self) {
        self.changed.notified().await;
    }

    /// The running lane that excludes `lane`, if any.
    pub fn blocker(&self, lane: &Lane) -> Option<&Lane> {
        self.running
            .iter()
            .find(|running| running.excludes(lane))
            .map(|running| &running.lane)
    }

    fn has_capacity(&self, lane: &Lane) -> bool {
        budgets(lane, false).into_iter().flatten().all(|budget| {
            self.running
                .iter()
                .filter(|running| budgets(&running.lane, running.durable()).contains(&Some(budget)))
                .count()
                < budget.limit()
        })
    }

    /// The earlier queued long job `pending[index]` must not overtake.
    pub fn ahead<T>(pending: &VecDeque<(Lane, T)>, index: usize) -> Option<&(Lane, T)> {
        let (lane, _) = pending.get(index)?;
        if !long_job(lane) {
            return None;
        }
        pending
            .iter()
            .take(index)
            .find(|(earlier, _)| long_job(earlier) && lanes_conflict(earlier, lane))
    }

    /// The queued request that may start now.
    pub fn next<T>(&self, pending: &VecDeque<(Lane, T)>) -> Option<usize> {
        (0..pending.len()).find(|index| {
            let lane = &pending[*index].0;
            self.has_capacity(lane)
                && self.blocker(lane).is_none()
                && Self::ahead(pending, *index).is_none()
        })
    }
}

impl FromIterator<Lane> for Admission {
    fn from_iter<I: IntoIterator<Item = Lane>>(lanes: I) -> Self {
        let mut admission = Self::default();
        for lane in lanes {
            admission.insert(lane);
        }
        admission
    }
}

impl<const N: usize> From<[Lane; N]> for Admission {
    fn from(lanes: [Lane; N]) -> Self {
        lanes.into_iter().collect()
    }
}

/// A cleanup excludes work on the checkouts it removes and whole-host jobs
/// on hosts it touches, matching a single Removal of each target. Creation
/// and work on unrelated checkouts proceed while its teardowns run.
fn cleanup_conflicts(targets: &BTreeSet<(Option<String>, String)>, other: &Lane) -> bool {
    let owns = |host: &Option<String>, key: &String| targets.contains(&(host.clone(), key.clone()));
    match other {
        Lane::Removal(host, key)
        | Lane::Terminal(host, Some(key))
        | Lane::SessionControl(host, Some(key))
        | Lane::Metadata(host, key)
        | Lane::Action(host, Some(key))
        | Lane::Target(host, key) => owns(host, key),
        Lane::Host(host) => targets.iter().any(|(target_host, _)| target_host == host),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_tui::{ActionSurface, SessionTarget, UiAction};

    fn removal(key: &str) -> Lane {
        Lane::Removal(None, key.into())
    }

    fn take<T>(admission: &Admission, pending: &mut VecDeque<(Lane, T)>) -> Option<T> {
        let position = admission.next(pending)?;
        pending.remove(position).map(|(_, request)| request)
    }

    #[test]
    fn opening_and_copying_wait_for_nothing_and_exclude_nothing() {
        for action in [
            UiAction::OpenLink { url: "u".into() },
            UiAction::OpenPrLink {
                url: "u".into(),
                linear: false,
            },
            UiAction::OpenPrDefault { url: "u".into() },
            UiAction::OpenSlotEditor {
                target: SessionTarget::Main,
            },
            UiAction::Copy {
                value: "v".into(),
                label: "l".into(),
            },
            UiAction::OpenEditor { key: "row".into() },
            UiAction::OpenUrl {
                key: "row".into(),
                kind: wt_tui::UrlKind::PullRequest,
            },
        ] {
            assert_eq!(lane_for(&action).unwrap(), Lane::Device, "{action:?}");
        }
        assert!(!lanes_conflict(&Lane::Device, &Lane::Device));
        // Four long jobs, a layout write and another open are all running.
        let busy = Admission::from([
            removal("a"),
            removal("b"),
            removal("c"),
            removal("d"),
            Lane::Presentation,
            Lane::Device,
        ]);
        let mut pending = VecDeque::from([(Lane::Device, "open")]);
        assert_eq!(take(&busy, &mut pending), Some("open"));
        // Their own bound still holds.
        let saturated = Admission::from([Lane::Device, Lane::Device, Lane::Device, Lane::Device]);
        let mut pending = VecDeque::from([(Lane::Device, "fifth")]);
        assert_eq!(take(&saturated, &mut pending), None);
    }

    #[test]
    fn layout_writes_serialize_with_each_other_but_not_behind_long_jobs() {
        let fold = lane_for(&UiAction::FoldSection {
            key: "inbox".into(),
            folded: true,
        })
        .unwrap();
        assert_eq!(fold, Lane::Presentation);
        let busy = Admission::from([removal("a"), removal("b"), removal("c"), removal("d")]);
        let mut pending = VecDeque::from([(fold.clone(), "fold")]);
        assert_eq!(take(&busy, &mut pending), Some("fold"));
        let mut pending = VecDeque::from([(fold.clone(), "second")]);
        assert_eq!(take(&Admission::from([fold]), &mut pending), None);
    }

    #[test]
    fn target_writes_keep_capacity_that_long_jobs_cannot_take() {
        let ready = lane_for(&UiAction::GithubMarkReady {
            key: "row".into(),
            pin: None,
        })
        .unwrap();
        assert_eq!(ready, Lane::Target(None, "row".into()));
        let long = Admission::from([
            removal("a"),
            removal("b"),
            Lane::Action(None, Some("c".into())),
            Lane::Action(None, Some("d".into())),
        ]);
        let mut pending = VecDeque::from([
            (Lane::Action(None, Some("e".into())), "fifth long job"),
            (ready.clone(), "mark ready"),
        ]);
        assert_eq!(take(&long, &mut pending), Some("mark ready"));
        assert_eq!(take(&long, &mut pending), None);
        // Two per host, and the same checkout still serializes.
        let writing = Admission::from([
            Lane::Target(None, "one".into()),
            Lane::Target(None, "two".into()),
        ]);
        let mut pending = VecDeque::from([
            (Lane::Target(None, "three".into()), "third here"),
            (
                Lane::Target(Some("builder".into()), "three".into()),
                "remote",
            ),
        ]);
        assert_eq!(take(&writing, &mut pending), Some("remote"));
        assert_eq!(take(&writing, &mut pending), None);
        let mut pending = VecDeque::from([(ready.clone(), "same checkout")]);
        assert_eq!(take(&Admission::from([ready]), &mut pending), None);
    }

    #[test]
    fn a_busy_remote_host_does_not_use_up_another_hosts_single_slots() {
        let remote = Some("builder".to_owned());
        for (busy, local) in [
            (Lane::Preparation(remote.clone()), Lane::Preparation(None)),
            (Lane::Prefetch(remote.clone()), Lane::Prefetch(None)),
            (Lane::Creation(remote.clone()), Lane::Creation(None)),
            (
                Lane::Metadata(remote.clone(), "row".into()),
                Lane::Metadata(None, "row".into()),
            ),
            (Lane::HostControl(remote.clone()), Lane::HostControl(None)),
            (Lane::HostControl(remote.clone()), Lane::ViewControl),
        ] {
            let same_host = match &busy {
                Lane::Metadata(host, _) => Lane::Metadata(host.clone(), "other".into()),
                lane => lane.clone(),
            };
            let admission = Admission::from([busy]);
            let mut pending = VecDeque::from([(same_host, "same host"), (local, "local")]);
            assert_eq!(take(&admission, &mut pending), Some("local"));
            assert_eq!(take(&admission, &mut pending), None);
        }
        // Long jobs on a remote do not count against this machine's four.
        let admission = Admission::from(std::array::from_fn::<_, 4, _>(|index| {
            Lane::Action(remote.clone(), Some(format!("row-{index}")))
        }));
        let mut pending = VecDeque::from([
            (Lane::Action(remote, Some("fifth".into())), "remote"),
            (Lane::Action(None, Some("fifth".into())), "local"),
        ]);
        assert_eq!(take(&admission, &mut pending), Some("local"));
        assert_eq!(take(&admission, &mut pending), None);
    }

    #[tokio::test]
    async fn a_durable_removal_excludes_only_its_own_checkout() {
        let mut admission = Admission::default();
        let hold = admission.insert(removal("old"));
        let holds = ["b", "c", "d"].map(|key| admission.insert(removal(key)));
        let waiting = || {
            VecDeque::from([
                (Lane::Host(None), "restack"),
                (Lane::Action(None, Some("other".into())), "action elsewhere"),
            ])
        };
        // Planning: four removals fill the long-job budget and each excludes
        // whole-host work.
        assert_eq!(take(&admission, &mut waiting()), None);
        for hold in &holds {
            hold.durable();
        }
        hold.durable();
        // Each hand-over wakes the scheduler.
        tokio::time::timeout(std::time::Duration::from_secs(1), admission.changed())
            .await
            .expect("a narrowed hold wakes the scheduler");
        let mut pending = waiting();
        assert_eq!(take(&admission, &mut pending), Some("restack"));
        let mut pending = VecDeque::from([
            (removal("old"), "second removal"),
            (Lane::Terminal(None, Some("old".into())), "enter"),
            (Lane::Target(None, "old".into()), "mark ready"),
            (Lane::Metadata(None, "old".into()), "title"),
            (Lane::Action(None, Some("old".into())), "action"),
            (Lane::SessionControl(None, Some("old".into())), "stop"),
            (
                Lane::FleetCleanup(BTreeSet::from([(None, "old".to_owned())])),
                "cleanup",
            ),
        ]);
        assert_eq!(take(&admission, &mut pending), None, "{:?}", pending[0].0);
        // The teardowns themselves stay bounded.
        let mut pending = VecDeque::from([(removal("fifth"), "fifth removal")]);
        assert_eq!(take(&admission, &mut pending), None);
        admission.remove(&removal("old"));
        assert_eq!(take(&admission, &mut pending), Some("fifth removal"));
    }

    #[test]
    fn only_a_removal_can_hand_over() {
        let mut admission = Admission::default();
        admission.insert(Lane::Creation(None)).durable();
        let mut pending = VecDeque::from([(Lane::Host(None), "restack")]);
        assert_eq!(take(&admission, &mut pending), None);
    }

    #[test]
    fn a_queued_whole_host_request_is_not_overtaken_by_later_long_jobs() {
        let mut admission = Admission::from([Lane::Creation(None)]);
        let mut pending = VecDeque::from([
            (Lane::Host(None), "restack"),
            (removal("later"), "later removal"),
            (Lane::Action(None, Some("row".into())), "later action"),
            (Lane::Creation(Some("builder".into())), "creation elsewhere"),
            (Lane::Metadata(None, "row".into()), "title"),
            (Lane::Target(None, "row".into()), "mark ready"),
        ]);
        // Other hosts and short writes pass; later long jobs here do not.
        assert_eq!(take(&admission, &mut pending), Some("creation elsewhere"));
        assert_eq!(take(&admission, &mut pending), Some("title"));
        assert_eq!(take(&admission, &mut pending), Some("mark ready"));
        assert_eq!(take(&admission, &mut pending), None);
        assert_eq!(
            Admission::ahead(&pending, 1).map(|(_, request)| *request),
            Some("restack")
        );
        admission.remove(&Lane::Creation(None));
        assert_eq!(take(&admission, &mut pending), Some("restack"));
        admission.insert(Lane::Host(None));
        assert_eq!(take(&admission, &mut pending), None);
        admission.remove(&Lane::Host(None));
        assert_eq!(take(&admission, &mut pending), Some("later removal"));
    }

    #[test]
    fn an_action_owns_one_checkout_and_stack_operations_own_the_host() {
        let run = |surface| UiAction::RunAction {
            surface,
            id: "any".into(),
            arg: None,
            extras: String::new(),
        };
        let row = lane_for(&run(ActionSurface::Row { key: "row".into() })).unwrap();
        assert_eq!(row, Lane::Action(None, Some("row".into())));
        let remote = lane_for(&run(ActionSurface::Row {
            key: wt_core::remote_worktree_ledger_key("builder", "row"),
        }))
        .unwrap();
        assert_eq!(
            remote,
            Lane::Action(Some("builder".into()), Some("row".into()))
        );
        for surface in [
            ActionSurface::Manager { key: None },
            ActionSurface::Slot {
                target: SessionTarget::WtSource,
            },
        ] {
            assert_eq!(lane_for(&run(surface)).unwrap(), Lane::Action(None, None));
        }
        assert_eq!(
            lane_for(&UiAction::Restack { key: "row".into() }).unwrap(),
            Lane::Host(None)
        );
        for (other, conflict) in [
            (removal("row"), true),
            (removal("other"), false),
            (Lane::Target(None, "row".into()), true),
            (Lane::Target(None, "other".into()), false),
            (Lane::Action(None, Some("row".into())), true),
            (Lane::Action(None, Some("other".into())), false),
            (Lane::Action(None, None), false),
            (Lane::Host(None), true),
            (Lane::Host(Some("builder".into())), false),
            (Lane::Creation(None), false),
            (Lane::Terminal(None, Some("row".into())), false),
            (
                Lane::FleetCleanup(BTreeSet::from([(None, "row".to_owned())])),
                true,
            ),
            (remote.clone(), false),
        ] {
            assert_eq!(lanes_conflict(&row, &other), conflict, "{other:?}");
            assert_eq!(
                lanes_conflict(&other, &row),
                conflict,
                "symmetric {other:?}"
            );
        }
        // A prompt for the manager or a slot holds no checkout in place.
        assert!(!lanes_conflict(&Lane::Action(None, None), &removal("row")));
        assert!(lanes_conflict(&Lane::Action(None, None), &Lane::Host(None)));
    }
}
