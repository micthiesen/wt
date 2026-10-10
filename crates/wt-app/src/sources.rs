//! Independent I/O lanes composed into a prepared board. Only explicit refresh
//! requests cross from presentation back into fetch scheduling.

use std::{path::Path, sync::Arc, time::Duration};
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
}

pub fn start(scope: &TaskScope, context: &AppContext) -> BoardSources {
    let local_sources = crate::local_source::start(scope, context);
    let local = local_sources.board;
    let enabled = std::env::var("WT_GITHUB").as_deref() != Ok("off");
    let github = start_source(
        scope,
        RefreshPolicy {
            debounce: Duration::from_millis(100),
            minimum_interval: GITHUB_MINIMUM,
        },
        {
            let context = context.clone();
            let local = local.clone();
            move |cancel| {
                let context = context.clone();
                let branches = branches(&local.snapshot());
                async move {
                    if !enabled || branches.is_empty() {
                        return Ok::<_, wt_github::GithubError>(GithubData::default());
                    }
                    if let Some(cached) =
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
                    client.fetch_worktrees(&branches, &cancel).await
                }
            }
        },
    );
    let github = if enabled {
        crate::github_events_source::overlay(scope, context, local.clone(), github)
    } else {
        github
    };
    let activity = crate::activity_source::start(scope, context.config.paths.cache_root.clone());
    let board = project(
        scope,
        local.clone(),
        github,
        activity,
        enabled,
        crate::origin::backstop(context),
    );
    let board = crate::dev_source::overlay(scope, context, local.clone(), board);
    let board = crate::origin::overlay(scope, context, board, local.clone());
    BoardSources {
        board,
        local,
        metadata: local_sources.metadata,
    }
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
        board.activity = lines.as_ref().clone();
    }
    if let SourceState::Failed(error) = activity.state {
        board.activity.push(format!(
            "Manager reports: {}",
            wt_core::sanitize_terminal_text(&error)
        ));
    }
    if let Some(data) = &github.data {
        for row in &mut board.rows {
            let Some(pr) =
                wt_github::pick_pr_for_worktree(Some(&row.branch), Path::new(&row.path), &data.prs)
            else {
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
