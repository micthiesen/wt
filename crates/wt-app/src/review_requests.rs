//! Host-local source and operations for GitHub review requests.

use std::{collections::HashSet, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use wt_github::{GithubClient, GithubOptions, PrChecks, ReviewRequestPr};
use wt_lifecycle::{CreateOptions, CreateResult};
use wt_runtime::{
    RefreshPolicy, SourceHandle, SourceSnapshot, SourceState, TaskScope, source_channel,
    start_source,
};
use wt_store::ReviewRequestDismissal;
use wt_tui::{Board, ReviewRequestRow};

use crate::context::AppContext;

const REVIEW_SECTION: &str = "Reviews";
const DEFAULT_BACKSTOP: Duration = Duration::from_secs(60);
const MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ReviewRequestSnapshot {
    pub requests: Vec<ReviewRequestPr>,
    pub dismissals: Vec<ReviewRequestDismissal>,
    pub issue_url_template: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewCheckout {
    pub url: String,
    pub updated_at: String,
    pub branch: String,
}

#[derive(Clone)]
pub struct ReviewRequests {
    context: AppContext,
    client: GithubClient,
    dismissals: SourceHandle<Vec<ReviewRequestDismissal>>,
}

impl ReviewRequests {
    pub fn start(
        scope: &TaskScope,
        context: &AppContext,
    ) -> (Self, SourceHandle<ReviewRequestSnapshot>) {
        let client = GithubClient::new(
            context.processes.clone(),
            context.config.paths.main_clone.clone(),
            GithubOptions::from_config(&context.config, false),
        );
        let enabled = std::env::var("WT_GITHUB").as_deref() != Ok("off");
        let requests = start_source(
            scope,
            RefreshPolicy {
                debounce: Duration::from_millis(100),
                minimum_interval: MIN_REFRESH_INTERVAL,
            },
            {
                let client = client.clone();
                move |cancellation| {
                    let client = client.clone();
                    async move {
                        if !enabled {
                            return Ok::<_, wt_github::GithubError>(Vec::new());
                        }
                        client.fetch_review_requests(&cancellation).await
                    }
                }
            },
        );
        let dismissals = start_source(scope, RefreshPolicy::default(), {
            let database = context.database.clone();
            move |_cancellation| {
                let database = database.clone();
                async move {
                    database
                        .call(|store| Ok(store.read_review_request_dismissals()?))
                        .await
                }
            }
        });
        let snapshot = combine_snapshot(
            scope,
            requests.clone(),
            dismissals.clone(),
            context
                .config
                .issue_tracker
                .as_ref()
                .and_then(|tracker| tracker.url_template.clone()),
        );
        requests.refresh();
        dismissals.refresh();

        let service = Self {
            context: context.clone(),
            client,
            dismissals: dismissals.clone(),
        };
        if enabled {
            let interval =
                context
                    .config
                    .github
                    .events
                    .as_ref()
                    .map_or(DEFAULT_BACKSTOP, |events| {
                        Duration::from_millis(
                            events
                                .backstop_poll_ms
                                .max(MIN_REFRESH_INTERVAL.as_millis() as f64)
                                .min(86_400_000.0) as u64,
                        )
                    });
            let refresh = snapshot.clone();
            let cancellation = scope.token();
            scope.spawn(async move {
                let mut ticker = tokio::time::interval(interval);
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                ticker.tick().await;
                loop {
                    tokio::select! {
                        _ = cancellation.cancelled() => break,
                        _ = ticker.tick() => { refresh.refresh(); }
                    }
                }
            });
        }
        (service, snapshot)
    }

    pub async fn dismiss(&self, url: &str, updated_at: &str) -> Result<()> {
        if url.trim().is_empty() || updated_at.trim().is_empty() {
            bail!("review request identity is incomplete; refresh and retry");
        }
        let dismissed_at = OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .context("format review request dismissal time")?;
        let dismissal = ReviewRequestDismissal {
            url: url.to_owned(),
            updated_at: updated_at.to_owned(),
            dismissed_at,
            extra: Default::default(),
        };
        self.context
            .database
            .call(move |store| {
                store.add_review_request_dismissal(&dismissal)?;
                Ok(())
            })
            .await
            .context("save review request dismissal")?;
        self.dismissals.refresh();
        Ok(())
    }

    /// Recheck the open request identity immediately before creating the
    /// checkout, then ask lifecycle to verify the fetched PR head under its
    /// per-slug lock. A changed request or branch collision fails closed.
    pub async fn checkout(&self, request: ReviewCheckout) -> Result<CreateResult> {
        let requests = self
            .client
            .fetch_review_requests(&self.context.cancellation)
            .await
            .context("recheck review request before checkout")?;
        let current = requests
            .iter()
            .find(|pr| {
                pr.url == request.url
                    && pr.updated_at == request.updated_at
                    && pr.head_ref_name.as_deref() == Some(request.branch.as_str())
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "review request changed after confirmation; refresh the review list and retry"
                )
            })?;
        let head = current.head_ref_oid.as_deref().ok_or_else(|| {
            anyhow::anyhow!("GitHub omitted the reviewed PR head; refusing an unpinned checkout")
        })?;

        let lifecycle = crate::lifecycle_ops::service(&self.context)?;
        let created = lifecycle
            .create_from_pull_request(
                &request.branch,
                current.number,
                head,
                CreateOptions::default(),
                &self.context.cancellation,
            )
            .await
            .context("create review worktree")?;
        let slug = created.target.slug().to_owned();
        self.context
            .database
            .call(move |store| {
                store.set_worktree_section(&slug, Some(REVIEW_SECTION))?;
                Ok(())
            })
            .await
            .context("place review worktree in Reviews section")?;
        Ok(created)
    }
}

fn combine_snapshot(
    scope: &TaskScope,
    requests: SourceHandle<Vec<ReviewRequestPr>>,
    dismissals: SourceHandle<Vec<ReviewRequestDismissal>>,
    issue_url_template: Option<String>,
) -> SourceHandle<ReviewRequestSnapshot> {
    let (handle, mut publisher) = source_channel();
    let cancellation = scope.token();
    scope.spawn(async move {
        let mut request_updates = requests.subscribe();
        let mut dismissal_updates = dismissals.subscribe();
        request_updates.mark_changed();
        dismissal_updates.mark_changed();
        let mut latest_requests = request_updates.borrow().clone();
        let mut latest_dismissals = dismissal_updates.borrow().clone();
        loop {
            publish_snapshot(
                &publisher,
                &latest_requests,
                &latest_dismissals,
                issue_url_template.as_deref(),
            );
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => break,
                request = publisher.requested() => {
                    if request.is_none() { break; }
                    requests.refresh();
                    dismissals.refresh();
                }
                changed = request_updates.changed() => {
                    if changed.is_err() { break; }
                    latest_requests = request_updates.borrow_and_update().clone();
                }
                changed = dismissal_updates.changed() => {
                    if changed.is_err() { break; }
                    latest_dismissals = dismissal_updates.borrow_and_update().clone();
                }
            }
        }
    });
    handle
}

fn publish_snapshot(
    publisher: &wt_runtime::SourcePublisher<ReviewRequestSnapshot>,
    requests: &SourceSnapshot<Vec<ReviewRequestPr>>,
    dismissals: &SourceSnapshot<Vec<ReviewRequestDismissal>>,
    issue_url_template: Option<&str>,
) {
    let data =
        requests
            .data
            .as_ref()
            .zip(dismissals.data.as_ref())
            .map(|(requests, dismissals)| ReviewRequestSnapshot {
                requests: requests.as_ref().clone(),
                dismissals: dismissals.as_ref().clone(),
                issue_url_template: issue_url_template.map(str::to_owned),
            });
    let state = match (&requests.state, &dismissals.state) {
        (SourceState::Failed(error), _) | (_, SourceState::Failed(error)) => {
            SourceState::Failed(error.clone())
        }
        (SourceState::Refreshing, _) | (_, SourceState::Refreshing) => SourceState::Refreshing,
        (SourceState::Ready, SourceState::Ready) => SourceState::Ready,
        _ => SourceState::Empty,
    };
    let updated_at = requests
        .updated_at
        .zip(dismissals.updated_at)
        .map(|(a, b)| a.min(b));
    publisher.publish(SourceSnapshot {
        data: data.map(Arc::new),
        state,
        updated_at,
        revision: requests.revision.max(dismissals.revision),
    });
}

pub fn overlay(
    scope: &TaskScope,
    board: SourceHandle<Board>,
    requests: SourceHandle<ReviewRequestSnapshot>,
) -> SourceHandle<Board> {
    let (handle, mut publisher) = source_channel();
    let cancellation = scope.token();
    scope.spawn(async move {
        let mut board_updates = board.subscribe();
        let mut request_updates = requests.subscribe();
        board_updates.mark_changed();
        request_updates.mark_changed();
        let mut latest_board = board_updates.borrow().clone();
        let mut latest_requests = request_updates.borrow().clone();
        loop {
            publish_board(&publisher, &latest_board, &latest_requests);
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => break,
                request = publisher.requested() => {
                    if request.is_none() { break; }
                    board.refresh();
                    requests.refresh();
                }
                changed = board_updates.changed() => {
                    if changed.is_err() { break; }
                    latest_board = board_updates.borrow_and_update().clone();
                }
                changed = request_updates.changed() => {
                    if changed.is_err() { break; }
                    latest_requests = request_updates.borrow_and_update().clone();
                }
            }
        }
    });
    handle
}

fn publish_board(
    publisher: &wt_runtime::SourcePublisher<Board>,
    base: &SourceSnapshot<Board>,
    requests: &SourceSnapshot<ReviewRequestSnapshot>,
) {
    let Some(board) = base.data.as_deref() else {
        publisher.publish(base.clone());
        return;
    };
    let mut projected = board.clone();
    projected.review_requests = requests
        .data
        .as_deref()
        .map(|snapshot| review_rows(board, snapshot))
        .unwrap_or_default();
    let state = match (&base.state, &requests.state, requests.data.is_some()) {
        (SourceState::Ready, SourceState::Failed(error), false) => {
            SourceState::Failed(error.clone())
        }
        (state, _, _) => state.clone(),
    };
    publisher.publish(SourceSnapshot {
        data: Some(Arc::new(projected)),
        state,
        updated_at: base.updated_at,
        revision: base.revision.max(requests.revision),
    });
}

fn review_rows(board: &Board, snapshot: &ReviewRequestSnapshot) -> Vec<ReviewRequestRow> {
    let live_branches = board
        .rows
        .iter()
        .map(|row| row.branch.as_str())
        .collect::<HashSet<_>>();
    let dismissed = snapshot
        .dismissals
        .iter()
        .map(|entry| (entry.url.as_str(), entry.updated_at.as_str()))
        .collect::<HashSet<_>>();
    snapshot
        .requests
        .iter()
        .filter_map(|pr| {
            let raw_branch = pr.head_ref_name.as_deref()?.trim();
            let branch = wt_core::sanitize_terminal_text(raw_branch);
            if branch.is_empty()
                || branch != raw_branch
                || live_branches.contains(branch.as_str())
                || dismissed.contains(&(pr.url.as_str(), pr.updated_at.as_str()))
            {
                return None;
            }
            let mut details = vec![format!("{} checks", check_label(pr.checks))];
            if let Some(review) = &pr.review_decision {
                details.push(wt_core::sanitize_terminal_text(&format!(
                    "review {}",
                    review.to_ascii_lowercase()
                )));
            }
            details.push(wt_core::sanitize_terminal_text(&format!(
                "+{} / -{} · {} files · {} comments",
                pr.additions, pr.deletions, pr.changed_files, pr.comment_count
            )));
            let issue_url = crate::issue_identity::resolve(&branch, None).and_then(|issue| {
                if let Some(number) = issue.strip_prefix("GH-") {
                    let number = number.parse::<u64>().ok()?;
                    if pr.url.starts_with("https://github.com/") {
                        return Some(format!(
                            "https://github.com/{}/issues/{number}",
                            pr.repo_name_with_owner
                        ));
                    }
                }
                snapshot
                    .issue_url_template
                    .as_deref()
                    .map(|template| template.replace("{id}", &issue))
            });
            Some(ReviewRequestRow {
                host: None,
                url: wt_core::sanitize_terminal_text(&pr.url),
                updated_at: pr.updated_at.clone(),
                branch,
                title: wt_core::sanitize_terminal_text(&pr.title),
                number: pr.number,
                author: wt_core::sanitize_terminal_text(pr.author.as_deref().unwrap_or("unknown")),
                details,
                issue_url: issue_url.map(|url| wt_core::sanitize_terminal_text(&url)),
            })
        })
        .collect()
}

fn check_label(checks: PrChecks) -> &'static str {
    match checks {
        PrChecks::Pass => "passing",
        PrChecks::Fail => "failing",
        PrChecks::Pending => "pending",
        PrChecks::None => "no checks",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projection_hides_only_exact_dismissal_and_live_branch() {
        let mut board = Board::default();
        board.rows.push(wt_tui::BoardRow {
            branch: "feature/live".into(),
            ..Default::default()
        });
        let request = |url: &str, updated_at: &str, branch: &str| ReviewRequestPr {
            number: 1,
            url: url.into(),
            title: "Review".into(),
            repo_name_with_owner: "org/repo".into(),
            head_ref_name: Some(branch.into()),
            head_ref_oid: Some("a".repeat(40)),
            author: Some("author".into()),
            is_draft: false,
            checks: PrChecks::Pending,
            review_decision: Some("REVIEW_REQUIRED".into()),
            additions: 2,
            deletions: 1,
            changed_files: 1,
            comment_count: 0,
            created_at: "created".into(),
            updated_at: updated_at.into(),
        };
        let snapshot = ReviewRequestSnapshot {
            requests: vec![
                request(
                    "https://github.com/org/repo/pull/1",
                    "old",
                    "feature/dismissed",
                ),
                request(
                    "https://github.com/org/repo/pull/1",
                    "new",
                    "feature/dismissed",
                ),
                request("https://github.com/org/repo/pull/2", "now", "feature/live"),
                request(
                    "https://github.com/org/repo/pull/3",
                    "now",
                    "feature/visible",
                ),
            ],
            dismissals: vec![ReviewRequestDismissal {
                url: "https://github.com/org/repo/pull/1".into(),
                updated_at: "old".into(),
                dismissed_at: "dismissed".into(),
                extra: Default::default(),
            }],
            issue_url_template: None,
        };
        let rows = review_rows(&board, &snapshot);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].updated_at, "new");
        assert_eq!(rows[1].branch, "feature/visible");
    }
}
