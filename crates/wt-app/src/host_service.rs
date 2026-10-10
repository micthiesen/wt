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
                    | UiAction::PrepareStopSession { .. }
                    | UiAction::PrepareStopTerminal { .. }
            );
        let session_changed = matches!(
            command,
            UiAction::StopSession { .. } | UiAction::StopTerminal { .. }
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
                    .filter(|pr| pr.state == "OPEN")
                    .ok_or_else(|| anyhow::anyhow!("{} has no open pull request", row.slug))?;
                UiReply {
                    modal: Some(wt_tui::UiModal::Confirm {
                        action: wt_tui::ConfirmAction::Github { key, ship },
                        title: if ship {
                            format!("Ship PR #{}?", pr.number)
                        } else {
                            format!("Mark PR #{} ready?", pr.number)
                        },
                        lines: if ship {
                            vec![format!("{}: {}", row.branch, pr.title),
                        "Mark ready, request the configured reviewer, and arm merge when ready.".into()]
                        } else {
                            vec![
                                format!("{}: {}", row.branch, pr.title),
                                "Remove draft status from this pull request.".into(),
                            ]
                        },
                        cancel_key: Some(if ship { 'E' } else { 'e' }),
                    }),
                    ..Default::default()
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
