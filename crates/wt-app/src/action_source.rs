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
    Confirmed, Domain, LeafHandle, Mutation, Pending, Persistence, RefreshPolicy, SourceHandle,
    SourceSnapshot, SourceSpec, SourceState, Stamp, Target, TaskScope, derived_channel,
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

/// Sources an action's declared effects refresh on completion. The issue
/// statuses leaf also settles issue-status expectations, so it must be the
/// confirmable leaf, not a display view.
pub struct RefreshTargets {
    pub git: LeafHandle<Vec<wt_vcs::WorktreeSnapshot>>,
    pub github: LeafHandle<wt_github::GithubData>,
    pub dev: Option<LeafHandle<Vec<wt_dev::DevStatusRow>>>,
    pub origin: Option<LeafHandle<wt_vcs::FetchOriginReport>>,
    pub issues: LeafHandle<StatusBatch>,
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
        SourceSpec {
            name: "action-runs",
            domain: Domain::Actions,
            persistence: Persistence::Never("authoritative action store; read directly"),
            policy: RefreshPolicy {
                debounce: Duration::from_millis(50),
                minimum_interval: Duration::from_millis(250),
            },
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
        SourceSpec {
            name: "action-logs",
            domain: Domain::Actions,
            persistence: Persistence::Never("log files are the source"),
            policy: RefreshPolicy {
                debounce: Duration::from_millis(30),
                minimum_interval: Duration::from_millis(250),
            },
        },
        move |_| {
            let service = log_service.clone();
            let runs = log_runs.subscribe().borrow().clone();
            let cursors = log_cursors.clone();
            async move {
                let service = service.map_err(anyhow::Error::msg)?;
                load_logs(&service, runs.data().map(Arc::as_ref), &cursors).await
            }
        },
    );
    watch(
        scope,
        context.config.paths.log_dir.join("actions"),
        runs.view(),
        logs.view(),
    );
    project(scope, board, runs.view(), logs.view(), targets)
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
    history.data().is_some_and(|history| {
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
    let (output, mut publisher) = derived_channel(scope, "action-board");
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
        let mut last: Option<SourceSnapshot<Board>> = None;
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
                    let error = match snapshot.state() {SourceState::Failed(error) => Some(error.clone()), _ => None};
                    let data = snapshot.data().cloned();
                    if data == last_runs && error == last_error { continue; }
                    last_runs = data; last_error = error;
                    if let Some(history) = snapshot.data() {
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
            let now = tokio::time::Instant::now();
            tracker.renew_running(now);
            tracker.settle(targets.issues.confirmed().as_ref(), now);
            let next = compose(
                boards.borrow().clone(),
                updates.borrow().clone(),
                log_updates.borrow().clone(),
                &tracker,
            );
            if last.as_ref().is_some_and(|last| last.same_view(&next)) {
                continue;
            }
            last = Some(next.clone());
            publisher.publish(next);
        }
    });
    output
}

/// An action that sets an issue's status, shown on its row until the issue
/// tracker reports it. Display-only: nothing decides on this projection.
#[derive(Clone, Debug, PartialEq, Eq)]
struct IssueExpectation {
    run_id: String,
    row_key: String,
    issue_id: String,
    status: String,
}

impl Mutation<StatusBatch> for IssueExpectation {
    /// The newest run for an issue replaces older ones.
    fn key(&self) -> String {
        self.issue_id.clone()
    }

    fn target(&self) -> Target {
        Target::row(None, self.row_key.clone())
    }

    fn label(&self) -> String {
        wt_core::sanitize_terminal_text(&format!("moving {} to {}", self.issue_id, self.status))
    }

    fn domains(&self) -> &'static [Domain] {
        &[Domain::Issues]
    }

    fn apply(&self, view: &mut StatusBatch) {
        view.statuses
            .insert(self.issue_id.clone(), self.status.clone());
    }

    fn observed(&self, confirmed: &StatusBatch) -> bool {
        confirmed.statuses.get(&self.issue_id) == Some(&self.status)
    }
}

/// Completed-action attention and issue-status expectations.
///
/// A running run's expectation is in flight. The engine bounds in-flight
/// entries (60s), but an action may legitimately run longer, so every pass
/// re-begins the expectations of runs that are still running; one stays
/// visible exactly while its run is live. A successful completion accepts it
/// at the moment the completion was observed: only a tracker read that
/// started afterwards and shows the status settles it, otherwise the
/// accepted deadline drops it. A failed, killed or uncertain run drops it at
/// once. A settled run never comes back.
struct Tracker {
    boot_ms: u64,
    handled: BTreeSet<String>,
    seen: BTreeSet<String>,
    pending: Pending<StatusBatch, IssueExpectation>,
    /// Operation id of each run with a pending expectation.
    ops: BTreeMap<String, u64>,
    next_op: u64,
    /// Runs whose expectation was accepted (completion observed).
    accepted: BTreeSet<String>,
    /// Expectations of runs that were running at the last observation.
    running: BTreeMap<String, IssueExpectation>,
    settled: BTreeSet<String>,
    attention: VecDeque<wt_tui::AttentionLine>,
}

impl Tracker {
    fn new(boot_ms: u64) -> Self {
        Self {
            boot_ms,
            handled: BTreeSet::new(),
            seen: BTreeSet::new(),
            pending: Pending::default(),
            ops: BTreeMap::new(),
            next_op: 0,
            accepted: BTreeSet::new(),
            running: BTreeMap::new(),
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
        self.running.clear();
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
                self.drop_issue(&expectation.issue_id);
                self.settled.insert(meta.run_id.clone());
                continue;
            }
            let mutation = IssueExpectation {
                run_id: meta.run_id.clone(),
                row_key: meta
                    .worktree_ref
                    .as_ref()
                    .map(wt_core::worktree_ledger_key)
                    .unwrap_or_else(|| meta.slug.clone()),
                issue_id: expectation.issue_id.clone(),
                status: expectation.status.clone(),
            };
            let op = match self.ops.get(&meta.run_id) {
                Some(op) => *op,
                None => {
                    self.next_op = self.next_op.wrapping_add(1);
                    self.ops.insert(meta.run_id.clone(), self.next_op);
                    self.next_op
                }
            };
            if !self.pending.contains(op) {
                self.pending.begin(op, mutation.clone(), now);
            }
            if terminal {
                if self.accepted.insert(meta.run_id.clone()) {
                    self.pending.accept(op, Stamp::at(now), now);
                }
            } else {
                self.running.insert(meta.run_id.clone(), mutation);
            }
        }
        self.sweep();
        completed
    }

    /// Keep expectations of still-running runs in flight past the engine's
    /// in-flight bound.
    fn renew_running(&mut self, now: tokio::time::Instant) {
        for (run_id, mutation) in &self.running {
            if let Some(op) = self.ops.get(run_id)
                && self.pending.contains(*op)
                && !self.accepted.contains(run_id)
            {
                self.pending.begin(*op, mutation.clone(), now);
            }
        }
    }

    fn drop_issue(&mut self, issue_id: &str) {
        let ops = self
            .ops
            .values()
            .copied()
            .filter(|op| {
                self.pending
                    .mutation(*op)
                    .is_some_and(|mutation| mutation.issue_id == issue_id)
            })
            .collect::<Vec<_>>();
        for op in ops {
            self.pending.remove(op);
        }
    }

    /// Settle against the confirmed tracker read. Without one (empty or
    /// restored) only deadlines apply.
    fn settle(&mut self, statuses: Option<&Confirmed<StatusBatch>>, now: tokio::time::Instant) {
        let empty = StatusBatch::default();
        let confirmed = statuses.map_or(&empty, |statuses| &**statuses);
        let observed = statuses.map(Confirmed::observed);
        for expired in self.pending.settle(confirmed, |_, _| observed, now) {
            tracing::info!(
                issue = expired.mutation.issue_id,
                status = expired.mutation.status,
                "issue tracker did not reflect the action's status before the deadline"
            );
        }
        self.sweep();
    }

    /// Forget runs whose expectation is gone; they never reappear.
    fn sweep(&mut self) {
        let pending = &self.pending;
        let settled = &mut self.settled;
        let accepted = &mut self.accepted;
        self.ops.retain(|run_id, op| {
            let live = pending.contains(*op);
            if !live {
                settled.insert(run_id.clone());
                accepted.remove(run_id);
            }
            live
        });
        let ops = &self.ops;
        self.running.retain(|run_id, _| ops.contains_key(run_id));
    }

    fn next_expiry(&self) -> Option<tokio::time::Instant> {
        self.pending.next_deadline()
    }

    /// Issue statuses the pending expectations show.
    fn projected_statuses(&self) -> BTreeMap<String, String> {
        if self.pending.is_empty() {
            return BTreeMap::new();
        }
        self.pending.project(&StatusBatch::default()).statuses
    }
}

fn compose(
    board: SourceSnapshot<Board>,
    runs: SourceSnapshot<ActionHistory>,
    logs: SourceSnapshot<ActionLogs>,
    tracker: &Tracker,
) -> SourceSnapshot<Board> {
    let Some(data) = board.data() else {
        return board;
    };
    let mut data = data.as_ref().clone();
    if let SourceState::Failed(error) = runs.state() {
        crate::activity_source::append_attention(
            &mut data,
            "Action history",
            &wt_core::sanitize_terminal_text(error),
        );
    }
    data.slot_logs.clear();
    let projected = tracker.projected_statuses();
    for row in &mut data.rows {
        row.logs.clear();
        if let Some(status) = row.issue_id.as_ref().and_then(|id| projected.get(id)) {
            row.issue_status = Some(status.clone());
        }
        let history = runs.data().map(Arc::as_ref);
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
                .data()
                .and_then(|history| history.logs.get(&run.meta.run_id))
            {
                row.logs.push(log.clone());
            }
        }
    }
    if let Some(history) = runs.data() {
        let row_keys = data
            .rows
            .iter()
            .map(|row| row.key.as_str())
            .collect::<BTreeSet<_>>();
        for run in &history.runs {
            if !row_keys.contains(run.meta.action_key.as_str())
                && !row_keys.contains(run.meta.slug.as_str())
                && let Some(log) = logs
                    .data()
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
    data.pending.extend(tracker.pending.views());
    board
        .with_data(Some(Arc::new(data)))
        .with_input(&runs)
        .with_input(&logs)
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

    fn expected_runs(tracker: &Tracker) -> Vec<String> {
        tracker
            .pending
            .mutations()
            .map(|mutation| mutation.run_id.clone())
            .collect()
    }

    fn batch(status: &str) -> StatusBatch {
        StatusBatch {
            ids: vec!["ENG-1".into()],
            statuses: [("ENG-1".into(), status.into())].into(),
        }
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
        assert!(tracker.pending.is_empty());
        assert!(
            tracker
                .observe(&[run("new", "running", 110)], now)
                .is_empty()
        );
        assert_eq!(expected_runs(&tracker), ["new"]);
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
        assert!(tracker.pending.is_empty());
        tracker.observe(&[run("new", "succeeded", 110)], now);
        assert!(
            tracker.pending.is_empty(),
            "a dropped expectation never revives"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn expectation_waits_for_fresh_reader_or_deadline_and_never_reappears() {
        let mut tracker = Tracker::new(100);
        tracker.observe(&[run("new", "running", 110)], tokio::time::Instant::now());
        // A tracker read that started before the run completed.
        let stale = Confirmed::for_test(batch("Review"));
        tokio::time::advance(Duration::from_secs(1)).await;
        tracker.observe(&[run("new", "succeeded", 110)], tokio::time::Instant::now());
        tracker.settle(Some(&stale), tokio::time::Instant::now());
        assert_eq!(expected_runs(&tracker), ["new"]);
        assert_eq!(
            tracker.pending.views()[0].phase,
            wt_runtime::PendingPhase::Accepted
        );
        tokio::time::advance(Duration::from_millis(10)).await;
        let fresh = Confirmed::for_test(batch("Review"));
        tracker.settle(Some(&fresh), tokio::time::Instant::now());
        assert!(tracker.pending.is_empty());
        tracker.observe(&[run("new", "succeeded", 110)], tokio::time::Instant::now());
        assert!(tracker.pending.is_empty());
        // Without a confirming read the accepted deadline drops it.
        tracker.observe(
            &[run("second", "succeeded", 120)],
            tokio::time::Instant::now(),
        );
        assert_eq!(expected_runs(&tracker), ["second"]);
        tokio::time::advance(Duration::from_secs(29)).await;
        tracker.settle(None, tokio::time::Instant::now());
        assert_eq!(expected_runs(&tracker), ["second"]);
        tokio::time::advance(Duration::from_secs(1)).await;
        tracker.settle(None, tokio::time::Instant::now());
        assert!(tracker.pending.is_empty());
        tracker.observe(
            &[run("second", "succeeded", 120)],
            tokio::time::Instant::now(),
        );
        assert!(tracker.pending.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_long_running_action_stays_in_flight_while_it_runs() {
        let mut tracker = Tracker::new(100);
        tracker.observe(&[run("slow", "running", 110)], tokio::time::Instant::now());
        for _ in 0..5 {
            tokio::time::advance(Duration::from_secs(50)).await;
            let now = tokio::time::Instant::now();
            tracker.renew_running(now);
            tracker.settle(Some(&Confirmed::for_test(batch("Review"))), now);
            assert_eq!(expected_runs(&tracker), ["slow"]);
            assert_eq!(
                tracker.pending.views()[0].phase,
                wt_runtime::PendingPhase::InFlight
            );
        }
        // An uncertain start drops it at once.
        tracker.observe(
            &[run("slow", "ambiguous", 110)],
            tokio::time::Instant::now(),
        );
        assert!(tracker.pending.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn pending_status_is_projected_onto_the_row_and_marked() {
        let mut tracker = Tracker::new(100);
        tracker.observe(&[run("new", "running", 110)], tokio::time::Instant::now());
        let board = Board {
            rows: vec![wt_tui::BoardRow {
                key: "row".into(),
                issue_id: Some("ENG-1".into()),
                issue_status: Some("Todo".into()),
                ..wt_tui::BoardRow::default()
            }],
            ..Board::default()
        };
        let output = compose(
            SourceSnapshot::test_live(Some(board), SourceState::Ready, Domain::Git),
            SourceSnapshot::default(),
            SourceSnapshot::default(),
            &tracker,
        );
        let data = output.data().unwrap();
        assert_eq!(data.rows[0].issue_status.as_deref(), Some("Review"));
        assert_eq!(
            data.pending,
            vec![wt_runtime::PendingView {
                target: Target::row(None, "row"),
                label: "moving ENG-1 to Review".into(),
                phase: wt_runtime::PendingPhase::InFlight,
            }]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn issue_status_expectation_meets_the_mutation_contract() {
        wt_runtime::conformance::mutation(
            IssueExpectation {
                run_id: "run-1".into(),
                row_key: "row".into(),
                issue_id: "ENG-1".into(),
                status: "Review".into(),
            },
            batch("Todo"),
            batch("Review"),
        )
        .await;
    }
}
