//! Independent I/O lanes composed into a prepared board. Only explicit refresh
//! requests cross from presentation back into fetch scheduling.

use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use wt_github::{GithubClient, GithubData, GithubOptions, PrChecks, PrReview};
use wt_runtime::{
    RefreshPolicy, SourceHandle, SourceSnapshot, SourceState, TaskScope, source_channel,
    start_source,
};
use wt_tui::Board;

use crate::context::AppContext;

const GITHUB_MINIMUM: Duration = Duration::from_secs(10);

pub struct BoardSources {
    pub board: SourceHandle<Board>,
    pub local: SourceHandle<Board>,
    pub metadata: SourceHandle<crate::local_source::Metadata>,
    pub github: SourceHandle<GithubData>,
    pub github_actions: crate::github_actions::GithubActions,
    pub github_pickers: Arc<crate::github_pickers::GithubPickers>,
    pub naming: crate::naming_source::NamingCommands,
    pub sessions: crate::session_source::SessionSources,
    pub automations: crate::automation_source::AutomationCommands,
    pub history: crate::history_source::HistoryCommands,
    pub perf: crate::perf_source::PerfCommands,
    pub hard_refresh: crate::hard_refresh::HardRefreshCommands,
    pub review_requests: crate::review_requests::ReviewRequests,
}

pub fn start(scope: &TaskScope, context: &AppContext) -> BoardSources {
    let local_sources = crate::local_source::start(scope, context);
    let history = crate::history_source::start(
        scope,
        context,
        local_sources.metadata.clone(),
        local_sources.git.clone(),
    );
    let sessions = crate::session_source::start(scope, context, local_sources.git.clone());
    let session_activity = crate::session_activity::start(scope, context, &sessions).activity;
    let local = local_sources.board;
    // This exact pipeline runs on every host. The controller combines its
    // completed snapshots; transport never reimplements a feature source.
    let fleet = local.clone();
    let enabled = std::env::var("WT_GITHUB").as_deref() != Ok("off");
    let bypass_github_cache = Arc::new(AtomicBool::new(false));
    let github = start_source(
        scope,
        RefreshPolicy {
            debounce: Duration::from_millis(100),
            minimum_interval: GITHUB_MINIMUM,
        },
        {
            let context = context.clone();
            let local = fleet.clone();
            let bypass = bypass_github_cache.clone();
            move |cancel| {
                let context = context.clone();
                let bypass = bypass.clone();
                let branches = branches(&local.snapshot());
                async move {
                    if !enabled || branches.is_empty() {
                        return Ok::<_, wt_github::GithubError>((
                            GithubData::default(),
                            tokio::time::Instant::now(),
                        ));
                    }
                    if !bypass.swap(false, Ordering::AcqRel)
                        && let Some(cached) =
                            crate::github_events_source::load(&context, &branches).await
                    {
                        return Ok(cached);
                    }
                    let ci = has_workflows(&context.config.paths.main_clone).await;
                    let client = GithubClient::new(
                        context.processes,
                        context.config.paths.main_clone.clone(),
                        GithubOptions::from_config(&context.config, ci),
                    );
                    let observed_at = tokio::time::Instant::now();
                    client
                        .fetch_worktrees(&branches, &cancel)
                        .await
                        .map(|data| (data, observed_at))
                }
            }
        },
    );
    let github = observe_github(scope, github);
    let github = if enabled {
        crate::github_events_source::overlay(scope, context, fleet.clone(), github)
    } else {
        github
    };
    let (github_actions, github) =
        crate::github_actions::GithubActions::start(scope, context, github);
    let github_pickers = Arc::new(crate::github_pickers::GithubPickers::start(
        context,
        github.clone(),
    ));
    let activity = crate::activity_source::start(scope, context.config.paths.cache_root.clone());
    let board = project(
        scope,
        fleet,
        github.clone(),
        activity,
        enabled,
        crate::origin::backstop(context),
    );
    let dev = crate::dev_source::overlay(scope, context, local.clone(), board);
    let origin = crate::origin::overlay(scope, context, dev.board, local.clone());
    let issues = crate::issue_source::start(scope, context, origin.board);
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
    let naming = crate::naming_source::start(
        scope,
        context,
        local_sources.git.clone(),
        local_sources.metadata.clone(),
        board,
    );
    let board =
        crate::session_board::overlay(scope, naming.board, &sessions, session_activity.clone());
    let board = crate::history_source::overlay(scope, board, history.snapshot);
    let (review_requests, review_snapshot) =
        crate::review_requests::ReviewRequests::start(scope, context);
    let board = crate::review_requests::overlay(scope, board, review_snapshot);
    let automations = crate::automation_source::start(
        scope,
        context,
        crate::automation_source::AutomationSources {
            board,
            git: local_sources.git,
            metadata: local_sources.metadata.clone(),
            github: github.clone(),
            sessions: sessions.discoveries.clone(),
            inventory: sessions.inventory.clone(),
            activity: session_activity.clone(),
            edits: local_sources.edits,
        },
    );
    let perf = crate::perf_source::start(scope, context, automations.board);
    let hard_refresh = crate::hard_refresh::HardRefreshCommands::new(
        perf.board.clone(),
        github.clone(),
        origin.origin,
        bypass_github_cache,
        naming.commands.clone(),
        github_pickers.clone(),
    );
    BoardSources {
        board: perf.board,
        local,
        metadata: local_sources.metadata,
        github,
        github_actions,
        github_pickers,
        naming: naming.commands,
        sessions,
        automations: automations.commands,
        history: history.commands,
        perf: perf.commands,
        hard_refresh,
        review_requests,
    }
}

/// Preserve the age of a daemon observation through the ordinary fetch lane.
/// Reading the same file again must not make old data fresh for automations.
fn observe_github(
    scope: &TaskScope,
    input: SourceHandle<(GithubData, tokio::time::Instant)>,
) -> SourceHandle<GithubData> {
    let (source, mut publisher) = source_channel();
    let cancel = scope.token();
    scope.spawn(async move {
        let mut updates = input.subscribe();
        updates.mark_changed();
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                request = publisher.requested() => {
                    if request.is_none() { break; }
                    input.refresh();
                    continue;
                },
                changed = updates.changed() => if changed.is_err() { break; },
            }
            let snapshot = updates.borrow_and_update().clone();
            publisher.publish(SourceSnapshot {
                data: snapshot
                    .data
                    .as_ref()
                    .map(|value| Arc::new(value.0.clone())),
                updated_at: snapshot.data.as_ref().map(|value| value.1),
                state: snapshot.state,
                revision: 0,
            });
        }
    });
    source
}

async fn has_workflows(root: &Path) -> bool {
    let mut entries = match tokio::fs::read_dir(root.join(".github/workflows")).await {
        Ok(entries) => entries,
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(%error, "cannot inspect workflow files");
            }
            return false;
        }
    };
    loop {
        match entries.next_entry().await {
            Ok(Some(entry)) => {
                if matches!(
                    entry.path().extension().and_then(|s| s.to_str()),
                    Some("yml" | "yaml")
                ) {
                    return true;
                }
            }
            Ok(None) => return false,
            Err(error) => {
                tracing::warn!(%error, "cannot inspect workflow files");
                return false;
            }
        }
    }
}

pub(crate) fn branches(snapshot: &SourceSnapshot<Board>) -> Vec<String> {
    let mut branches: Vec<_> = snapshot
        .data
        .iter()
        .flat_map(|board| &board.rows)
        .map(|row| row.branch.clone())
        .filter(|branch| !branch.is_empty())
        .collect();
    branches.sort();
    branches.dedup();
    branches
}

fn project(
    scope: &TaskScope,
    local: SourceHandle<Board>,
    github: SourceHandle<GithubData>,
    activity: SourceHandle<Vec<String>>,
    enabled: bool,
    backstop: Duration,
) -> SourceHandle<Board> {
    let (source, mut publisher) = source_channel();
    let cancel = scope.token();
    scope.spawn(async move {
        let mut local_updates = local.subscribe();
        let mut github_updates = github.subscribe();
        let mut activity_updates = activity.subscribe();
        local_updates.mark_changed();
        github_updates.mark_changed();
        activity_updates.mark_changed();
        let mut previous_branches = Vec::new();
        let mut interval =
            tokio::time::interval_at(tokio::time::Instant::now() + backstop, backstop);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                request = publisher.requested() => {
                    if request.is_none() { break; }
                    local.refresh();
                    activity.refresh();
                    if enabled && !branches(&local.snapshot()).is_empty() { github.refresh(); }
                    continue;
                }
                changed = local_updates.changed() => {
                    if changed.is_err() { break; }
                    let next = branches(&local_updates.borrow_and_update());
                    if next != previous_branches {
                        previous_branches = next;
                        if enabled { github.refresh(); }
                    }
                }
                changed = github_updates.changed() => {
                    if changed.is_err() { break; }
                    github_updates.borrow_and_update();
                }
                changed = activity_updates.changed() => {
                    if changed.is_err() { break; }
                    activity_updates.borrow_and_update();
                }
                _ = interval.tick(), if enabled => { github.refresh(); continue; }
            }
            let local_snapshot = local_updates.borrow().clone();
            let github_snapshot = github_updates.borrow().clone();
            let activity_snapshot = activity_updates.borrow().clone();
            // The worktree-era guard stats directories. Keep it off the input
            // and async executor threads; all rows are prepared in one job.
            let result = tokio::task::spawn_blocking(move || {
                compose(local_snapshot, github_snapshot, activity_snapshot)
            })
            .await;
            match result {
                Ok(snapshot) if !cancel.is_cancelled() => publisher.publish(snapshot),
                Ok(_) => break,
                Err(error) => {
                    tracing::error!(%error, "board projection failed");
                    let mut snapshot = local.snapshot();
                    snapshot.state =
                        SourceState::Failed(format!("board projection: {error}").into());
                    publisher.publish(snapshot);
                }
            }
        }
    });
    source
}

fn compose(
    mut local: SourceSnapshot<Board>,
    github: SourceSnapshot<GithubData>,
    activity: SourceSnapshot<Vec<String>>,
) -> SourceSnapshot<Board> {
    let Some(board) = local.data.as_ref() else {
        return local;
    };
    let mut board = board.as_ref().clone();
    if let Some(lines) = activity.data {
        board.activity.extend(lines.iter().cloned());
    }
    if let SourceState::Failed(error) = activity.state {
        board.activity.push(format!(
            "Manager reports: {}",
            wt_core::sanitize_terminal_text(&error)
        ));
    }
    if let Some(data) = &github.data {
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
            row.details
                .push(format!("PR #{}: {}", pr.number, clean(&pr.title)));
            row.details.push(clean(&pr.url));
            row.details.push(format!(
                "GitHub: {} · base {}",
                clean(state),
                clean(&pr.base_ref_name)
            ));
            if pr.state == "OPEN" {
                let checks = match pr.checks {
                    PrChecks::Pass => "passing",
                    PrChecks::Fail => "failing",
                    PrChecks::Pending => "pending",
                    PrChecks::None => "none",
                };
                row.details.push(format!("Checks: {checks}"));
                for failed in &pr.failed_checks {
                    row.details.push(format!("  {}", clean(failed)));
                }
                let review = match pr.review {
                    PrReview::Approved => "approved",
                    PrReview::ChangesRequested => "changes requested",
                    PrReview::Pending => "pending",
                    PrReview::Unrequested => "not requested",
                    PrReview::None => "none",
                };
                row.details.push(format!("Review: {review}"));
                if !pr.requested_reviewers.is_empty() {
                    row.details.push(format!(
                        "Reviewers: {}",
                        clean(&pr.requested_reviewers.join(", "))
                    ));
                }
                if let Some(bot) = &pr.review_bot {
                    row.details.push(format!(
                        "Review bot: {} · {} unresolved",
                        clean(&bot.state),
                        bot.unresolved
                    ));
                }
                if pr.unresolved_threads_total > 0 {
                    row.details.push(format!(
                        "Unresolved threads: {}",
                        pr.unresolved_threads_total
                    ));
                }
                if let Some(queue) = data.merge_queue.get(&row.branch) {
                    row.details.push(format!(
                        "Merge queue: #{} · {:?}",
                        queue.position, queue.state
                    ));
                } else if pr.auto_merge.is_some() {
                    row.details.push("Merge when ready: armed".into());
                }
                row.needs_attention |= pr.checks == PrChecks::Fail
                    || pr.review == PrReview::ChangesRequested
                    || pr.unresolved_threads > 0;
            }
            for comment in &pr.comments {
                row.details.push(format!(
                    "{}: {}",
                    clean(&comment.author),
                    clean(&comment.body)
                ));
            }
        }
    }
    local.data = Some(Arc::new(board));
    if let SourceState::Failed(error) = github.state {
        // A remote failure stays visible while local updates and last-good
        // remote badges continue. Refreshing local data cannot mask it.
        if !matches!(local.state, SourceState::Failed(_)) {
            local.state = SourceState::Failed(
                format!("GitHub: {}", wt_core::sanitize_terminal_text(&error)).into(),
            );
        }
    } else if github.state == SourceState::Refreshing && local.state == SourceState::Ready {
        local.state = SourceState::Refreshing;
    }
    local
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_runtime::SourcePublisher;
    use wt_tui::BoardRow;

    fn publish_local(publisher: &SourcePublisher<Board>, title: &str, branch: &str) {
        publisher.publish(SourceSnapshot {
            data: Some(Arc::new(Board {
                name: "test".into(),
                rows: vec![BoardRow {
                    key: "row".into(),
                    title: title.into(),
                    branch: branch.into(),
                    path: "/tmp/nonexistent-wt-source-fixture".into(),
                    ..BoardRow::default()
                }],
                ..Board::default()
            })),
            state: SourceState::Ready,
            updated_at: Some(tokio::time::Instant::now()),
            revision: 0,
        });
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

    #[tokio::test]
    async fn projection_consumes_data_published_before_it_subscribes() {
        let scope = TaskScope::new();
        let (local, local_publisher) = source_channel();
        let (github, _) = source_channel();
        let (activity, _) = source_channel();
        publish_local(&local_publisher, "already available", "feature");
        let source = project(
            &scope,
            local,
            github,
            activity,
            false,
            Duration::from_secs(180),
        );
        wait_for(&source, |snapshot| {
            snapshot
                .data
                .as_ref()
                .is_some_and(|board| board.rows[0].title == "already available")
        })
        .await;
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
    }

    #[tokio::test]
    async fn local_updates_stay_live_during_remote_work_and_retain_remote_errors_and_data() {
        let scope = TaskScope::new();
        let (local, mut local_publisher) = source_channel();
        let (github, mut github_publisher) = source_channel();
        let (activity, activity_publisher) = source_channel();
        let source = project(
            &scope,
            local,
            github,
            activity,
            true,
            Duration::from_secs(180),
        );
        // The initial explicit refresh must not fetch an empty branch set and
        // consume the network rate-limit window before inventory is available.
        source.refresh();
        assert_eq!(local_publisher.requested().await, Some(()));
        assert!(
            tokio::time::timeout(Duration::from_millis(5), github_publisher.requested())
                .await
                .is_err()
        );
        publish_local(&local_publisher, "initial", "feature");
        assert_eq!(github_publisher.requested().await, Some(()));
        wait_for(&source, |s| {
            s.data
                .as_ref()
                .is_some_and(|b| b.rows[0].title == "initial")
        })
        .await;
        github_publisher.publish(SourceSnapshot {
            data: None,
            state: SourceState::Refreshing,
            updated_at: None,
            revision: 0,
        });
        publish_local(&local_publisher, "renamed while fetching", "feature");
        wait_for(&source, |s| {
            s.data
                .as_ref()
                .is_some_and(|b| b.rows[0].title == "renamed while fetching")
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
        let data = Arc::new(GithubData {
            prs: [("feature".into(), pr)].into(),
            ..GithubData::default()
        });
        github_publisher.publish(SourceSnapshot {
            data: Some(data.clone()),
            state: SourceState::Ready,
            updated_at: None,
            revision: 0,
        });
        wait_for(&source, |s| {
            s.data
                .as_ref()
                .is_some_and(|b| b.rows[0].badge.contains("#42"))
        })
        .await;
        github_publisher.publish(SourceSnapshot {
            data: Some(data),
            state: SourceState::Failed("offline".into()),
            updated_at: None,
            revision: 0,
        });
        publish_local(&local_publisher, "renamed while offline", "feature");
        wait_for(&source, |s| {
            matches!(s.state, SourceState::Failed(_))
                && s.data
                    .as_ref()
                    .is_some_and(|b| b.rows[0].title == "renamed while offline")
        })
        .await;
        let snapshot = source.snapshot();
        let row = &snapshot.data.as_ref().unwrap().rows[0];
        assert!(row.badge.contains("#42"));
        assert!(row.needs_attention);
        assert!(row.details.iter().any(|line| line == "Checks: failing"));
        activity_publisher.publish(SourceSnapshot {
            data: Some(Arc::new(vec!["manager report".into()])),
            state: SourceState::Ready,
            updated_at: None,
            revision: 0,
        });
        wait_for(&source, |s| {
            s.data
                .as_ref()
                .is_some_and(|b| b.activity == ["manager report"])
        })
        .await;
        assert!(
            tokio::time::timeout(Duration::from_millis(5), local_publisher.requested())
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(5), github_publisher.requested())
                .await
                .is_err()
        );
        publish_local(&local_publisher, "new branch", "another");
        assert_eq!(github_publisher.requested().await, Some(()));
        wait_for(&source, |s| {
            s.data
                .as_ref()
                .is_some_and(|b| b.rows[0].branch == "another")
        })
        .await;
        assert!(
            !source.snapshot().data.unwrap().rows[0]
                .badge
                .contains("#42")
        );
        source.refresh();
        assert_eq!(local_publisher.requested().await, Some(()));
        assert_eq!(github_publisher.requested().await, Some(()));
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
    }
}
