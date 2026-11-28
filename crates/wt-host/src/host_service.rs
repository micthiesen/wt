//! The host-local application boundary. Both an in-process controller and the
//! SSH worker call this service, so commands and sources have one implementation.
use anyhow::Result;
use tokio_util::sync::CancellationToken;
use wt_core::Domain;
use wt_runtime::{SourceSnapshot, Stamp, TaskScope};
use wt_tui::{ActionSurface, Board, ReplyOutcome, UiAction, UiReply};

use wt_app_core::context::AppContext;
use wt_sources::{github_actions::GithubAction, sources::BoardSources};

pub struct HostService {
    pub context: AppContext,
    pub sources: BoardSources,
}

/// How long stamp-only changes keep flowing after an accepted write; the
/// longest pending deadline for an accepted or unconfirmed change.
const SETTLEMENT_WINDOW: std::time::Duration = std::time::Duration::from_secs(30);

/// Complete an operation executed by this process, before scheduling its
/// confirming reads. Never use this to classify a foreign or aggregate write.
/// Explicit outcomes retain their original ownership and clock.
pub fn complete_local(effect: &wt_tui::Effect, mut reply: UiReply) -> UiReply {
    if !reply.failed && reply.outcome.is_none() && matches!(effect, wt_tui::Effect::Mutation(_)) {
        // A mutation can discover changed preconditions and ask for a new
        // confirmation instead of writing. Its optimistic effect must settle
        // as no change, not as an accepted write waiting for readback.
        reply.outcome = Some(if reply.modal.is_some() {
            ReplyOutcome::NoChange
        } else {
            ReplyOutcome::Accepted(Stamp::now())
        });
    }
    if matches!(
        reply.outcome,
        Some(ReplyOutcome::Accepted(_) | ReplyOutcome::Uncertain)
    ) {
        wt_runtime::hold_for_settlement(SETTLEMENT_WINDOW);
    }
    reply
}

/// The oldest GitHub read a GitHub write may rely on for its PR checks. The
/// board's action facts go unknown at the same age, so a palette does not
/// offer what this bound would refuse.
const WRITE_EVIDENCE_AGE: std::time::Duration = wt_sources::action_source::PR_EVIDENCE_AGE;

/// A PR-dependent request can arrive before the shared GitHub leaf has a
/// recent enough read of its branch: wait briefly for that leaf rather than
/// treating the restored board as proof of presence or absence. A palette
/// waits for the board to show the read before it sends, so a palette commit
/// only waits here when the read aged out or failed in between.
const ACTION_GITHUB_WAIT: std::time::Duration = std::time::Duration::from_secs(12);

impl HostService {
    /// The host's current board and row layout, as the wire protocol sends it.
    pub fn snapshot(&self) -> wt_app_core::host_protocol::HostSnapshot {
        let snapshot = self.sources.board.snapshot();
        let metadata = self.sources.metadata.snapshot();
        let mut layout = std::collections::BTreeMap::new();
        if let Some(board) = snapshot.data() {
            for row in &board.rows {
                let entry = metadata.data().map(|data| &data.0["slugs"][&row.key]);
                layout.insert(
                    row.key.clone(),
                    wt_app_core::host_protocol::RowLayout {
                        base_branch: entry
                            .and_then(|e| e["baseBranch"].as_str())
                            .map(str::to_owned),
                        work: entry.and_then(|e| wt_core::parse_work_status(&e["work"])),
                    },
                );
            }
        }
        wt_app_core::host_protocol::HostSnapshot {
            board: snapshot.data().map(|board| (**board).clone()),
            state: snapshot.state().into(),
            domains: snapshot.domains().clone(),
            layout,
        }
    }

    pub async fn shutdown_automations(&self) -> Result<()> {
        self.sources.automation_drain.shutdown().await
    }

    pub fn start(scope: &TaskScope, mut context: AppContext, commands: CancellationToken) -> Self {
        // Closing the view stops reads. An accepted mutation instead belongs to
        // its command drain, including when an SSH connection disappears.
        context.cancellation = commands;
        let sources = wt_sources::sources::start(scope, &context);
        sources.board.refresh();
        let adopting = context.clone();
        let metadata = sources.metadata.clone();
        scope.spawn(async move {
            match crate::section_actions::adopt_explicit_sections(&adopting).await {
                Ok(true) => {
                    metadata.refresh();
                }
                Ok(false) => {}
                Err(error) => tracing::warn!(%error, "could not adopt explicit sections"),
            }
        });
        // wt no longer generates titles or summaries; drop their cache.
        let retired = context.config.paths.cache_root.join("naming-v1.json");
        scope.spawn(async move {
            match tokio::fs::remove_file(&retired).await {
                Ok(()) => {
                    tracing::info!(path = %retired.display(), "removed the retired naming cache")
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => tracing::warn!(%error, "could not remove the retired naming cache"),
            }
        });
        Self { context, sources }
    }

    /// Run one host-owned request, then refresh exactly the domains its
    /// effect names and classify the reply for the TUI's pending engine.
    pub async fn execute(&self, command: UiAction) -> Result<UiReply> {
        self.execute_with_shutdown(command, None, None).await
    }

    /// Run a request owned by a scheduler: the interactive controller or a
    /// worker's command loop. Shutdown may stop waiting for a durable removal
    /// only after its worker is proven running; other accepted mutations
    /// continue through their normal completion. `hold` is the request's
    /// place in that scheduler, which a removal narrows once its worker runs.
    pub async fn execute_with_shutdown(
        &self,
        command: UiAction,
        shutdown: Option<&CancellationToken>,
        hold: Option<&wt_app_core::lanes::LaneHold>,
    ) -> Result<UiReply> {
        let snapshot = self.sources.board.snapshot();
        let empty = Board::default();
        let effect = command.effect(snapshot.data().map_or(&empty, |board| board.as_ref()));
        let result = self.run(command, &snapshot, shutdown, hold).await;
        // Stamp the write before any refresh is queued, so the confirming
        // fetch always starts at or after it.
        let reply = match result {
            Ok(reply) => reply,
            Err(error)
                if error
                    .downcast_ref::<wt_ops::commands::_destroy::RemovalUncertain>()
                    .is_some() =>
            {
                UiReply::uncertain(format!("{error:#}"))
            }
            Err(error) => {
                // A mutation that failed partway may still have changed
                // something; re-read what it touches.
                self.refresh_after(&effect, &UiReply::default());
                return Err(error);
            }
        };
        let reply = complete_local(&effect, reply);
        self.refresh_after(&effect, &reply);
        Ok(reply)
    }

    fn refresh_after(&self, effect: &wt_tui::Effect, reply: &UiReply) {
        // Domains without a handle here are reread through the board
        // pipeline: from its last stage, or from the local board stage.
        let mut refresh_full_board = false;
        let mut refresh_local_board = false;
        for domain in effect.domains() {
            match domain {
                // A user-caused read of the rows the change names, ahead of
                // background reads; a change that names none reads them all.
                // A created row is only named by the reply.
                Domain::Git => {
                    match effect.rows() {
                        Some(rows) => self.sources.git.request_keys(
                            wt_runtime::Request::Direct,
                            rows.into_iter().chain(reply.select_when_visible.clone()),
                        ),
                        None => self.sources.git.request(wt_runtime::Request::Direct),
                    };
                }
                Domain::Metadata => {
                    self.sources.metadata.refresh();
                }
                // The read that may confirm an accepted write must be a
                // direct fetch that started after it, not a daemon snapshot.
                Domain::Github => match &reply.outcome {
                    Some(ReplyOutcome::Accepted(at)) => {
                        self.sources.github_refresh.refresh_live_since(*at)
                    }
                    _ => self.sources.github_refresh.refresh(),
                },
                Domain::Reviews => self.sources.review_requests.refresh(),
                Domain::Sessions => {
                    self.sources.sessions.inventory.refresh();
                    self.sources.sessions.discoveries.refresh();
                    self.sources.session_activity.refresh();
                }
                Domain::Actions | Domain::Issues => refresh_full_board = true,
                Domain::Activity
                | Domain::Dev
                | Domain::Origin
                | Domain::Presentation
                | Domain::History
                | Domain::Perf
                | Domain::Automations
                | Domain::Host => refresh_local_board = true,
            }
        }
        if refresh_full_board {
            self.sources.board.refresh();
        }
        if refresh_local_board {
            self.sources.local.refresh();
        }
    }

    /// Refresh tmux and saved-conversation history for one slug. The picker
    /// open on the board rebuilds from the discoveries source as each
    /// harness completes.
    fn queue_session_discovery(&self, context: &AppContext, slug: &str) {
        self.sources.sessions.inventory.refresh();
        for harness in wt_app_core::harness::visible_harnesses(&context.config) {
            self.sources
                .sessions
                .commands
                .discover(slug.to_owned(), harness);
        }
    }

    async fn run(
        &self,
        command: UiAction,
        snapshot: &SourceSnapshot<Board>,
        shutdown: Option<&CancellationToken>,
        hold: Option<&wt_app_core::lanes::LaneHold>,
    ) -> Result<UiReply> {
        if command.is_board_menu() {
            anyhow::bail!("this menu is built from the board and never asks the host");
        }
        let context = self.context.for_action(&command)?;
        // Background read behind an open picker, which the board built: no
        // reply, and a target without a slug has nothing to refresh.
        if let UiAction::DiscoverSessions { key, target } = &command {
            if let Ok(slug) =
                wt_app_core::harness::picker_slug(&context, key.as_deref(), *target).await
            {
                self.queue_session_discovery(&context, &slug);
            }
            return Ok(UiReply::default());
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
        let board = snapshot.data().map(|board| board.as_ref());
        let branch = self.pr_dependent_branch(&command, board);
        let github = self.sources.github.confirmed();
        let github_ready = github.as_ref().is_some_and(|github| {
            github.fresh_within(WRITE_EVIDENCE_AGE)
                && branch.as_deref().is_none_or(|branch| {
                    !matches!(github.covered_pr(branch), wt_github::CoveredPr::Uncovered)
                })
        });
        let github = if !github_ready && self.pr_dependent_command(&command) {
            self.await_confirmed_github(branch.as_deref())
                .await
                .or_else(|| github.filter(|github| github.fresh_within(WRITE_EVIDENCE_AGE)))
        } else {
            github
        };
        let github = github.as_ref();
        // A GitHub write must also not act on a read from long ago (a lane
        // that has been failing, say): its PR and head checks need a recent
        // read. Landing evidence for cleanup stays valid at any age.
        let github_for_writes = github.filter(|github| github.fresh_within(WRITE_EVIDENCE_AGE));
        if let UiAction::SubmitReviewers {
            key,
            pr_number,
            original,
            selected,
        } = &command
        {
            let board = board.ok_or_else(|| anyhow::anyhow!("worktree board is still loading"))?;
            return Box::pin(self.sources.github_pickers.submit_reviewers(
                wt_sources::github_pickers::ReviewerSubmission {
                    key,
                    pr_number: *pr_number,
                    original,
                    selected,
                    board,
                    github: github_for_writes,
                },
            ))
            .await;
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
            _ => None,
        };
        if let Some(action) = github_action {
            let board = board.ok_or_else(|| anyhow::anyhow!("worktree board is still loading"))?;
            return Box::pin(
                self.sources
                    .github_actions
                    .execute(action, board, github_for_writes),
            )
            .await;
        }
        let reply = match command {
            UiAction::ReviewCheckout {
                url,
                updated_at,
                branch,
            } => {
                let created = Box::pin(self.sources.review_requests.checkout(
                    wt_sources::review_requests::ReviewCheckout {
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
            UiAction::RestoreRemoved {
                key,
                removed_at,
                branch,
            } => {
                Box::pin(crate::history_actions::restore(
                    &context,
                    &key,
                    &removed_at,
                    &branch,
                ))
                .await?
            }
            UiAction::ToggleRemovedAutomations { key, seen } => {
                Box::pin(crate::history_actions::toggle_automations_paused_seen(
                    &context, &key, seen,
                ))
                .await?
            }
            UiAction::Restack { key } => {
                Box::pin(wt_ops::restack_action::start(&context, &key, github, board)).await?
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
            action @ (UiAction::PrepareDevLogs { .. } | UiAction::RunAction { .. }) => {
                Box::pin(crate::action_palette::execute(
                    &context,
                    action,
                    board,
                    github.filter(|github| github.fresh_within(WRITE_EVIDENCE_AGE)),
                ))
                .await?
            }
            command => {
                Box::pin(crate::controller_actions::execute_with_shutdown(
                    &context, command, board, github, shutdown, hold,
                ))
                .await?
            }
        };
        Ok(reply)
    }

    fn action_requires_pr(&self, command: &UiAction) -> bool {
        let UiAction::RunAction { surface, id, .. } = command else {
            return false;
        };
        self.surface_action_requires_pr(surface, id)
    }

    fn surface_action_requires_pr(&self, surface: &ActionSurface, id: &str) -> bool {
        wt_ops::action_dispatch::definitions(&self.context, surface)
            .into_iter()
            .find(|definition| definition.id == *id)
            .is_some_and(|definition| {
                definition.requires.iter().any(|requirement| {
                    matches!(
                        requirement,
                        wt_config::RequireTag::Pr | wt_config::RequireTag::PrReady
                    )
                })
            })
    }

    fn pr_dependent_command(&self, command: &UiAction) -> bool {
        self.action_requires_pr(command)
            || matches!(
                command,
                UiAction::SubmitReviewers { .. }
                    | UiAction::GithubMarkReady { .. }
                    | UiAction::GithubSetAutoMerge { .. }
                    | UiAction::GithubShip { .. }
                    | UiAction::GithubFailedChecks { .. }
            )
    }

    fn pr_dependent_branch(&self, command: &UiAction, board: Option<&Board>) -> Option<String> {
        let key = match command {
            UiAction::RunAction { surface, id, .. }
                if self.surface_action_requires_pr(surface, id) =>
            {
                match surface {
                    ActionSurface::Row { key } | ActionSurface::Manager { key: Some(key) } => {
                        key.as_str()
                    }
                    _ => return None,
                }
            }
            UiAction::SubmitReviewers { key, .. }
            | UiAction::GithubMarkReady { key, .. }
            | UiAction::GithubSetAutoMerge { key, .. }
            | UiAction::GithubShip { key, .. }
            | UiAction::GithubFailedChecks { key } => key.as_str(),
            _ => return None,
        };
        board?
            .rows
            .iter()
            .find(|row| row.key == key)
            .map(|row| row.branch.clone())
    }

    async fn await_confirmed_github(
        &self,
        branch: Option<&str>,
    ) -> Option<wt_runtime::Confirmed<wt_github::GithubData>> {
        self.sources.github_refresh.refresh();
        wait_for_confirmed_github(
            self.sources.github.clone(),
            branch.map(str::to_owned),
            ACTION_GITHUB_WAIT,
            self.context.cancellation.clone(),
        )
        .await
    }
}

async fn wait_for_confirmed_github(
    source: wt_runtime::LeafHandle<wt_github::GithubData>,
    branch: Option<String>,
    wait: std::time::Duration,
    cancellation: CancellationToken,
) -> Option<wt_runtime::Confirmed<wt_github::GithubData>> {
    let covers_branch = |github: &wt_runtime::Confirmed<wt_github::GithubData>| {
        github.fresh_within(WRITE_EVIDENCE_AGE)
            && branch.as_deref().is_none_or(|branch| {
                !matches!(github.covered_pr(branch), wt_github::CoveredPr::Uncovered)
            })
    };
    if let Some(github) = source.confirmed()
        && covers_branch(&github)
    {
        return Some(github);
    }
    let mut updates = source.subscribe();
    let wait_for_read = async {
        loop {
            let confirmed = {
                let snapshot = updates.borrow_and_update();
                source.confirm(&snapshot)
            };
            if let Some(github) = confirmed.filter(|github| covers_branch(github)) {
                return Some(github);
            }
            if updates.changed().await.is_err() {
                return None;
            }
        }
    };
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => None,
        result = tokio::time::timeout(wait, wait_for_read) => result.ok().flatten(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_runtime::{Persistence, RefreshPolicy, SourceSpec, TaskScope, leaf_channel};

    #[tokio::test]
    async fn board_menus_never_reach_the_host() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let scope = TaskScope::new();
        let host = HostService::start(
            &scope,
            fixture.ctx.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        // A board menu reaching a host means a second, unwitnessed path to
        // the same menu. Hosts refuse it rather than answer from their state.
        for action in [
            UiAction::PrepareStatus { key: "one".into() },
            UiAction::PrepareHarnesses { key: "one".into() },
            UiAction::PrepareSessions {
                key: Some("one".into()),
                target: wt_tui::SessionTarget::Harness,
            },
            UiAction::PrepareStopTerminal {
                key: "one".into(),
                target: wt_tui::SessionTarget::Shell,
            },
            UiAction::PrepareReviewers { key: "one".into() },
            UiAction::PrepareGithub {
                key: "one".into(),
                ship: true,
            },
            UiAction::OnHost {
                host: None,
                action: Box::new(UiAction::PrepareCreate {
                    initial: String::new(),
                    placement: None,
                }),
            },
        ] {
            assert!(action.is_board_menu());
            let error = host.execute(action).await.unwrap_err();
            assert!(
                error.to_string().contains("built from the board"),
                "{error:#}"
            );
        }
        // Content a host fetches is asked of it, whatever the board lists.
        for action in [
            UiAction::GithubFailedChecks { key: "one".into() },
            UiAction::PrepareDevLogs { key: "one".into() },
        ] {
            assert!(!action.is_board_menu(), "{action:?}");
        }
        scope
            .shutdown(std::time::Duration::from_secs(5))
            .await
            .unwrap();
        fixture.close().await.unwrap();
    }

    /// The whole path of a board-built confirmation, on the real source
    /// graph and a tmux server of the fixture's own: the board lists the
    /// terminal, the menu built from it names that exact terminal, and the
    /// host stops that one or nothing.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_terminal_stops_by_the_identity_the_board_showed_or_not_at_all() {
        let mut fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        std::sync::Arc::make_mut(&mut fixture.ctx.config)
            .tmux
            .socket = format!(
            "wt-stop-terminal-{}",
            fixture._root.path().file_name().unwrap().to_string_lossy()
        );
        let cancel = &fixture.ctx.cancellation;
        let tmux = wt_tmux::TmuxClient::new(
            fixture.ctx.processes.clone(),
            wt_tmux::TmuxServer::named(fixture.ctx.config.tmux.socket.clone())
                .with_cwd(fixture.ctx.home.clone()),
        );
        let shell = wt_tmux::CreateSession {
            name: "one-shell".into(),
            cwd: fixture.ctx.cwd.clone(),
            command: vec!["sleep".into(), "300".into()],
            width: None,
            height: None,
        };
        // A server left with no session exits and numbers its sessions from
        // the start again; a real one outlives any one terminal.
        let other = wt_tmux::CreateSession {
            name: "two-shell".into(),
            ..shell.clone()
        };
        tmux.create_session(&other, cancel).await.unwrap();
        tmux.create_session(&shell, cancel).await.unwrap();
        let scope = TaskScope::new();
        let host = HostService::start(
            &scope,
            fixture.ctx.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        // The confirmation the model opens for Shift+F10, from this board.
        let confirmed = |board: &Board| {
            let menu = UiAction::PrepareStopTerminal {
                key: "one".into(),
                target: wt_tui::SessionTarget::Shell,
            }
            .menu(board);
            let wt_tui::Menu::Local(wt_tui::LocalMenu::Modal { modal, .. }) = menu else {
                return None;
            };
            let wt_tui::UiModal::Confirm {
                action:
                    wt_tui::ConfirmAction::StopTerminal {
                        key,
                        target,
                        session_id,
                        created_at,
                    },
                ..
            } = *modal
            else {
                panic!("a stop confirmation")
            };
            Some(UiAction::StopTerminal {
                key,
                target,
                session_id,
                created_at,
            })
        };
        let mut updates = host.sources.board.subscribe();
        let mut listed = async |other_than: Option<&UiAction>| {
            let snapshot = tokio::time::timeout(
                std::time::Duration::from_secs(60),
                updates.wait_for(|snapshot| {
                    snapshot
                        .data()
                        .and_then(|board| confirmed(board))
                        .is_some_and(|stop| Some(&stop) != other_than)
                }),
            )
            .await
            .expect("the board lists the terminal")
            .unwrap();
            confirmed(snapshot.data().unwrap()).unwrap()
        };
        let shown = listed(None).await;
        let running = async || {
            tmux.list_sessions(cancel)
                .await
                .unwrap()
                .into_iter()
                .any(|session| session.name == "one-shell")
        };

        // The shell is replaced after the confirmation was drawn.
        tmux.kill_session("one-shell", cancel).await.unwrap();
        tmux.create_session(&shell, cancel).await.unwrap();
        let refused = host.execute(shown.clone()).await.unwrap_err();
        assert!(
            wt_app_core::host_errors::error_reply(&refused).stale,
            "{refused:#}"
        );
        assert!(running().await, "a terminal nobody confirmed was stopped");

        // The board catches up, and the confirmation rebuilt from it stops
        // the terminal that is there now.
        host.sources.sessions.inventory.refresh();
        let current = listed(Some(&shown)).await;
        let reply = host.execute(current).await.unwrap();
        assert!(!reply.failed, "{}", reply.message);
        assert!(!running().await);

        let _ = tmux.kill_server(cancel).await;
        scope
            .shutdown(std::time::Duration::from_secs(5))
            .await
            .unwrap();
        fixture.close().await.unwrap();
    }

    #[test]
    fn local_completion_classifies_only_successful_unclassified_mutations() {
        let effect = UiAction::FoldSection {
            key: "inbox".into(),
            folded: true,
        }
        .effect(&Board::default());
        let before = Stamp::now();
        let reply = complete_local(&effect, UiReply::default());
        let Some(ReplyOutcome::Accepted(stamp)) = reply.outcome else {
            panic!("local write must be accepted")
        };
        assert_eq!(stamp.at_or_after(&before), Some(true));
        assert_eq!(Stamp::now().at_or_after(&stamp), Some(true));
        assert!(
            complete_local(
                &effect,
                UiReply {
                    failed: true,
                    ..Default::default()
                }
            )
            .outcome
            .is_none()
        );
        let read = UiAction::PrepareSection { key: "one".into() }.effect(&Board::default());
        assert!(complete_local(&read, UiReply::default()).outcome.is_none());
        // Reading failed checks writes nothing. Its plain answer must not be
        // stamped as an accepted write, which would mark the row pending and
        // refresh session sources.
        let failed_checks = UiAction::GithubFailedChecks { key: "one".into() };
        let failed_checks = failed_checks.effect(&Board::default());
        assert!(failed_checks.domains().is_empty());
        assert!(
            complete_local(&failed_checks, UiReply::message("no failing checks"))
                .outcome
                .is_none()
        );
        let recheck = complete_local(
            &effect,
            UiReply {
                modal: Some(wt_tui::UiModal::Log {
                    title: "Review changed preconditions".into(),
                    lines: Vec::new(),
                    close_key: None,
                    refresh: None,
                }),
                ..Default::default()
            },
        );
        assert_eq!(recheck.outcome, Some(ReplyOutcome::NoChange));
        assert!(
            recheck.modal.is_some(),
            "the new prompt must remain visible"
        );
        let foreign: Stamp =
            serde_json::from_value(serde_json::json!({"epoch": 2, "offset_ms": 42})).unwrap();
        for outcome in [
            ReplyOutcome::NoChange,
            ReplyOutcome::Deferred,
            ReplyOutcome::Uncertain,
            ReplyOutcome::Accepted(foreign),
        ] {
            let reply = complete_local(
                &effect,
                UiReply {
                    outcome: Some(outcome),
                    ..Default::default()
                },
            );
            assert_eq!(reply.outcome, Some(outcome));
        }
    }

    #[tokio::test]
    async fn folding_durable_metadata_returns_accepted_on_the_local_clock() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let action = UiAction::FoldSection {
            key: "test-section".into(),
            folded: true,
        };
        assert!(wt_app_core::host_routing::controller_owned(&action));
        let board = Board::default();
        let effect = action.effect(&board);
        let result = crate::controller_actions::execute(&fixture.ctx, action, Some(&board), None)
            .await
            .unwrap();
        let reply = complete_local(&effect, result);
        let Some(ReplyOutcome::Accepted(stamp)) = reply.outcome else {
            panic!("successful fold must not become unconfirmed")
        };
        assert!(stamp.local_instant().is_some());
        assert!(
            fixture
                .ctx
                .database
                .call(|store| Ok(store.is_section_folded("test-section")?))
                .await
                .unwrap()
        );
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn waits_for_the_shared_github_leaf_to_cover_the_selected_branch() {
        let scope = TaskScope::new();
        let (source, publisher) = leaf_channel(
            &scope,
            SourceSpec {
                name: "github-action-wait-test",
                domain: Domain::Github,
                persistence: Persistence::Never("test fixture"),
                policy: RefreshPolicy::default(),
            },
        );
        let wait = tokio::spawn(wait_for_confirmed_github(
            source,
            Some("feature/one".into()),
            std::time::Duration::from_secs(1),
            CancellationToken::new(),
        ));
        tokio::task::yield_now().await;
        let mut data = wt_github::GithubData::default();
        data.covered_branches.insert("feature/one".into());
        publisher.publish_live(data, Stamp::now());

        let confirmed = wait.await.unwrap().expect("shared leaf should confirm");
        assert!(matches!(
            confirmed.covered_pr("feature/one"),
            wt_github::CoveredPr::Absent
        ));
    }
}
