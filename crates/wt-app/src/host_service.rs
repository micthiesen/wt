//! The host-local application boundary. Both an in-process controller and the
//! SSH worker call this service, so commands and sources have one implementation.
use anyhow::Result;
use tokio_util::sync::CancellationToken;
use wt_runtime::TaskScope;
use wt_tui::{UiAction, UiReply};

use crate::{context::AppContext, sources::BoardSources};

pub struct HostService {
    pub context: AppContext,
    pub sources: BoardSources,
}

impl HostService {
    pub fn start(scope: &TaskScope, mut context: AppContext, commands: CancellationToken) -> Self {
        // Closing the view stops reads. An accepted mutation instead belongs to
        // its command drain, including when an SSH connection disappears.
        context.cancellation = commands;
        let sources = crate::sources::start(scope, &context);
        sources.board.refresh();
        Self { context, sources }
    }

    pub async fn execute(&self, command: UiAction) -> Result<UiReply> {
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
        let state_only = matches!(
            command,
            UiAction::FoldSection { .. }
                | UiAction::MoveSection { .. }
                | UiAction::RenameSection { .. }
                | UiAction::SetTitle { .. }
                | UiAction::ToggleArchive { .. }
                | UiAction::SetStatus { .. }
                | UiAction::SetBase { .. }
                | UiAction::SetIssueOverride { .. }
        );
        let state_only = state_only
            || matches!(
                command,
                UiAction::ToggleAutomations { .. }
                    | UiAction::CancelAutomations
                    | UiAction::ToggleRemovedAutomations { .. }
            );
        let read_only = matches!(
            command,
            UiAction::Copy { .. }
                | UiAction::OpenEditor { .. }
                | UiAction::OpenUrl { .. }
                | UiAction::PrepareRemove { .. }
                | UiAction::PrepareCleanup
                | UiAction::PrepareStatus { .. }
                | UiAction::PrepareBase { .. }
                | UiAction::PrepareSection { .. }
                | UiAction::PrepareActions { .. }
                | UiAction::PrepareAction { .. }
                | UiAction::GenerateTitle { .. }
                | UiAction::PrepareCreate { .. }
                | UiAction::PrepareGithub { .. }
                | UiAction::PrepareRestoreRemoved { .. }
        );
        let read_only = read_only
            || matches!(
                command,
                UiAction::PrepareSessions { .. }
                    | UiAction::PrepareHarnesses { .. }
                    | UiAction::PrepareStopTerminal { .. }
            );
        let session_changed = matches!(
            command,
            UiAction::StopSession { .. }
                | UiAction::KillSession { .. }
                | UiAction::StopTerminal { .. }
        );
        let created = matches!(command, UiAction::Create { .. });
        let snapshot = self.sources.board.snapshot();
        if matches!(
            command,
            UiAction::PrepareReviewers { .. } | UiAction::SubmitReviewers { .. }
        ) {
            let github = self.sources.github.snapshot();
            let board = snapshot
                .data
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("worktree board is still loading"))?;
            let data = github
                .data
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("GitHub data is still loading"))?;
            return match command {
                UiAction::PrepareReviewers { key } => {
                    Box::pin(
                        self.sources
                            .github_pickers
                            .prepare_reviewers(&key, board, data),
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
                            github: data,
                            actions: &self.sources.github_actions,
                        },
                    ))
                    .await
                }
                _ => unreachable!(),
            };
        }
        let github_action = match &command {
            UiAction::GithubMarkReady { key } => {
                Some(crate::github_actions::GithubAction::MarkReady { key: key.clone() })
            }
            UiAction::GithubSetAutoMerge { key, enable } => {
                Some(crate::github_actions::GithubAction::SetAutoMerge {
                    key: key.clone(),
                    enable: *enable,
                })
            }
            UiAction::GithubShip { key } => {
                Some(crate::github_actions::GithubAction::Ship { key: key.clone() })
            }
            UiAction::GithubFailedChecks { key } => {
                Some(crate::github_actions::GithubAction::FailedChecks { key: key.clone() })
            }
            UiAction::PrepareAction {
                surface: wt_tui::ActionSurface::Row { key },
                id,
                ..
            } if id == crate::action_palette::AUTO_MERGE_ID => {
                Some(crate::github_actions::GithubAction::ToggleAutoMerge { key: key.clone() })
            }
            _ => None,
        };
        if let Some(action) = github_action {
            let github = self.sources.github.snapshot();
            let board = snapshot
                .data
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("worktree board is still loading"))?;
            let data = github
                .data
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("GitHub data is still loading"))?;
            return Box::pin(self.sources.github_actions.execute(action, board, data)).await;
        }
        let reply = match command {
            UiAction::PrepareReviewCheckout {
                url,
                updated_at,
                branch,
            } => {
                let row = snapshot
                    .data
                    .as_ref()
                    .and_then(|board| {
                        board.review_requests.iter().find(|row| {
                            row.url == url && row.updated_at == updated_at && row.branch == branch
                        })
                    })
                    .ok_or_else(|| {
                        anyhow::anyhow!("review request changed; refresh and select it again")
                    })?;
                return Ok(UiReply {
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
                });
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
                return Ok(UiReply {
                    message: "Review request dismissed until it changes".into(),
                    ..Default::default()
                });
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
                let github = self.sources.github.snapshot();
                let empty = wt_github::GithubData::default();
                Box::pin(crate::restack_action::start(
                    &self.context,
                    &key,
                    github.data.as_deref().unwrap_or(&empty),
                    snapshot.data.as_deref(),
                ))
                .await?
            }
            UiAction::PrepareGithub { key, ship } => {
                let github = self.sources.github.snapshot();
                let row = snapshot
                    .data
                    .as_ref()
                    .and_then(|board| board.rows.iter().find(|row| row.key == key))
                    .ok_or_else(|| anyhow::anyhow!("worktree is no longer on the board"))?;
                let pr = github
                    .data
                    .as_ref()
                    .and_then(|data| data.prs.get(&row.branch))
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
                            action: wt_tui::ConfirmAction::Github { key, ship },
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
                let github = self.sources.github.snapshot();
                let empty = wt_github::GithubData::default();
                Box::pin(crate::action_palette::execute(
                    &self.context,
                    action,
                    snapshot.data.as_deref(),
                    github.data.as_deref().unwrap_or(&empty),
                ))
                .await?
            }
            command => {
                let github = self.sources.github.snapshot();
                let empty = wt_github::GithubData::default();
                Box::pin(crate::controller_actions::execute(
                    &self.context,
                    command,
                    snapshot.data.as_deref(),
                    github.data.as_deref().unwrap_or(&empty),
                ))
                .await?
            }
        };
        if created && !reply.failed {
            self.sources.git.refresh();
            self.sources.metadata.refresh();
        } else if session_changed {
            self.sources.sessions.inventory.refresh();
        } else if state_only {
            self.sources.metadata.refresh();
        } else if !read_only {
            self.sources.local.refresh();
        }
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
