//! Independent I/O lanes composed into a prepared board. Only explicit refresh
//! requests cross from presentation back into fetch scheduling.

use std::{path::Path, sync::Arc};

use wt_github::{GithubData, PrChecks, PrReview};
use wt_runtime::{
    LeafHandle, SourceHandle, SourceSnapshot, SourceState, TaskScope, derived_channel,
};
use wt_tui::Board;
use wt_vcs::WorktreeSnapshot;

use crate::context::AppContext;

pub struct BoardSources {
    pub board: SourceHandle<Board>,
    pub local: SourceHandle<Board>,
    pub git: LeafHandle<Vec<WorktreeSnapshot>>,
    pub metadata: LeafHandle<crate::local_source::Metadata>,
    /// Raw PR data as GitHub (or the events daemon) reported it. There is no
    /// optimistic overlay: pending writes are shown by the TUI's own engine.
    pub github: LeafHandle<GithubData>,
    pub github_refresh: crate::github_source::GithubRefresh,
    pub github_actions: crate::github_actions::GithubActions,
    pub github_pickers: Arc<crate::github_pickers::GithubPickers>,
    pub naming: crate::naming_source::NamingCommands,
    pub sessions: crate::session_source::SessionSources,
    /// Harness transcripts and the persisted primary harness.
    pub session_activity: SourceHandle<crate::session_activity::SessionActivitySnapshot>,
    pub automations: crate::automation_source::AutomationCommands,
    pub automation_drain: Arc<crate::automation_source::AutomationDrain>,
    pub history: crate::history_source::HistoryCommands,
    pub perf: crate::perf_source::PerfCommands,
    pub hard_refresh: crate::hard_refresh::HardRefreshCommands,
    pub review_requests: crate::review_requests::ReviewRequests,
    /// Palettes the host prepared for the selection, published on the board.
    pub palettes: crate::palette_source::PaletteCommands,
}

pub fn start(scope: &TaskScope, context: &AppContext) -> BoardSources {
    let local_sources = crate::local_source::start(scope, context);
    let history = crate::history_source::start(
        scope,
        context,
        local_sources.metadata.view(),
        local_sources.git.view(),
    );
    let sessions = crate::session_source::start(scope, context, local_sources.git.clone());
    let session_activity = crate::session_activity::start(scope, context, &sessions).activity;
    let local = local_sources.board;
    // This exact pipeline runs on every host. The controller combines its
    // completed snapshots; transport never reimplements a feature source.
    let enabled = std::env::var("WT_GITHUB").as_deref() != Ok("off");
    let (github, github_refresh) = crate::github_source::start(
        scope,
        context,
        local_sources.git.clone(),
        enabled,
        crate::origin::backstop(context),
    );
    let github_actions =
        crate::github_actions::GithubActions::start(scope, context, github_refresh.clone());
    let github_pickers = Arc::new(crate::github_pickers::GithubPickers::start(context));
    let activity = crate::activity_source::start(
        scope,
        context.config.paths.cache_root.clone(),
        context.config.paths.app_log_dir.clone(),
    );
    let board = project(scope, local.clone(), github.view(), activity.view());
    let dev = crate::dev_source::overlay(scope, context, local.clone(), board);
    let origin = crate::origin::overlay(scope, context, dev.board, local.clone());
    let issues = crate::issue_source::start(scope, context, origin.board);
    let attention_sources = crate::attention_source::AttentionSources {
        metadata: local_sources.metadata.view(),
        github: github.view(),
        issue_statuses: issues.statuses.view(),
        dev_status: dev.status.as_ref().map(|status| status.view()),
        manager_sessions: sessions.discoveries.view(),
    };
    let board = crate::action_source::overlay(
        scope,
        context,
        issues.board,
        crate::action_source::RefreshTargets {
            git: local_sources.git.clone(),
            github: github.clone(),
            dev: dev.status,
            origin: origin.origin.clone(),
            issues: issues.statuses,
        },
    );
    let git_facts = crate::git_presentation::start(
        scope,
        context,
        local_sources.git.view(),
        local_sources.metadata.view(),
        github.view(),
    );
    let board = crate::git_presentation::overlay(
        scope,
        board,
        git_facts,
        local_sources.git.view(),
        local_sources.metadata.view(),
        github.view(),
        context.config.branch.base.clone(),
    );
    let naming = crate::naming_source::start(
        scope,
        context,
        local_sources.git.view(),
        local_sources.metadata.view(),
        board,
    );
    let board = crate::session_board::overlay(
        scope,
        naming.board,
        &sessions,
        session_activity.clone(),
        crate::harness::AppHarness::new(context),
    );
    let board = crate::history_source::overlay(scope, board, history.snapshot);
    let (review_requests, review_snapshot) =
        crate::review_requests::ReviewRequests::start(scope, context);
    let board = crate::review_requests::overlay(scope, board, review_snapshot);
    let board = crate::attention_source::overlay(scope, context, board, attention_sources);
    let board =
        crate::work_presentation::overlay(scope, context.config.clone(), board, github.view());
    let palettes = crate::palette_source::start(scope);
    let board = crate::palette_source::overlay(scope, board, palettes.snapshot.view());
    let automations = crate::automation_source::start(
        scope,
        context,
        crate::automation_source::AutomationSources {
            board,
            git: local_sources.git.clone(),
            metadata: local_sources.metadata.clone(),
            github: github.clone(),
            sessions: sessions.discoveries.clone(),
            inventory: sessions.inventory.clone(),
            activity: session_activity.clone(),
            edits: local_sources.edits,
        },
    );
    let board = crate::update_source::overlay(scope, context, automations.board);
    let perf = crate::perf_source::start(scope, context, board);
    let hard_refresh = crate::hard_refresh::HardRefreshCommands::new(
        perf.board.clone(),
        github_refresh.clone(),
        origin.origin.as_ref().map(|origin| origin.view()),
        naming.commands.clone(),
        github_pickers.clone(),
    );
    BoardSources {
        board: perf.board,
        local,
        git: local_sources.git,
        metadata: local_sources.metadata,
        github,
        github_refresh,
        github_actions,
        github_pickers,
        naming: naming.commands,
        sessions,
        session_activity,
        automations: automations.commands,
        automation_drain: automations.drain,
        history: history.commands,
        perf: perf.commands,
        hard_refresh,
        review_requests,
        palettes: palettes.commands,
    }
}

fn project(
    scope: &TaskScope,
    local: SourceHandle<Board>,
    github: SourceHandle<GithubData>,
    activity: SourceHandle<crate::activity_source::ActivitySnapshot>,
) -> SourceHandle<Board> {
    let (source, mut publisher) = derived_channel(scope, "github-board");
    let cancel = scope.token();
    scope.spawn(async move {
        let mut local_updates = local.subscribe();
        let mut github_updates = github.subscribe();
        let mut activity_updates = activity.subscribe();
        local_updates.mark_changed();
        github_updates.mark_changed();
        activity_updates.mark_changed();
        let mut failures = crate::activity_source::FailureClock::default();
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                request = publisher.requested() => {
                    if request.is_none() { break; }
                    local.refresh();
                    activity.refresh();
                    // The GitHub leaf skips until Git inventory is confirmed.
                    github.refresh();
                    continue;
                }
                changed = local_updates.changed() => {
                    if changed.is_err() { break; }
                    local_updates.borrow_and_update();
                }
                changed = github_updates.changed() => {
                    if changed.is_err() { break; }
                    github_updates.borrow_and_update();
                }
                changed = activity_updates.changed() => {
                    if changed.is_err() { break; }
                    activity_updates.borrow_and_update();
                }
            }
            let local_snapshot = local_updates.borrow().clone();
            let github_snapshot = github_updates.borrow().clone();
            let activity_snapshot = activity_updates.borrow().clone();
            // The worktree-era guard stats directories. Keep it off the input
            // and async executor threads; all rows are prepared in one job.
            // Clone rather than move, so a panicked pass keeps the clock.
            let mut pass_failures = failures.clone();
            let result = tokio::task::spawn_blocking(move || {
                let snapshot = compose(
                    local_snapshot,
                    github_snapshot,
                    activity_snapshot,
                    &mut pass_failures,
                );
                (snapshot, pass_failures)
            })
            .await;
            match result {
                Ok((snapshot, pass_failures)) if !cancel.is_cancelled() => {
                    failures = pass_failures;
                    publisher.publish_if_changed(snapshot);
                }
                Ok(_) => break,
                Err(error) => {
                    tracing::error!(%error, "board projection failed");
                    publisher.publish(local.snapshot().with_state(SourceState::Failed(
                        format!("board projection: {error}").into(),
                    )));
                }
            }
        }
    });
    source
}

fn compose(
    local: SourceSnapshot<Board>,
    github: SourceSnapshot<GithubData>,
    activity: SourceSnapshot<crate::activity_source::ActivitySnapshot>,
    failures: &mut crate::activity_source::FailureClock,
) -> SourceSnapshot<Board> {
    let Some(board) = local.data() else {
        // This compose reports nothing, so every tracked failure has cleared.
        failures.sweep();
        return local;
    };
    let mut board = board.as_ref().clone();
    if let Some(feed) = activity.data() {
        board.activity.extend(feed.activity.iter().cloned());
        board.attention.extend(feed.attention.iter().cloned());
    }
    if let SourceState::Failed(error) = activity.state() {
        let text = wt_core::sanitize_terminal_text(error);
        let at_ms = failures.at("attention", "activity feed", &text);
        board.activity.push(wt_tui::ActivityLine {
            at_ms,
            level: "ERROR".into(),
            channel: "activity".into(),
            source: "activity feed".into(),
            text: text.clone(),
        });
        board.attention.push(wt_tui::AttentionLine {
            at_ms,
            source: "activity feed".into(),
            text,
        });
    }
    if let Some(data) = github.data() {
        for row in &mut board.rows {
            let Some(pr) = (if wt_core::is_remote_worktree_ledger_key(&row.key) {
                data.prs.get(&row.branch)
            } else {
                wt_github::pick_pr_for_worktree(Some(&row.branch), Path::new(&row.path), &data.prs)
            }) else {
                continue;
            };
            let clean = wt_core::sanitize_terminal_text;
            row.pr_url = Some(pr.url.clone());
            let state = if pr.is_draft { "draft" } else { &pr.state };
            row.badge = format!("{}  #{} {}", row.badge, pr.number, clean(state))
                .trim()
                .to_owned();
            if pr.state == "OPEN" {
                row.needs_attention |= pr.checks == PrChecks::Fail
                    || pr.review == PrReview::ChangesRequested
                    || pr.unresolved_threads > 0;
            }
        }
    }
    failures.sweep();
    crate::activity_source::bound_feeds(&mut board);
    let mut state = local.state().clone();
    if let SourceState::Failed(error) = github.state() {
        // A remote failure stays visible while local updates and last-good
        // remote badges continue. Refreshing local data cannot mask it.
        if !matches!(state, SourceState::Failed(_)) {
            state = SourceState::Failed(
                format!("GitHub: {}", wt_core::sanitize_terminal_text(error)).into(),
            );
        }
    } else if *github.state() == SourceState::Refreshing && state == SourceState::Ready {
        state = SourceState::Refreshing;
    }
    local
        .with_data(Some(Arc::new(board)))
        .with_state(state)
        .with_input(&github)
        .with_input(&activity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use wt_core::Domain;
    use wt_runtime::{Persistence, RefreshPolicy, SourceSpec, Stamp};

    /// Persisted sources whose type runs `wt_runtime::conformance::
    /// persisted_source` in its owning module. Adding a `Disk` source without
    /// a conformance test (and an entry here) fails `every_source_declares_and_is_covered`.
    const PERSISTED_WITH_CONFORMANCE: &[&str] = &[
        "git",
        "github",
        "review-requests",
        "issue-statuses",
        "dev-status",
        "session-discoveries",
        "git-presentation",
    ];

    /// Sources that read durable SQLite; it is authoritative and never cached.
    const SQLITE_READERS: &[&str] = &["metadata", "review-dismissals", "removed-history"];

    #[tokio::test]
    async fn every_source_declares_and_is_covered() {
        let fixture = crate::commands::test_support::CommandFixture::new()
            .await
            .unwrap();
        let scope = TaskScope::new();
        // Construction registers every source; nothing needs to run.
        let _sources = start(&scope, &fixture.ctx);
        let entries = scope.sources();
        scope.shutdown(Duration::from_secs(5)).await.unwrap();
        assert!(entries.len() > 20, "the whole graph registers: {entries:?}");
        let mut names = std::collections::BTreeSet::new();
        let mut leaf_domains = std::collections::BTreeSet::new();
        for entry in &entries {
            let wt_runtime::SourceKind::Leaf {
                domain,
                persisted,
                never_reason,
            } = &entry.kind
            else {
                continue;
            };
            assert!(
                names.insert(entry.name),
                "duplicate leaf name {}",
                entry.name
            );
            leaf_domains.insert(*domain);
            if *persisted {
                assert!(
                    PERSISTED_WITH_CONFORMANCE.contains(&entry.name),
                    "{} persists without a conformance test",
                    entry.name
                );
            } else {
                assert!(
                    never_reason.is_some_and(|reason| !reason.trim().is_empty()),
                    "{} must say why it is never persisted",
                    entry.name
                );
            }
            if SQLITE_READERS.contains(&entry.name) {
                assert!(
                    !persisted,
                    "{} reads SQLite and must not be cached",
                    entry.name
                );
            }
        }
        for name in SQLITE_READERS {
            assert!(names.contains(name), "{name} is still a leaf");
        }
        for domain in wt_tui::SETTLING_DOMAINS {
            assert!(
                leaf_domains.contains(domain),
                "pending changes wait on {domain:?}, which no source publishes"
            );
        }
        fixture.close().await.unwrap();
    }
    use wt_runtime::{DerivedPublisher, LeafPublisher, leaf_channel};
    use wt_tui::BoardRow;

    fn spec<T>(name: &'static str, domain: Domain) -> SourceSpec<T> {
        SourceSpec {
            name,
            domain,
            persistence: Persistence::Never("test fixture"),
            policy: RefreshPolicy::default(),
        }
    }

    fn publish_local(publisher: &DerivedPublisher<Board>, title: &str, branch: &str) {
        publisher.publish(SourceSnapshot::test_live(
            Some(Board {
                name: "test".into(),
                rows: vec![BoardRow {
                    key: "row".into(),
                    title: title.into(),
                    branch: branch.into(),
                    path: "/tmp/nonexistent-wt-source-fixture".into(),
                    ..BoardRow::default()
                }],
                ..Board::default()
            }),
            SourceState::Ready,
            Domain::Git,
        ));
    }

    async fn wait_for(
        source: &SourceHandle<Board>,
        predicate: impl Fn(&SourceSnapshot<Board>) -> bool,
    ) {
        tokio::time::timeout(
            Duration::from_secs(3),
            source.subscribe().wait_for(predicate),
        )
        .await
        .unwrap()
        .unwrap();
    }

    fn rows(snapshot: &SourceSnapshot<Board>) -> &[BoardRow] {
        snapshot.data().map_or(&[], |board| &board.rows)
    }

    struct Inputs {
        scope: TaskScope,
        local: DerivedPublisher<Board>,
        github: LeafPublisher<GithubData>,
        activity: LeafPublisher<crate::activity_source::ActivitySnapshot>,
        board: SourceHandle<Board>,
    }

    fn inputs(before: impl FnOnce(&DerivedPublisher<Board>)) -> Inputs {
        let scope = TaskScope::new();
        let (local, local_publisher) = derived_channel(&scope, "local");
        let (github, github_publisher) = leaf_channel(&scope, spec("github", Domain::Github));
        let (activity, activity_publisher) =
            leaf_channel(&scope, spec("activity", Domain::Activity));
        before(&local_publisher);
        let board = project(&scope, local, github.view(), activity.view());
        Inputs {
            scope,
            local: local_publisher,
            github: github_publisher,
            activity: activity_publisher,
            board,
        }
    }

    #[tokio::test]
    async fn projection_consumes_data_published_before_it_subscribes() {
        let inputs = inputs(|local| publish_local(local, "already available", "feature"));
        wait_for(&inputs.board, |snapshot| {
            rows(snapshot)
                .first()
                .is_some_and(|row| row.title == "already available")
        })
        .await;
        inputs.scope.shutdown(Duration::from_secs(1)).await.unwrap();
    }

    #[tokio::test]
    async fn local_updates_stay_live_during_remote_work_and_retain_remote_errors_and_data() {
        let mut inputs = inputs(|_| {});
        // An explicit refresh reaches every input; the GitHub leaf itself
        // decides whether it can fetch yet.
        inputs.board.refresh();
        assert_eq!(inputs.local.requested().await, Some(()));
        assert_eq!(inputs.github.requested().await, Some(()));
        assert_eq!(inputs.activity.requested().await, Some(()));
        publish_local(&inputs.local, "initial", "feature");
        wait_for(&inputs.board, |s| {
            rows(s).first().is_some_and(|row| row.title == "initial")
        })
        .await;
        inputs.github.set_state(SourceState::Refreshing);
        publish_local(&inputs.local, "renamed while fetching", "feature");
        wait_for(&inputs.board, |s| {
            rows(s)
                .first()
                .is_some_and(|row| row.title == "renamed while fetching")
                && *s.state() == SourceState::Refreshing
        })
        .await;
        let pr = serde_json::from_value(serde_json::json!({
            "number": 42, "url": "https://github.com/o/r/pull/42", "headRefName": "feature",
            "baseRefName": "main", "title": "A PR", "isDraft": false, "state": "OPEN",
            "checks": "fail", "failedChecks": ["tests"], "review": "pending", "reviewRequests": 0,
            "requestedReviewers": [], "suggestedReviewers": [], "comments": [],
            "unresolvedThreads": 0, "unresolvedThreadsTotal": 0
        }))
        .unwrap();
        inputs.github.publish_live(
            GithubData {
                prs: [("feature".into(), pr)].into(),
                ..GithubData::default()
            },
            Stamp::now(),
        );
        wait_for(&inputs.board, |s| {
            rows(s).first().is_some_and(|row| row.badge.contains("#42"))
                && s.domains().contains_key(&Domain::Github)
        })
        .await;
        inputs
            .github
            .set_state(SourceState::Failed("offline".into()));
        publish_local(&inputs.local, "renamed while offline", "feature");
        wait_for(&inputs.board, |s| {
            matches!(s.state(), SourceState::Failed(_))
                && rows(s)
                    .first()
                    .is_some_and(|row| row.title == "renamed while offline")
        })
        .await;
        let snapshot = inputs.board.snapshot();
        let row = &rows(&snapshot)[0];
        assert!(row.badge.contains("#42"));
        assert!(row.needs_attention);
        inputs.activity.publish_live(
            crate::activity_source::ActivitySnapshot {
                activity: vec![wt_tui::ActivityLine {
                    at_ms: 1,
                    level: "INFO".into(),
                    channel: "attention".into(),
                    source: "manager".into(),
                    text: "manager report".into(),
                }],
                attention: vec![wt_tui::AttentionLine {
                    at_ms: 1,
                    source: "manager".into(),
                    text: "manager report".into(),
                }],
            },
            Stamp::now(),
        );
        wait_for(&inputs.board, |s| {
            s.data()
                .is_some_and(|b| b.activity.iter().any(|line| line.text == "manager report"))
        })
        .await;
        // Data changes never request fetches.
        assert!(
            tokio::time::timeout(Duration::from_millis(5), inputs.github.requested())
                .await
                .is_err()
        );
        publish_local(&inputs.local, "new branch", "another");
        wait_for(&inputs.board, |s| {
            rows(s).first().is_some_and(|row| row.branch == "another")
        })
        .await;
        assert!(!rows(&inputs.board.snapshot())[0].badge.contains("#42"));
        inputs.scope.shutdown(Duration::from_secs(1)).await.unwrap();
    }
}
