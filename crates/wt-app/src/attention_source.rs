//! Narrate state transitions that otherwise change only prepared details.
//!
//! This source only observes existing snapshots. It never refetches per row,
//! and it seeds each domain silently so the first board is not replayed as news.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::Arc,
};

use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use wt_dev::DevStatusRow;
use wt_github::{GithubClient, GithubData, GithubOptions, PrComment};
use wt_harness::DerivedState;
use wt_runtime::{SourceHandle, SourceSnapshot, SourceState, TaskScope, source_channel};
use wt_tui::Board;

use crate::{
    context::AppContext,
    issue_source::StatusBatch,
    local_source::Metadata,
    session_source::{DiscoveredSession, SessionDiscoveries},
};

const MAX_ATTENTION_LINES: usize = 50;
const MAX_ATTENTION_FEED: usize = 200;
const COMMENT_BODY_CHARS: usize = 100;
const MAX_COMMENT_LINES: usize = 3;

#[derive(Clone)]
pub struct AttentionSources {
    pub metadata: SourceHandle<Metadata>,
    pub github: SourceHandle<GithubData>,
    pub issue_statuses: SourceHandle<StatusBatch>,
    pub dev_status: Option<SourceHandle<Vec<DevStatusRow>>>,
    pub manager_sessions: SourceHandle<SessionDiscoveries>,
}

pub fn overlay(
    scope: &TaskScope,
    context: &AppContext,
    board: SourceHandle<Board>,
    sources: AttentionSources,
) -> SourceHandle<Board> {
    let (source, mut publisher) = source_channel();
    let cancel = scope.token();
    let github_enabled = std::env::var("WT_GITHUB").as_deref() != Ok("off");
    let github_client = GithubClient::new(
        context.processes.clone(),
        context.config.paths.main_clone.clone(),
        GithubOptions::from_config(&context.config, false),
    );
    let login_tx = if github_enabled {
        let (sender, mut requests) = mpsc::channel::<oneshot::Sender<Result<String, String>>>(1);
        let login_cancel = cancel.clone();
        let login_client = github_client.clone();
        scope.spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = login_cancel.cancelled() => break,
                    request = requests.recv() => {
                        let Some(reply) = request else { break; };
                        let result = tokio::select! {
                            biased;
                            _ = login_cancel.cancelled() => break,
                            result = login_client.viewer_login(&login_cancel) => result.map_err(|error| error.to_string()),
                        };
                        let _ = reply.send(result);
                    }
                }
            }
        });
        Some(sender)
    } else {
        None
    };
    let refresh_board = board.clone();
    scope.spawn(async move {
        let mut boards = board.subscribe();
        let mut metadata = sources.metadata.subscribe();
        let mut github = sources.github.subscribe();
        let mut issues = sources.issue_statuses.subscribe();
        let mut dev = sources.dev_status.as_ref().map(|handle| handle.subscribe());
        let mut manager = sources.manager_sessions.subscribe();
        boards.mark_changed();
        metadata.mark_changed();
        github.mark_changed();
        issues.mark_changed();
        manager.mark_changed();
        if let Some(dev) = dev.as_mut() {
            dev.mark_changed();
        }

        let mut transitions = TransitionTracker::default();
        let mut login: Option<String> = None;
        let mut login_reply: Option<oneshot::Receiver<Result<String, String>>> = None;
        let mut login_attempted_revision = None;
        let mut last_projection: Option<(Option<Arc<Board>>, SourceState)> = None;

        loop {
            if github_enabled
                && login.is_none()
                && login_reply.is_none()
                && matches!(github.borrow().state, SourceState::Ready)
                && github
                    .borrow()
                    .data
                    .as_ref()
                    .is_some_and(|data| has_comments(data.as_ref()))
                && login_attempted_revision != Some(github.borrow().revision)
            {
                let (reply, result) = oneshot::channel();
                if login_tx.as_ref().is_some_and(|sender| sender.try_send(reply).is_ok()) {
                    login_reply = Some(result);
                    login_attempted_revision = Some(github.borrow().revision);
                }
            }

            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                request = publisher.requested() => {
                    if request.is_none() { break; }
                    refresh_board.refresh();
                    continue;
                }
                result = async {
                    match login_reply.as_mut() {
                        Some(reply) => Some(reply.await),
                        None => std::future::pending().await,
                    }
                }, if login_reply.is_some() => {
                    login_reply = None;
                    if let Some(Ok(Ok(viewer))) = result {
                        login = Some(viewer);
                    }
                    // A failed or unavailable identity never treats the viewer's
                    // own comments as other people's news. Retry on the next
                    // GitHub revision, without polling GitHub here.
                }
                changed = boards.changed() => if changed.is_err() { break; },
                changed = metadata.changed() => if changed.is_err() { break; },
                changed = github.changed() => if changed.is_err() { break; },
                changed = issues.changed() => if changed.is_err() { break; },
                changed = manager.changed() => if changed.is_err() { break; },
                changed = async { if let Some(dev) = dev.as_mut() { dev.changed().await } else { std::future::pending().await } } => if changed.is_err() { break; },
            }

            let board_snapshot = boards.borrow_and_update().clone();
            let metadata_snapshot = metadata.borrow_and_update().clone();
            let github_snapshot = github.borrow_and_update().clone();
            let issue_snapshot = issues.borrow_and_update().clone();
            let manager_snapshot = manager.borrow_and_update().clone();
            let dev_snapshot = dev
                .as_mut()
                .map(|updates| updates.borrow_and_update().clone());
            observe_metadata(&mut transitions, &metadata_snapshot);
            observe_issues(&mut transitions, &issue_snapshot);
            let visible = board_snapshot
                .data
                .as_deref()
                .map(|board| {
                    board
                        .rows
                        .iter()
                        .filter(|row| !wt_core::is_remote_worktree_ledger_key(&row.key))
                        .map(|row| row.slug.as_str())
                        .collect::<BTreeSet<_>>()
                })
                .unwrap_or_default();
            if let Some(snapshot) = dev_snapshot.as_ref() {
                observe_dev(&mut transitions, snapshot, &visible);
            }
            observe_manager(&mut transitions, &manager_snapshot);
            if github_enabled && let Some(viewer) = login.as_deref() {
                observe_comments(
                    &mut transitions,
                    &github_snapshot,
                    viewer,
                    board_snapshot.data.as_deref(),
                );
            }

            let seen_ms = metadata_snapshot
                .data
                .as_deref()
                .and_then(|(state, _)| state.get("attentionSeenTs"))
                .and_then(Value::as_u64)
                .unwrap_or_default();
            let projection = compose(board_snapshot, &transitions.events, seen_ms);
            let key = (projection.data.clone(), projection.state.clone());
            if last_projection.as_ref() != Some(&key) {
                last_projection = Some(key);
                publisher.publish(projection);
            }
        }
    });
    source
}

fn compose(
    mut board: SourceSnapshot<Board>,
    events: &VecDeque<wt_tui::AttentionLine>,
    seen_ms: u64,
) -> SourceSnapshot<Board> {
    let Some(existing) = board.data.as_ref() else {
        return board;
    };
    let mut prepared = existing.as_ref().clone();
    prepared.attention.extend(events.iter().cloned());
    prepared.attention.sort_by_key(|event| event.at_ms);
    prepared.attention.dedup_by(|left, right| {
        left.at_ms == right.at_ms && left.source == right.source && left.text == right.text
    });
    if prepared.attention.len() > MAX_ATTENTION_FEED {
        prepared
            .attention
            .drain(..prepared.attention.len() - MAX_ATTENTION_FEED);
    }
    prepared.attention_seen_ms = seen_ms;
    crate::activity_source::bound_feeds(&mut prepared);
    board.data = Some(Arc::new(prepared));
    board
}

fn observe_metadata(tracker: &mut TransitionTracker, snapshot: &SourceSnapshot<Metadata>) {
    if !matches!(snapshot.state, SourceState::Ready) {
        return;
    }
    if let Some((state, _)) = snapshot.data.as_deref() {
        tracker.metadata(state);
    }
}

fn observe_issues(tracker: &mut TransitionTracker, snapshot: &SourceSnapshot<StatusBatch>) {
    if matches!(snapshot.state, SourceState::Ready)
        && let Some(batch) = snapshot.data.as_deref()
    {
        tracker.issue_statuses(batch);
    }
}

fn observe_dev(
    tracker: &mut TransitionTracker,
    snapshot: &SourceSnapshot<Vec<DevStatusRow>>,
    visible: &BTreeSet<&str>,
) {
    if matches!(snapshot.state, SourceState::Ready)
        && let Some(rows) = snapshot.data.as_deref()
    {
        tracker.dev_status(rows, visible);
    }
}

fn observe_manager(tracker: &mut TransitionTracker, snapshot: &SourceSnapshot<SessionDiscoveries>) {
    if matches!(snapshot.state, SourceState::Ready)
        && let Some(discoveries) = snapshot.data.as_deref()
    {
        tracker.manager_state(discoveries);
    }
}

fn observe_comments(
    tracker: &mut TransitionTracker,
    snapshot: &SourceSnapshot<GithubData>,
    viewer: &str,
    board: Option<&Board>,
) {
    if matches!(snapshot.state, SourceState::Ready)
        && let Some(data) = snapshot.data.as_deref()
    {
        tracker.comments(data, viewer, board);
    }
}

#[derive(Default)]
struct TransitionTracker {
    statuses: Option<BTreeMap<String, Option<String>>>,
    sections: Option<BTreeMap<String, Option<String>>>,
    issues: Option<BTreeMap<String, String>>,
    dev: Option<BTreeMap<String, bool>>,
    manager: Option<(String, Option<DerivedState>)>,
    comments: BTreeMap<String, String>,
    events: VecDeque<wt_tui::AttentionLine>,
}

impl TransitionTracker {
    fn push(&mut self, line: String) {
        let text = wt_core::sanitize_terminal_text(&line);
        let at_ms = crate::activity_source::epoch_ms();
        let source = "wt".to_owned();
        tracing::info!(target: "wt_attention", event_at_ms = at_ms, event_channel = "attention", event_source = %source, event_text = %text, "attention event");
        self.events.push_back(wt_tui::AttentionLine {
            at_ms,
            source,
            text,
        });
        while self.events.len() > MAX_ATTENTION_LINES {
            self.events.pop_front();
        }
    }

    fn metadata(&mut self, state: &Value) {
        let next_statuses = local_statuses(state);
        let next_sections = sections(state);
        if let Some(previous) = self.statuses.clone() {
            for (slug, at) in &next_statuses {
                if !previous.contains_key(slug) || previous.get(slug) == Some(at) {
                    continue;
                }
                let Some(record) = state
                    .get("slugs")
                    .and_then(|slugs| slugs.get(slug))
                    .and_then(|entry| entry.get("work"))
                else {
                    continue;
                };
                self.push(format!(
                    "{}: {}",
                    wt_core::worktree_ledger_label(slug),
                    describe_status(record)
                ));
            }
        }
        if let Some(previous) = self.sections.clone() {
            for (key, section) in &next_sections {
                if !previous.contains_key(key) || previous.get(key) == Some(section) {
                    continue;
                }
                let destination = section
                    .as_ref()
                    .map_or_else(|| "the inbox".to_owned(), Clone::clone);
                self.push(format!(
                    "{} moved to {destination}",
                    wt_core::worktree_ledger_label(key)
                ));
            }
        }
        self.statuses = Some(next_statuses);
        self.sections = Some(next_sections);
    }

    fn issue_statuses(&mut self, batch: &StatusBatch) {
        let next = batch.statuses.clone();
        if let Some(previous) = self.issues.clone() {
            for (id, status) in &next {
                if let Some(old) = previous.get(id).filter(|old| *old != status) {
                    self.push(format!("issues: #{id}: {old} → {status}"));
                }
            }
        }
        self.issues = Some(next);
    }

    fn dev_status(&mut self, rows: &[DevStatusRow], visible: &BTreeSet<&str>) {
        let mut next = self.dev.clone().unwrap_or_default();
        next.retain(|slug, _| visible.contains(slug.as_str()));
        for row in rows {
            if !visible.contains(row.slug.as_str()) {
                continue;
            }
            let Some(status) = &row.status else {
                continue;
            };
            if status.crashed && next.get(&row.slug) == Some(&false) {
                self.push(format!(
                    "{}: dev server crashed, see `wt dev logs`",
                    wt_core::worktree_ledger_label(&row.slug)
                ));
            }
            next.insert(row.slug.clone(), status.crashed);
        }
        /*
         * A row-level status error is unknown, not a stopped server. Keep the
         * last successful value above; a later confirmed crash can still fire.
         */
        self.dev = Some(next);
    }

    fn manager_state(&mut self, discoveries: &[DiscoveredSession]) {
        let current = discoveries
            .iter()
            .find(|entry| entry.key.slug == "manager")
            .map(|entry| {
                (
                    format!("{}:{}", entry.key.harness.as_str(), entry.key.session_id),
                    entry.session.extras.derived_state,
                )
            });
        let previous = self.manager.clone();
        self.manager = current.clone();
        if let (Some((old_id, old_state)), Some((id, Some(DerivedState::Asking)))) =
            (previous, current)
            && old_id == id
            && old_state != Some(DerivedState::Asking)
        {
            self.push("manager session is waiting on input (m to attach)".into());
        }
    }

    fn comments(&mut self, data: &GithubData, viewer: &str, board: Option<&Board>) {
        for (branch, pr) in &data.prs {
            let key = format!("{branch}#{}", pr.number);
            let latest = latest_foreign_at(&pr.comments, viewer);
            let Some(mark) = self.comments.get(&key).cloned() else {
                self.comments.insert(key, latest);
                continue;
            };
            let fresh = new_comments_since(&pr.comments, &mark, viewer);
            if fresh.is_empty() {
                continue;
            }
            self.comments.insert(
                key,
                fresh
                    .last()
                    .map(|comment| comment.created_at.clone())
                    .unwrap_or(mark),
            );
            let label = board
                .and_then(|board| board.rows.iter().find(|row| row.branch == *branch))
                .map(|row| wt_core::worktree_ledger_label(&row.key))
                .unwrap_or_else(|| branch.clone());
            for line in comment_lines(&fresh) {
                self.push(format!("{label}: {line}"));
            }
        }
        while self.comments.len() > 256 {
            if let Some(first) = self.comments.keys().next().cloned() {
                self.comments.remove(&first);
            }
        }
    }
}

fn local_statuses(state: &Value) -> BTreeMap<String, Option<String>> {
    state
        .get("slugs")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|entries| entries.iter())
        .map(|(slug, entry)| {
            (
                slug.clone(),
                entry
                    .get("work")
                    .and_then(|work| work.get("at"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            )
        })
        .collect()
}

fn sections(state: &Value) -> BTreeMap<String, Option<String>> {
    let mut next = state
        .get("slugs")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|entries| entries.iter())
        .map(|(slug, entry)| {
            (
                slug.clone(),
                entry
                    .get("section")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            )
        })
        .collect::<BTreeMap<_, _>>();
    if let Some(layouts) = state.get("remoteLayouts").and_then(Value::as_object) {
        next.extend(layouts.iter().map(|(key, layout)| {
            (
                key.clone(),
                layout
                    .get("section")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            )
        }));
    }
    next
}

fn describe_status(record: &Value) -> String {
    let status = record
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let label = match status {
        "needs-human" => "needs you".to_owned(),
        "ready" => "ready to merge".to_owned(),
        "needs-testing" => "needs testing".to_owned(),
        "dropped" => "dropped, will not land".to_owned(),
        other => format!("→ {other}"),
    };
    let risk = record
        .get("risk")
        .and_then(Value::as_str)
        .map(|value| format!(" (risk: {value})"))
        .unwrap_or_default();
    let note = record
        .get("note")
        .and_then(Value::as_str)
        .map(|value| format!(" - {}", truncate(value, 160)))
        .unwrap_or_default();
    let blocked = record
        .get("blockedOn")
        .and_then(Value::as_str)
        .map(|value| format!(" [blocked on: {}]", truncate(value, 80)))
        .unwrap_or_default();
    let verify = record
        .get("verifyAfterMerge")
        .and_then(Value::as_str)
        .map(|value| format!(" [verify after merge: {}]", truncate(value, 100)))
        .unwrap_or_default();
    format!("{label}{risk}{blocked}{verify}{note}")
}

fn has_comments(data: &GithubData) -> bool {
    data.prs.values().any(|pr| !pr.comments.is_empty())
}

fn latest_foreign_at(comments: &[PrComment], viewer: &str) -> String {
    comments
        .iter()
        .filter(|comment| !comment.author.eq_ignore_ascii_case(viewer))
        .map(|comment| comment.created_at.as_str())
        .max()
        .unwrap_or_default()
        .to_owned()
}

fn new_comments_since<'a>(
    comments: &'a [PrComment],
    mark: &str,
    viewer: &str,
) -> Vec<&'a PrComment> {
    let mut fresh = comments
        .iter()
        .filter(|comment| {
            !comment.author.eq_ignore_ascii_case(viewer) && comment.created_at.as_str() > mark
        })
        .collect::<Vec<_>>();
    fresh.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    fresh
}

fn comment_lines(fresh: &[&PrComment]) -> Vec<String> {
    if fresh.len() > MAX_COMMENT_LINES {
        let authors = fresh
            .iter()
            .map(|comment| comment.author.as_str())
            .collect::<BTreeSet<_>>();
        return vec![format!(
            "{} new PR comments ({})",
            fresh.len(),
            authors.into_iter().collect::<Vec<_>>().join(", ")
        )];
    }
    fresh
        .iter()
        .map(|comment| {
            let body = comment
                .body
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let body = truncate(&body, COMMENT_BODY_CHARS);
            format!("{} commented: {body}", comment.author)
        })
        .collect()
}

fn truncate(value: &str, limit: usize) -> String {
    let mut chars = value.chars();
    let prefix = chars.by_ref().take(limit).collect::<String>();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comment(author: &str, body: &str, created_at: &str) -> PrComment {
        PrComment {
            author: author.into(),
            body: body.into(),
            created_at: created_at.into(),
        }
    }

    fn pr(comments: Vec<PrComment>) -> wt_github::PullRequest {
        wt_github::PullRequest {
            id: None,
            number: 12,
            url: "https://example.invalid/pull/12".into(),
            head_ref_name: "feature".into(),
            head_ref_oid: None,
            base_ref_name: "main".into(),
            merge_commit_oid: None,
            title: "Feature".into(),
            is_draft: false,
            state: "OPEN".into(),
            mergeable: None,
            merge_state_status: None,
            checks: wt_github::PrChecks::None,
            failed_checks: vec![],
            review: wt_github::PrReview::None,
            review_requests: 0,
            requested_reviewers: vec![],
            suggested_reviewers: vec![],
            review_bot: None,
            auto_merge: None,
            comments,
            unresolved_threads: 0,
            unresolved_threads_total: 0,
            merged_at: None,
            closed_at: None,
        }
    }

    fn dev(crashed: bool) -> DevStatusRow {
        DevStatusRow {
            slug: "one".into(),
            status: Some(wt_dev::DevServerStatus {
                running: !crashed,
                starting: false,
                crashed,
                port: None,
                url: None,
                since: None,
                waiting: None,
                rebased_since: None,
                restarts: None,
            }),
            error: None,
        }
    }

    #[test]
    fn first_observations_are_silent_then_status_and_section_changes_are_narrated() {
        let initial = serde_json::json!({"slugs":{"one":{"section":"Build","work":{"state":"working","at":"1"}}}});
        let changed = serde_json::json!({"slugs":{"one":{"section":"Review","work":{"state":"ready","risk":"low","note":"ship it","at":"2"}}}});
        let mut tracker = TransitionTracker::default();
        tracker.metadata(&initial);
        assert!(tracker.events.is_empty());
        tracker.metadata(&changed);
        assert!(
            tracker
                .events
                .iter()
                .any(|line| line.text.contains("ready to merge (risk: low)"))
        );
        assert!(
            tracker
                .events
                .iter()
                .any(|line| line.text.contains("moved to Review"))
        );
    }

    #[test]
    fn comments_seed_quietly_filter_viewer_and_limit_large_backlog() {
        let mut tracker = TransitionTracker::default();
        let first = GithubData {
            prs: [(
                "feature".into(),
                pr(vec![
                    comment("me", "mine", "2026-01-01"),
                    comment("other", "old", "2026-01-02"),
                ]),
            )]
            .into(),
            ..Default::default()
        };
        tracker.comments(&first, "me", None);
        assert!(tracker.events.is_empty());
        let second = GithubData {
            prs: [(
                "feature".into(),
                pr(vec![
                    comment("me", "mine", "2026-01-01"),
                    comment("other", "old", "2026-01-02"),
                    comment("a", "hello\nworld", "2026-01-03"),
                    comment("b", "second", "2026-01-04"),
                    comment("c", "third", "2026-01-05"),
                    comment("d", "fourth", "2026-01-06"),
                ]),
            )]
            .into(),
            ..Default::default()
        };
        tracker.comments(&second, "me", None);
        assert_eq!(tracker.events.len(), 1);
        assert!(tracker.events[0].text.contains("4 new PR comments"));
    }

    #[test]
    fn issue_statuses_and_dev_crashes_only_narrate_transitions() {
        let mut tracker = TransitionTracker::default();
        let issue = StatusBatch {
            ids: vec!["ENG-1".into()],
            statuses: [("ENG-1".into(), "Open".into())].into(),
        };
        tracker.issue_statuses(&issue);
        tracker.dev_status(&[dev(false)], &BTreeSet::from(["one"]));
        assert!(tracker.events.is_empty());
        tracker.issue_statuses(&StatusBatch {
            ids: issue.ids,
            statuses: [("ENG-1".into(), "Review".into())].into(),
        });
        tracker.dev_status(&[dev(true)], &BTreeSet::from(["one"]));
        assert!(
            tracker
                .events
                .iter()
                .any(|line| line.text.contains("#ENG-1: Open → Review"))
        );
        assert!(
            tracker
                .events
                .iter()
                .any(|line| line.text.contains("dev server crashed"))
        );
    }

    #[test]
    fn attention_tail_is_bounded() {
        let mut tracker = TransitionTracker::default();
        for index in 0..MAX_ATTENTION_LINES + 5 {
            tracker.push(format!("line {index}"));
        }
        assert_eq!(tracker.events.len(), MAX_ATTENTION_LINES);
        assert_eq!(
            tracker.events.front().map(|line| line.text.as_str()),
            Some("line 5")
        );
    }

    #[test]
    fn failed_source_keeps_last_good_transition_baseline() {
        let mut tracker = TransitionTracker::default();
        let first = SourceSnapshot {
            data: Some(Arc::new(StatusBatch {
                ids: vec!["ENG-1".into()],
                statuses: [("ENG-1".into(), "Open".into())].into(),
            })),
            state: SourceState::Ready,
            updated_at: None,
            revision: 1,
        };
        observe_issues(&mut tracker, &first);
        let failed = SourceSnapshot {
            data: Some(Arc::new(StatusBatch {
                ids: vec!["ENG-1".into()],
                statuses: [("ENG-1".into(), "Unknown".into())].into(),
            })),
            state: SourceState::Failed("temporary".into()),
            updated_at: None,
            revision: 2,
        };
        observe_issues(&mut tracker, &failed);
        assert!(tracker.events.is_empty());
        observe_issues(
            &mut tracker,
            &SourceSnapshot {
                data: Some(Arc::new(StatusBatch {
                    ids: vec!["ENG-1".into()],
                    statuses: [("ENG-1".into(), "Review".into())].into(),
                })),
                state: SourceState::Ready,
                updated_at: None,
                revision: 3,
            },
        );
        assert!(
            tracker
                .events
                .iter()
                .any(|line| line.text.contains("Open → Review"))
        );
    }

    #[test]
    fn manager_asking_transition_is_bound_to_session_identity() {
        let manager = |session_id: &str, state| DiscoveredSession {
            key: crate::session_source::SessionKey {
                slug: "manager".into(),
                harness: wt_core::HarnessId::Codex,
                session_id: session_id.into(),
            },
            session: wt_harness::HarnessSession {
                display_name: "manager".into(),
                session_id: session_id.into(),
                tmux_session_name: "manager".into(),
                last_active_ms: None,
                is_live: true,
                extras: wt_harness::HarnessExtras {
                    derived_state: state,
                    ..Default::default()
                },
            },
        };
        let mut tracker = TransitionTracker::default();
        tracker.manager_state(&[manager("one", Some(DerivedState::Working))]);
        tracker.manager_state(&[manager("one", Some(DerivedState::Asking))]);
        tracker.manager_state(&[manager("two", Some(DerivedState::Asking))]);
        assert_eq!(tracker.events.len(), 1);
    }
}
