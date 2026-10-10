//! Durable action metadata and bounded log tails drive the action pane.
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use notify::{RecursiveMode, Watcher};
use wt_actions::{ActionRun, ActionRunStatus, ActionService};
use wt_config::EffectTag;
use wt_platform::process::ProcessStream;
use wt_runtime::{
    RefreshPolicy, SourceHandle, SourceSnapshot, SourceState, TaskScope, source_channel,
    start_source,
};
use wt_tui::{Board, LogView};

use crate::{context::AppContext, issue_source::StatusBatch};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ActionHistory {
    runs: Vec<ActionRun>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ActionLogs {
    logs: BTreeMap<String, LogView>,
}

#[derive(Default)]
struct TailCursor {
    stdout: Option<u64>,
    stderr: Option<u64>,
    lines: VecDeque<String>,
    pending_stdout: String,
    pending_stderr: String,
}

#[derive(Default)]
struct TailCursors {
    by_run: BTreeMap<String, TailCursor>,
}

const TERMINAL_HISTORY_LIMIT: usize = 120;
const MAX_TAILS: usize = 32;
const TAIL_LINES: usize = 100;
const TAIL_SEED_BYTES: usize = 64 * 1024;
const TAIL_DELTA_BYTES: usize = 32 * 1024;

pub struct RefreshTargets {
    pub git: SourceHandle<Vec<wt_vcs::WorktreeSnapshot>>,
    pub github: SourceHandle<wt_github::GithubData>,
    pub dev: Option<SourceHandle<Vec<wt_dev::DevStatusRow>>>,
    pub origin: Option<SourceHandle<wt_vcs::FetchOriginReport>>,
    pub issues: SourceHandle<StatusBatch>,
}

impl RefreshTargets {
    fn completed(&self, run: &ActionRun) {
        for effect in &run.meta.affects {
            match effect {
                EffectTag::Git => {
                    self.git.refresh();
                    if let Some(origin) = &self.origin {
                        origin.refresh();
                    }
                }
                EffectTag::Github => {
                    self.github.refresh();
                }
                EffectTag::Dev => {
                    if let Some(dev) = &self.dev {
                        dev.refresh();
                    }
                }
                EffectTag::Issue => {
                    self.issues.refresh();
                }
            }
        }
        if run.meta.issue_status.is_some() {
            self.issues.refresh();
        }
    }
}

pub fn overlay(
    scope: &TaskScope,
    context: &AppContext,
    board: SourceHandle<Board>,
    targets: RefreshTargets,
) -> SourceHandle<Board> {
    let service = crate::actions::service(context).map_err(|error| error.to_string());
    let run_service = service.clone();
    let reconciled = Arc::new(AtomicBool::new(false));
    let run_reconciled = reconciled.clone();
    let runs = start_source(
        scope,
        RefreshPolicy {
            debounce: Duration::from_millis(50),
            minimum_interval: Duration::from_millis(250),
        },
        move |cancel| {
            let service = run_service.clone();
            let reconciled = run_reconciled.clone();
            async move {
                let service = service.map_err(anyhow::Error::msg)?;
                if !reconciled.swap(true, Ordering::AcqRel)
                    && let Err(error) = service.reconcile_runs(&cancel).await
                {
                    reconciled.store(false, Ordering::Release);
                    return Err(error.into());
                }
                Ok::<_, anyhow::Error>(ActionHistory {
                    runs: service.list_runs_bounded(TERMINAL_HISTORY_LIMIT).await?,
                })
            }
        },
    );
    let cursors = Arc::new(tokio::sync::Mutex::new(TailCursors::default()));
    let log_runs = runs.clone();
    let log_service = service.clone();
    let log_cursors = cursors.clone();
    let logs = start_source(
        scope,
        RefreshPolicy {
            debounce: Duration::from_millis(30),
            minimum_interval: Duration::from_millis(250),
        },
        move |_| {
            let service = log_service.clone();
            let runs = log_runs.subscribe().borrow().clone();
            let cursors = log_cursors.clone();
            async move {
                let service = service.map_err(anyhow::Error::msg)?;
                load_logs(&service, runs.data.as_deref(), &cursors).await
            }
        },
    );
    watch(
        scope,
        context.config.paths.log_dir.join("actions"),
        runs.clone(),
        logs.clone(),
    );
    project(scope, board, runs, logs, targets)
}

fn watch(
    scope: &TaskScope,
    directory: PathBuf,
    runs: SourceHandle<ActionHistory>,
    logs: SourceHandle<ActionLogs>,
) {
    let cancel = scope.token();
    scope.spawn(async move {
        let run_callback = runs.clone();
        let log_callback = logs.clone();
        let watcher = tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&directory)?;
            let mut watcher = notify::recommended_watcher(
                move |event: notify::Result<notify::Event>| match event {
                    Ok(event) if !matches!(event.kind, notify::EventKind::Access(_)) => {
                        for path in event.paths {
                            match path.file_name().and_then(|name| name.to_str()) {
                                Some("meta.json" | "done.json") => {
                                    run_callback.refresh();
                                }
                                Some("stream.log" | "stderr.log") => {
                                    log_callback.refresh();
                                }
                                _ => {}
                            }
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "action notifications failed");
                        run_callback.refresh();
                        log_callback.refresh();
                    }
                    _ => {}
                },
            )?;
            watcher.watch(&directory, RecursiveMode::Recursive)?;
            Ok::<_, anyhow::Error>(watcher)
        })
        .await;
        if !matches!(watcher, Ok(Ok(_))) {
            tracing::warn!(?watcher, "action history uses refresh backstop");
        }
        runs.refresh();
        logs.refresh();
        let mut run_updates = runs.subscribe();
        run_updates.mark_changed();
        let period = Duration::from_secs(1);
        let mut active_tick =
            tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        active_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut active = has_active_runs(&run_updates.borrow());
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = active_tick.tick(), if active => { logs.refresh(); }
                changed = run_updates.changed() => {
                    if changed.is_err() { break; }
                    active = has_active_runs(&run_updates.borrow_and_update());
                    logs.refresh();
                }
            }
        }
        drop(watcher);
    });
}

async fn load_logs(
    service: &ActionService,
    history: Option<&ActionHistory>,
    cursors: &tokio::sync::Mutex<TailCursors>,
) -> anyhow::Result<ActionLogs> {
    let Some(history) = history else {
        return Ok(ActionLogs::default());
    };
    let mut selected = BTreeSet::new();
    let mut latest_slots = BTreeSet::new();
    let mut tail_count = 0usize;
    for run in &history.runs {
        if matches!(
            run.meta.status,
            ActionRunStatus::Running | ActionRunStatus::Ambiguous
        ) {
            if tail_count < MAX_TAILS {
                selected.insert(run.meta.run_id.clone());
                tail_count += 1;
            }
            continue;
        }
        let key = if run.meta.action_key.is_empty() {
            &run.meta.slug
        } else {
            &run.meta.action_key
        };
        if latest_slots.insert(key.clone()) && tail_count < MAX_TAILS {
            selected.insert(run.meta.run_id.clone());
            tail_count += 1;
        }
    }
    let mut cursors = cursors.lock().await;
    cursors.by_run.retain(|run_id, _| selected.contains(run_id));
    let mut logs = BTreeMap::new();
    for run in &history.runs {
        if !selected.contains(&run.meta.run_id) {
            continue;
        }
        let cursor = cursors.by_run.entry(run.meta.run_id.clone()).or_default();
        for (stream, label, offset, pending) in [
            (
                ProcessStream::Stdout,
                "out",
                &mut cursor.stdout,
                &mut cursor.pending_stdout,
            ),
            (
                ProcessStream::Stderr,
                "err",
                &mut cursor.stderr,
                &mut cursor.pending_stderr,
            ),
        ] {
            let limit = if offset.is_none() {
                TAIL_SEED_BYTES
            } else {
                TAIL_DELTA_BYTES
            };
            let Ok((next, bytes)) = service
                .read_log_chunk(&run.meta.run_id, stream, *offset, limit)
                .await
            else {
                continue;
            };
            *offset = Some(next);
            append_log_lines(&mut cursor.lines, pending, label, &bytes);
        }
        if !matches!(
            run.meta.status,
            ActionRunStatus::Running | ActionRunStatus::Ambiguous
        ) {
            flush_pending(&mut cursor.lines, &mut cursor.pending_stdout, "out");
            flush_pending(&mut cursor.lines, &mut cursor.pending_stderr, "err");
        }
        logs.insert(
            run.meta.run_id.clone(),
            LogView {
                id: run.meta.run_id.clone(),
                title: wt_core::sanitize_terminal_text(&run.meta.action_name),
                lines: cursor.lines.iter().cloned().collect(),
            },
        );
    }
    Ok(ActionLogs { logs })
}

fn append_log_lines(lines: &mut VecDeque<String>, pending: &mut String, label: &str, bytes: &[u8]) {
    pending.push_str(&String::from_utf8_lossy(bytes));
    let mut complete = Vec::new();
    let mut start = 0;
    for (index, character) in pending.char_indices() {
        if character == '\n' {
            complete.push(pending[start..index].trim_end_matches('\r').to_owned());
            start = index + character.len_utf8();
        }
    }
    if start > 0 {
        pending.drain(..start);
    }
    for line in complete {
        push_tail_line(lines, label, line);
    }
    if pending.len() > 16 * 1024 {
        let mut start = pending.len() - 16 * 1024;
        while !pending.is_char_boundary(start) {
            start += 1;
        }
        pending.drain(..start);
    }
}

fn flush_pending(lines: &mut VecDeque<String>, pending: &mut String, label: &str) {
    if !pending.is_empty() {
        push_tail_line(lines, label, std::mem::take(pending));
    }
}

fn push_tail_line(lines: &mut VecDeque<String>, label: &str, line: String) {
    lines.push_back(wt_core::sanitize_terminal_text(&format!(
        "[{label}] {line}"
    )));
    while lines.len() > TAIL_LINES {
        lines.pop_front();
    }
}

fn has_active_runs(history: &SourceSnapshot<ActionHistory>) -> bool {
    history.data.as_ref().is_some_and(|history| {
        history.runs.iter().any(|run| {
            matches!(
                run.meta.status,
                ActionRunStatus::Running | ActionRunStatus::Ambiguous
            )
        })
    })
}

fn project(
    scope: &TaskScope,
    board: SourceHandle<Board>,
    runs: SourceHandle<ActionHistory>,
    logs: SourceHandle<ActionLogs>,
    targets: RefreshTargets,
) -> SourceHandle<Board> {
    let (output, mut publisher) = source_channel();
    let cancel = scope.token();
    scope.spawn(async move {
        let mut boards = board.subscribe();
        let mut updates = runs.subscribe();
        let mut log_updates = logs.subscribe();
        let mut statuses = targets.issues.subscribe();
        boards.mark_changed(); updates.mark_changed(); log_updates.mark_changed(); statuses.mark_changed();
        let mut tracker = Tracker::new(epoch_ms());
        let mut last_runs = None;
        let mut last_error = None;
        loop {
            let next_expiry = tracker.next_expiry();
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                request = publisher.requested() => {
                    if request.is_none() { break; }
                    board.refresh(); runs.refresh(); logs.refresh(); continue;
                }
                changed = boards.changed() => {
                    if changed.is_err() { break; }
                    boards.borrow_and_update();
                }
                changed = updates.changed() => {
                    if changed.is_err() { break; }
                    let snapshot = updates.borrow_and_update().clone();
                    let error = match &snapshot.state {SourceState::Failed(error) => Some(error.clone()), _ => None};
                    if snapshot.data == last_runs && error == last_error { continue; }
                    last_runs = snapshot.data.clone(); last_error = error;
                    if let Some(history) = &snapshot.data {
                        for run in tracker.observe(&history.runs, tokio::time::Instant::now()) {
                            targets.completed(run);
                            tracing::info!(slug = run.meta.slug, action = run.meta.action_name, status = ?run.meta.status, "action completed");
                        }
                    }
                }
                changed = log_updates.changed() => {
                    if changed.is_err() { break; }
                    log_updates.borrow_and_update();
                }
                changed = statuses.changed() => {
                    if changed.is_err() { break; }
                    statuses.borrow_and_update();
                }
                _ = async { match next_expiry {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending::<()>().await,
                }} => {}
            }
            tracker.settle(&statuses.borrow(), tokio::time::Instant::now());
            publisher.publish(compose(
                boards.borrow().clone(),
                updates.borrow().clone(),
                log_updates.borrow().clone(),
                &tracker,
            ));
        }
    });
    output
}

struct Expected {
    run_id: String,
    status: String,
    completed: Option<tokio::time::Instant>,
}

struct Tracker {
    boot_ms: u64,
    handled: BTreeSet<String>,
    seen: BTreeSet<String>,
    expected: BTreeMap<String, Expected>,
    settled: BTreeSet<String>,
    attention: VecDeque<wt_tui::AttentionLine>,
}

impl Tracker {
    fn new(boot_ms: u64) -> Self {
        Self {
            boot_ms,
            handled: BTreeSet::new(),
            seen: BTreeSet::new(),
            expected: BTreeMap::new(),
            settled: BTreeSet::new(),
            attention: VecDeque::new(),
        }
    }

    fn observe<'a>(
        &mut self,
        runs: &'a [ActionRun],
        now: tokio::time::Instant,
    ) -> Vec<&'a ActionRun> {
        let mut completed = Vec::new();
        // Input is newest first. A failed newer command must suppress an older
        // successful command's pending projection for the same issue.
        let mut latest_issue = BTreeSet::new();
        for run in runs {
            let meta = &run.meta;
            let terminal = !matches!(
                meta.status,
                ActionRunStatus::Running | ActionRunStatus::Ambiguous
            );
            let current = self.seen.contains(&meta.run_id) || meta.started_at >= self.boot_ms;
            if terminal && self.handled.insert(meta.run_id.clone()) && current {
                let label = match meta.status {
                    ActionRunStatus::Succeeded => "succeeded",
                    ActionRunStatus::Failed => "failed",
                    ActionRunStatus::Killed => "killed",
                    ActionRunStatus::Running | ActionRunStatus::Ambiguous => "uncertain",
                };
                let detail = meta
                    .extra
                    .get("error")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let line = format!(
                    "Action {label}: {} / {}{}",
                    meta.slug,
                    meta.action_name,
                    if detail.is_empty() {
                        String::new()
                    } else {
                        format!(": {detail}")
                    }
                );
                let text = wt_core::sanitize_terminal_text(&line);
                let at_ms = epoch_ms();
                let source = wt_core::sanitize_terminal_text(&meta.slug);
                tracing::info!(target: "wt_attention", event_at_ms = at_ms, event_channel = "attention", event_source = %source, event_text = %text, "attention event");
                self.attention.push_back(wt_tui::AttentionLine {
                    at_ms,
                    source,
                    text,
                });
                while self.attention.len() > 10 {
                    self.attention.pop_front();
                }
                completed.push(run);
            }
            self.seen.insert(meta.run_id.clone());
            let Some(expectation) = &meta.issue_status else {
                continue;
            };
            if !latest_issue.insert(expectation.issue_id.clone()) {
                continue;
            }
            if self.settled.contains(&meta.run_id) {
                continue;
            }
            let valid = meta.status == ActionRunStatus::Running
                || (meta.status == ActionRunStatus::Succeeded && current);
            if !valid {
                self.expected.remove(&expectation.issue_id);
                self.settled.insert(meta.run_id.clone());
                continue;
            }
            let entry = self
                .expected
                .entry(expectation.issue_id.clone())
                .or_insert_with(|| Expected {
                    run_id: meta.run_id.clone(),
                    status: expectation.status.clone(),
                    completed: None,
                });
            if entry.run_id != meta.run_id {
                *entry = Expected {
                    run_id: meta.run_id.clone(),
                    status: expectation.status.clone(),
                    completed: None,
                };
            }
            if terminal && entry.completed.is_none() {
                entry.completed = Some(now);
            }
        }
        completed
    }

    fn settle(&mut self, statuses: &SourceSnapshot<StatusBatch>, now: tokio::time::Instant) {
        self.expected.retain(|id, expected| {
            let Some(completed) = expected.completed else {
                return true;
            };
            let caught_up = matches!(statuses.state, SourceState::Ready)
                && statuses.updated_at.is_some_and(|at| at >= completed)
                && statuses
                    .data
                    .as_ref()
                    .is_some_and(|batch| batch.statuses.get(id) == Some(&expected.status));
            let keep = !caught_up && now < completed + Duration::from_secs(12);
            if !keep {
                self.settled.insert(expected.run_id.clone());
            }
            keep
        });
    }

    fn next_expiry(&self) -> Option<tokio::time::Instant> {
        self.expected
            .values()
            .filter_map(|e| e.completed.map(|at| at + Duration::from_secs(12)))
            .min()
    }
}

fn compose(
    mut board: SourceSnapshot<Board>,
    runs: SourceSnapshot<ActionHistory>,
    logs: SourceSnapshot<ActionLogs>,
    tracker: &Tracker,
) -> SourceSnapshot<Board> {
    let Some(data) = &board.data else {
        return board;
    };
    let mut data = data.as_ref().clone();
    if let SourceState::Failed(error) = &runs.state {
        crate::activity_source::append_attention(
            &mut data,
            "Action history",
            &wt_core::sanitize_terminal_text(error),
        );
    }
    data.slot_logs.clear();
    for row in &mut data.rows {
        row.logs.clear();
        if let Some(expected) = row
            .issue_id
            .as_ref()
            .and_then(|id| tracker.expected.get(id))
        {
            row.issue_status = Some(expected.status.clone());
        }
        let history = runs.data.as_deref();
        let latest = history
            .into_iter()
            .flat_map(|history| history.runs.iter())
            .find(|run| {
                run.meta
                    .worktree_ref
                    .as_ref()
                    .map(wt_core::worktree_ledger_key)
                    .unwrap_or_else(|| run.meta.slug.clone())
                    == row.key
            });
        if let Some(run) = latest {
            let label = match run.meta.status {
                ActionRunStatus::Running => "running",
                ActionRunStatus::Ambiguous => "start uncertain",
                ActionRunStatus::Succeeded => "succeeded",
                ActionRunStatus::Failed => "failed",
                ActionRunStatus::Killed => "killed",
            };
            row.details.push(format!(
                "Action: {} ({label})",
                wt_core::sanitize_terminal_text(&run.meta.action_name)
            ));
            if matches!(
                run.meta.status,
                ActionRunStatus::Running | ActionRunStatus::Ambiguous
            ) {
                row.badge.push_str(&format!(" · action {label}"));
            }
            if matches!(
                run.meta.status,
                ActionRunStatus::Failed | ActionRunStatus::Ambiguous
            ) {
                row.needs_attention = true;
                if let Some(error) = run
                    .meta
                    .extra
                    .get("error")
                    .and_then(serde_json::Value::as_str)
                {
                    row.details.push(wt_core::sanitize_terminal_text(error));
                }
            }
            if let Some(log) = logs
                .data
                .as_deref()
                .and_then(|history| history.logs.get(&run.meta.run_id))
            {
                row.logs.push(log.clone());
            }
        }
    }
    if let Some(history) = runs.data.as_deref() {
        let row_keys = data
            .rows
            .iter()
            .map(|row| row.key.as_str())
            .collect::<BTreeSet<_>>();
        for run in &history.runs {
            if !row_keys.contains(run.meta.action_key.as_str())
                && !row_keys.contains(run.meta.slug.as_str())
                && let Some(log) = logs
                    .data
                    .as_deref()
                    .and_then(|history| history.logs.get(&run.meta.run_id))
            {
                data.slot_logs
                    .entry(run.meta.slug.clone())
                    .or_default()
                    .push(log.clone());
            }
        }
    }
    data.attention.retain(|line| {
        !line.text.starts_with("Action succeeded:")
            && !line.text.starts_with("Action failed:")
            && !line.text.starts_with("Action killed:")
            && !line.text.starts_with("Action uncertain:")
    });
    for event in &tracker.attention {
        if !data.activity.iter().any(|line| {
            line.at_ms == event.at_ms && line.source == event.source && line.text == event.text
        }) {
            data.activity.push(wt_tui::ActivityLine {
                at_ms: event.at_ms,
                level: "INFO".into(),
                channel: "attention".into(),
                source: event.source.clone(),
                text: event.text.clone(),
            });
        }
    }
    data.attention.extend(tracker.attention.iter().cloned());
    crate::activity_source::bound_feeds(&mut data);
    board.data = Some(Arc::new(data));
    board
}

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incremental_log_tail_keeps_partial_line_and_bounds_unterminated_output() {
        let mut lines = VecDeque::new();
        let mut pending = String::new();
        append_log_lines(&mut lines, &mut pending, "out", b"first\npartial");
        assert_eq!(lines.back().map(String::as_str), Some("[out] first"));
        assert_eq!(pending, "partial");
        append_log_lines(&mut lines, &mut pending, "out", b" line\n");
        assert_eq!(lines.back().map(String::as_str), Some("[out] partial line"));
        append_log_lines(&mut lines, &mut pending, "err", &vec![b'x'; 20 * 1024]);
        assert_eq!(pending.len(), 16 * 1024);
    }

    fn run(id: &str, status: &str, started: u64) -> ActionRun {
        serde_json::from_value(serde_json::json!({
            "meta": {"version":1,"slug":"row","runId":id,"kind":"shell","actionId":"move","actionName":"Move issue","prompt":"","affects":["issue"],"startedAt":started,"status":status,"issueStatus":{"issueId":"ENG-1","status":"Review"}},
            "runDir":"/tmp/unused","command":[],"cwd":"/tmp/unused"
        })).unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn completion_refreshes_once_and_newer_failure_cannot_revive_older_guard() {
        let now = tokio::time::Instant::now();
        let mut tracker = Tracker::new(100);
        assert!(
            tracker
                .observe(&[run("old", "succeeded", 90)], now)
                .is_empty()
        );
        assert!(tracker.expected.is_empty());
        assert!(
            tracker
                .observe(&[run("new", "running", 110)], now)
                .is_empty()
        );
        assert_eq!(tracker.expected["ENG-1"].run_id, "new");
        assert_eq!(
            tracker.observe(&[run("new", "succeeded", 110)], now).len(),
            1
        );
        assert!(
            tracker
                .observe(&[run("new", "succeeded", 110)], now)
                .is_empty()
        );
        tracker.observe(
            &[run("later", "failed", 120), run("new", "succeeded", 110)],
            now,
        );
        assert!(tracker.expected.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn expectation_waits_for_fresh_reader_or_deadline_and_never_reappears() {
        let now = tokio::time::Instant::now();
        let mut tracker = Tracker::new(100);
        tracker.observe(&[run("new", "running", 110)], now);
        let complete = now + Duration::from_secs(1);
        tracker.observe(&[run("new", "succeeded", 110)], complete);
        let mut statuses = SourceSnapshot {
            data: Some(Arc::new(StatusBatch {
                ids: vec!["ENG-1".into()],
                statuses: [("ENG-1".into(), "Review".into())].into(),
            })),
            state: SourceState::Ready,
            updated_at: Some(now),
            revision: 0,
        };
        tracker.settle(&statuses, complete);
        assert!(!tracker.expected.is_empty());
        statuses.updated_at = Some(complete);
        tracker.settle(&statuses, complete);
        assert!(tracker.expected.is_empty());
        tracker.observe(&[run("new", "succeeded", 110)], complete);
        assert!(tracker.expected.is_empty());
        tracker.observe(&[run("second", "succeeded", 120)], complete);
        statuses.updated_at = None;
        tracker.settle(&statuses, complete + Duration::from_secs(12));
        assert!(tracker.expected.is_empty());
    }
}
