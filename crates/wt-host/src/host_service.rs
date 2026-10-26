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

/// The oldest GitHub read a GitHub write may rely on for its PR checks.
const WRITE_EVIDENCE_AGE: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// A source refresh starts when the palette opens. If a PR-dependent action is
/// selected before that first read completes, wait briefly for this same leaf
/// rather than treating the restored board as proof of presence or absence.
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
        Self { context, sources }
    }

    /// Run one host-owned request, then refresh exactly the domains its
    /// effect names and classify the reply for the TUI's pending engine.
    pub async fn execute(&self, command: UiAction) -> Result<UiReply> {
        self.execute_with_shutdown(command, None).await
    }

    /// Run a request owned by the interactive controller. Shutdown may stop
    /// waiting for a durable removal only after its worker is proven running;
    /// other accepted mutations continue through their normal completion.
    pub async fn execute_with_shutdown(
        &self,
        command: UiAction,
        shutdown: Option<&CancellationToken>,
    ) -> Result<UiReply> {
        let snapshot = self.sources.board.snapshot();
        let empty = Board::default();
        let effect = command.effect(snapshot.data().map_or(&empty, |board| board.as_ref()));
        let result = self.run(command, &snapshot, shutdown).await;
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
                Domain::Git => {
                    self.sources.git.refresh();
                }
                Domain::Metadata => {
                    self.sources.metadata.refresh();
                }
                // The read that may confirm an accepted write must be a
                // direct fetch that started after it, not a daemon snapshot.
                Domain::Github if matches!(reply.outcome, Some(ReplyOutcome::Accepted(_))) => {
                    self.sources.github_refresh.refresh_live()
                }
                Domain::Github => self.sources.github_refresh.refresh(),
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
                | Domain::Naming
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
    /// rebuilds from the discoveries source as each harness completes.
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
    ) -> Result<UiReply> {
        if command.is_board_menu() {
            anyhow::bail!("this menu is built from the board and never asks the host");
        }
        let context = self.context.for_action(&command)?;
        match &command {
            // Background read behind an open picker: no reply, and a target
            // without a slug has nothing to refresh.
            UiAction::DiscoverSessions { key, target } => {
                if let Ok(slug) =
                    wt_app_core::harness::picker_slug(&context, key.as_deref(), *target).await
                {
                    self.queue_session_discovery(&context, &slug);
                }
                return Ok(UiReply::default());
            }
            // Remote rows, and local targets the board has no entries for, are
            // answered here from the same discovery snapshot the board uses.
            UiAction::PrepareSessions { key, target } => {
                let history = self.sources.sessions.discoveries.snapshot();
                let (slug, entries) = wt_app_core::harness::cached_session_options(
                    &context,
                    key.clone(),
                    *target,
                    history.data().map_or(&[], |entries| entries.as_slice()),
                )
                .await?;
                self.queue_session_discovery(&context, &slug);
                return Ok(UiReply {
                    modal: Some(wt_tui::session_picker(
                        key.clone(),
                        *target,
                        &entries,
                        &wt_app_core::harness::visible_harnesses(&context.config),
                    )),
                    ..Default::default()
                });
            }
            _ => {}
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
        if let UiAction::WarmPalette { surface } = &command {
            // Prefetch only from confirmed GitHub data. Otherwise the copy is
            // dropped so the palette asks the host, which refreshes GitHub.
            let fresh = github
                .as_ref()
                .filter(|github| github_ready && github.fresh_within(WRITE_EVIDENCE_AGE));
            let modal = crate::action_palette::warm(&context, surface.clone(), board, fresh).await;
            self.sources.palettes.publish(surface.menu_key(), modal);
            return Ok(UiReply::default());
        }
        let github = if !github_ready && self.pr_dependent_command(&command) {
            self.await_confirmed_github(branch.as_deref())
                .await
                .or_else(|| github.filter(|github| github.fresh_within(WRITE_EVIDENCE_AGE)))
        } else {
            if !github_ready && matches!(command, UiAction::PrepareActions { .. }) {
                self.sources.github_refresh.refresh();
            }
            github
        };
        let github = github.as_ref();
        // A GitHub write must also not act on a read from long ago (a lane
        // that has been failing, say): its PR and head checks need a recent
        // read. Landing evidence for cleanup stays valid at any age.
        let github_for_writes = github.filter(|github| github.fresh_within(WRITE_EVIDENCE_AGE));
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
                        wt_sources::github_pickers::ReviewerSubmission {
                            key: &key,
                            pr_number,
                            original: &original,
                            selected: &selected,
                            board,
                            github: github_for_writes,
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
            UiAction::ToggleRemovedAutomations { key } => {
                Box::pin(crate::history_actions::toggle_automations_paused(
                    &context, &key,
                ))
                .await?
            }
            UiAction::Restack { key } => {
                Box::pin(wt_ops::restack_action::start(&context, &key, github, board)).await?
            }
            // The board builds the prompt for a row whose PR it shows. Here
            // the board had no PR, so confirmed GitHub data decides between
            // loading, no PR, and a PR that appeared since the board was made.
            UiAction::PrepareGithub { key, .. } => {
                let row = board
                    .and_then(|board| board.rows.iter().find(|row| row.key == key))
                    .ok_or_else(|| {
                        wt_app_core::host_errors::notice("worktree is no longer on the board")
                    })?;
                let github =
                    github.ok_or_else(|| anyhow::anyhow!("GitHub data is still loading"))?;
                return Err(match github.covered_pr(&row.branch) {
                    wt_github::CoveredPr::Present(_) => {
                        wt_app_core::host_errors::stale("GitHub data changed; press e again")
                    }
                    wt_github::CoveredPr::Absent => {
                        wt_app_core::host_errors::notice("no PR for this row")
                    }
                    wt_github::CoveredPr::Uncovered => {
                        anyhow::anyhow!("GitHub data is not confirmed for this branch")
                    }
                });
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
                    &context,
                    action,
                    board,
                    github.filter(|github| github.fresh_within(WRITE_EVIDENCE_AGE)),
                ))
                .await?
            }
            command => {
                Box::pin(crate::controller_actions::execute_with_shutdown(
                    &context, command, board, github, shutdown,
                ))
                .await?
            }
        };
        Ok(reply)
    }

    fn action_requires_pr(&self, command: &UiAction) -> bool {
        let (surface, id) = match command {
            UiAction::PrepareAction { surface, id, .. }
            | UiAction::RunAction { surface, id, .. } => (surface, id),
            _ => return false,
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
                UiAction::PrepareGithub { .. }
                    | UiAction::PrepareReviewers { .. }
                    | UiAction::SubmitReviewers { .. }
                    | UiAction::GithubMarkReady { .. }
                    | UiAction::GithubSetAutoMerge { .. }
                    | UiAction::GithubShip { .. }
                    | UiAction::GithubFailedChecks { .. }
            )
    }

    fn pr_dependent_branch(&self, command: &UiAction, board: Option<&Board>) -> Option<String> {
        let key = match command {
            UiAction::PrepareAction { surface, id, .. }
            | UiAction::RunAction { surface, id, .. }
                if self.surface_action_requires_pr(surface, id) =>
            {
                match surface {
                    ActionSurface::Row { key } | ActionSurface::Manager { key: Some(key) } => {
                        key.as_str()
                    }
                    _ => return None,
                }
            }
            UiAction::PrepareGithub { key, .. }
            | UiAction::PrepareReviewers { key }
            | UiAction::SubmitReviewers { key, .. }
            | UiAction::GithubMarkReady { key, .. }
            | UiAction::GithubSetAutoMerge { key, .. }
            | UiAction::GithubShip { key, .. }
            | UiAction::GithubFailedChecks { key } => key.as_str(),
            UiAction::PrepareActions {
                surface: ActionSurface::Row { key } | ActionSurface::Manager { key: Some(key) },
            }
            | UiAction::WarmPalette {
                surface: ActionSurface::Row { key } | ActionSurface::Manager { key: Some(key) },
            } => key.as_str(),
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
        ] {
            assert!(action.is_board_menu());
            let error = host.execute(action).await.unwrap_err();
            assert!(
                error.to_string().contains("built from the board"),
                "{error:#}"
            );
        }
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
