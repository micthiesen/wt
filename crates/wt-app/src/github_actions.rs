//! Host-local GitHub actions used by the TUI controller.
//!
//! Mutations go through `wt-github`; this module owns their bounded retry
//! lifetime and the short-lived overlay needed while GitHub's read model catches
//! up with a successful write.

use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::Duration,
};

use anyhow::Result;
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinSet,
    time::{Instant, sleep},
};
use tokio_util::sync::CancellationToken;
use wt_github::{GithubClient, GithubData, GithubOptions, PrChecks, PrMergeTarget};
use wt_runtime::{SourceHandle, SourceSnapshot, TaskScope, source_channel};
use wt_tui::{Board, UiModal, UiReply};

use crate::context::AppContext;

const RETRY_EVERY: Duration = Duration::from_secs(30);
const RETRY_LIMIT: Duration = Duration::from_secs(20 * 60);
const MAX_RETRIES: usize = 4;
const FAILED_LOG_LIMIT: usize = 200;
const OVERLAY_LIFETIME: Duration = Duration::from_secs(12);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GithubAction {
    MarkReady { key: String },
    SetAutoMerge { key: String, enable: bool },
    ToggleAutoMerge { key: String },
    Ship { key: String },
    FailedChecks { key: String },
}

#[derive(Clone)]
pub struct GithubActions {
    client: GithubClient,
    github: SourceHandle<GithubData>,
    overlay_tx: mpsc::Sender<OverlayUpdate>,
    retry_tx: mpsc::Sender<RetryCommand>,
    cancellation: CancellationToken,
    reviewers_enabled: bool,
    default_reviewer: Option<String>,
}

impl GithubActions {
    /// Starts the refresh overlay and the single scoped retry owner.
    pub fn start(
        scope: &TaskScope,
        context: &AppContext,
        github: SourceHandle<GithubData>,
    ) -> (Self, SourceHandle<GithubData>) {
        let client = GithubClient::new(
            context.processes.clone(),
            context.config.paths.main_clone.clone(),
            GithubOptions::from_config(&context.config, false),
        );
        let (overlay_tx, overlay_rx) = mpsc::channel(16);
        let overlaid_github = overlay_github(scope, github.clone(), overlay_rx);
        let (retry_tx, retry_rx) = mpsc::channel(16);
        let cancellation = scope.token();
        spawn_retry_owner(
            scope,
            client.clone(),
            github.clone(),
            overlay_tx.clone(),
            cancellation.clone(),
            retry_rx,
        );

        (
            Self {
                client,
                github,
                overlay_tx,
                retry_tx,
                cancellation: context.cancellation.clone(),
                reviewers_enabled: context.config.github.reviewers,
                default_reviewer: context.config.github.default_reviewer.clone(),
            },
            overlaid_github,
        )
    }

    pub async fn execute(
        &self,
        action: GithubAction,
        board: &Board,
        github: &GithubData,
    ) -> Result<UiReply> {
        let key = action.key();
        let Some(row) = resolve_row(board, key) else {
            return Ok(failed(format!(
                "worktree {key:?} is no longer on the board"
            )));
        };
        let branch = row.branch.clone();
        let Some(pr) = github.prs.get(&branch) else {
            return Ok(failed(format!("no PR data for {}", row.branch)));
        };
        if pr.state != "OPEN" {
            return Ok(failed(format!(
                "PR #{} is {}",
                pr.number,
                pr.state.to_lowercase()
            )));
        }

        match action {
            GithubAction::MarkReady { .. } => {
                self.mark_ready(pr.number, &branch, pr.is_draft).await
            }
            GithubAction::SetAutoMerge { enable, .. } => {
                self.set_auto_merge(pr, &branch, github, enable).await
            }
            GithubAction::ToggleAutoMerge { .. } => {
                self.toggle_auto_merge(pr, &branch, github).await
            }
            GithubAction::Ship { .. } => self.ship(pr, &branch).await,
            GithubAction::FailedChecks { .. } => self.failed_checks(pr, &branch).await,
        }
    }

    async fn mark_ready(&self, number: u64, branch: &str, is_draft: bool) -> Result<UiReply> {
        if !is_draft {
            return Ok(reply(format!("PR #{number} is already ready")));
        }
        let result = self
            .client
            .mark_pull_request_ready(number, &self.cancellation)
            .await;
        if !result.ok {
            return Ok(failed(
                result.error.unwrap_or_else(|| "mark ready failed".into()),
            ));
        }
        self.overlay(OverlayUpdate::Ready(branch.to_owned()));
        self.refresh_github();
        Ok(reply(format!("marked PR #{number} ready for review")))
    }

    async fn set_auto_merge(
        &self,
        pr: &wt_github::PullRequest,
        branch: &str,
        github: &GithubData,
        enable: bool,
    ) -> Result<UiReply> {
        let number = pr.number;
        let armed = pr.auto_merge.is_some() || github.merge_queue.contains_key(branch);
        if enable {
            if armed {
                return Ok(reply(format!(
                    "merge when ready is already armed for PR #{number}"
                )));
            }
            if self.retry_pending(number).await? {
                return Ok(reply(format!("PR #{number} is already waiting on checks")));
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
            if result.ok {
                self.overlay(OverlayUpdate::Armed(branch.to_owned()));
                self.refresh_github();
                return Ok(reply(format!("merge when ready armed for PR #{number}")));
            }
            if result.retryable == Some(true) {
                match self.start_retry(target.clone(), branch.to_owned()).await? {
                    RetryStart::Started => {
                        return Ok(reply(format!(
                            "PR #{number}: required checks have not reported; will arm when they do"
                        )));
                    }
                    RetryStart::AlreadyRunning => {
                        return Ok(reply(format!("PR #{number} is already waiting on checks")));
                    }
                    RetryStart::AtCapacity => {
                        return Ok(failed(
                            "four GitHub merge retries are already active; try again shortly",
                        ));
                    }
                }
            }
            return Ok(failed(
                result.error.unwrap_or_else(|| "auto-merge failed".into()),
            ));
        }

        let cancellation = self.cancel_retry(number).await?;
        if let Some(result) = self.finish_retry_cancel(pr, branch, cancellation).await {
            return Ok(result);
        }
        if !armed {
            return Ok(reply(format!(
                "merge when ready is not armed for PR #{number}"
            )));
        }
        let Some(target) = merge_target_for_disable(pr) else {
            return Ok(failed(format!(
                "cannot inspect PR #{number}: missing PR node id"
            )));
        };
        let result = self
            .client
            .disable_auto_merge(&target, &self.cancellation)
            .await;
        if !result.ok {
            return Ok(failed(
                result
                    .error
                    .unwrap_or_else(|| "disable auto-merge failed".into()),
            ));
        }
        self.overlay(OverlayUpdate::Disarmed(branch.to_owned()));
        self.refresh_github();
        Ok(reply(format!(
            "cancelled merge-when-ready for PR #{number}"
        )))
    }

    /// Toggle the actual queue/classic armed state. A queued retry counts as
    /// pending even though GitHub has not accepted an arm yet, so a second
    /// toggle cancels it instead of reporting that nothing is armed.
    pub async fn toggle_auto_merge(
        &self,
        pr: &wt_github::PullRequest,
        branch: &str,
        github: &GithubData,
    ) -> Result<UiReply> {
        let cancellation = self.cancel_retry(pr.number).await?;
        if let Some(result) = self.finish_retry_cancel(pr, branch, cancellation).await {
            return Ok(result);
        }
        let armed = pr.auto_merge.is_some() || github.merge_queue.contains_key(branch);
        self.set_auto_merge(pr, branch, github, !armed).await
    }

    /// Optimistically project a completed reviewer edit and request a fresh
    /// source read, using the same bounded settle behavior as PR mutations.
    pub fn optimistic_reviewers(
        &self,
        branch: impl Into<String>,
        requested: Vec<String>,
        review_requests: u32,
    ) {
        self.overlay(OverlayUpdate::Reviewers {
            branch: branch.into(),
            requested,
            review_requests,
        });
        self.refresh_github();
    }

    async fn finish_retry_cancel(
        &self,
        pr: &wt_github::PullRequest,
        branch: &str,
        cancellation: RetryCancel,
    ) -> Option<UiReply> {
        let number = pr.number;
        match cancellation {
            RetryCancel::NotPending => None,
            RetryCancel::Cancelled => Some(reply(format!(
                "cancelled pending merge-when-ready arm for PR #{number}"
            ))),
            RetryCancel::Armed => {
                // Cancellation raced with a successful in-flight request. Wait
                // for that request to finish, then remove the actual queue or
                // classic arm before acknowledging the toggle.
                let Some(target) = merge_target_for_disable(pr) else {
                    return Some(failed(format!(
                        "PR #{number} armed while cancellation was in flight, but its node id is missing; refresh and inspect the PR"
                    )));
                };
                let result = self
                    .client
                    .disable_auto_merge(&target, &self.cancellation)
                    .await;
                if !result.ok {
                    return Some(failed(format!(
                        "PR #{number} armed while cancellation was in flight; could not disarm it: {}",
                        result
                            .error
                            .unwrap_or_else(|| "unknown GitHub error".into())
                    )));
                }
                self.overlay(OverlayUpdate::Disarmed(branch.to_owned()));
                self.refresh_github();
                Some(reply(format!(
                    "cancelled pending arm and disarmed merge-when-ready for PR #{number}"
                )))
            }
            RetryCancel::Failed(error) => Some(failed(format!(
                "pending merge request for PR #{number} ended while cancellation was in flight: {error}; refresh GitHub to inspect its state"
            ))),
            RetryCancel::GaveUp => Some(reply(format!(
                "pending merge-when-ready retry for PR #{number} stopped waiting"
            ))),
        }
    }

    async fn ship(&self, pr: &wt_github::PullRequest, branch: &str) -> Result<UiReply> {
        let number = pr.number;
        let was_draft = pr.is_draft;
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
        let needs_arm = pr.auto_merge.is_none();
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
            return Ok(reply(format!("PR #{number} is already shipped")));
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
        if reviewer_result.as_ref().is_some_and(|result| result.ok) {
            self.refresh_github();
        }
        if let Some(result) = ready_result.as_ref() {
            if !result.ok {
                return Ok(failed(
                    result
                        .error
                        .clone()
                        .unwrap_or_else(|| "mark ready failed".into()),
                ));
            }
            self.overlay(OverlayUpdate::Ready(branch.to_owned()));
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
            if !result.ok {
                let mut message = result.error.unwrap_or_else(|| "auto-merge failed".into());
                if let Some(reviewer_error) = reviewer_error {
                    message.push_str("; ");
                    message.push_str(&reviewer_error);
                }
                return Ok(failed(message));
            }
            self.overlay(OverlayUpdate::Armed(branch.to_owned()));
        }
        self.refresh_github();
        let message = match reviewer_error {
            Some(error) => format!("shipped PR #{number}; {error}"),
            None => format!("shipped PR #{number}"),
        };
        Ok(reply(message))
    }

    async fn failed_checks(&self, pr: &wt_github::PullRequest, branch: &str) -> Result<UiReply> {
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

    fn overlay(&self, update: OverlayUpdate) {
        if self.overlay_tx.try_send(update).is_err() {
            tracing::warn!("GitHub mutation overlay queue is unavailable; refreshing source only");
        }
    }

    fn refresh_github(&self) {
        let _ = self.github.refresh();
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

#[derive(Clone, Debug)]
enum OverlayUpdate {
    Ready(String),
    Armed(String),
    Disarmed(String),
    Reviewers {
        branch: String,
        requested: Vec<String>,
        review_requests: u32,
    },
}

#[derive(Clone, Debug)]
enum OverlayValue {
    Ready,
    Armed,
    Disarmed,
    Reviewers {
        requested: Vec<String>,
        review_requests: u32,
    },
}

#[derive(Clone, Debug)]
struct OverlayEntry {
    value: OverlayValue,
    expires_at: Instant,
}

fn overlay_github(
    scope: &TaskScope,
    input: SourceHandle<GithubData>,
    mut updates: mpsc::Receiver<OverlayUpdate>,
) -> SourceHandle<GithubData> {
    let (handle, mut publisher) = source_channel();
    let cancellation = scope.token();
    scope.spawn(async move {
        let mut upstream = input.subscribe();
        let mut overlays: BTreeMap<String, Vec<OverlayEntry>> = BTreeMap::new();
        let mut latest = upstream.borrow().clone();
        loop {
            publish_overlay(&publisher, &latest, &mut overlays);
            let deadline = overlays
                .values()
                .flatten()
                .map(|entry| entry.expires_at)
                .min();
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => break,
                changed = upstream.changed() => {
                    if changed.is_err() { break; }
                    latest = upstream.borrow_and_update().clone();
                }
                update = updates.recv() => {
                    let Some(update) = update else { break; };
                    let (branch, value) = match update {
                        OverlayUpdate::Ready(branch) => (branch, OverlayValue::Ready),
                        OverlayUpdate::Armed(branch) => (branch, OverlayValue::Armed),
                        OverlayUpdate::Disarmed(branch) => (branch, OverlayValue::Disarmed),
                        OverlayUpdate::Reviewers { branch, requested, review_requests } =>
                            (branch, OverlayValue::Reviewers { requested, review_requests }),
                    };
                    let entries = overlays.entry(branch).or_default();
                    entries.retain(|entry| !same_overlay_field(&entry.value, &value));
                    entries.push(OverlayEntry {
                        value,
                        expires_at: Instant::now() + OVERLAY_LIFETIME,
                    });
                }
                _ = publisher.requested() => {
                    let _ = input.refresh();
                }
                _ = async {
                    if let Some(deadline) = deadline { tokio::time::sleep_until(deadline).await; }
                    else { std::future::pending::<()>().await; }
                } => {
                    let now = Instant::now();
                    overlays.retain(|_, entries| {
                        entries.retain(|entry| entry.expires_at > now);
                        !entries.is_empty()
                    });
                }
            }
        }
    });
    handle
}

fn publish_overlay(
    publisher: &wt_runtime::SourcePublisher<GithubData>,
    upstream: &SourceSnapshot<GithubData>,
    overlays: &mut BTreeMap<String, Vec<OverlayEntry>>,
) {
    let Some(data) = upstream.data.as_deref() else {
        publisher.publish(upstream.clone());
        return;
    };
    let mut projected = data.clone();
    overlays.retain(|branch, entries| {
        let Some(pr) = projected.prs.get_mut(branch) else {
            entries.retain(|entry| entry.expires_at > Instant::now());
            return !entries.is_empty();
        };
        entries.retain(|entry| {
            let caught_up = match &entry.value {
                OverlayValue::Ready => !pr.is_draft,
                OverlayValue::Armed => {
                    pr.auto_merge.is_some() || projected.merge_queue.contains_key(branch)
                }
                OverlayValue::Disarmed => {
                    pr.auto_merge.is_none() && !projected.merge_queue.contains_key(branch)
                }
                OverlayValue::Reviewers {
                    requested,
                    review_requests,
                } => pr.requested_reviewers == *requested && pr.review_requests == *review_requests,
            };
            if caught_up || entry.expires_at <= Instant::now() {
                return false;
            }
            match &entry.value {
                OverlayValue::Ready => pr.is_draft = false,
                OverlayValue::Armed => {
                    if pr.auto_merge.is_none() {
                        pr.auto_merge = Some(wt_github::AutoMerge {
                            enabled_at: time::OffsetDateTime::now_utc()
                                .format(&time::format_description::well_known::Rfc3339)
                                .unwrap_or_default(),
                            merge_method: wt_github::AutoMergeMethod::Rebase,
                        });
                    }
                }
                OverlayValue::Disarmed => {
                    pr.auto_merge = None;
                    projected.merge_queue.remove(branch);
                }
                OverlayValue::Reviewers {
                    requested,
                    review_requests,
                } => {
                    pr.requested_reviewers = requested.clone();
                    pr.review_requests = *review_requests;
                }
            }
            true
        });
        !entries.is_empty()
    });
    publisher.publish(SourceSnapshot {
        data: Some(Arc::new(projected)),
        state: upstream.state.clone(),
        updated_at: upstream.updated_at,
        revision: upstream.revision,
    });
}

fn same_overlay_field(left: &OverlayValue, right: &OverlayValue) -> bool {
    matches!(
        (left, right),
        (OverlayValue::Ready, OverlayValue::Ready)
            | (OverlayValue::Armed, OverlayValue::Armed)
            | (OverlayValue::Armed, OverlayValue::Disarmed)
            | (OverlayValue::Disarmed, OverlayValue::Armed)
            | (OverlayValue::Disarmed, OverlayValue::Disarmed)
            | (
                OverlayValue::Reviewers { .. },
                OverlayValue::Reviewers { .. }
            )
    )
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
    github: SourceHandle<GithubData>,
    overlay_tx: mpsc::Sender<OverlayUpdate>,
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
                                let github = github.clone();
                                let overlay_tx = overlay_tx.clone();
                                jobs.spawn(async move {
                                    let result = retry_arm(client, github, overlay_tx, target.clone(), branch, token).await;
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
                            RetryOutcome::Failed(error) => tracing::warn!(pr = finished.number, %error, "background merge-when-ready retry failed"),
                            RetryOutcome::GaveUp => tracing::warn!(pr = finished.number, "gave up waiting for required checks to appear"),
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
    github: SourceHandle<GithubData>,
    overlay_tx: mpsc::Sender<OverlayUpdate>,
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
            let _ = overlay_tx.send(OverlayUpdate::Armed(branch.clone())).await;
            let _ = github.refresh();
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
            Self::MarkReady { key }
            | Self::SetAutoMerge { key, .. }
            | Self::ToggleAutoMerge { key }
            | Self::Ship { key }
            | Self::FailedChecks { key } => key,
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

fn merge_target(pr: &wt_github::PullRequest) -> Option<PrMergeTarget> {
    Some(PrMergeTarget {
        id: pr.id.clone().filter(|id| !id.is_empty())?,
        number: pr.number,
        base_ref_name: pr.base_ref_name.clone(),
        head_ref_oid: pr.head_ref_oid.clone().filter(|sha| !sha.is_empty())?,
    })
}

fn merge_target_for_disable(pr: &wt_github::PullRequest) -> Option<PrMergeTarget> {
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

fn failed(message: impl Into<String>) -> UiReply {
    UiReply {
        message: message.into(),
        failed: true,
        ..UiReply::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_github::{AutoMerge, PullRequest};
    use wt_runtime::SourceState;

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
    fn overlay_preserves_other_pr_and_merge_queue_data() {
        let mut data = GithubData::default();
        let mut first = pr("feature/one");
        first.is_draft = true;
        let mut second = pr("feature/two");
        second.number = 8;
        second.auto_merge = Some(AutoMerge {
            enabled_at: "then".into(),
            merge_method: wt_github::AutoMergeMethod::Rebase,
        });
        data.prs.insert("feature/one".into(), first);
        data.prs.insert("feature/two".into(), second.clone());
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
        let upstream = SourceSnapshot {
            data: Some(Arc::new(data.clone())),
            state: SourceState::Ready,
            updated_at: None,
            revision: 1,
        };
        let mut overlays = BTreeMap::from([(
            "feature/one".into(),
            vec![
                OverlayEntry {
                    value: OverlayValue::Ready,
                    expires_at: Instant::now() + Duration::from_secs(10),
                },
                OverlayEntry {
                    value: OverlayValue::Reviewers {
                        requested: vec!["new-reviewer".into()],
                        review_requests: 1,
                    },
                    expires_at: Instant::now() + Duration::from_secs(10),
                },
            ],
        )]);
        let (handle, publisher) = source_channel();
        publish_overlay(&publisher, &upstream, &mut overlays);
        let result = handle.snapshot();
        let result = result.data.unwrap();
        assert!(!result.prs["feature/one"].is_draft);
        assert_eq!(
            result.prs["feature/one"].requested_reviewers,
            ["new-reviewer"]
        );
        assert_eq!(result.prs["feature/one"].review_requests, 1);
        assert_eq!(result.prs["feature/two"], second);
        assert_eq!(result.merge_queue["feature/two"].position, 3);
        assert_eq!(
            overlays["feature/one"].len(),
            2,
            "stale upstream must keep the optimistic overlay"
        );

        let mut caught_up = data;
        caught_up.prs.get_mut("feature/one").unwrap().is_draft = false;
        caught_up
            .prs
            .get_mut("feature/one")
            .unwrap()
            .requested_reviewers = vec!["new-reviewer".into()];
        caught_up
            .prs
            .get_mut("feature/one")
            .unwrap()
            .review_requests = 1;
        let caught_up = SourceSnapshot {
            data: Some(Arc::new(caught_up)),
            state: SourceState::Ready,
            updated_at: None,
            revision: 2,
        };
        publish_overlay(&publisher, &caught_up, &mut overlays);
        assert!(overlays.is_empty());
    }

    #[test]
    fn disarmed_overlay_removes_stale_queue_and_classic_badges() {
        let mut data = GithubData::default();
        let mut row = pr("feature/one");
        row.auto_merge = Some(AutoMerge {
            enabled_at: "old".into(),
            merge_method: wt_github::AutoMergeMethod::Rebase,
        });
        data.prs.insert("feature/one".into(), row);
        data.merge_queue.insert(
            "feature/one".into(),
            wt_github::MergeQueueEntry {
                head_ref_name: "feature/one".into(),
                position: 2,
                state: wt_github::MergeQueueState::Queued,
                enqueued_at: "old".into(),
                estimated_time_to_merge: None,
            },
        );
        let upstream = SourceSnapshot {
            data: Some(Arc::new(data)),
            state: wt_runtime::SourceState::Ready,
            updated_at: None,
            revision: 1,
        };
        let mut overlays = BTreeMap::from([(
            "feature/one".into(),
            vec![OverlayEntry {
                value: OverlayValue::Disarmed,
                expires_at: Instant::now() + Duration::from_secs(10),
            }],
        )]);
        let (handle, publisher) = source_channel();
        publish_overlay(&publisher, &upstream, &mut overlays);
        let result = handle.snapshot().data.unwrap();
        assert!(result.prs["feature/one"].auto_merge.is_none());
        assert!(!result.merge_queue.contains_key("feature/one"));
        assert_eq!(overlays["feature/one"].len(), 1);
    }

    #[tokio::test]
    async fn overlay_forwards_refresh_requests_to_the_original_github_source() {
        let scope = TaskScope::new();
        let (input, mut input_publisher) = source_channel::<GithubData>();
        let (_updates_tx, updates_rx) = mpsc::channel(2);
        let output = overlay_github(&scope, input, updates_rx);
        assert!(output.refresh());
        tokio::time::timeout(Duration::from_secs(1), input_publisher.requested())
            .await
            .expect("derived refresh should reach the underlying source")
            .expect("original source stays open");
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn mark_ready_targets_the_selected_open_pr_and_publishes_an_overlay() {
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
        let (github_source, _publisher) = source_channel::<GithubData>();
        let (overlay_tx, mut overlay_rx) = mpsc::channel(2);
        let (retry_tx, _retry_rx) = mpsc::channel(2);
        let actions = GithubActions {
            client,
            github: github_source,
            overlay_tx,
            retry_tx,
            cancellation: fixture.ctx.cancellation.clone(),
            reviewers_enabled: false,
            default_reviewer: None,
        };
        let mut github = GithubData::default();
        github.prs.insert("feature/one".into(), pr("feature/one"));
        let board = Board {
            rows: vec![wt_tui::BoardRow {
                key: "one".into(),
                slug: "one".into(),
                branch: "feature/one".into(),
                ..Default::default()
            }],
            ..Default::default()
        };

        let reply = actions
            .execute(
                GithubAction::MarkReady { key: "one".into() },
                &board,
                &github,
            )
            .await
            .unwrap();
        assert!(!reply.failed);
        assert!(reply.message.contains("ready for review"));
        assert!(
            matches!(overlay_rx.recv().await, Some(OverlayUpdate::Ready(branch)) if branch == "feature/one")
        );
        assert_eq!(
            fs::read_to_string(calls).unwrap().trim(),
            r#"["pr", "ready", "7"]"#
        );
        fixture.close().await.unwrap();
    }
}
