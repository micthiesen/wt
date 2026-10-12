//! Host-local GitHub actions used by the TUI controller.
//!
//! Mutations go through `wt-github`; this module owns their bounded retry
//! lifetime and a private ledger of the writes it made. GitHub's read model
//! lags writes, so the ledger keeps the enable/disable no-op guards and retry
//! cancellation honest until a read that started after a write shows it. The
//! ledger is never published: displays, automations and planners read only
//! confirmed GitHub data.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::Result;
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinSet,
    time::{Instant, sleep},
};
use tokio_util::sync::CancellationToken;
use wt_github::{GithubClient, GithubData, GithubOptions, PrChecks, PrMergeTarget, PullRequest};
use wt_runtime::{Confirmed, Domain, Mutation, Pending, PendingPhase, Stamp, Target, TaskScope};
use wt_tui::{Board, PrPin, ReplyOutcome, UiModal, UiReply};

use crate::{context::AppContext, sources::GithubRefresh};

const RETRY_EVERY: Duration = Duration::from_secs(30);
const RETRY_LIMIT: Duration = Duration::from_secs(20 * 60);
const MAX_RETRIES: usize = 4;
const FAILED_LOG_LIMIT: usize = 200;
/// The write named a PR the host cannot confirm as the one the user saw.
pub(crate) const STALE_PIN: &str = "PR changed or still loading; review again";
const UNCONFIRMED_ENABLE: &str = "previous attempt unconfirmed; waiting for refresh";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GithubAction {
    MarkReady {
        key: String,
        pin: Option<PrPin>,
    },
    SetAutoMerge {
        key: String,
        enable: bool,
        pin: Option<PrPin>,
    },
    Ship {
        key: String,
        pin: Option<PrPin>,
    },
    FailedChecks {
        key: String,
    },
}

#[derive(Clone)]
pub struct GithubActions {
    client: GithubClient,
    writes: Writes,
    retry_tx: mpsc::Sender<RetryCommand>,
    cancellation: CancellationToken,
    reviewers_enabled: bool,
    default_reviewer: Option<String>,
}

impl GithubActions {
    /// Starts the single scoped retry owner.
    pub fn start(scope: &TaskScope, context: &AppContext, refresh: GithubRefresh) -> Self {
        let client = GithubClient::new(
            context.processes.clone(),
            context.config.paths.main_clone.clone(),
            GithubOptions::from_config(&context.config, false),
        );
        let writes = Writes::new(refresh);
        let (retry_tx, retry_rx) = mpsc::channel(16);
        spawn_retry_owner(
            scope,
            client.clone(),
            writes.clone(),
            scope.token(),
            retry_rx,
        );
        Self {
            client,
            writes,
            retry_tx,
            cancellation: context.cancellation.clone(),
            reviewers_enabled: context.config.github.reviewers,
            default_reviewer: context.config.github.default_reviewer.clone(),
        }
    }

    /// The board resolves the row only; PR identity, head and state come
    /// from confirmed GitHub data, checked against the PR the user saw.
    pub async fn execute(
        &self,
        action: GithubAction,
        board: &Board,
        github: Option<&Confirmed<GithubData>>,
    ) -> Result<UiReply> {
        let key = action.key();
        let Some(row) = resolve_row(board, key) else {
            return Ok(failed(format!(
                "worktree {key:?} is no longer on the board"
            )));
        };
        let branch = row.branch.clone();
        if let Some(pin) = action.write_pin()
            && !pin_matches(pin, github.and_then(|data| data.prs.get(&branch)))
        {
            return Ok(failed(STALE_PIN));
        }
        let Some(github) = github else {
            return Ok(failed("GitHub data is still loading"));
        };
        let Some(pr) = github.prs.get(&branch) else {
            return Ok(UiReply {
                message: "no PR for this row".into(),
                notice: true,
                outcome: Some(ReplyOutcome::NoChange),
                ..UiReply::default()
            });
        };
        if pr.state != "OPEN" {
            return Ok(failed(format!(
                "PR #{} is {}",
                pr.number,
                pr.state.to_lowercase()
            )));
        }

        match action {
            GithubAction::MarkReady { .. } => self.mark_ready(pr, &branch, github).await,
            GithubAction::SetAutoMerge { enable, .. } => {
                self.set_auto_merge(pr, &branch, github, enable).await
            }
            GithubAction::Ship { .. } => self.ship(pr, &branch, github).await,
            GithubAction::FailedChecks { .. } => self.failed_checks(pr, &branch).await,
        }
    }

    async fn mark_ready(
        &self,
        pr: &PullRequest,
        branch: &str,
        github: &Confirmed<GithubData>,
    ) -> Result<UiReply> {
        let number = pr.number;
        let effective = self.writes.effective(github);
        if !effective_pr(&effective, branch, number).is_some_and(|pr| pr.is_draft) {
            return Ok(no_change(format!("PR #{number} is already ready")));
        }
        let result = self
            .client
            .mark_pull_request_ready(number, &self.cancellation)
            .await;
        let write = GithubWrite::ready(branch, number);
        if !result.ok {
            return Ok(self.write_failed(
                write,
                result.error.unwrap_or_else(|| "mark ready failed".into()),
            ));
        }
        let at = self.writes.accepted(write);
        Ok(accepted(
            format!("marked PR #{number} ready for review"),
            at,
        ))
    }

    async fn set_auto_merge(
        &self,
        pr: &PullRequest,
        branch: &str,
        github: &Confirmed<GithubData>,
        enable: bool,
    ) -> Result<UiReply> {
        let number = pr.number;
        if enable {
            match self.writes.guard(github, branch, number, true) {
                Guard::Send => {}
                Guard::AlreadyDone => {
                    return Ok(no_change(format!(
                        "merge when ready is already armed for PR #{number}"
                    )));
                }
                Guard::Unconfirmed => return Ok(failed(UNCONFIRMED_ENABLE)),
            }
            if self.retry_pending(number).await? {
                return Ok(deferred(format!(
                    "PR #{number} is already waiting on checks"
                )));
            }
            let Some(target) = merge_target(pr) else {
                return Ok(failed(format!(
                    "PR #{number} cache lacks its id or head SHA; refresh GitHub and retry"
                )));
            };
            let result = self
                .client
                .enable_auto_merge(&target, &self.cancellation)
                .await;
            let write = GithubWrite::auto_merge(branch, number, true);
            if result.ok {
                let at = self.writes.accepted(write);
                return Ok(accepted(
                    format!("merge when ready armed for PR #{number}"),
                    at,
                ));
            }
            if result.retryable == Some(true) {
                match self.start_retry(target.clone(), branch.to_owned()).await? {
                    RetryStart::Started => {
                        return Ok(deferred(format!(
                            "PR #{number}: required checks have not reported; will arm when they do"
                        )));
                    }
                    RetryStart::AlreadyRunning => {
                        return Ok(deferred(format!(
                            "PR #{number} is already waiting on checks"
                        )));
                    }
                    RetryStart::AtCapacity => {
                        return Ok(failed(
                            "four GitHub merge retries are already active; try again shortly",
                        ));
                    }
                }
            }
            return Ok(self.write_failed(
                write,
                result.error.unwrap_or_else(|| "auto-merge failed".into()),
            ));
        }

        let cancellation = self.cancel_retry(number).await?;
        if let Some(result) = self.finish_retry_cancel(pr, branch, cancellation).await {
            return Ok(result);
        }
        // Sent whenever this host's own writes say the PR may be armed, even
        // when the lagging read still shows it disarmed.
        if self.writes.guard(github, branch, number, false) != Guard::Send {
            return Ok(no_change(format!(
                "merge when ready is not armed for PR #{number}"
            )));
        }
        self.disable(pr, branch).await
    }

    async fn disable(&self, pr: &PullRequest, branch: &str) -> Result<UiReply> {
        let number = pr.number;
        let Some(target) = merge_target_for_disable(pr) else {
            return Ok(failed(format!(
                "cannot inspect PR #{number}: missing PR node id"
            )));
        };
        let result = self
            .client
            .disable_auto_merge(&target, &self.cancellation)
            .await;
        let write = GithubWrite::auto_merge(branch, number, false);
        if !result.ok {
            return Ok(self.write_failed(
                write,
                result
                    .error
                    .unwrap_or_else(|| "disable auto-merge failed".into()),
            ));
        }
        let at = self.writes.accepted(write);
        Ok(accepted(
            format!("cancelled merge-when-ready for PR #{number}"),
            at,
        ))
    }

    async fn finish_retry_cancel(
        &self,
        pr: &PullRequest,
        branch: &str,
        cancellation: RetryCancel,
    ) -> Option<UiReply> {
        let number = pr.number;
        match cancellation {
            RetryCancel::NotPending => None,
            RetryCancel::Cancelled => Some(no_change(format!(
                "cancelled pending merge-when-ready arm for PR #{number}"
            ))),
            RetryCancel::Armed => {
                // Cancellation raced with a successful in-flight request. Wait
                // for that request to finish, then remove the actual queue or
                // classic arm before acknowledging the request.
                let Some(target) = merge_target_for_disable(pr) else {
                    return Some(failed(format!(
                        "PR #{number} armed while cancellation was in flight, but its node id is missing; refresh and inspect the PR"
                    )));
                };
                let result = self
                    .client
                    .disable_auto_merge(&target, &self.cancellation)
                    .await;
                let write = GithubWrite::auto_merge(branch, number, false);
                if !result.ok {
                    return Some(self.write_failed(
                        write,
                        format!(
                            "PR #{number} armed while cancellation was in flight; could not disarm it: {}",
                            result
                                .error
                                .unwrap_or_else(|| "unknown GitHub error".into())
                        ),
                    ));
                }
                let at = self.writes.accepted(write);
                Some(accepted(
                    format!("cancelled pending arm and disarmed merge-when-ready for PR #{number}"),
                    at,
                ))
            }
            RetryCancel::Failed(error) => Some(failed(format!(
                "pending merge request for PR #{number} ended while cancellation was in flight: {error}; refresh GitHub to inspect its state"
            ))),
            RetryCancel::GaveUp => Some(no_change(format!(
                "pending merge-when-ready retry for PR #{number} stopped waiting"
            ))),
        }
    }

    async fn ship(
        &self,
        pr: &PullRequest,
        branch: &str,
        github: &Confirmed<GithubData>,
    ) -> Result<UiReply> {
        let number = pr.number;
        let effective = self.writes.effective(github);
        let shown = effective_pr(&effective, branch, number).unwrap_or(pr);
        let was_draft = shown.is_draft;
        let reviewer = self
            .reviewers_enabled
            .then_some(self.default_reviewer.as_deref())
            .flatten()
            .filter(|login| {
                !pr.requested_reviewers
                    .iter()
                    .any(|existing| existing == *login)
            })
            .map(str::to_owned);
        // Preserve the existing TUI's ship contract: it checks classic auto
        // merge, but not a queue entry, before deciding whether arming is needed.
        let needs_arm = shown.auto_merge.is_none();
        let target = if needs_arm {
            match merge_target(pr) {
                Some(target) => Some(target),
                None => {
                    return Ok(failed(format!(
                        "PR #{number} cache lacks its id or head SHA; refresh GitHub and retry"
                    )));
                }
            }
        } else {
            None
        };
        if !was_draft && reviewer.is_none() && !needs_arm {
            return Ok(no_change(format!("PR #{number} is already shipped")));
        }

        let mark_ready = async {
            if was_draft {
                Some(
                    self.client
                        .mark_pull_request_ready(number, &self.cancellation)
                        .await,
                )
            } else {
                None
            }
        };
        let request_reviewer = async {
            if let Some(login) = reviewer.as_ref() {
                Some(
                    self.client
                        .edit_reviewers(
                            number,
                            std::slice::from_ref(login),
                            &[],
                            &self.cancellation,
                        )
                        .await,
                )
            } else {
                None
            }
        };
        let (ready_result, reviewer_result) = tokio::join!(mark_ready, request_reviewer);
        let mut at = None;
        if reviewer_result.as_ref().is_some_and(|result| result.ok) {
            at = Some(Stamp::now());
            self.writes.refresh.refresh_live();
        }
        if let Some(result) = ready_result {
            let write = GithubWrite::ready(branch, number);
            if !result.ok {
                return Ok(self.write_failed(
                    write,
                    result.error.unwrap_or_else(|| "mark ready failed".into()),
                ));
            }
            at = Some(self.writes.accepted(write));
        }

        let reviewer_error = reviewer_result
            .filter(|result| !result.ok)
            .and_then(|result| result.error)
            .map(|error| format!("reviewer request failed: {error}"));

        if let Some(target) = target.as_ref() {
            let result = self
                .client
                .enable_auto_merge(target, &self.cancellation)
                .await;
            let write = GithubWrite::auto_merge(branch, number, true);
            if !result.ok {
                let mut message = result.error.unwrap_or_else(|| "auto-merge failed".into());
                if let Some(reviewer_error) = reviewer_error {
                    message.push_str("; ");
                    message.push_str(&reviewer_error);
                }
                return Ok(self.write_failed(write, message));
            }
            at = Some(self.writes.accepted(write));
        }
        let message = match reviewer_error {
            Some(error) => format!("shipped PR #{number}; {error}"),
            None => format!("shipped PR #{number}"),
        };
        Ok(match at {
            Some(at) => accepted(message, at),
            // Only the reviewer request ran, and it failed.
            None => failed(message),
        })
    }

    async fn failed_checks(&self, pr: &PullRequest, branch: &str) -> Result<UiReply> {
        if pr.checks != PrChecks::Fail {
            return Ok(reply("no failing checks"));
        }
        let (run_id, mut lines) = match self
            .client
            .fetch_failed_run_log(branch, &self.cancellation)
            .await
        {
            Ok(result) => result,
            Err(error) => return Ok(failed(format!("failed to fetch CI logs: {error}"))),
        };
        if lines.len() > FAILED_LOG_LIMIT {
            lines.truncate(FAILED_LOG_LIMIT);
            lines.push(format!("… truncated at {FAILED_LOG_LIMIT} lines; run `gh run view {run_id} --log-failed` for the rest"));
        }
        Ok(UiReply {
            message: format!("failed CI logs for {} (run {run_id})", pr.head_ref_name),
            modal: Some(UiModal::Log {
                title: format!("Failed checks: {} #{run_id}", pr.head_ref_name),
                lines,
                close_key: Some('f'),
                refresh: None,
            }),
            ..UiReply::default()
        })
    }

    /// A failed write is definite unless the command drain cancelled it
    /// mid-request: then GitHub may have applied it, so it stays recorded as
    /// unconfirmed and is never resent automatically.
    fn write_failed(&self, write: GithubWrite, error: String) -> UiReply {
        if !self.cancellation.is_cancelled() {
            return failed(error);
        }
        self.writes.uncertain(write);
        UiReply {
            message: format!("{error}; the request may have reached GitHub and is unconfirmed"),
            outcome: Some(ReplyOutcome::Uncertain),
            ..UiReply::default()
        }
    }

    async fn start_retry(&self, target: PrMergeTarget, branch: String) -> Result<RetryStart> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.retry_tx
            .send(RetryCommand::Start {
                target,
                branch,
                reply: reply_tx,
            })
            .await
            .map_err(|_| anyhow::anyhow!("GitHub retry owner has stopped"))?;
        Ok(reply_rx.await.unwrap_or(RetryStart::AtCapacity))
    }

    async fn retry_pending(&self, number: u64) -> Result<bool> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.retry_tx
            .send(RetryCommand::IsPending {
                number,
                reply: reply_tx,
            })
            .await
            .map_err(|_| anyhow::anyhow!("GitHub retry owner has stopped"))?;
        Ok(reply_rx.await.unwrap_or(false))
    }

    async fn cancel_retry(&self, number: u64) -> Result<RetryCancel> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.retry_tx
            .send(RetryCommand::Cancel {
                number,
                reply: reply_tx,
            })
            .await
            .map_err(|_| anyhow::anyhow!("GitHub retry owner has stopped"))?;
        Ok(reply_rx.await.unwrap_or(RetryCancel::NotPending))
    }
}

/// A write this host made, or may have made, to one PR field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GithubWrite {
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
    pub(crate) fn auto_merge(branch: &str, number: u64, enable: bool) -> Self {
        Self {
            branch: branch.to_owned(),
            number,
            change: WriteChange::AutoMerge { enable },
        }
    }

    fn ready(branch: &str, number: u64) -> Self {
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

pub(crate) type Ledger = Pending<GithubData, GithubWrite>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Guard {
    /// The write would change something; send it.
    Send,
    /// Confirmed data plus this host's own writes already show the result.
    AlreadyDone,
    /// An earlier enable may or may not have happened. Refuse rather than
    /// resend: lost mutation replies are never retried automatically.
    Unconfirmed,
}

/// This host's GitHub writes, settled against confirmed reads.
#[derive(Clone)]
struct Writes {
    ledger: Arc<Mutex<Ledger>>,
    next_op: Arc<AtomicU64>,
    refresh: GithubRefresh,
}

impl Writes {
    fn new(refresh: GithubRefresh) -> Self {
        Self {
            ledger: Arc::default(),
            next_op: Arc::new(AtomicU64::new(1)),
            refresh,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Ledger> {
        self.ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Record a write GitHub acknowledged just now and ask for a direct read,
    /// not the events daemon's older snapshot, to confirm it.
    fn accepted(&self, write: GithubWrite) -> Stamp {
        let at = Stamp::now();
        record_accepted(&mut self.lock(), self.op(), write, at);
        self.refresh.refresh_live();
        at
    }

    fn uncertain(&self, write: GithubWrite) {
        let op = self.op();
        let now = Instant::now();
        let mut ledger = self.lock();
        ledger.begin(op, write, now);
        ledger.uncertain(op, now);
        drop(ledger);
        self.refresh.refresh_live();
    }

    fn op(&self) -> u64 {
        self.next_op.fetch_add(1, Ordering::Relaxed)
    }

    fn guard(
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

    fn effective(&self, github: &Confirmed<GithubData>) -> GithubData {
        let mut ledger = self.lock();
        settle(&mut ledger, github, Instant::now());
        ledger.project(github)
    }
}

fn record_accepted(ledger: &mut Ledger, op: u64, write: GithubWrite, at: Stamp) {
    let now = Instant::now();
    ledger.begin(op, write, now);
    ledger.accept(op, at, now);
}

/// Drop records a read that started after them shows, and expired ones.
fn settle(ledger: &mut Ledger, github: &Confirmed<GithubData>, now: Instant) {
    let observed = github.observed();
    for expired in ledger.settle(github, |_, _| Some(observed), now) {
        tracing::info!(
            write = %expired.mutation.key(),
            phase = ?expired.phase,
            "GitHub write not yet reflected by GitHub reads"
        );
    }
}

/// D3: disable is sent whenever this host's writes say the PR may be armed;
/// enable is refused while an earlier enable is unconfirmed.
fn auto_merge_guard(
    ledger: &mut Ledger,
    github: &Confirmed<GithubData>,
    branch: &str,
    number: u64,
    enable: bool,
    now: Instant,
) -> Guard {
    settle(ledger, github, now);
    let unconfirmed = |wanted: bool| {
        ledger.mutations().zip(ledger.views()).any(|(write, view)| {
            view.phase == PendingPhase::Unconfirmed
                && write.branch == branch
                && write.number == number
                && write.change == WriteChange::AutoMerge { enable: wanted }
        })
    };
    if enable && unconfirmed(true) {
        return Guard::Unconfirmed;
    }
    let effective = ledger.project(github);
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
    let confirmed_armed =
        effective_pr(github, branch, number).is_some_and(|pr| armed(github, branch, pr));
    if effective_armed || (confirmed_armed && unconfirmed(false)) {
        Guard::Send
    } else {
        Guard::AlreadyDone
    }
}

fn effective_pr<'a>(data: &'a GithubData, branch: &str, number: u64) -> Option<&'a PullRequest> {
    data.prs.get(branch).filter(|pr| pr.number == number)
}

fn armed(data: &GithubData, branch: &str, pr: &PullRequest) -> bool {
    pr.auto_merge.is_some() || data.merge_queue.contains_key(branch)
}

/// The PR the user saw must be the PR confirmed data names now, at the same
/// head. With neither a pin nor a PR there is nothing to write to.
fn pin_matches(pin: Option<&PrPin>, pr: Option<&PullRequest>) -> bool {
    match (pin, pr) {
        (None, None) => true,
        (Some(pin), Some(pr)) => pin.number == pr.number && pin.head_sha == pr.head_ref_oid,
        _ => false,
    }
}

enum RetryCommand {
    Start {
        target: PrMergeTarget,
        branch: String,
        reply: oneshot::Sender<RetryStart>,
    },
    IsPending {
        number: u64,
        reply: oneshot::Sender<bool>,
    },
    Cancel {
        number: u64,
        reply: oneshot::Sender<RetryCancel>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RetryStart {
    Started,
    AlreadyRunning,
    AtCapacity,
}

struct RetryFinished {
    number: u64,
    result: RetryOutcome,
}

enum RetryOutcome {
    Armed,
    Failed(String),
    GaveUp,
    Cancelled,
}

#[derive(Clone, Debug)]
enum RetryCancel {
    NotPending,
    Cancelled,
    Armed,
    Failed(String),
    GaveUp,
}

fn retry_cancel_result(outcome: &RetryOutcome) -> RetryCancel {
    match outcome {
        RetryOutcome::Armed => RetryCancel::Armed,
        RetryOutcome::Failed(error) => RetryCancel::Failed(error.clone()),
        RetryOutcome::GaveUp => RetryCancel::GaveUp,
        RetryOutcome::Cancelled => RetryCancel::Cancelled,
    }
}

fn spawn_retry_owner(
    scope: &TaskScope,
    client: GithubClient,
    writes: Writes,
    cancellation: CancellationToken,
    mut commands: mpsc::Receiver<RetryCommand>,
) {
    scope.spawn(async move {
        let mut jobs: JoinSet<RetryFinished> = JoinSet::new();
        let mut active: HashMap<u64, CancellationToken> = HashMap::new();
        let mut cancel_waiters: HashMap<u64, Vec<oneshot::Sender<RetryCancel>>> = HashMap::new();
        loop {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => break,
                command = commands.recv() => {
                    let Some(command) = command else { break; };
                    match command {
                        RetryCommand::Start { target, branch, reply } => {
                            let result = if active.contains_key(&target.number) {
                                RetryStart::AlreadyRunning
                            } else if active.len() >= MAX_RETRIES {
                                RetryStart::AtCapacity
                            } else {
                                let token = cancellation.child_token();
                                active.insert(target.number, token.clone());
                                let client = client.clone();
                                let writes = writes.clone();
                                jobs.spawn(async move {
                                    let result = retry_arm(client, writes, target.clone(), branch, token).await;
                                    RetryFinished { number: target.number, result }
                                });
                                RetryStart::Started
                            };
                            let _ = reply.send(result);
                        }
                        RetryCommand::IsPending { number, reply } => {
                            let _ = reply.send(active.contains_key(&number));
                        }
                        RetryCommand::Cancel { number, reply } => {
                            if let Some(token) = active.get(&number) {
                                token.cancel();
                                cancel_waiters.entry(number).or_default().push(reply);
                            } else {
                                let _ = reply.send(RetryCancel::NotPending);
                            }
                        }
                    }
                }
                Some(joined) = jobs.join_next(), if !jobs.is_empty() => {
                    if let Ok(finished) = joined {
                        active.remove(&finished.number);
                        if let Some(waiters) = cancel_waiters.remove(&finished.number) {
                            let result = retry_cancel_result(&finished.result);
                            for waiter in waiters { let _ = waiter.send(result.clone()); }
                        }
                        match finished.result {
                            RetryOutcome::Armed => tracing::info!(pr = finished.number, "armed merge when ready after checks appeared"),
                            RetryOutcome::Failed(error) => tracing::warn!(pr = finished.number, %error, event_channel = "attention", event_text = %format_args!("#{}: merge when ready failed: {error}", finished.number), "background merge-when-ready retry failed"),
                            RetryOutcome::GaveUp => tracing::warn!(pr = finished.number, event_channel = "attention", event_text = %format_args!("#{}: merge when ready gave up waiting for required checks", finished.number), "gave up waiting for required checks to appear"),
                            RetryOutcome::Cancelled => tracing::info!(pr = finished.number, "cancelled pending merge-when-ready retry"),
                        }
                    }
                }
            }
        }
        for token in active.values() { token.cancel(); }
        while let Some(joined) = jobs.join_next().await {
            if let Ok(finished) = joined
                && let Some(waiters) = cancel_waiters.remove(&finished.number)
            {
                let result = retry_cancel_result(&finished.result);
                for waiter in waiters { let _ = waiter.send(result.clone()); }
            }
        }
        for waiters in cancel_waiters.into_values() {
            for waiter in waiters { let _ = waiter.send(RetryCancel::Cancelled); }
        }
    });
}

async fn retry_arm(
    client: GithubClient,
    writes: Writes,
    target: PrMergeTarget,
    branch: String,
    cancellation: CancellationToken,
) -> RetryOutcome {
    let started = Instant::now();
    loop {
        tokio::select! {
            _ = cancellation.cancelled() => return RetryOutcome::Cancelled,
            _ = sleep(RETRY_EVERY) => {}
        }
        if started.elapsed() >= RETRY_LIMIT {
            return RetryOutcome::GaveUp;
        }
        let result = client.enable_auto_merge(&target, &cancellation).await;
        if result.ok {
            // Recorded so a disarm before the next read still sends disable.
            writes.accepted(GithubWrite::auto_merge(&branch, target.number, true));
            return RetryOutcome::Armed;
        }
        if result.retryable != Some(true) {
            return RetryOutcome::Failed(
                result.error.unwrap_or_else(|| "auto-merge failed".into()),
            );
        }
    }
}

impl GithubAction {
    fn key(&self) -> &str {
        match self {
            Self::MarkReady { key, .. }
            | Self::SetAutoMerge { key, .. }
            | Self::Ship { key, .. }
            | Self::FailedChecks { key } => key,
        }
    }

    /// The PR a write must still target; `None` for reads.
    fn write_pin(&self) -> Option<Option<&PrPin>> {
        match self {
            Self::MarkReady { pin, .. }
            | Self::SetAutoMerge { pin, .. }
            | Self::Ship { pin, .. } => Some(pin.as_ref()),
            Self::FailedChecks { .. } => None,
        }
    }
}

fn resolve_row<'a>(board: &'a Board, key: &str) -> Option<&'a wt_tui::BoardRow> {
    let mut matching = board
        .rows
        .iter()
        .filter(|row| row.key == key || row.slug == key);
    let first = matching.next()?;
    matching.next().is_none().then_some(first)
}

fn merge_target(pr: &PullRequest) -> Option<PrMergeTarget> {
    Some(PrMergeTarget {
        id: pr.id.clone().filter(|id| !id.is_empty())?,
        number: pr.number,
        base_ref_name: pr.base_ref_name.clone(),
        head_ref_oid: pr.head_ref_oid.clone().filter(|sha| !sha.is_empty())?,
    })
}

fn merge_target_for_disable(pr: &PullRequest) -> Option<PrMergeTarget> {
    Some(PrMergeTarget {
        id: pr.id.clone().filter(|id| !id.is_empty())?,
        number: pr.number,
        base_ref_name: pr.base_ref_name.clone(),
        head_ref_oid: pr.head_ref_oid.clone().unwrap_or_default(),
    })
}

fn reply(message: impl Into<String>) -> UiReply {
    UiReply {
        message: message.into(),
        ..UiReply::default()
    }
}

fn accepted(message: impl Into<String>, at: Stamp) -> UiReply {
    UiReply {
        outcome: Some(ReplyOutcome::Accepted(at)),
        ..reply(message)
    }
}

fn no_change(message: impl Into<String>) -> UiReply {
    UiReply {
        outcome: Some(ReplyOutcome::NoChange),
        ..reply(message)
    }
}

fn deferred(message: impl Into<String>) -> UiReply {
    UiReply {
        outcome: Some(ReplyOutcome::Deferred),
        ..reply(message)
    }
}

fn failed(message: impl Into<String>) -> UiReply {
    UiReply {
        message: message.into(),
        failed: true,
        ..UiReply::default()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use wt_github::AutoMerge;

    fn pr(branch: &str) -> PullRequest {
        PullRequest {
            id: Some("PR_node".into()),
            number: 7,
            url: "https://example.test/pr/7".into(),
            head_ref_name: branch.into(),
            head_ref_oid: Some("deadbeef".into()),
            base_ref_name: "main".into(),
            merge_commit_oid: None,
            title: "test".into(),
            is_draft: true,
            state: "OPEN".into(),
            mergeable: None,
            merge_state_status: None,
            checks: PrChecks::Pending,
            failed_checks: vec![],
            review: wt_github::PrReview::None,
            review_requests: 0,
            requested_reviewers: vec![],
            suggested_reviewers: vec![],
            review_bot: None,
            auto_merge: None,
            comments: vec![],
            unresolved_threads: 0,
            unresolved_threads_total: 0,
            merged_at: None,
            closed_at: None,
        }
    }

    /// GitHub data with one open draft PR #7 on `branch`.
    pub(crate) fn open_pr_data(branch: &str) -> GithubData {
        let mut data = GithubData::default();
        data.prs.insert(branch.into(), pr(branch));
        data
    }

    /// The same data with classic auto-merge armed on every PR.
    pub(crate) fn with_auto_merge(mut data: GithubData) -> GithubData {
        for pr in data.prs.values_mut() {
            pr.auto_merge = Some(AutoMerge {
                enabled_at: "then".into(),
                merge_method: wt_github::AutoMergeMethod::Rebase,
            });
        }
        data
    }

    pub(crate) fn ledger_with_accepted_enable(branch: &str, number: u64, at: Stamp) -> Ledger {
        let mut ledger = Ledger::default();
        record_accepted(
            &mut ledger,
            1,
            GithubWrite::auto_merge(branch, number, true),
            at,
        );
        ledger
    }

    #[test]
    fn row_resolution_rejects_ambiguous_key_or_slug_matches() {
        let board = Board {
            rows: vec![
                wt_tui::BoardRow {
                    key: "host/one".into(),
                    slug: "one".into(),
                    ..Default::default()
                },
                wt_tui::BoardRow {
                    key: "one".into(),
                    slug: "two".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert!(resolve_row(&board, "one").is_none());
    }

    #[test]
    fn merge_target_keeps_the_exact_head_and_requires_identity() {
        let target = merge_target(&pr("feature/one")).unwrap();
        assert_eq!(target.head_ref_oid, "deadbeef");
        assert!(
            merge_target(&PullRequest {
                id: None,
                ..pr("feature/one")
            })
            .is_none()
        );
        assert!(
            merge_target(&PullRequest {
                head_ref_oid: None,
                ..pr("feature/one")
            })
            .is_none()
        );
    }

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

    /// D2/D3: arm, then disarm before any read shows the arm. The lagging
    /// read says "not armed", but this host armed it, so disable is sent.
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
                &mut Ledger::default(),
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
    async fn an_unconfirmed_enable_refuses_enable_and_still_allows_disable() {
        let stale = Confirmed::for_test(open_pr_data("feature"));
        let mut ledger = Ledger::default();
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
        let mut ledger = Ledger::default();
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

    #[test]
    fn pins_require_the_same_pr_and_head() {
        let pin = PrPin {
            number: 7,
            head_sha: Some("deadbeef".into()),
        };
        let current = pr("feature");
        assert!(pin_matches(Some(&pin), Some(&current)));
        assert!(pin_matches(None, None));
        assert!(!pin_matches(None, Some(&current)));
        assert!(!pin_matches(Some(&pin), None));
        let moved = PullRequest {
            head_ref_oid: Some("cafe".into()),
            ..current.clone()
        };
        assert!(!pin_matches(Some(&pin), Some(&moved)));
        let replaced = PullRequest {
            number: 8,
            ..current
        };
        assert!(!pin_matches(Some(&pin), Some(&replaced)));
    }

    #[cfg(unix)]
    struct FakeGh {
        fixture: crate::commands::test_support::CommandFixture,
        calls: std::path::PathBuf,
        actions: GithubActions,
    }

    #[cfg(unix)]
    async fn fake_gh() -> FakeGh {
        use std::{fs, os::unix::fs::PermissionsExt};

        let fixture = crate::commands::test_support::CommandFixture::new()
            .await
            .unwrap();
        let fake = fixture._root.path().join("fake-gh");
        let calls = fixture._root.path().join("gh-calls.jsonl");
        fs::write(
            &fake,
            format!(
                "#!/usr/bin/env python3\nimport json,sys\nwith open({calls:?},'a') as f: f.write(json.dumps(sys.argv[1:])+'\\n')\n",
                calls = calls.to_string_lossy()
            ),
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        let client = GithubClient::new(
            fixture.ctx.processes.clone(),
            fixture.ctx.config.paths.main_clone.clone(),
            GithubOptions::from_config(&fixture.ctx.config, false),
        )
        .with_gh_program(fake);
        let (retry_tx, _retry_rx) = mpsc::channel(2);
        let actions = GithubActions {
            client,
            writes: Writes::new(GithubRefresh::detached()),
            retry_tx,
            cancellation: fixture.ctx.cancellation.clone(),
            reviewers_enabled: false,
            default_reviewer: None,
        };
        FakeGh {
            fixture,
            calls,
            actions,
        }
    }

    fn board() -> Board {
        Board {
            rows: vec![wt_tui::BoardRow {
                key: "one".into(),
                slug: "one".into(),
                branch: "feature/one".into(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn seen() -> Option<PrPin> {
        Some(PrPin {
            number: 7,
            head_sha: Some("deadbeef".into()),
        })
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn mark_ready_targets_the_selected_open_pr_and_records_the_write() {
        let fake = fake_gh().await;
        let github = Confirmed::for_test(open_pr_data("feature/one"));
        let reply = fake
            .actions
            .execute(
                GithubAction::MarkReady {
                    key: "one".into(),
                    pin: seen(),
                },
                &board(),
                Some(&github),
            )
            .await
            .unwrap();
        assert!(!reply.failed);
        assert!(reply.message.contains("ready for review"));
        assert!(matches!(reply.outcome, Some(ReplyOutcome::Accepted(_))));
        // The lagging read still shows a draft; the record makes a repeat a no-op.
        assert!(!fake.actions.writes.effective(&github).prs["feature/one"].is_draft);
        assert_eq!(
            std::fs::read_to_string(&fake.calls).unwrap().trim(),
            r#"["pr", "ready", "7"]"#
        );
        fake.fixture.close().await.unwrap();
    }

    /// C1: a write names the PR the user reviewed. A different PR, a moved
    /// head, or no confirmed data refuses the write before anything is sent.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_pin_mismatch_or_missing_confirmed_data_refuses_the_write() {
        let fake = fake_gh().await;
        let mut moved = open_pr_data("feature/one");
        moved.prs.get_mut("feature/one").unwrap().head_ref_oid = Some("cafe".into());
        let moved = Confirmed::for_test(moved);
        let current = Confirmed::for_test(open_pr_data("feature/one"));
        let requests = [
            (
                GithubAction::SetAutoMerge {
                    key: "one".into(),
                    enable: true,
                    pin: seen(),
                },
                Some(&moved),
            ),
            (
                GithubAction::Ship {
                    key: "one".into(),
                    pin: Some(PrPin {
                        number: 6,
                        head_sha: Some("deadbeef".into()),
                    }),
                },
                Some(&current),
            ),
            (
                GithubAction::MarkReady {
                    key: "one".into(),
                    pin: seen(),
                },
                None,
            ),
        ];
        for (action, github) in requests {
            let reply = fake
                .actions
                .execute(action, &board(), github)
                .await
                .unwrap();
            assert!(reply.failed);
            assert_eq!(reply.message, STALE_PIN);
        }
        assert!(!fake.calls.exists(), "no GitHub request may be sent");
        fake.fixture.close().await.unwrap();
    }
}
