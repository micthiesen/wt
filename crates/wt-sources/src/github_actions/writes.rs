//! The write ledger: this host's own GitHub writes, settled against confirmed
//! reads. It keeps the enable/disable no-op guards honest while GitHub's read
//! model lags, and is never published.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use tokio::time::Instant;
use wt_core::Domain;
use wt_github::{GithubData, PullRequest};
use wt_runtime::{Confirmed, Mutation, Pending, PendingPhase, Stamp, Target, TaskScope};

use crate::github_source::GithubRefresh;

/// A write this host made, or may have made, to one PR field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GithubWrite {
    branch: String,
    number: u64,
    change: WriteChange,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WriteChange {
    AutoMerge { enable: bool },
    Ready,
}

impl GithubWrite {
    pub fn auto_merge(branch: &str, number: u64, enable: bool) -> Self {
        Self {
            branch: branch.to_owned(),
            number,
            change: WriteChange::AutoMerge { enable },
        }
    }

    pub(super) fn ready(branch: &str, number: u64) -> Self {
        Self {
            branch: branch.to_owned(),
            number,
            change: WriteChange::Ready,
        }
    }
}

impl Mutation<GithubData> for GithubWrite {
    fn key(&self) -> String {
        let field = match self.change {
            WriteChange::AutoMerge { .. } => "merge",
            WriteChange::Ready => "ready",
        };
        format!("{}#{}:{field}", self.branch, self.number)
    }

    fn target(&self) -> Target {
        Target::row(None, self.branch.clone())
    }

    fn label(&self) -> String {
        match self.change {
            WriteChange::AutoMerge { enable: true } => "arming merge".into(),
            WriteChange::AutoMerge { enable: false } => "disarming merge".into(),
            WriteChange::Ready => "marking ready".into(),
        }
    }

    fn domains(&self) -> &'static [Domain] {
        &[Domain::Github]
    }

    fn apply(&self, view: &mut GithubData) {
        let Some(pr) = view
            .prs
            .get_mut(&self.branch)
            .filter(|pr| pr.number == self.number)
        else {
            return;
        };
        match self.change {
            WriteChange::AutoMerge { enable: true } => {
                if pr.auto_merge.is_none() && !view.merge_queue.contains_key(&self.branch) {
                    pr.auto_merge = Some(wt_github::AutoMerge {
                        enabled_at: String::new(),
                        merge_method: wt_github::AutoMergeMethod::Rebase,
                    });
                }
            }
            WriteChange::AutoMerge { enable: false } => {
                pr.auto_merge = None;
                view.merge_queue.remove(&self.branch);
            }
            WriteChange::Ready => pr.is_draft = false,
        }
    }

    fn observed(&self, confirmed: &GithubData) -> bool {
        // A different or missing PR makes the record irrelevant.
        let Some(pr) = confirmed
            .prs
            .get(&self.branch)
            .filter(|pr| pr.number == self.number)
        else {
            return true;
        };
        match self.change {
            WriteChange::AutoMerge { enable } => armed(confirmed, &self.branch, pr) == enable,
            WriteChange::Ready => !pr.is_draft,
        }
    }
}

/// The write ledger's records: pending, accepted or unconfirmed writes.
pub type LedgerState = Pending<GithubData, GithubWrite>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Guard {
    /// The write would change something; send it.
    Send,
    /// Confirmed data plus this host's own writes already show the result.
    AlreadyDone,
    /// An earlier enable may or may not have happened. Refuse rather than
    /// resend: lost mutation replies are never retried automatically.
    Unconfirmed,
}

/// This host's GitHub writes, settled against confirmed reads, shared by the
/// executor, the retry owner and the confirmer.
#[derive(Clone)]
pub(super) struct WriteLedger {
    state: Arc<Mutex<LedgerState>>,
    next_op: Arc<AtomicU64>,
    pub(super) refresh: GithubRefresh,
    recorded: Arc<tokio::sync::Notify>,
}

/// Spacing of follow-up reads while a write is unconfirmed. The GitHub lane
/// enforces its own minimum interval; this only keeps a read queued.
const CONFIRM_POLL: Duration = Duration::from_secs(5);

/// GitHub's read model lags writes, so the read right after a write may not
/// show it. While any write is unconfirmed, keep asking for direct reads
/// (rate-limited by the GitHub lane) until one confirms it or its deadline
/// passes, so the displayed change is confirmed rather than expiring.
pub(super) fn spawn_confirmer(scope: &TaskScope, ledger: WriteLedger) {
    let cancel = scope.token();
    scope.spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                _ = ledger.recorded.notified() => {}
            }
            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(CONFIRM_POLL) => {}
                }
                let open = {
                    let mut state = ledger.lock();
                    if let Some(github) = ledger.refresh.confirmed()
                        && settle(&mut state, &github, Instant::now())
                    {
                        let read = ledger.refresh.last_read();
                        tracing::info!(
                            read_started = ?read.started,
                            read_finished = ?read.finished,
                            observed = ?github.observed(),
                            "latest direct GitHub read when a write expired"
                        );
                    } else if state.next_deadline().is_some_and(|d| d <= Instant::now()) {
                        // No live read at all: let deadlines lapse.
                        state.settle(&GithubData::default(), |_, _| None, Instant::now());
                    }
                    !state.is_empty()
                };
                if !open {
                    break;
                }
                ledger.refresh.refresh_live();
            }
        }
    });
}

impl WriteLedger {
    pub(super) fn new(refresh: GithubRefresh) -> Self {
        Self {
            state: Arc::default(),
            next_op: Arc::new(AtomicU64::new(1)),
            refresh,
            recorded: Arc::default(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LedgerState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Record a write GitHub acknowledged just now and ask for a direct read,
    /// not the events daemon's older snapshot, to confirm it.
    pub(super) fn accepted(&self, write: GithubWrite) -> Stamp {
        let at = Stamp::now();
        record_accepted(&mut self.lock(), self.op(), write, at);
        self.refresh.refresh_live();
        self.recorded.notify_one();
        at
    }

    pub(super) fn uncertain(&self, write: GithubWrite) {
        let op = self.op();
        let now = Instant::now();
        let mut state = self.lock();
        state.begin(op, write, now);
        state.uncertain(op, now);
        drop(state);
        self.refresh.refresh_live();
        self.recorded.notify_one();
    }

    fn op(&self) -> u64 {
        self.next_op.fetch_add(1, Ordering::Relaxed)
    }

    pub(super) fn guard(
        &self,
        github: &Confirmed<GithubData>,
        branch: &str,
        number: u64,
        enable: bool,
    ) -> Guard {
        auto_merge_guard(
            &mut self.lock(),
            github,
            branch,
            number,
            enable,
            Instant::now(),
        )
    }

    /// Confirmed data with this host's unsettled writes applied.
    pub(super) fn effective(&self, github: &Confirmed<GithubData>) -> GithubData {
        let mut state = self.lock();
        settle(&mut state, github, Instant::now());
        state.project(github)
    }
}

pub(super) fn record_accepted(state: &mut LedgerState, op: u64, write: GithubWrite, at: Stamp) {
    let now = Instant::now();
    state.begin(op, write, now);
    state.accept(op, at, now);
}

/// Drop records a read that started after them shows, and expired ones.
/// Returns whether any record expired unconfirmed.
fn settle(state: &mut LedgerState, github: &Confirmed<GithubData>, now: Instant) -> bool {
    let observed = github.observed();
    let expired = state.settle(github, |_, _| Some(observed), now);
    for expired in &expired {
        tracing::info!(
            write = %expired.mutation.key(),
            phase = ?expired.phase,
            accepted_at = ?expired.accepted_at,
            ?observed,
            "GitHub write not yet reflected by GitHub reads"
        );
    }
    !expired.is_empty()
}

/// Disable is sent whenever this host's writes say the PR may be armed;
/// enable is refused while an earlier enable is unconfirmed.
fn auto_merge_guard(
    state: &mut LedgerState,
    github: &Confirmed<GithubData>,
    branch: &str,
    number: u64,
    enable: bool,
    now: Instant,
) -> Guard {
    settle(state, github, now);
    // A read that already shows the PR armed answers an arm request, whatever
    // earlier attempt is still unconfirmed.
    let confirmed_armed =
        effective_pr(github, branch, number).is_some_and(|pr| armed(github, branch, pr));
    let unconfirmed = |wanted: bool| {
        state.mutations().zip(state.views()).any(|(write, view)| {
            view.phase == PendingPhase::Unconfirmed
                && write.branch == branch
                && write.number == number
                && write.change == WriteChange::AutoMerge { enable: wanted }
        })
    };
    // A read cannot answer an arm while one of our disarms is unconfirmed:
    // it may predate the disarm.
    if enable && confirmed_armed && !unconfirmed(false) {
        return Guard::AlreadyDone;
    }
    if enable && unconfirmed(true) {
        return Guard::Unconfirmed;
    }
    let effective = state.project(github);
    let effective_armed =
        effective_pr(&effective, branch, number).is_some_and(|pr| armed(&effective, branch, pr));
    if enable {
        return if effective_armed {
            Guard::AlreadyDone
        } else {
            Guard::Send
        };
    }
    // An unconfirmed disable may not have happened: trust the read then.
    if effective_armed || (confirmed_armed && unconfirmed(false)) {
        Guard::Send
    } else {
        Guard::AlreadyDone
    }
}

/// The PR on `branch` only while it is still PR `number`.
pub(super) fn effective_pr<'a>(
    data: &'a GithubData,
    branch: &str,
    number: u64,
) -> Option<&'a PullRequest> {
    data.prs.get(branch).filter(|pr| pr.number == number)
}

/// Classic auto-merge or a merge-queue entry.
fn armed(data: &GithubData, branch: &str, pr: &PullRequest) -> bool {
    pr.auto_merge.is_some() || data.merge_queue.contains_key(branch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github_actions::tests::{
        ledger_with_accepted_enable, open_pr_data, pr, with_auto_merge,
    };

    #[test]
    fn ledger_projection_touches_only_its_pr_and_settles_on_matching_data() {
        let mut data = open_pr_data("feature/one");
        let mut second = with_auto_merge(open_pr_data("feature/two")).prs["feature/two"].clone();
        second.number = 8;
        data.prs.insert("feature/two".into(), second);
        data.merge_queue.insert(
            "feature/two".into(),
            wt_github::MergeQueueEntry {
                head_ref_name: "feature/two".into(),
                position: 3,
                state: wt_github::MergeQueueState::Queued,
                enqueued_at: "now".into(),
                estimated_time_to_merge: None,
            },
        );
        let mut view = data.clone();
        GithubWrite::ready("feature/one", 7).apply(&mut view);
        GithubWrite::auto_merge("feature/two", 8, false).apply(&mut view);
        // A record for another PR number on the same branch is inert.
        GithubWrite::auto_merge("feature/one", 99, true).apply(&mut view);
        assert!(!view.prs["feature/one"].is_draft);
        assert!(view.prs["feature/one"].auto_merge.is_none());
        assert!(view.prs["feature/two"].auto_merge.is_none());
        assert!(!view.merge_queue.contains_key("feature/two"));
        assert!(GithubWrite::ready("feature/one", 7).observed(&view));
        assert!(!GithubWrite::ready("feature/one", 7).observed(&data));
        assert!(GithubWrite::auto_merge("feature/two", 8, false).observed(&view));
        assert!(!GithubWrite::auto_merge("feature/two", 8, false).observed(&data));
        assert!(GithubWrite::auto_merge("gone", 1, true).observed(&data));
    }

    /// Arm, then disarm before any read shows the arm. The lagging read says
    /// "not armed", but this host armed it, so disable is sent.
    #[tokio::test(start_paused = true)]
    async fn arm_then_disarm_within_the_github_floor_sends_disable() {
        let _ = Stamp::now();
        tokio::time::advance(Duration::from_secs(1)).await;
        let stale = Confirmed::for_test(open_pr_data("feature"));
        tokio::time::advance(Duration::from_millis(10)).await;
        let mut ledger = ledger_with_accepted_enable("feature", 7, Stamp::now());
        tokio::time::advance(Duration::from_secs(3)).await;
        // Without the ledger the read alone would call this a no-op.
        assert_eq!(
            auto_merge_guard(
                &mut LedgerState::default(),
                &stale,
                "feature",
                7,
                false,
                Instant::now()
            ),
            Guard::AlreadyDone
        );
        assert_eq!(
            auto_merge_guard(&mut ledger, &stale, "feature", 7, false, Instant::now()),
            Guard::Send
        );
        // A second arm is a no-op, not a resend.
        assert_eq!(
            auto_merge_guard(&mut ledger, &stale, "feature", 7, true, Instant::now()),
            Guard::AlreadyDone
        );
        // After the disable is accepted, a repeated disarm is a no-op and a
        // re-arm is sent, still before any read caught up.
        record_accepted(
            &mut ledger,
            2,
            GithubWrite::auto_merge("feature", 7, false),
            Stamp::now(),
        );
        assert_eq!(
            auto_merge_guard(&mut ledger, &stale, "feature", 7, false, Instant::now()),
            Guard::AlreadyDone
        );
        assert_eq!(
            auto_merge_guard(&mut ledger, &stale, "feature", 7, true, Instant::now()),
            Guard::Send
        );
    }

    #[tokio::test(start_paused = true)]
    async fn github_data_meets_the_persisted_source_contract() {
        let mut data = with_auto_merge(open_pr_data("feature"));
        data.prs.insert("other".into(), pr("other"));
        wt_runtime::conformance::persisted_source("github", wt_core::Domain::Github, data).await;
    }

    #[tokio::test(start_paused = true)]
    async fn every_ledger_write_meets_the_mutation_contract() {
        let draft = open_pr_data("feature");
        let mut ready = draft.clone();
        ready.prs.get_mut("feature").unwrap().is_draft = false;
        let armed = with_auto_merge(draft.clone());
        wt_runtime::conformance::mutation(
            GithubWrite::auto_merge("feature", 7, true),
            draft.clone(),
            armed.clone(),
        )
        .await;
        wt_runtime::conformance::mutation(
            GithubWrite::auto_merge("feature", 7, false),
            armed,
            draft.clone(),
        )
        .await;
        wt_runtime::conformance::mutation(GithubWrite::ready("feature", 7), draft, ready).await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_read_cannot_answer_an_arm_while_a_disarm_is_unconfirmed() {
        let armed = Confirmed::for_test(with_auto_merge(open_pr_data("feature")));
        let mut ledger = LedgerState::default();
        ledger.begin(
            1,
            GithubWrite::auto_merge("feature", 7, false),
            Instant::now(),
        );
        ledger.uncertain(1, Instant::now());
        assert_eq!(
            auto_merge_guard(&mut ledger, &armed, "feature", 7, true, Instant::now()),
            Guard::Send
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_read_showing_the_arm_answers_an_unconfirmed_enable() {
        let armed = Confirmed::for_test(with_auto_merge(open_pr_data("feature")));
        let mut ledger = LedgerState::default();
        ledger.begin(
            1,
            GithubWrite::auto_merge("feature", 7, true),
            Instant::now(),
        );
        ledger.uncertain(1, Instant::now());
        assert_eq!(
            auto_merge_guard(&mut ledger, &armed, "feature", 7, true, Instant::now()),
            Guard::AlreadyDone
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_unconfirmed_enable_refuses_enable_and_still_allows_disable() {
        let stale = Confirmed::for_test(open_pr_data("feature"));
        let mut ledger = LedgerState::default();
        ledger.begin(
            1,
            GithubWrite::auto_merge("feature", 7, true),
            Instant::now(),
        );
        ledger.uncertain(1, Instant::now());
        assert_eq!(
            auto_merge_guard(&mut ledger, &stale, "feature", 7, true, Instant::now()),
            Guard::Unconfirmed
        );
        assert_eq!(
            auto_merge_guard(&mut ledger, &stale, "feature", 7, false, Instant::now()),
            Guard::Send
        );
        // An unconfirmed disable keeps a confirmed arm disarmable.
        let armed = Confirmed::for_test(with_auto_merge(open_pr_data("feature")));
        let mut ledger = LedgerState::default();
        ledger.begin(
            1,
            GithubWrite::auto_merge("feature", 7, false),
            Instant::now(),
        );
        ledger.uncertain(1, Instant::now());
        assert_eq!(
            auto_merge_guard(&mut ledger, &armed, "feature", 7, false, Instant::now()),
            Guard::Send
        );
        // The record expires at its deadline; the read decides again.
        tokio::time::advance(Duration::from_secs(31)).await;
        assert_eq!(
            auto_merge_guard(&mut ledger, &stale, "feature", 7, false, Instant::now()),
            Guard::AlreadyDone
        );
    }
}
