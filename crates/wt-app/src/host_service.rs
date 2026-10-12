//! The host-local application boundary. Both an in-process controller and the
//! SSH worker call this service, so commands and sources have one implementation.
use anyhow::Result;
use tokio_util::sync::CancellationToken;
use wt_runtime::{Domain, SourceSnapshot, Stamp, TaskScope};
use wt_tui::{Board, PrPin, ReplyOutcome, UiAction, UiReply};

use crate::{context::AppContext, github_actions::GithubAction, sources::BoardSources};

pub struct HostService {
    pub context: AppContext,
    pub sources: BoardSources,
}

/// How long stamp-only changes keep flowing after an accepted write; the
/// longest pending deadline for an accepted or unconfirmed change.
const SETTLEMENT_WINDOW: std::time::Duration = std::time::Duration::from_secs(30);

impl HostService {
    pub fn start(scope: &TaskScope, mut context: AppContext, commands: CancellationToken) -> Self {
        // Closing the view stops reads. An accepted mutation instead belongs to
        // its command drain, including when an SSH connection disappears.
        context.cancellation = commands;
        let sources = crate::sources::start(scope, &context);
        sources.board.refresh();
        Self { context, sources }
    }

    /// Run one host-owned request, then refresh exactly the domains its
    /// effect names and classify the reply for the TUI's pending engine.
    pub async fn execute(&self, command: UiAction) -> Result<UiReply> {
        let snapshot = self.sources.board.snapshot();
        let empty = Board::default();
        let effect = command.effect(snapshot.data().map_or(&empty, |board| board.as_ref()));
        let result = self.run(command, &snapshot).await;
        // Stamp the write before any refresh is queued, so the confirming
        // fetch always starts at or after it.
        let done = Stamp::now();
        let mut reply = match result {
            Ok(reply) => reply,
            Err(error) => {
                // A mutation that failed partway may still have changed
                // something; re-read what it touches.
                self.refresh_after(&effect, &UiReply::default());
                return Err(error);
            }
        };
        if !reply.failed && reply.outcome.is_none() && matches!(effect, wt_tui::Effect::Mutation(_))
        {
            reply.outcome = Some(ReplyOutcome::Accepted(done));
        }
        if matches!(
            reply.outcome,
            Some(ReplyOutcome::Accepted(_) | ReplyOutcome::Uncertain)
        ) {
            // Let identical refetches carry their stamps to the TUI until
            // the change is confirmed.
            wt_runtime::hold_for_settlement(SETTLEMENT_WINDOW);
        }
        self.refresh_after(&effect, &reply);
        Ok(reply)
    }

    fn refresh_after(&self, effect: &wt_tui::Effect, reply: &UiReply) {
        let mut local = false;
        let mut board = false;
        for domain in effect.domains() {
            match domain {
                Domain::Git => {
                    self.sources.git.refresh();
                }
                Domain::Metadata => {
                    self.sources.metadata.refresh();
                }
                // The read that may confirm an accepted write must be a
                // direct fetch that started after it, not a daemon snapshot.
                Domain::Github if matches!(reply.outcome, Some(ReplyOutcome::Accepted(_))) => {
                    self.sources.refresh_github_live()
                }
                Domain::Github => self.sources.github_refresh.refresh(),
                Domain::Reviews => self.sources.review_requests.refresh(),
                Domain::Sessions => {
                    self.sources.sessions.inventory.refresh();
                    self.sources.sessions.discoveries.refresh();
                    self.sources.session_activity.refresh();
                }
                Domain::Actions | Domain::Issues => board = true,
                Domain::Activity
                | Domain::Dev
                | Domain::Origin
                | Domain::Naming
                | Domain::Presentation
                | Domain::History
                | Domain::Perf
                | Domain::Automations
                | Domain::Host => local = true,
            }
        }
        if board {
            self.sources.board.refresh();
        }
        if local {
            self.sources.local.refresh();
        }
    }

    async fn run(&self, command: UiAction, snapshot: &SourceSnapshot<Board>) -> Result<UiReply> {
        if matches!(command, UiAction::PrepareHardRefresh) {
            return Ok(UiReply {
                modal: Some(wt_tui::UiModal::Confirm {
                    title: "Clear derived caches?".into(),
                    lines: vec!["Refetch current data and regenerate automatic titles. Saved worktree state and running work are preserved.".into()],
                    action: wt_tui::ConfirmAction::HardRefresh,
                    cancel_key: None,
                }),
                ..Default::default()
            });
        }
        if matches!(command, UiAction::HardRefresh) {
            self.sources
                .hard_refresh
                .refresh()
                .await
                .map_err(anyhow::Error::msg)?;
            return Ok(UiReply {
                message: "Caches cleared; refreshing…".into(),
                ..Default::default()
            });
        }
        // Safety and identity checks read only live-fetched GitHub data; the
        // board is display data used to resolve rows.
        let github = self.sources.github.confirmed();
        let github = github.as_ref();
        let board = snapshot.data().map(|board| board.as_ref());
        if matches!(
            command,
            UiAction::PrepareReviewers { .. } | UiAction::SubmitReviewers { .. }
        ) {
            let board = board.ok_or_else(|| anyhow::anyhow!("worktree board is still loading"))?;
            return match command {
                UiAction::PrepareReviewers { key } => {
                    Box::pin(
                        self.sources
                            .github_pickers
                            .prepare_reviewers(&key, board, github),
                    )
                    .await
                }
                UiAction::SubmitReviewers {
                    key,
                    pr_number,
                    original,
                    selected,
                } => {
                    Box::pin(self.sources.github_pickers.submit_reviewers(
                        crate::github_pickers::ReviewerSubmission {
                            key: &key,
                            pr_number,
                            original: &original,
                            selected: &selected,
                            board,
                            github,
                        },
                    ))
                    .await
                }
                _ => unreachable!(),
            };
        }
        let github_action = match &command {
            UiAction::GithubMarkReady { key, pin } => Some(GithubAction::MarkReady {
                key: key.clone(),
                pin: pin.clone(),
            }),
            UiAction::GithubSetAutoMerge { key, enable, pin } => Some(GithubAction::SetAutoMerge {
                key: key.clone(),
                enable: *enable,
                pin: pin.clone(),
            }),
            UiAction::GithubShip { key, pin } => Some(GithubAction::Ship {
                key: key.clone(),
                pin: pin.clone(),
            }),
            UiAction::GithubFailedChecks { key } => {
                Some(GithubAction::FailedChecks { key: key.clone() })
            }
            // The TUI resolves this toggle into an explicit direction from
            // what the user saw. The host never guesses a direction.
            UiAction::PrepareAction { id, .. } if id == wt_tui::AUTO_MERGE_ACTION_ID => {
                return Ok(UiReply {
                    message:
                        "merge when ready needs an explicit arm or disarm; update wt and retry"
                            .into(),
                    failed: true,
                    ..Default::default()
                });
            }
            _ => None,
        };
        if let Some(action) = github_action {
            let board = board.ok_or_else(|| anyhow::anyhow!("worktree board is still loading"))?;
            return Box::pin(self.sources.github_actions.execute(action, board, github)).await;
        }
        let reply = match command {
            UiAction::PrepareReviewCheckout {
                url,
                updated_at,
                branch,
            } => {
                let row = board
                    .and_then(|board| {
                        board.review_requests.iter().find(|row| {
                            row.url == url && row.updated_at == updated_at && row.branch == branch
                        })
                    })
                    .ok_or_else(|| {
                        anyhow::anyhow!("review request changed; refresh and select it again")
                    })?;
                UiReply {
                    modal: Some(wt_tui::UiModal::Confirm {
                        title: format!("Check out PR #{} for review?", row.number),
                        lines: vec![
                            row.title.clone(),
                            branch.clone(),
                            "Create a worktree in Reviews at the verified PR head.".into(),
                        ],
                        action: wt_tui::ConfirmAction::ReviewCheckout {
                            url,
                            updated_at,
                            branch,
                        },
                        cancel_key: Some('w'),
                    }),
                    ..Default::default()
                }
            }
            UiAction::ReviewCheckout {
                url,
                updated_at,
                branch,
            } => {
                let created = Box::pin(self.sources.review_requests.checkout(
                    crate::review_requests::ReviewCheckout {
                        url,
                        updated_at,
                        branch,
                    },
                ))
                .await?;
                UiReply {
                    message: format!("Created {} for review", created.target.slug()),
                    select_when_visible: Some(wt_core::worktree_target_key(&created.target)),
                    ..Default::default()
                }
            }
            UiAction::DismissReviewRequest { url, updated_at } => {
                self.sources
                    .review_requests
                    .dismiss(&url, &updated_at)
                    .await?;
                UiReply {
                    message: "Review request dismissed until it changes".into(),
                    ..Default::default()
                }
            }
            UiAction::PrepareRestoreRemoved { key } => {
                Box::pin(crate::history_actions::prepare_restore(&self.context, &key)).await?
            }
            UiAction::RestoreRemoved {
                key,
                removed_at,
                branch,
            } => {
                Box::pin(crate::history_actions::restore(
                    &self.context,
                    &key,
                    &removed_at,
                    &branch,
                ))
                .await?
            }
            UiAction::ToggleRemovedAutomations { key } => {
                Box::pin(crate::history_actions::toggle_automations_paused(
                    &self.context,
                    &key,
                ))
                .await?
            }
            UiAction::Restack { key } => {
                Box::pin(crate::restack_action::start(
                    &self.context,
                    &key,
                    github,
                    board,
                ))
                .await?
            }
            UiAction::PrepareGithub { key, ship } => {
                let row = board
                    .and_then(|board| board.rows.iter().find(|row| row.key == key))
                    .ok_or_else(|| anyhow::anyhow!("worktree is no longer on the board"))?;
                let github =
                    github.ok_or_else(|| anyhow::anyhow!("GitHub data is still loading"))?;
                let pr = github
                    .prs
                    .get(&row.branch)
                    .ok_or_else(|| crate::controller::notice("no PR for this row"))?;
                if pr.state != "OPEN" {
                    anyhow::bail!("PR #{} is not open", pr.number);
                }
                let config = &self.context.config.github;
                let reviewer = config
                    .reviewers
                    .then_some(config.default_reviewer.as_deref())
                    .flatten();
                match github_prompt(pr, ship, reviewer) {
                    GithubPrompt::Done(text) => UiReply {
                        message: text,
                        ..Default::default()
                    },
                    GithubPrompt::Confirm(title) => UiReply {
                        modal: Some(wt_tui::UiModal::Confirm {
                            // The write executes only against this same PR and head.
                            action: wt_tui::ConfirmAction::Github {
                                key,
                                ship,
                                pin: Some(PrPin {
                                    number: pr.number,
                                    head_sha: pr.head_ref_oid.clone(),
                                }),
                            },
                            title,
                            lines: vec![format!("{}: {}", row.branch, pr.title)],
                            cancel_key: Some(if ship { 'E' } else { 'e' }),
                        }),
                        ..Default::default()
                    },
                }
            }
            UiAction::CancelAutomations => {
                let cancelled = self.sources.automations.cancel_pending().await?;
                UiReply {
                    message: format!(
                        "Cancelled {cancelled} queued automations; running actions continue"
                    ),
                    ..Default::default()
                }
            }
            UiAction::GenerateTitle { key } => {
                self.sources
                    .naming
                    .request(key)
                    .map_err(anyhow::Error::msg)?;
                UiReply {
                    message: "Generating title…".into(),
                    ..Default::default()
                }
            }
            action @ (UiAction::PrepareActions { .. }
            | UiAction::PrepareAction { .. }
            | UiAction::RunAction { .. }) => {
                Box::pin(crate::action_palette::execute(
                    &self.context,
                    action,
                    board,
                    github,
                ))
                .await?
            }
            command => {
                Box::pin(crate::controller_actions::execute(
                    &self.context,
                    command,
                    board,
                    github,
                ))
                .await?
            }
        };
        Ok(reply)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum GithubPrompt {
    /// Nothing left to do; show this message instead of a confirmation.
    Done(String),
    /// Confirmation title listing only the remaining steps.
    Confirm(String),
}

/// `e` / `E` pre-checks from the TS keymap: mark-ready refuses a PR that is
/// already ready, and ship lists only the steps still needed.
fn github_prompt(pr: &wt_github::PullRequest, ship: bool, reviewer: Option<&str>) -> GithubPrompt {
    if !ship {
        return if pr.is_draft {
            GithubPrompt::Confirm(format!("Mark #{} ready for review?", pr.number))
        } else {
            GithubPrompt::Done(format!("PR #{} is already ready", pr.number))
        };
    }
    let mut steps = Vec::new();
    if pr.is_draft {
        steps.push("mark ready".to_owned());
    }
    if let Some(reviewer) = reviewer
        && !pr.requested_reviewers.iter().any(|login| login == reviewer)
    {
        steps.push(format!("request {reviewer}"));
    }
    if pr.auto_merge.is_none() {
        steps.push("arm auto-merge".to_owned());
    }
    if steps.is_empty() {
        GithubPrompt::Done(format!("#{} already shipped", pr.number))
    } else {
        GithubPrompt::Confirm(format!("Ship #{}? ({})", pr.number, steps.join(", ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(draft: bool, reviewers: &[&str], armed: bool) -> wt_github::PullRequest {
        let auto_merge =
            armed.then(|| serde_json::json!({"enabledAt": "x", "mergeMethod": "SQUASH"}));
        serde_json::from_value(serde_json::json!({
            "number": 7, "url": "u", "headRefName": "b", "baseRefName": "main",
            "mergeCommitOid": null, "title": "t", "isDraft": draft, "state": "OPEN",
            "mergeable": null, "mergeStateStatus": null, "checks": "none",
            "failedChecks": [], "review": "none", "reviewRequests": 0,
            "requestedReviewers": reviewers, "suggestedReviewers": [], "autoMerge": auto_merge,
            "comments": [], "unresolvedThreads": 0, "unresolvedThreadsTotal": 0,
            "mergedAt": null, "closedAt": null
        }))
        .unwrap()
    }

    #[test]
    fn mark_ready_refuses_a_ready_pr_and_ship_lists_remaining_steps() {
        assert_eq!(
            github_prompt(&pr(false, &[], false), false, None),
            GithubPrompt::Done("PR #7 is already ready".into())
        );
        assert_eq!(
            github_prompt(&pr(true, &[], false), false, None),
            GithubPrompt::Confirm("Mark #7 ready for review?".into())
        );
        assert_eq!(
            github_prompt(&pr(true, &[], false), true, Some("ana")),
            GithubPrompt::Confirm("Ship #7? (mark ready, request ana, arm auto-merge)".into())
        );
        assert_eq!(
            github_prompt(&pr(false, &["ana"], false), true, Some("ana")),
            GithubPrompt::Confirm("Ship #7? (arm auto-merge)".into())
        );
        assert_eq!(
            github_prompt(&pr(false, &["ana"], true), true, Some("ana")),
            GithubPrompt::Done("#7 already shipped".into())
        );
    }
}
