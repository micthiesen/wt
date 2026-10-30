//! Prepared projection of the durable removed-worktree ledger.
//!
//! Ledger rows stay ready even while history is closed. Opening the pane
//! hydrates tracker and landing facts in the background without hiding rows.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use anyhow::Result;
use futures_util::{StreamExt, stream};
use serde::Deserialize;
use serde_json::Value;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::sync::watch;
use wt_core::{Domain, sanitize_terminal_text};
use wt_platform::process::CommandSpec;
use wt_runtime::{
    Persistence, RefreshPolicy, SourceHandle, SourceSpec, SourceState, Stamp, TaskScope,
    derived_channel, leaf_channel,
};
use wt_store::RemovedWorktree;
use wt_tui::{Board, RemovedHistoryRow, RemovedHistorySnapshot};
use wt_vcs::WorktreeSnapshot;

use crate::local_source::Metadata;
use wt_app_core::{context::AppContext, issue_identity};

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
    /// Enable or suspend external history enrichment. Repeated values are cheap.
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
    let (source, mut publisher) = leaf_channel(
        scope,
        SourceSpec {
            name: "removed-history",
            domain: Domain::History,
            persistence: Persistence::Never("authoritative SQLite; read directly"),
            policy: RefreshPolicy::default(),
        },
    );
    #[expect(
        clippy::disallowed_methods,
        reason = "pane activity control signal, not board data"
    )]
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
        let mut last_prepared = RemovedHistorySnapshot::default();

        loop {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => break,
                request = publisher.requested() => {
                    if request.is_none() { break; }
                    refresh_needed = true;
                }
                changed = active.changed() => {
                    if changed.is_err() { break; }
                    is_active = *active.borrow_and_update();
                    if is_active {
                        metadata.refresh();
                        live.refresh();
                        refresh_needed = true;
                    }
                }
                changed = metadata_updates.changed() => {
                    if changed.is_err() { break; }
                    metadata_updates.borrow_and_update();
                    refresh_needed = true;
                }
                changed = live_updates.changed() => {
                    if changed.is_err() { break; }
                    live_updates.borrow_and_update();
                    refresh_needed = true;
                }
            }

            if !refresh_needed {
                continue;
            }
            refresh_needed = false;

            let metadata_snapshot = metadata_updates.borrow().clone();
            let live_snapshot = live_updates.borrow().clone();
            if !matches!(live_snapshot.state(), SourceState::Ready)
                || !matches!(metadata_snapshot.state(), SourceState::Ready)
            {
                // Inputs that are not ready yet read as refreshing.
                let state = match SourceState::combine([
                    ("Git", live_snapshot.state()),
                    ("State", metadata_snapshot.state()),
                ]) {
                    SourceState::Empty => SourceState::Refreshing,
                    state => state,
                };
                // The last prepared history stays visible.
                publisher.set_state(state);
                continue;
            }
            let Some(live_rows) = live_snapshot.data() else {
                live.refresh();
                continue;
            };
            if metadata_snapshot.data().is_none() {
                metadata.refresh();
                continue;
            }

            let live_slugs = live_rows
                .iter()
                .filter(|row| !row.worktree.is_main)
                .map(|row| row.worktree.target.slug().to_owned())
                .collect::<BTreeSet<_>>();
            let observed = Stamp::now();
            let records = match read_records(&context, &live_slugs).await {
                Ok(records) => records,
                Err(error) => {
                    publisher.set_state(SourceState::Failed(format!("history: {error:#}").into()));
                    continue;
                }
            };
            last_prepared = prepare_ledger(&context, &records, &last_prepared);
            let records_seen = records.clone();
            publisher.publish_live(last_prepared.clone(), observed);
            if !is_active {
                continue;
            }
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
                    }
                    continue;
                }
                result = enrich(&context, records, &request_cancellation) => result,
            };
            match prepared {
                Ok((mut snapshot, fact_error)) if !cancellation.is_cancelled() => {
                    let proven =
                        keep_strongest_landing(&mut snapshot, &last_prepared, &records_seen);
                    if !proven.is_empty()
                        && let Err(error) = context
                            .database
                            .call(move |store| {
                                Ok(store.record_removed_production_landings(&proven)?)
                            })
                            .await
                    {
                        tracing::warn!(%error, "could not record production landings");
                    }
                    last_prepared = snapshot.clone();
                    publisher.publish_live_with_state(
                        snapshot,
                        observed,
                        fact_error
                            .map(|error| SourceState::Failed(error.into()))
                            .unwrap_or(SourceState::Ready),
                    );
                }
                Ok(_) => break,
                Err(_error) if cancellation.is_cancelled() => break,
                Err(error) => {
                    tracing::warn!(%error, "removed-worktree history refresh failed");
                    publisher.set_state(SourceState::Failed(format!("history: {error:#}").into()));
                }
            }
        }
    });

    HistorySources {
        snapshot: source.view(),
        commands: HistoryCommands {
            active: active_sender,
        },
    }
}

/// Attach prepared history to each host's board. Refreshes always read the
/// durable ledger; only an open pane requests external enrichment.
pub fn overlay(
    scope: &TaskScope,
    board: SourceHandle<Board>,
    history: SourceHandle<RemovedHistorySnapshot>,
) -> SourceHandle<Board> {
    let (output, mut publisher) = derived_channel(scope, "history-board");
    let cancellation = scope.token();
    scope.spawn(async move {
        let mut boards = board.subscribe();
        let mut histories = history.subscribe();
        boards.mark_changed();
        histories.mark_changed();
        let mut failures = wt_app_core::feeds::FailureClock::default();
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
            let mut data = base.data().map(|board| board.as_ref().clone());
            if let Some(board) = data.as_mut() {
                if let Some(history) = history_snapshot.data() {
                    board.removed_history = history.as_ref().clone();
                }
                if let SourceState::Failed(error) = history_snapshot.state() {
                    let detail = sanitize_terminal_text(&format!("History: {error}"));
                    if !board.attention.iter().any(|line| line.text == detail) {
                        failures.attention(board, "History", &detail);
                    }
                }
                failures.sweep();
            }
            publisher.publish_if_changed(
                base.with_data(data.map(Arc::new))
                    .with_input(&history_snapshot),
            );
        }
    });
    output
}

async fn read_records(
    context: &AppContext,
    live_slugs: &BTreeSet<String>,
) -> Result<Vec<RemovedWorktree>> {
    let records = context
        .database
        .call(|store| Ok(store.read_removed_worktrees()?))
        .await?;
    let cutoff = now_ms().saturating_sub(MAX_AGE.as_millis().min(i64::MAX as u128) as i64);
    Ok(filter_records(records, live_slugs, cutoff))
}

/// A landing never un-happens: keep a row's earlier, stronger answer when a
/// later pass falls back (a failed or skipped production check), so switching
/// rows never flips production back to base. Returns the production landings
/// the removal records do not hold yet, to persist.
fn keep_strongest_landing(
    snapshot: &mut RemovedHistorySnapshot,
    previous: &RemovedHistorySnapshot,
    records: &[RemovedWorktree],
) -> Vec<(String, String)> {
    let mut proven = Vec::new();
    for row in &mut snapshot.rows {
        let earlier = previous
            .rows
            .iter()
            .find(|old| old.key == row.key && old.removed_at == row.removed_at)
            .and_then(|old| old.landed_on);
        if earlier == Some(wt_tui::LandingKind::Production) {
            row.landed_on = earlier;
            row.production_landed = Some(true);
        } else if row.landed_on.is_none() {
            row.landed_on = earlier;
        }
        if row.landed_on == Some(wt_tui::LandingKind::Production)
            && let Some(record) = records
                .iter()
                .find(|record| record.slug == row.slug && record.removed_at == row.removed_at)
            && saved_landing(record) != Some(true)
        {
            proven.push((record.slug.clone(), record.removed_at.clone()));
        }
    }
    proven
}

/// Publish durable facts before any external I/O. Preserve enriched display
/// fields for the same removal record until the background read replaces them.
fn prepare_ledger(
    context: &AppContext,
    records: &[RemovedWorktree],
    previous: &RemovedHistorySnapshot,
) -> RemovedHistorySnapshot {
    let statuses = BTreeMap::new();
    let rows = records
        .iter()
        .cloned()
        .map(|entry| {
            let mut row = present(
                context
                    .config
                    .issue_tracker
                    .as_ref()
                    .and_then(|tracker| tracker.url_template.as_deref()),
                entry,
                &statuses,
                None,
                None,
            );
            if let Some(old) = previous
                .rows
                .iter()
                .find(|old| old.key == row.key && old.removed_at == row.removed_at)
            {
                row.issue_status = old.issue_status.clone();
                row.issue_url = row.issue_url.or_else(|| old.issue_url.clone());
                row.landed_on = old.landed_on.or(row.landed_on);
            }
            row
        })
        .collect();
    RemovedHistorySnapshot { rows }
}

async fn enrich(
    context: &AppContext,
    records: Vec<RemovedWorktree>,
    cancellation: &tokio_util::sync::CancellationToken,
) -> Result<(RemovedHistorySnapshot, Option<String>)> {
    let statuses = read_issue_statuses(context, &records, cancellation).await;
    let legacy_merges = read_legacy_pr_merges(context, &records, cancellation).await;
    let landing = read_landing_facts(context, &records, &legacy_merges, cancellation).await;
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
            let landed = landing.values.get(&entry.slug).copied().flatten();
            present(
                context
                    .config
                    .issue_tracker
                    .as_ref()
                    .and_then(|tracker| tracker.url_template.as_deref()),
                entry,
                &statuses,
                github_repo.as_deref(),
                landed,
            )
        })
        .collect();
    let error = landing
        .had_query_error
        .then(|| "Could not verify production landing for one or more removed worktrees".into());
    Ok((RemovedHistorySnapshot { rows }, error))
}

fn present(
    issue_url_template: Option<&str>,
    entry: RemovedWorktree,
    statuses: &BTreeMap<String, String>,
    github_repo: Option<&str>,
    landing_override: Option<bool>,
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
    if let Some(note) = entry.work.as_ref().and_then(|work| work.note.as_ref()) {
        details.push(sanitize_terminal_text(note));
    }
    let git_state = entry
        .extra
        .get("gitState")
        .and_then(Value::as_str)
        .map(sanitize_terminal_text);
    let pr_state = entry
        .extra
        .get("prState")
        .and_then(Value::as_str)
        .map(sanitize_terminal_text);
    let saved_landing = saved_landing(&entry);
    let production_landed = landing_override.or(saved_landing);
    let landed_on = production_landed.map(|production| {
        if production {
            wt_tui::LandingKind::Production
        } else {
            wt_tui::LandingKind::Base
        }
    });
    let issue_status = issue_id
        .as_ref()
        .and_then(|id| statuses.get(id))
        .map(|status| sanitize_terminal_text(status));
    let verify_owed = entry.work.as_ref().is_some_and(|work| {
        work.verify_after_merge.is_some() && work.state != "verified" && work.state != "dropped"
    });
    let day_label = removal_day_label(&entry.removed_at);
    let age = wt_core::work_age(&entry.removed_at, now_ms());
    RemovedHistoryRow {
        key: entry.slug.clone(),
        host: None,
        slug: sanitize_terminal_text(&entry.slug),
        branch: sanitize_terminal_text(&entry.branch),
        title: entry
            .extra
            .get("title")
            .and_then(Value::as_str)
            .map(sanitize_terminal_text)
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_else(|| sanitize_terminal_text(&entry.slug)),
        removed_at: entry.removed_at,
        details,
        work_state: entry
            .work
            .as_ref()
            .map(|work| sanitize_terminal_text(&work.state)),
        blocked_on: entry
            .work
            .as_ref()
            .and_then(|work| work.blocked_on.as_deref())
            .map(sanitize_terminal_text),
        verify_steps: entry
            .work
            .as_ref()
            .and_then(|work| work.verify_after_merge.as_deref())
            .map(sanitize_terminal_text),
        verify_owed,
        git_state,
        pr_state,
        issue_url,
        pr_url: entry
            .extra
            .get("prUrl")
            .and_then(Value::as_str)
            .map(sanitize_terminal_text),
        issue_status,
        production_landed,
        landed_on,
        day_label,
        age,
        automations_paused: entry.automations_paused.unwrap_or(false),
    }
}

fn removal_day_label(timestamp: &str) -> Option<String> {
    let timestamp_ms = u64::try_from(parse_timestamp_ms(timestamp)?).ok()?;
    let day = local_removal_day(timestamp_ms)?;
    let today = local_removal_day(u64::try_from(now_ms()).ok()?)?;
    if day == today {
        return Some("today".into());
    }
    if today.previous_day() == Some(day) {
        return Some("yesterday".into());
    }
    let weekday = match day.weekday() {
        time::Weekday::Monday => "Mon",
        time::Weekday::Tuesday => "Tue",
        time::Weekday::Wednesday => "Wed",
        time::Weekday::Thursday => "Thu",
        time::Weekday::Friday => "Fri",
        time::Weekday::Saturday => "Sat",
        time::Weekday::Sunday => "Sun",
    };
    let month = match day.month() {
        time::Month::January => "Jan",
        time::Month::February => "Feb",
        time::Month::March => "Mar",
        time::Month::April => "Apr",
        time::Month::May => "May",
        time::Month::June => "Jun",
        time::Month::July => "Jul",
        time::Month::August => "Aug",
        time::Month::September => "Sep",
        time::Month::October => "Oct",
        time::Month::November => "Nov",
        time::Month::December => "Dec",
    };
    Some(format!("{weekday} {} {month}", day.day()))
}

fn local_removal_day(timestamp_ms: u64) -> Option<time::Date> {
    let (year, month, day, hour, _, _) =
        wt_app_core::display_time::local_time_components(timestamp_ms)?;
    let month = time::Month::try_from(month).ok()?;
    let mut date = time::Date::from_calendar_date(year, month, day).ok()?;
    if hour < 4 {
        date = date.previous_day()?;
    }
    Some(date)
}

async fn read_legacy_pr_merges(
    context: &AppContext,
    records: &[RemovedWorktree],
    cancellation: &tokio_util::sync::CancellationToken,
) -> BTreeMap<u64, String> {
    let wanted = records
        .iter()
        .filter(|entry| entry.extra.get("landedOnAtRemoval").is_none())
        .filter(|entry| entry.extra.get("gitState").and_then(Value::as_str) == Some("merged"))
        .filter_map(|entry| entry.extra.get("prNumber").and_then(Value::as_u64))
        .collect::<HashSet<_>>();
    if wanted.is_empty() || cancellation.is_cancelled() {
        return BTreeMap::new();
    }
    let mut spec = CommandSpec::new("git").args([
        "log",
        "--first-parent",
        "--format=%H%x00%s",
        &context.config.branch.base,
    ]);
    spec.cwd = Some(context.config.paths.main_clone.clone());
    spec.env = git_read_env();
    spec.timeout = Duration::from_secs(10);
    spec.output_limit = 2 * 1024 * 1024;
    let Ok(output) = context.processes.run(spec, cancellation).await else {
        return BTreeMap::new();
    };
    if !output.status.success() || output.stdout_truncated {
        return BTreeMap::new();
    }
    parse_legacy_pr_merges(&output.stdout, &wanted)
}

fn parse_legacy_pr_merges(bytes: &[u8], wanted: &HashSet<u64>) -> BTreeMap<u64, String> {
    let text = String::from_utf8_lossy(bytes);
    let mut found = BTreeMap::new();
    for line in text.lines() {
        let Some((sha, subject)) = line.split_once('\0') else {
            continue;
        };
        if !(40..=64).contains(&sha.len()) || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        let Some(tail) = subject.strip_prefix("Merge pull request #") else {
            continue;
        };
        let digits = tail.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0
            || !tail[digits..]
                .chars()
                .next()
                .is_none_or(char::is_whitespace)
        {
            continue;
        }
        let Ok(number) = tail[..digits].parse::<u64>() else {
            continue;
        };
        if wanted.contains(&number) {
            found.entry(number).or_insert_with(|| sha.to_owned());
        }
    }
    found
}

fn classify_landing(
    saved: Option<bool>,
    merged_on_base: bool,
    on_production: Option<bool>,
) -> Option<bool> {
    if on_production == Some(true) {
        Some(true)
    } else {
        saved.or_else(|| merged_on_base.then_some(false))
    }
}

/// Where a removal's work had landed as last recorded: its state at removal,
/// upgraded to production once a later check proved it (`landedOn`). A
/// landing never un-happens, so the stronger record wins.
fn saved_landing(entry: &RemovedWorktree) -> Option<bool> {
    if entry.extra.get(LANDED_ON).and_then(Value::as_str) == Some("production") {
        return Some(true);
    }
    match entry.extra.get("landedOnAtRemoval").and_then(Value::as_str) {
        Some("production") => Some(true),
        Some("base") => Some(false),
        _ => None,
    }
}

/// The removal record field holding a production landing proven after
/// removal, so reopening history starts from it instead of re-proving it.
const LANDED_ON: &str = "landedOn";

fn has_durable_landing_proof(landing_marker: Option<&str>, git_state: Option<&str>) -> bool {
    match landing_marker {
        Some("base" | "production") => true,
        Some(_) => false,
        None => git_state == Some("merged"),
    }
}

async fn read_landing_facts(
    context: &AppContext,
    records: &[RemovedWorktree],
    legacy_merges: &BTreeMap<u64, String>,
    cancellation: &tokio_util::sync::CancellationToken,
) -> LandingFacts {
    let production = context.config.branch.production.as_deref();
    let base = context.config.branch.base.as_str();
    let candidates: Vec<(String, Option<bool>, bool, Option<String>)> = records
        .iter()
        .map(|entry| {
            let saved = saved_landing(entry);
            let durable_landing_proof = has_durable_landing_proof(
                entry.extra.get("landedOnAtRemoval").and_then(Value::as_str),
                entry.extra.get("gitState").and_then(Value::as_str),
            );
            let merged_on_base = durable_landing_proof;
            let merge_oid = durable_landing_proof
                .then(|| {
                    entry
                        .extra
                        .get("prMergeCommitOid")
                        .and_then(Value::as_str)
                        .or_else(|| {
                            entry
                                .extra
                                .get("prNumber")
                                .and_then(Value::as_u64)
                                .and_then(|number| legacy_merges.get(&number).map(String::as_str))
                        })
                })
                .flatten();
            (
                entry.slug.clone(),
                saved,
                merged_on_base,
                merge_oid.map(str::to_owned),
            )
        })
        .collect::<Vec<_>>();
    let Some(production) = production else {
        return LandingFacts {
            values: candidates
                .into_iter()
                .map(|(slug, saved, merged, _)| {
                    let result = saved.or_else(|| merged.then_some(false));
                    (slug, result)
                })
                .collect(),
            had_query_error: false,
        };
    };
    if production == base {
        return LandingFacts {
            values: candidates
                .into_iter()
                .map(|(slug, saved, merged, _)| {
                    let result = (saved.is_some() || merged).then_some(true);
                    (slug, result)
                })
                .collect(),
            had_query_error: false,
        };
    }

    let mut queries = candidates
        .iter()
        .filter(|(_, saved, _, merge_oid)| *saved != Some(true) && merge_oid.is_some())
        .cloned()
        .collect::<Vec<_>>();
    let main_clone = context.config.paths.main_clone.clone();
    let processes = context.processes.clone();
    let production = production.to_owned();
    let cancellation_child = cancellation.child_token();
    let checks = stream::iter(queries.drain(..))
        .map(|(slug, saved, merged, merge_oid)| {
            let context_path = main_clone.clone();
            let runner = processes.clone();
            let production = production.clone();
            let cancellation = cancellation_child.child_token();
            async move {
                let Some(merge_oid) = merge_oid else {
                    return (slug, saved.or_else(|| merged.then_some(false)), false);
                };
                let mut spec = CommandSpec::new("git").args([
                    "merge-base",
                    "--is-ancestor",
                    &merge_oid,
                    &production,
                ]);
                spec.cwd = Some(context_path);
                spec.env = git_read_env();
                spec.timeout = Duration::from_secs(8);
                spec.output_limit = 16 * 1024;
                let (landing, failed) = match runner.run(spec, &cancellation).await {
                    Ok(output) => classify_production_exit(saved, merged, output.status.code()),
                    Err(_) => classify_production_exit(saved, merged, None),
                };
                (slug, landing, failed)
            }
        })
        .buffer_unordered(4)
        .collect::<Vec<_>>()
        .await;
    let checks = checks.into_iter().collect::<Vec<_>>();
    let had_query_error = checks.iter().any(|(_, _, failed)| *failed);
    let mut result = checks
        .into_iter()
        .map(|(slug, value, _)| (slug, value))
        .collect::<HashMap<_, _>>();
    for (slug, saved, merged, merge_oid) in candidates {
        result.entry(slug).or_insert_with(|| {
            if saved == Some(true) {
                Some(true)
            } else if merge_oid.is_none() && merged {
                Some(false)
            } else {
                saved
            }
        });
    }
    LandingFacts {
        values: result,
        had_query_error,
    }
}

struct LandingFacts {
    values: HashMap<String, Option<bool>>,
    had_query_error: bool,
}

fn classify_production_exit(
    saved: Option<bool>,
    merged_on_base: bool,
    exit_code: Option<i32>,
) -> (Option<bool>, bool) {
    match exit_code {
        Some(0) => (classify_landing(saved, merged_on_base, Some(true)), false),
        Some(1) => (classify_landing(saved, merged_on_base, Some(false)), false),
        _ => (classify_landing(saved, merged_on_base, None), true),
    }
}

fn git_read_env() -> Vec<(std::ffi::OsString, Option<std::ffi::OsString>)> {
    vec![
        ("GIT_OPTIONAL_LOCKS".into(), Some("0".into())),
        ("GIT_TERMINAL_PROMPT".into(), Some("0".into())),
        ("LC_ALL".into(), Some("C".into())),
    ]
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
            tracing::warn!(
                status = %output.status,
                stderr = %output.stderr_text().trim(),
                "removed-history issue status reader failed"
            );
            return BTreeMap::new();
        }
        Err(error) => {
            if !cancellation.is_cancelled() {
                tracing::warn!(%error, "removed-history issue status reader failed");
            }
            return BTreeMap::new();
        }
    };
    let Some(statuses) = parse_issue_statuses(&output.stdout, &ids) else {
        tracing::warn!("removed-history issue status reader returned invalid JSON");
        return BTreeMap::new();
    };
    statuses
}

fn parse_issue_statuses(stdout: &[u8], ids: &BTreeSet<String>) -> Option<BTreeMap<String, String>> {
    // Readers may add fields (a title, a URL); only these two are needed.
    #[derive(Deserialize)]
    struct Response {
        issues: Vec<Issue>,
    }
    #[derive(Deserialize)]
    struct Issue {
        id: String,
        status: String,
    }
    let response = serde_json::from_slice::<Response>(stdout).ok()?;
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
    Some(statuses)
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
    wt_ops::commands::issue::repo_web_url(output.stdout_text().trim())
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
    use wt_runtime::leaf_channel;

    fn removed(slug: &str, landed: &str) -> RemovedWorktree {
        let mut extra = Map::new();
        extra.insert("landedOnAtRemoval".into(), json!(landed));
        RemovedWorktree {
            slug: slug.into(),
            branch: format!("m/{slug}"),
            removed_at: "2026-10-09T10:00:00Z".into(),
            work: None,
            automations_paused: None,
            extra,
        }
    }

    #[test]
    fn issue_statuses_ignore_fields_the_reader_adds() {
        let ids = BTreeSet::from(["COZ-1".to_owned()]);
        let stdout = br#"{"issues":[{"id":"COZ-1","status":"In Review","title":"t"},{"id":"COZ-9","status":"Done"}]}"#;
        assert_eq!(
            parse_issue_statuses(stdout, &ids),
            Some(BTreeMap::from([("COZ-1".into(), "In Review".into())]))
        );
        assert_eq!(parse_issue_statuses(b"nope", &ids), None);
    }

    #[test]
    fn a_production_landing_is_kept_and_recorded_once_proven() {
        let records = vec![removed("one", "base")];
        let row = |landing| {
            let mut row = present(None, records[0].clone(), &BTreeMap::new(), None, landing);
            row.removed_at = records[0].removed_at.clone();
            row
        };
        // The check proved production: kept, and owed to the record.
        let mut snapshot = RemovedHistorySnapshot {
            rows: vec![row(Some(true))],
        };
        let proven =
            keep_strongest_landing(&mut snapshot, &RemovedHistorySnapshot::default(), &records);
        assert_eq!(proven, [("one".to_owned(), records[0].removed_at.clone())]);
        // A later pass that fell back to the saved base answer does not
        // flip the row back.
        let previous = snapshot;
        let mut fallback = RemovedHistorySnapshot {
            rows: vec![row(None)],
        };
        assert_eq!(fallback.rows[0].landed_on, Some(wt_tui::LandingKind::Base));
        keep_strongest_landing(&mut fallback, &previous, &records);
        assert_eq!(
            fallback.rows[0].landed_on,
            Some(wt_tui::LandingKind::Production)
        );
        // Once recorded, a fresh read starts at production and owes nothing.
        let mut recorded = records[0].clone();
        recorded.extra.insert(LANDED_ON.into(), json!("production"));
        assert_eq!(saved_landing(&recorded), Some(true));
        let fresh = present(None, recorded.clone(), &BTreeMap::new(), None, None);
        assert_eq!(fresh.landed_on, Some(wt_tui::LandingKind::Production));
        let mut again = RemovedHistorySnapshot { rows: vec![fresh] };
        assert!(keep_strongest_landing(&mut again, &previous, &[recorded]).is_empty());
    }

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
            None,
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
        assert_eq!(row.verify_steps.as_deref(), Some("check production"));
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
            None,
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
    fn legacy_merge_scan_requires_exact_pr_number_and_keeps_newest_match() {
        let wanted = HashSet::from([12, 123]);
        let rows = parse_legacy_pr_merges(
            b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\0Merge pull request #12 from x\n\
              bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\0Merge pull request #123x bad\n\
              cccccccccccccccccccccccccccccccccccccccc\0Merge pull request #12 again\n\
              dddddddddddddddddddddddddddddddddddddddd\0Merge pull request #123 from valid\n",
            &wanted,
        );
        assert_eq!(
            rows.get(&12).map(String::as_str),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert_eq!(
            rows.get(&123).map(String::as_str),
            Some("dddddddddddddddddddddddddddddddddddddddd")
        );
    }

    #[test]
    fn production_query_does_not_invent_base_landing() {
        assert_eq!(classify_landing(None, false, Some(false)), None);
        assert_eq!(classify_landing(None, true, Some(false)), Some(false));
        assert_eq!(classify_landing(Some(false), false, Some(true)), Some(true));
        assert_eq!(classify_landing(Some(false), false, None), Some(false));
    }

    #[test]
    fn production_query_only_exit_one_means_negative_and_other_errors_surface() {
        assert_eq!(
            classify_production_exit(None, false, Some(0)),
            (Some(true), false)
        );
        assert_eq!(
            classify_production_exit(None, false, Some(1)),
            (None, false)
        );
        assert_eq!(
            classify_production_exit(None, false, Some(128)),
            (None, true)
        );
        assert_eq!(
            classify_production_exit(Some(false), true, Some(128)),
            (Some(false), true)
        );
        assert_eq!(
            classify_production_exit(None, true, None),
            (Some(false), true)
        );
    }

    #[test]
    fn legacy_pr_identity_without_landing_proof_stays_unknown() {
        assert!(!has_durable_landing_proof(None, Some("open")));
        assert!(!has_durable_landing_proof(None, None));
        assert!(has_durable_landing_proof(None, Some("merged")));
        assert!(!has_durable_landing_proof(Some("unlanded"), Some("merged")));
        assert!(has_durable_landing_proof(Some("base"), None));
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
    async fn history_is_prepared_before_open_and_survives_close_and_refresh() {
        let mut fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let started = scratch.path().join("tracker-started");
        Arc::make_mut(&mut fixture.ctx.config).issue_tracker =
            Some(wt_config::IssueTrackerConfig {
                prefix: Some("coz".into()),
                status_command: Some(vec![
                    "sh".into(),
                    "-c".into(),
                    "touch \"$1\"; sleep 30".into(),
                    "history-test".into(),
                    started.to_string_lossy().into_owned(),
                ]),
                ..Default::default()
            });
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
                        extra: Map::from_iter([(
                            "issueId".into(),
                            Value::String("coz-123".into()),
                        )]),
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
        let scope = TaskScope::new();
        fn spec<T>(domain: Domain) -> SourceSpec<T> {
            SourceSpec {
                name: "test",
                domain,
                persistence: Persistence::Never("test"),
                policy: RefreshPolicy::default(),
            }
        }
        let (metadata, metadata_publisher) = leaf_channel(&scope, spec(Domain::Metadata));
        let (live, live_publisher) = leaf_channel(&scope, spec(Domain::Git));
        metadata_publisher.publish_live(metadata_value, Stamp::now());
        live_publisher.publish_live(inventory, Stamp::now());

        let history = start(&scope, &fixture.ctx, metadata.view(), live.view());
        assert!(history.snapshot.snapshot().data().is_none());
        let mut updates = history.snapshot.subscribe();
        let snapshot = tokio::time::timeout(
            Duration::from_secs(2),
            updates.wait_for(|snapshot| {
                snapshot
                    .data()
                    .is_some_and(|data| data.rows.iter().any(|row| row.slug == "historical"))
            }),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(snapshot.domains().contains_key(&Domain::History));
        let rows = snapshot.data().unwrap().rows.clone();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].slug, "historical");
        drop(snapshot);

        assert!(
            !started.exists(),
            "external readers remain dormant while closed"
        );
        history.commands.set_active(true);
        tokio::time::timeout(Duration::from_secs(2), async {
            while !started.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            history.snapshot.snapshot().data().unwrap().rows,
            rows,
            "slow tracker enrichment cannot hide prepared rows"
        );

        history.commands.set_active(false);
        live_publisher.set_state(SourceState::Refreshing);
        tokio::time::timeout(
            Duration::from_secs(2),
            updates.wait_for(|snapshot| matches!(snapshot.state(), SourceState::Refreshing)),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            history.snapshot.snapshot().data().unwrap().rows,
            rows,
            "closing history retains its prepared rows during input refresh"
        );
        history.commands.set_active(true);
        tokio::task::yield_now().await;
        assert_eq!(
            history.snapshot.snapshot().data().unwrap().rows,
            rows,
            "reopening uses existing rows immediately"
        );
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
        fixture.close().await.unwrap();
    }
}
