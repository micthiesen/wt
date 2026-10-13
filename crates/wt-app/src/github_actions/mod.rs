//! Host-local GitHub actions used by the TUI controller.
//!
//! Mutations go through `wt-github`. This module resolves and checks each
//! request against confirmed GitHub data; `writes` keeps the write ledger
//! that guards repeats while GitHub's reads lag, and `retry` owns the bounded
//! merge-when-ready retries. The ledger is never published: displays,
//! automations and planners read only confirmed GitHub data.

mod retry;
#[cfg(test)]
pub(crate) mod tests;
mod writes;

use std::ops::ControlFlow;

use anyhow::Result;
use tokio_util::sync::CancellationToken;
use wt_github::{
    CoveredPr, GhActionResult, GithubClient, GithubData, GithubOptions, PrChecks, PrMergeTarget,
    PullRequest,
};
use wt_runtime::{Confirmed, Stamp, TaskScope};
use wt_tui::{Board, PrPin, UiModal, UiReply};

use self::{
    retry::{Retries, RetryCancel, RetryStart},
    writes::{GithubWrite, Guard, WriteLedger, effective_pr, spawn_confirmer},
};
use crate::{context::AppContext, github_source::GithubRefresh};

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
    ledger: WriteLedger,
    retries: Retries,
    cancellation: CancellationToken,
    reviewers_enabled: bool,
    default_reviewer: Option<String>,
}

/// What a ship request will do, decided before anything is sent.
struct ShipPlan {
    number: u64,
    was_draft: bool,
    reviewer: Option<String>,
    /// Where to arm merge-when-ready; `None` when it is already armed.
    target: Option<PrMergeTarget>,
}

/// What a ship has done so far.
struct ShipProgress {
    /// When the latest accepted write happened.
    at: Option<Stamp>,
    marked_ready: bool,
    reviewer_error: Option<String>,
}

impl GithubActions {
    /// Starts the write confirmer and the single scoped retry owner.
    pub fn start(scope: &TaskScope, context: &AppContext, refresh: GithubRefresh) -> Self {
        let client = GithubClient::new(
            context.processes.clone(),
            context.config.paths.main_clone.clone(),
            GithubOptions::from_config(&context.config, false),
        );
        let ledger = WriteLedger::new(refresh);
        spawn_confirmer(scope, ledger.clone());
        let retries = Retries::start(scope, client.clone(), ledger.clone());
        Self {
            client,
            ledger,
            retries,
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
            return Ok(UiReply::failure(format!(
                "worktree {key:?} is no longer on the board"
            )));
        };
        let branch = row.branch.clone();
        let Some(github) = github else {
            return Ok(UiReply::failure("GitHub data is still loading"));
        };
        let pr = match github.covered_pr(&branch) {
            CoveredPr::Uncovered => {
                return Ok(UiReply::failure(
                    "GitHub data is still loading for this branch",
                ));
            }
            CoveredPr::Absent => None,
            CoveredPr::Present(pr) => Some(pr),
        };
        if let Some(pin) = action.write_pin()
            && !pin_matches(pin, pr)
        {
            return Ok(UiReply::failure(STALE_PIN));
        }
        let Some(pr) = pr else {
            return Ok(UiReply {
                notice: true,
                ..UiReply::no_change("no PR for this row")
            });
        };
        if pr.state != "OPEN" {
            return Ok(UiReply::failure(format!(
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
            GithubAction::Ship { .. } => Ok(self.ship(pr, &branch, github).await),
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
        let effective = self.ledger.effective(github);
        if !effective_pr(&effective, branch, number).is_some_and(|pr| pr.is_draft) {
            return Ok(UiReply::no_change(format!("PR #{number} is already ready")));
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
                result.ambiguous,
            ));
        }
        let at = self.ledger.accepted(write);
        Ok(UiReply::accepted(
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
        if enable {
            self.enable(pr, branch, github).await
        } else {
            self.disarm(pr, branch, github).await
        }
    }

    async fn enable(
        &self,
        pr: &PullRequest,
        branch: &str,
        github: &Confirmed<GithubData>,
    ) -> Result<UiReply> {
        let number = pr.number;
        match self.ledger.guard(github, branch, number, true) {
            Guard::Send => {}
            Guard::AlreadyDone => {
                return Ok(UiReply::no_change(format!(
                    "merge when ready is already armed for PR #{number}"
                )));
            }
            Guard::Unconfirmed => return Ok(UiReply::failure(UNCONFIRMED_ENABLE)),
        }
        if self.retries.is_pending(number).await? {
            return Ok(UiReply::deferred(format!(
                "PR #{number} is already waiting on checks"
            )));
        }
        let Some(target) = merge_target(pr) else {
            return Ok(UiReply::failure(missing_target(number)));
        };
        let result = self
            .client
            .enable_auto_merge(&target, &self.cancellation)
            .await;
        let write = GithubWrite::auto_merge(branch, number, true);
        if result.ok {
            let at = self.ledger.accepted(write);
            return Ok(UiReply::accepted(
                format!("merge when ready armed for PR #{number}"),
                at,
            ));
        }
        if result.retryable == Some(true) {
            return Ok(
                match self.retries.start_arm(target, branch.to_owned()).await? {
                    RetryStart::Started => UiReply::deferred(format!(
                        "PR #{number}: required checks have not reported; will arm when they do"
                    )),
                    RetryStart::AlreadyRunning => {
                        UiReply::deferred(format!("PR #{number} is already waiting on checks"))
                    }
                    RetryStart::AtCapacity => UiReply::failure(
                        "four GitHub merge retries are already active; try again shortly",
                    ),
                },
            );
        }
        Ok(self.write_failed(
            write,
            result.error.unwrap_or_else(|| "auto-merge failed".into()),
            result.ambiguous,
        ))
    }

    /// Cancels a pending retry first, then disables any arm.
    async fn disarm(
        &self,
        pr: &PullRequest,
        branch: &str,
        github: &Confirmed<GithubData>,
    ) -> Result<UiReply> {
        let number = pr.number;
        let cancellation = self.retries.cancel(number).await?;
        if let Some(result) = self.finish_retry_cancel(pr, branch, cancellation).await {
            return Ok(result);
        }
        // Sent whenever this host's own writes say the PR may be armed, even
        // when the lagging read still shows it disarmed.
        if self.ledger.guard(github, branch, number, false) != Guard::Send {
            return Ok(UiReply::no_change(format!(
                "merge when ready is not armed for PR #{number}"
            )));
        }
        self.disable(pr, branch).await
    }

    async fn disable(&self, pr: &PullRequest, branch: &str) -> Result<UiReply> {
        let number = pr.number;
        let Some(target) = merge_target_for_disable(pr) else {
            return Ok(UiReply::failure(format!(
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
                result.ambiguous,
            ));
        }
        let at = self.ledger.accepted(write);
        Ok(UiReply::accepted(
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
            RetryCancel::Cancelled => Some(UiReply::no_change(format!(
                "cancelled pending merge-when-ready arm for PR #{number}"
            ))),
            RetryCancel::Armed => {
                // Cancellation raced with a successful in-flight request. Wait
                // for that request to finish, then remove the actual queue or
                // classic arm before acknowledging the request.
                let Some(target) = merge_target_for_disable(pr) else {
                    return Some(UiReply::failure(format!(
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
                        result.ambiguous,
                    ));
                }
                let at = self.ledger.accepted(write);
                Some(UiReply::accepted(
                    format!("cancelled pending arm and disarmed merge-when-ready for PR #{number}"),
                    at,
                ))
            }
            RetryCancel::Failed(error) => Some(UiReply::failure(format!(
                "pending merge request for PR #{number} ended while cancellation was in flight: {error}; refresh GitHub to inspect its state"
            ))),
            RetryCancel::GaveUp => Some(UiReply::no_change(format!(
                "pending merge-when-ready retry for PR #{number} stopped waiting"
            ))),
        }
    }

    /// Mark ready and request the default reviewer concurrently, then arm
    /// merge-when-ready. Each step runs only when it would change something.
    async fn ship(
        &self,
        pr: &PullRequest,
        branch: &str,
        github: &Confirmed<GithubData>,
    ) -> UiReply {
        let plan = match self.ship_plan(pr, branch, github) {
            ControlFlow::Continue(plan) => plan,
            ControlFlow::Break(reply) => return reply,
        };
        let (ready_result, reviewer_result) = tokio::join!(
            self.ship_mark_ready(&plan),
            self.ship_request_reviewer(&plan)
        );
        let progress =
            match self.ship_record_first_steps(&plan, branch, ready_result, reviewer_result) {
                ControlFlow::Continue(progress) => progress,
                ControlFlow::Break(reply) => return reply,
            };
        let progress = match self.ship_arm(&plan, branch, progress).await {
            ControlFlow::Continue(progress) => progress,
            ControlFlow::Break(reply) => return reply,
        };
        ship_reply(plan.number, progress)
    }

    /// Decide the steps from the PR as this host's writes show it. Stops
    /// when an arm is needed but impossible, or when nothing is left to do.
    fn ship_plan(
        &self,
        pr: &PullRequest,
        branch: &str,
        github: &Confirmed<GithubData>,
    ) -> ControlFlow<UiReply, ShipPlan> {
        let number = pr.number;
        let effective = self.ledger.effective(github);
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
        // Ship checks classic auto merge, but not a queue entry, before
        // deciding whether arming is needed.
        let needs_arm = shown.auto_merge.is_none();
        let target = if needs_arm {
            match merge_target(pr) {
                Some(target) => Some(target),
                None => return ControlFlow::Break(UiReply::failure(missing_target(number))),
            }
        } else {
            None
        };
        if !was_draft && reviewer.is_none() && !needs_arm {
            return ControlFlow::Break(UiReply::no_change(format!(
                "PR #{number} is already shipped"
            )));
        }
        ControlFlow::Continue(ShipPlan {
            number,
            was_draft,
            reviewer,
            target,
        })
    }

    async fn ship_mark_ready(&self, plan: &ShipPlan) -> Option<GhActionResult> {
        if !plan.was_draft {
            return None;
        }
        Some(
            self.client
                .mark_pull_request_ready(plan.number, &self.cancellation)
                .await,
        )
    }

    async fn ship_request_reviewer(&self, plan: &ShipPlan) -> Option<GhActionResult> {
        let login = plan.reviewer.as_ref()?;
        Some(
            self.client
                .edit_reviewers(
                    plan.number,
                    std::slice::from_ref(login),
                    &[],
                    &self.cancellation,
                )
                .await,
        )
    }

    /// Record the concurrent mark-ready and reviewer results. A failed mark
    /// ready stops the ship; a failed reviewer request is reported at the end.
    fn ship_record_first_steps(
        &self,
        plan: &ShipPlan,
        branch: &str,
        ready_result: Option<GhActionResult>,
        reviewer_result: Option<GhActionResult>,
    ) -> ControlFlow<UiReply, ShipProgress> {
        let mut progress = ShipProgress {
            at: None,
            marked_ready: false,
            reviewer_error: None,
        };
        if reviewer_result.as_ref().is_some_and(|result| result.ok) {
            progress.at = Some(Stamp::now());
            self.ledger.refresh.refresh_live();
        }
        if let Some(result) = ready_result {
            let write = GithubWrite::ready(branch, plan.number);
            if !result.ok {
                let mut message = result.error.unwrap_or_else(|| "mark ready failed".into());
                // Say what already happened, so a retry does not repeat it.
                if progress.at.is_some() {
                    message = format!("requested a reviewer, but marking ready failed: {message}");
                }
                return ControlFlow::Break(self.write_failed(write, message, result.ambiguous));
            }
            progress.at = Some(self.ledger.accepted(write));
            progress.marked_ready = true;
        }
        progress.reviewer_error = reviewer_result
            .filter(|result| !result.ok)
            .and_then(|result| result.error)
            .map(|error| format!("reviewer request failed: {error}"));
        ControlFlow::Continue(progress)
    }

    /// Arm merge-when-ready when the plan needs it. A failure names the
    /// steps that already happened.
    async fn ship_arm(
        &self,
        plan: &ShipPlan,
        branch: &str,
        mut progress: ShipProgress,
    ) -> ControlFlow<UiReply, ShipProgress> {
        let Some(target) = plan.target.as_ref() else {
            return ControlFlow::Continue(progress);
        };
        let number = plan.number;
        let result = self
            .client
            .enable_auto_merge(target, &self.cancellation)
            .await;
        let write = GithubWrite::auto_merge(branch, number, true);
        if !result.ok {
            let ambiguous = result.ambiguous;
            let mut message = result.error.unwrap_or_else(|| "auto-merge failed".into());
            if progress.marked_ready {
                message = format!("marked PR #{number} ready, but arming failed: {message}");
            }
            if let Some(reviewer_error) = progress.reviewer_error {
                message.push_str("; ");
                message.push_str(&reviewer_error);
            }
            return ControlFlow::Break(self.write_failed(write, message, ambiguous));
        }
        progress.at = Some(self.ledger.accepted(write));
        ControlFlow::Continue(progress)
    }

    async fn failed_checks(&self, pr: &PullRequest, branch: &str) -> Result<UiReply> {
        if pr.checks != PrChecks::Fail {
            return Ok(UiReply::message("no failing checks"));
        }
        let (run_id, mut lines) = match self
            .client
            .fetch_failed_run_log(branch, &self.cancellation)
            .await
        {
            Ok(result) => result,
            Err(error) => {
                return Ok(UiReply::failure(format!(
                    "failed to fetch CI logs: {error}"
                )));
            }
        };
        if lines.len() > FAILED_LOG_LIMIT {
            lines.truncate(FAILED_LOG_LIMIT);
            lines.push(format!("… truncated at {FAILED_LOG_LIMIT} lines; run `gh run view {run_id} --log-failed` for the rest"));
        }
        Ok(UiReply {
            modal: Some(UiModal::Log {
                title: format!("Failed checks: {} #{run_id}", pr.head_ref_name),
                lines,
                close_key: Some('f'),
                refresh: None,
            }),
            ..UiReply::message(format!(
                "failed CI logs for {} (run {run_id})",
                pr.head_ref_name
            ))
        })
    }

    /// A write that did not succeed. When it may still have reached GitHub
    /// (timeout, interruption, unconfirmed async merge, or the command drain
    /// stopping mid-write), record it as unconfirmed so neither the guards
    /// nor the user resend it blindly.
    fn write_failed(&self, write: GithubWrite, error: String, ambiguous: bool) -> UiReply {
        if !ambiguous && !self.cancellation.is_cancelled() {
            return UiReply::failure(error);
        }
        self.ledger.uncertain(write);
        UiReply::uncertain(format!(
            "{error}; the request may have reached GitHub and is unconfirmed; check before retrying"
        ))
    }
}

/// The final ship reply once every step that ran succeeded.
fn ship_reply(number: u64, progress: ShipProgress) -> UiReply {
    let message = match progress.reviewer_error {
        Some(error) => format!("shipped PR #{number}; {error}"),
        None => format!("shipped PR #{number}"),
    };
    match progress.at {
        Some(at) => UiReply::accepted(message, at),
        // Only the reviewer request ran, and it failed.
        None => UiReply::failure(message),
    }
}

fn missing_target(number: u64) -> String {
    format!("PR #{number} cache lacks its id or head SHA; refresh GitHub and retry")
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

/// The PR the user saw must be the PR confirmed data names now, at the same
/// head. With neither a pin nor a PR there is nothing to write to.
fn pin_matches(pin: Option<&PrPin>, pr: Option<&PullRequest>) -> bool {
    match (pin, pr) {
        (None, None) => true,
        (Some(pin), Some(pr)) => pin.number == pr.number && pin.head_sha == pr.head_ref_oid,
        _ => false,
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
