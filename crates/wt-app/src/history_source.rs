//! On-demand projection of the durable removed-worktree ledger.
//!
//! History is deliberately dormant while the ordinary fleet view is shown.
//! Opening it reads the bounded ledger and hydrates tracker statuses in one
//! batch; metadata and inventory changes refresh the same prepared snapshot.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::sync::watch;
use wt_platform::process::CommandSpec;
use wt_runtime::{SourceHandle, SourceSnapshot, SourceState, TaskScope, source_channel};
use wt_store::RemovedWorktree;
use wt_tui::{Board, RemovedHistoryRow, RemovedHistorySnapshot};
use wt_vcs::WorktreeSnapshot;

use crate::{context::AppContext, issue_identity, local_source::Metadata};

const MAX_ENTRIES: usize = 30;
const MAX_AGE: Duration = Duration::from_secs(14 * 24 * 60 * 60);

pub struct HistorySources {
    pub snapshot: SourceHandle<RemovedHistorySnapshot>,
    pub commands: HistoryCommands,
}

#[derive(Clone)]
pub struct HistoryCommands {
    active: watch::Sender<bool>,
}

impl HistoryCommands {
    /// Enable or suspend the history-only readers. Repeated values are cheap.
    pub fn set_active(&self, active: bool) {
        self.active.send_if_modified(|current| {
            if *current == active {
                false
            } else {
                *current = active;
                true
            }
        });
    }
}

pub fn start(
    scope: &TaskScope,
    context: &AppContext,
    metadata: SourceHandle<Metadata>,
    live: SourceHandle<Vec<WorktreeSnapshot>>,
) -> HistorySources {
    let (source, mut publisher) = source_channel();
    let (active_sender, mut active) = watch::channel(false);
    let cancellation = scope.token();
    let context = context.clone();
    scope.spawn(async move {
        let mut metadata_updates = metadata.subscribe();
        let mut live_updates = live.subscribe();
        metadata_updates.mark_changed();
        live_updates.mark_changed();
        let mut is_active = false;
        let mut refresh_needed = false;
        let mut last_data: Option<Arc<RemovedHistorySnapshot>> = None;

        loop {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => break,
                request = publisher.requested() => {
                    if request.is_none() { break; }
                    if is_active {
                        metadata_updates.mark_changed();
                        live_updates.mark_changed();
                        refresh_needed = true;
                    }
                }
                changed = active.changed() => {
                    if changed.is_err() { break; }
                    is_active = *active.borrow_and_update();
                    if is_active {
                        metadata.refresh();
                        live.refresh();
                        refresh_needed = true;
                    } else {
                        last_data = None;
                        publisher.publish(SourceSnapshot {
                            data: Some(Arc::new(RemovedHistorySnapshot::default())),
                            state: SourceState::Ready,
                            updated_at: Some(tokio::time::Instant::now()),
                            revision: 0,
                        });
                    }
                }
                changed = metadata_updates.changed() => {
                    if changed.is_err() { break; }
                    metadata_updates.borrow_and_update();
                    refresh_needed = is_active;
                }
                changed = live_updates.changed() => {
                    if changed.is_err() { break; }
                    live_updates.borrow_and_update();
                    refresh_needed = is_active;
                }
            }

            if !is_active || !refresh_needed {
                continue;
            }
            refresh_needed = false;

            let metadata_snapshot = metadata_updates.borrow().clone();
            let live_snapshot = live_updates.borrow().clone();
            if !matches!(live_snapshot.state, SourceState::Ready)
                || !matches!(metadata_snapshot.state, SourceState::Ready)
            {
                let state = match (&live_snapshot.state, &metadata_snapshot.state) {
                    (SourceState::Failed(error), _) => {
                        SourceState::Failed(format!("inventory: {error}").into())
                    }
                    (_, SourceState::Failed(error)) => {
                        SourceState::Failed(format!("state: {error}").into())
                    }
                    _ => SourceState::Refreshing,
                };
                publisher.publish(SourceSnapshot {
                    data: last_data.clone(),
                    state,
                    updated_at: None,
                    revision: 0,
                });
                continue;
            }
            let Some(live_rows) = live_snapshot.data.as_deref() else {
                live.refresh();
                continue;
            };
            if metadata_snapshot.data.is_none() {
                metadata.refresh();
                continue;
            }

            let live_slugs = live_rows
                .iter()
                .filter(|row| !row.worktree.is_main)
                .map(|row| row.worktree.target.slug().to_owned())
                .collect::<BTreeSet<_>>();
            let request_cancellation = cancellation.child_token();
            let prepared = tokio::select! {
                biased;
                _ = cancellation.cancelled() => break,
                changed = active.changed() => {
                    if changed.is_err() { break; }
                    is_active = *active.borrow_and_update();
                    request_cancellation.cancel();
                    if is_active {
                        refresh_needed = true;
                    } else {
                        last_data = None;
                        publisher.publish(SourceSnapshot {
                            data: Some(Arc::new(RemovedHistorySnapshot::default())),
                            state: SourceState::Ready,
                            updated_at: Some(tokio::time::Instant::now()),
                            revision: 0,
                        });
                    }
                    continue;
                }
                result = prepare(&context, live_slugs, &request_cancellation) => result,
            };
            match prepared {
                Ok(snapshot) if !cancellation.is_cancelled() => {
                    let snapshot = Arc::new(snapshot);
                    last_data = Some(snapshot.clone());
                    publisher.publish(SourceSnapshot {
                        data: Some(snapshot),
                        state: SourceState::Ready,
                        updated_at: Some(tokio::time::Instant::now()),
                        revision: 0,
                    });
                }
                Ok(_) => break,
                Err(_error) if cancellation.is_cancelled() => break,
                Err(error) => {
                    tracing::warn!(%error, "removed-worktree history refresh failed");
                    publisher.publish(SourceSnapshot {
                        data: last_data.clone(),
                        state: SourceState::Failed(format!("history: {error:#}").into()),
                        updated_at: None,
                        revision: 0,
                    });
                }
            }
        }
    });

    HistorySources {
        snapshot: source,
        commands: HistoryCommands {
            active: active_sender,
        },
    }
}

/// Attach the dormant history lane to each host's prepared board. Board refresh
/// requests reach the history lane too; its active gate decides whether that
/// request performs any disk or tracker work.
pub fn overlay(
    scope: &TaskScope,
    board: SourceHandle<Board>,
    history: SourceHandle<RemovedHistorySnapshot>,
) -> SourceHandle<Board> {
    let (output, mut publisher) = source_channel();
    let cancellation = scope.token();
    scope.spawn(async move {
        let mut boards = board.subscribe();
        let mut histories = history.subscribe();
        boards.mark_changed();
        histories.mark_changed();
        loop {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => break,
                request = publisher.requested() => {
                    if request.is_none() { break; }
                    board.refresh();
                    history.refresh();
                    continue;
                }
                changed = boards.changed() => if changed.is_err() { break; },
                changed = histories.changed() => if changed.is_err() { break; },
            }
            let base = boards.borrow_and_update().clone();
            let history_snapshot = histories.borrow_and_update().clone();
            let mut data = base.data.as_deref().cloned();
            if let Some(board) = data.as_mut() {
                if let Some(history) = history_snapshot.data.as_deref() {
                    board.removed_history = history.clone();
                }
                if let SourceState::Failed(error) = &history_snapshot.state {
                    let detail = clean(&format!("History: {error}"));
                    if !board.attention.iter().any(|line| line.text == detail) {
                        crate::activity_source::append_attention(board, "History", &detail);
                    }
                }
            }
            publisher.publish(SourceSnapshot {
                data: data.map(Arc::new),
                state: base.state,
                updated_at: base.updated_at.or(history_snapshot.updated_at),
                revision: 0,
            });
        }
    });
    output
}

async fn prepare(
    context: &AppContext,
    live_slugs: BTreeSet<String>,
    cancellation: &tokio_util::sync::CancellationToken,
) -> Result<RemovedHistorySnapshot> {
    let records = context
        .database
        .call(|store| Ok(store.read_removed_worktrees()?))
        .await?;
    let cutoff = now_ms().saturating_sub(MAX_AGE.as_millis().min(i64::MAX as u128) as i64);
    let records = filter_records(records, &live_slugs, cutoff);

    let statuses = read_issue_statuses(context, &records, cancellation).await;
    let needs_github_url = records.iter().any(|entry| {
        entry
            .extra
            .get("issueId")
            .and_then(Value::as_str)
            .is_some_and(|id| id.to_ascii_uppercase().starts_with("GH-"))
            || entry
                .extra
                .get("githubIssue")
                .and_then(Value::as_u64)
                .is_some()
    });
    let github_repo = if needs_github_url {
        read_github_repo_url(context, cancellation).await
    } else {
        None
    };

    let rows = records
        .into_iter()
        .map(|entry| {
            present(
                context
                    .config
                    .issue_tracker
                    .as_ref()
                    .and_then(|tracker| tracker.url_template.as_deref()),
                entry,
                &statuses,
                github_repo.as_deref(),
            )
        })
        .collect();
    Ok(RemovedHistorySnapshot { rows })
}

fn present(
    issue_url_template: Option<&str>,
    entry: RemovedWorktree,
    statuses: &BTreeMap<String, String>,
    github_repo: Option<&str>,
) -> RemovedHistoryRow {
    let issue_override = entry.extra.get("issueId").and_then(Value::as_str);
    let issue_id = issue_identity::resolve(&entry.slug, issue_override);
    let issue_url = issue_id
        .as_deref()
        .and_then(|id| {
            if let Some(number) = id
                .strip_prefix("GH-")
                .and_then(|value| value.parse::<u64>().ok())
            {
                return github_repo.map(|repo| format!("{repo}/issues/{number}"));
            }
            issue_url_template.map(|template| template.replace("{id}", id))
        })
        .or_else(|| {
            let issue = entry.extra.get("githubIssue").and_then(Value::as_u64)?;
            github_repo.map(|repo| format!("{repo}/issues/{issue}"))
        });
    let mut details = Vec::new();
    if let Some(work) = &entry.work {
        details.push(format!("Work: {}", clean(&work.state)));
        if let Some(note) = &work.note {
            details.push(clean(note));
        }
        if let Some(blocked) = &work.blocked_on {
            details.push(format!("Blocked on: {}", clean(blocked)));
        }
        if let Some(steps) = &work.verify_after_merge {
            details.push(format!("Verify after merge: {}", clean(steps)));
        }
    }
    if let Some(git_state) = entry.extra.get("gitState").and_then(Value::as_str) {
        details.push(format!("Git: {}", clean(git_state)));
    }
    if let Some(pr_state) = entry.extra.get("prState").and_then(Value::as_str) {
        details.push(format!("PR: {}", clean(pr_state)));
    }
    let production_landed = match entry.extra.get("landedOnAtRemoval").and_then(Value::as_str) {
        Some("production") => Some(true),
        Some("base") => Some(false),
        _ => None,
    };
    if production_landed == Some(true) {
        details.push("Landed on production when removed".into());
    } else if production_landed == Some(false) {
        details.push("Landed on base when removed".into());
    }
    let issue_status = issue_id
        .as_ref()
        .and_then(|id| statuses.get(id))
        .map(|status| clean(status));
    if let Some(status) = &issue_status {
        details.push(format!("Issue: {status}"));
    }
    if let Some(verify) = entry
        .work
        .as_ref()
        .and_then(|work| work.verify_after_merge.as_ref())
        && entry
            .work
            .as_ref()
            .is_some_and(|work| work.state != "verified" && work.state != "dropped")
    {
        details.push(format!("Verification is still owed: {}", clean(verify)));
    }
    RemovedHistoryRow {
        key: entry.slug.clone(),
        host: None,
        slug: clean(&entry.slug),
        branch: clean(&entry.branch),
        title: entry
            .extra
            .get("title")
            .and_then(Value::as_str)
            .map(clean)
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_else(|| clean(&entry.slug)),
        removed_at: entry.removed_at,
        details,
        issue_url,
        pr_url: entry.extra.get("prUrl").and_then(Value::as_str).map(clean),
        issue_status,
        production_landed,
        automations_paused: entry.automations_paused.unwrap_or(false),
    }
}

fn filter_records(
    records: Vec<RemovedWorktree>,
    live_slugs: &BTreeSet<String>,
    cutoff: i64,
) -> Vec<RemovedWorktree> {
    let mut records = records
        .into_iter()
        .filter(|entry| {
            !live_slugs.contains(&entry.slug)
                && parse_timestamp_ms(&entry.removed_at)
                    .is_some_and(|removed_at| removed_at >= cutoff)
        })
        .collect::<Vec<_>>();
    records.sort_by(|left, right| right.removed_at.cmp(&left.removed_at));
    records.truncate(MAX_ENTRIES);
    records
}

async fn read_issue_statuses(
    context: &AppContext,
    records: &[RemovedWorktree],
    cancellation: &tokio_util::sync::CancellationToken,
) -> BTreeMap<String, String> {
    let tracker = context.config.issue_tracker.as_ref();
    let Some(command) = tracker.and_then(|tracker| tracker.status_command.as_ref()) else {
        return BTreeMap::new();
    };
    let ids = records
        .iter()
        .filter_map(|entry| {
            issue_identity::resolve(
                &entry.slug,
                entry.extra.get("issueId").and_then(Value::as_str),
            )
        })
        .filter(|id| {
            issue_identity::is_tracker(id, tracker.and_then(|tracker| tracker.prefix.as_deref()))
        })
        .collect::<BTreeSet<_>>();
    if ids.is_empty() {
        return BTreeMap::new();
    }
    let mut argv = Vec::new();
    for arg in command {
        if arg == "{ids}" {
            argv.extend(ids.iter().cloned());
        } else {
            argv.push(arg.clone());
        }
    }
    let Some((program, args)) = argv.split_first() else {
        return BTreeMap::new();
    };
    let mut spec = CommandSpec::new(program)
        .args(args)
        .cwd(&context.config.paths.main_clone);
    spec.timeout = Duration::from_secs(30);
    let output = match context.processes.run(spec, cancellation).await {
        Ok(output) if output.status.success() => output,
        Ok(output) => {
            tracing::warn!(status = %output.status, "removed-history issue status reader failed");
            return BTreeMap::new();
        }
        Err(error) => {
            if !cancellation.is_cancelled() {
                tracing::warn!(%error, "removed-history issue status reader failed");
            }
            return BTreeMap::new();
        }
    };
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Response {
        issues: Vec<Issue>,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Issue {
        id: String,
        status: String,
    }
    let Ok(response) = serde_json::from_slice::<Response>(&output.stdout) else {
        tracing::warn!("removed-history issue status reader returned invalid JSON");
        return BTreeMap::new();
    };
    let mut statuses = BTreeMap::new();
    for issue in response.issues {
        if ids.contains(&issue.id)
            && !issue.status.trim().is_empty()
            && !issue.status.chars().any(char::is_control)
        {
            statuses
                .entry(issue.id)
                .or_insert_with(|| issue.status.trim().to_owned());
        }
    }
    statuses
}

async fn read_github_repo_url(
    context: &AppContext,
    cancellation: &tokio_util::sync::CancellationToken,
) -> Option<String> {
    let mut spec = CommandSpec::new("git")
        .args(["remote", "get-url", "origin"])
        .cwd(&context.config.paths.main_clone);
    spec.timeout = Duration::from_secs(5);
    let output = context.processes.run(spec, cancellation).await.ok()?;
    if !output.status.success() {
        return None;
    }
    repo_web_url(output.stdout_text().trim())
}

fn repo_web_url(remote: &str) -> Option<String> {
    let captures = regex::Regex::new(
        r"^(?:git@|ssh://git@|https?://)([^/:]+\.[^/:]+)[/:]([^/]+)/([^/]+?)(?:\.git)?/?$",
    )
    .ok()?
    .captures(remote.trim())?;
    Some(format!(
        "https://{}/{}/{}",
        &captures[1], &captures[2], &captures[3]
    ))
}

fn clean(text: &str) -> String {
    wt_core::sanitize_terminal_text(text)
}

fn now_ms() -> i64 {
    OffsetDateTime::now_utc()
        .unix_timestamp_nanos()
        .saturating_div(1_000_000) as i64
}

fn parse_timestamp_ms(value: &str) -> Option<i64> {
    let timestamp = OffsetDateTime::parse(value, &Rfc3339).ok()?;
    i64::try_from(timestamp.unix_timestamp_nanos() / 1_000_000).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Map, json};
    use wt_runtime::source_channel;

    #[test]
    fn presentation_preserves_manual_snapshot_links_and_unknown_landing() {
        let mut extra = Map::new();
        extra.insert("issueId".into(), json!("ENG-12"));
        extra.insert("githubIssue".into(), json!(44));
        extra.insert("title".into(), json!("Saved title"));
        extra.insert("prUrl".into(), json!("https://github.com/org/repo/pull/8"));
        extra.insert("landedOnAtRemoval".into(), json!("base"));
        let entry = RemovedWorktree {
            slug: "eng-12-fix".into(),
            branch: "m/eng-12-fix".into(),
            removed_at: "2026-10-09T10:00:00Z".into(),
            work: Some(wt_store::WorkStatusRecord {
                state: "ready".into(),
                at: "2026-10-09T09:00:00Z".into(),
                note: Some("saved note".into()),
                verify_after_merge: Some("check production".into()),
                ..serde_json::from_value(json!({"state":"ready","at":"2026-10-09T09:00:00Z"}))
                    .unwrap()
            }),
            automations_paused: Some(true),
            extra,
        };
        let statuses = BTreeMap::from([("ENG-12".into(), "In Review".into())]);
        let row = present(
            Some("https://issues.example/{id}"),
            entry,
            &statuses,
            Some("https://github.com/org/repo"),
        );
        assert_eq!(row.key, "eng-12-fix");
        assert_eq!(row.title, "Saved title");
        assert_eq!(
            row.issue_url.as_deref(),
            Some("https://issues.example/ENG-12")
        );
        assert_eq!(
            row.pr_url.as_deref(),
            Some("https://github.com/org/repo/pull/8")
        );
        assert_eq!(row.issue_status.as_deref(), Some("In Review"));
        assert_eq!(row.production_landed, Some(false));
        assert!(row.automations_paused);
        assert!(row.details.iter().any(|detail| detail == "saved note"));
        assert!(
            row.details
                .iter()
                .any(|detail| detail == "Verify after merge: check production")
        );
    }

    #[test]
    fn asserted_empty_issue_suppresses_slug_fallback_but_github_issue_is_link_fallback() {
        let mut extra = Map::new();
        extra.insert("issueId".into(), json!(""));
        extra.insert("githubIssue".into(), json!(44));
        let entry = RemovedWorktree {
            slug: "eng-12-fix".into(),
            branch: "m/eng-12-fix".into(),
            removed_at: "2026-10-09T10:00:00Z".into(),
            work: None,
            automations_paused: None,
            extra,
        };
        let row = present(
            None,
            entry,
            &BTreeMap::new(),
            Some("https://github.com/org/repo"),
        );
        assert_eq!(
            row.issue_url.as_deref(),
            Some("https://github.com/org/repo/issues/44")
        );
        assert_eq!(row.issue_status, None);
    }

    #[test]
    fn history_age_parser_rejects_invalid_and_keeps_rfc3339_precision() {
        assert_eq!(parse_timestamp_ms("not-a-date"), None);
        assert_eq!(
            parse_timestamp_ms("2026-10-09T10:00:00.123Z"),
            Some(1_791_540_000_123)
        );
    }

    #[test]
    fn history_filters_live_and_expired_rows_then_keeps_newest_thirty() {
        let cutoff = parse_timestamp_ms("2026-10-01T00:00:00Z").unwrap();
        let mut records = vec![RemovedWorktree {
            slug: "expired".into(),
            branch: "old".into(),
            removed_at: "2026-09-30T23:59:59Z".into(),
            work: None,
            automations_paused: None,
            extra: Map::new(),
        }];
        records.push(RemovedWorktree {
            slug: "live-again".into(),
            branch: "live".into(),
            removed_at: "2026-10-09T00:00:00Z".into(),
            work: None,
            automations_paused: None,
            extra: Map::new(),
        });
        for index in 0..32 {
            records.push(RemovedWorktree {
                slug: format!("row-{index:02}"),
                branch: format!("branch-{index:02}"),
                removed_at: format!("2026-10-09T00:00:{index:02}Z"),
                work: None,
                automations_paused: None,
                extra: Map::new(),
            });
        }
        let live = BTreeSet::from(["live-again".to_owned()]);
        let rows = filter_records(records, &live, cutoff);
        assert_eq!(rows.len(), MAX_ENTRIES);
        assert_eq!(rows.first().map(|row| row.slug.as_str()), Some("row-31"));
        assert!(
            rows.iter()
                .all(|row| row.slug != "live-again" && row.slug != "expired")
        );
    }

    #[tokio::test]
    async fn history_source_is_dormant_until_open_and_hides_restored_slug() {
        let fixture = crate::commands::test_support::CommandFixture::new()
            .await
            .unwrap();
        let inventory = fixture
            .ctx
            .repository
            .inventory_status(&fixture.ctx.cancellation)
            .await
            .unwrap();
        let restored_slug = inventory
            .iter()
            .find(|row| !row.worktree.is_main)
            .unwrap()
            .worktree
            .target
            .slug()
            .to_owned();
        let timestamp = OffsetDateTime::now_utc().format(&Rfc3339).unwrap();
        fixture
            .ctx
            .database
            .call(move |store| {
                let entries = [restored_slug, "historical".into()]
                    .into_iter()
                    .map(|slug| RemovedWorktree {
                        branch: format!("feature/{slug}"),
                        slug,
                        removed_at: timestamp.clone(),
                        work: None,
                        automations_paused: None,
                        extra: Map::new(),
                    })
                    .collect::<Vec<_>>();
                store.record_removed_worktrees(&entries, now_ms())?;
                Ok(())
            })
            .await
            .unwrap();

        let metadata_value = fixture
            .ctx
            .database
            .call(|store| Ok((store.read_wt_state()?, store.read_archived_keys()?)))
            .await
            .unwrap();
        let (metadata, metadata_publisher) = source_channel();
        let (live, live_publisher) = source_channel();
        metadata_publisher.publish(SourceSnapshot {
            data: Some(Arc::new(metadata_value)),
            state: SourceState::Ready,
            updated_at: Some(tokio::time::Instant::now()),
            revision: 0,
        });
        live_publisher.publish(SourceSnapshot {
            data: Some(Arc::new(inventory)),
            state: SourceState::Ready,
            updated_at: Some(tokio::time::Instant::now()),
            revision: 0,
        });

        let scope = TaskScope::new();
        let history = start(&scope, &fixture.ctx, metadata, live);
        assert!(history.snapshot.snapshot().data.is_none());
        history.commands.set_active(true);
        let mut updates = history.snapshot.subscribe();
        let snapshot = tokio::time::timeout(
            Duration::from_secs(2),
            updates.wait_for(|snapshot| {
                snapshot
                    .data
                    .as_ref()
                    .is_some_and(|data| data.rows.iter().any(|row| row.slug == "historical"))
            }),
        )
        .await
        .unwrap()
        .unwrap();
        let rows = snapshot.data.as_ref().unwrap().rows.clone();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].slug, "historical");
        drop(snapshot);

        history.commands.set_active(false);
        tokio::time::timeout(
            Duration::from_secs(2),
            updates.wait_for(|snapshot| {
                snapshot
                    .data
                    .as_ref()
                    .is_some_and(|data| data.rows.is_empty())
            }),
        )
        .await
        .unwrap()
        .unwrap();
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
        fixture.close().await.unwrap();
    }
}
