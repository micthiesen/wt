//! Bounded app-log backfill and manager reports for the output feeds.
use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use notify::Watcher;
use serde_json::Value;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use wt_core::Domain;
use wt_runtime::{LeafHandle, Persistence, RefreshPolicy, SourceSpec, TaskScope, start_source};
use wt_tui::{ActivityLine, AttentionLine};

const MAX_ACTIVITY: usize = 500;
const MAX_ATTENTION: usize = 200;
const MAX_TAIL_BYTES: u64 = 512 * 1024;
const REPEAT_WINDOW_MS: u64 = 5_000;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActivitySnapshot {
    pub activity: Vec<ActivityLine>,
    pub attention: Vec<AttentionLine>,
}

fn append_activity_at(
    board: &mut wt_tui::Board,
    at_ms: u64,
    level: &str,
    source: &str,
    text: &str,
) {
    let event = ActivityLine {
        at_ms,
        level: level.into(),
        channel: "activity".into(),
        source: wt_core::sanitize_terminal_text(source),
        text: wt_core::sanitize_terminal_text(text),
    };
    if !board.activity.iter().rev().take(20).any(|previous| {
        previous.source == event.source
            && previous.text == event.text
            && event.at_ms.saturating_sub(previous.at_ms) <= 5_000
    }) {
        board.activity.push(event);
    }
    bound_feeds(board);
}

fn append_attention_at(board: &mut wt_tui::Board, at_ms: u64, source: &str, text: &str) {
    let source = wt_core::sanitize_terminal_text(source);
    let text = wt_core::sanitize_terminal_text(text);
    if board.attention.iter().rev().take(20).any(|previous| {
        previous.source == source
            && previous.text == text
            && at_ms.saturating_sub(previous.at_ms) <= 5_000
    }) {
        return;
    }
    board.activity.push(ActivityLine {
        at_ms,
        level: "ERROR".into(),
        channel: "attention".into(),
        source: source.clone(),
        text: text.clone(),
    });
    board.attention.push(AttentionLine {
        at_ms,
        source,
        text,
    });
    bound_feeds(board);
}

/// First-seen times of the status lines a derived stage adds on every
/// compose. Stages rebuild from their upstream board, so stamping a lasting
/// failure with the current time would make it new on each pass: it could
/// never be acknowledged and every pass would republish the board. A line
/// keeps its time while each compose reports it; once a compose omits it
/// (`sweep`), a recurrence is a new event.
#[derive(Clone, Debug, Default)]
pub(crate) struct FailureClock {
    since: HashMap<(&'static str, String, String), u64>,
    seen: HashSet<(&'static str, String, String)>,
}

impl FailureClock {
    /// The time to show for this line in the current compose.
    pub(crate) fn at(&mut self, channel: &'static str, source: &str, text: &str) -> u64 {
        let key = (
            channel,
            wt_core::sanitize_terminal_text(source),
            wt_core::sanitize_terminal_text(text),
        );
        let at_ms = *self.since.entry(key.clone()).or_insert_with(epoch_ms);
        self.seen.insert(key);
        at_ms
    }

    /// Report a lasting condition on the attention feed.
    pub(crate) fn attention(&mut self, board: &mut wt_tui::Board, source: &str, text: &str) {
        let at_ms = self.at("attention", source, text);
        append_attention_at(board, at_ms, source, text);
    }

    /// Report a lasting condition on the activity feed.
    pub(crate) fn activity(
        &mut self,
        board: &mut wt_tui::Board,
        level: &str,
        source: &str,
        text: &str,
    ) {
        let at_ms = self.at("activity", source, text);
        append_activity_at(board, at_ms, level, source, text);
    }

    /// End one compose: lines it did not report have cleared.
    pub(crate) fn sweep(&mut self) {
        let seen = std::mem::take(&mut self.seen);
        self.since.retain(|key, _| seen.contains(key));
    }
}

pub(crate) fn bound_feeds(board: &mut wt_tui::Board) {
    board.activity.sort_by_key(|event| event.at_ms);
    board.attention.sort_by_key(|event| event.at_ms);
    // Sorted feeds hold the same line twice when the in-memory copy and
    // its log backfill meet, or when several processes report one event.
    // A burst inside the window is one event; an attention line identical
    // to the one directly above it adds nothing whatever the gap.
    board.activity.dedup_by(|later, earlier| {
        later.channel == earlier.channel
            && later.source == earlier.source
            && later.text == earlier.text
            && later.at_ms.saturating_sub(earlier.at_ms) <= REPEAT_WINDOW_MS
    });
    collapse_repeats(&mut board.attention);
    if board.activity.len() > MAX_ACTIVITY {
        board.activity.drain(..board.activity.len() - MAX_ACTIVITY);
    }
    if board.attention.len() > MAX_ATTENTION {
        board
            .attention
            .drain(..board.attention.len() - MAX_ATTENTION);
    }
}

pub fn start(
    scope: &TaskScope,
    cache_root: PathBuf,
    app_log_dir: PathBuf,
) -> LeafHandle<ActivitySnapshot> {
    start_with_watcher(
        scope,
        cache_root,
        app_log_dir,
        true,
        #[cfg(test)]
        None,
    )
}

#[cfg(test)]
type WatcherSetupGate = (
    tokio::sync::oneshot::Sender<()>,
    std::sync::mpsc::Receiver<()>,
);

fn start_with_watcher(
    scope: &TaskScope,
    cache_root: PathBuf,
    app_log_dir: PathBuf,
    native_watch: bool,
    #[cfg(test)] setup_gate: Option<WatcherSetupGate>,
) -> LeafHandle<ActivitySnapshot> {
    let manager_dir = cache_root.join("manager");
    let reports = manager_dir.join("reports.jsonl");
    let watched_log_dir = app_log_dir.clone();
    let polled_reports = reports.clone();
    let polled_logs = app_log_dir.clone();
    let source = start_source(
        scope,
        SourceSpec {
            name: "activity",
            domain: Domain::Activity,
            persistence: Persistence::Never("log files are the source"),
            policy: RefreshPolicy {
                debounce: Duration::from_millis(50),
                minimum_interval: Duration::from_millis(100),
            },
        },
        move |_| {
            let reports = reports.clone();
            let app_log_dir = app_log_dir.clone();
            async move {
                tokio::task::spawn_blocking(move || read_snapshot(&reports, &app_log_dir)).await?
            }
        },
    );
    let observed = source.view();
    let cancellation = scope.token();
    scope.spawn(async move {
        let callback = observed.clone();
        let setup_cancel = cancellation.clone();
        let watcher = tokio::task::spawn_blocking(move || {
            if setup_cancel.is_cancelled() {
                return Ok(None);
            }
            std::fs::create_dir_all(&manager_dir)?;
            std::fs::create_dir_all(&watched_log_dir)?;
            if !native_watch {
                return Ok(None);
            }
            let mut watcher =
                crate::watcher::new(move |event: notify::Result<notify::Event>| match event {
                    Ok(event)
                        if !matches!(event.kind, notify::EventKind::Access(_))
                            && event.paths.iter().any(|path| {
                                path.file_name().is_some_and(|name| {
                                    name == "reports.jsonl"
                                        || name.to_string_lossy().starts_with("wt-native")
                                })
                            }) =>
                    {
                        callback.refresh();
                    }
                    Err(_) => {
                        callback.refresh();
                    }
                    _ => {}
                })?;
            // Register canonical paths so aliases such as /tmp do not lose
            // log notifications during path matching.
            let paths = vec![
                notify::PathOp::watch_non_recursive(std::fs::canonicalize(&manager_dir)?),
                notify::PathOp::watch_non_recursive(std::fs::canonicalize(&watched_log_dir)?),
            ];
            if !setup_cancel.is_cancelled() {
                watcher.update_paths(paths)?;
            }
            #[cfg(test)]
            if let Some((entered, release)) = setup_gate {
                // Hold the native watcher before ownership returns to the
                // async source, making startup cancellation deterministic.
                let _ = entered.send(());
                release.recv_timeout(Duration::from_secs(10))?;
            }
            Ok::<_, anyhow::Error>(Some(watcher))
        })
        .await;
        let watcher = match watcher {
            Ok(Ok(watcher)) => watcher,
            error => {
                tracing::warn!(?error, "activity feeds use refresh backstop");
                None
            }
        };
        if cancellation.is_cancelled() {
            crate::watcher_cleanup::drop_watcher(watcher).await;
            return;
        }
        let fingerprint = || {
            let reports = polled_reports.clone();
            let logs = polled_logs.clone();
            let cancellation = cancellation.clone();
            tokio::task::spawn_blocking(move || feed_fingerprint(&reports, &logs, &cancellation))
        };
        let mut previous = fingerprint().await.ok();
        observed.refresh();
        let mut changed_files = tokio::time::interval(Duration::from_secs(1));
        changed_files.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let period = Duration::from_secs(30);
        let mut backstop = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        backstop.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => break,
                _ = backstop.tick() => { observed.refresh(); },
                _ = changed_files.tick() => {
                    let current = fingerprint().await.ok();
                    if current != previous {
                        previous = current;
                        observed.refresh();
                    }
                }
            }
        }
        crate::watcher_cleanup::drop_watcher(watcher).await;
    });
    source
}

/// Bound polling to the same seven rolling logs read by the source, plus the
/// manager report file. Precise timestamps and lengths detect same-second
/// appends without hashing or repeatedly parsing unchanged log contents.
fn feed_fingerprint(
    reports: &Path,
    directory: &Path,
    cancellation: &tokio_util::sync::CancellationToken,
) -> Vec<(PathBuf, u64, Option<SystemTime>)> {
    let mut paths = std::fs::read_dir(directory)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .take_while(|_| !cancellation.is_cancelled())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name().is_some_and(|name| {
                let name = name.to_string_lossy();
                name.starts_with("wt-native") && name.ends_with(".log")
            })
        })
        .collect::<Vec<_>>();
    paths.sort();
    let mut paths = paths.into_iter().rev().take(7).collect::<Vec<_>>();
    paths.push(reports.to_owned());
    paths
        .into_iter()
        .filter_map(|path| {
            let metadata = std::fs::metadata(&path).ok()?;
            Some((path, metadata.len(), metadata.modified().ok()))
        })
        .collect()
}

fn read_snapshot(reports: &Path, app_log_dir: &Path) -> anyhow::Result<ActivitySnapshot> {
    let mut activity = read_app_logs(app_log_dir)?;
    let batch = crate::commands::manager::read_reports_from(reports, 0)?;
    for report in batch.reports.into_iter().rev().take(MAX_ATTENTION).rev() {
        let Some(at_ms) = parse_timestamp_ms(&report.at) else {
            continue;
        };
        let level = match report.level {
            crate::commands::manager::ReportLevel::Info => "INFO",
            crate::commands::manager::ReportLevel::Ok => "INFO",
            crate::commands::manager::ReportLevel::Warn => "WARN",
            crate::commands::manager::ReportLevel::Err => "ERROR",
        };
        activity.push(ActivityLine {
            at_ms,
            level: level.into(),
            channel: "attention".into(),
            source: "manager".into(),
            text: wt_core::sanitize_terminal_text(&report.text),
        });
    }
    activity.sort_by_key(|event| event.at_ms);
    dedupe_repeats(&mut activity);
    let attention = attention_from_activity(&activity);
    if activity.len() > MAX_ACTIVITY {
        activity.drain(..activity.len() - MAX_ACTIVITY);
    }
    Ok(ActivitySnapshot {
        activity,
        attention,
    })
}

fn read_app_logs(directory: &Path) -> anyhow::Result<Vec<ActivityLine>> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.file_name().is_some_and(|name| {
            let name = name.to_string_lossy();
            name.starts_with("wt-native") && name.ends_with(".log")
        }) {
            paths.push(path);
        }
    }
    paths.sort();
    let mut events = Vec::new();
    for path in paths.iter().rev().take(7).rev() {
        let contents = match read_tail(path, MAX_TAIL_BYTES) {
            Ok(contents) => contents,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                continue;
            }
            Err(error) => return Err(error),
        };
        for line in contents.lines().filter(|line| !line.trim().is_empty()) {
            if let Some(event) = parse_app_log_line(line) {
                events.push(event);
            }
        }
    }
    events.sort_by_key(|event| event.at_ms);
    if events.len() > MAX_ACTIVITY {
        events.drain(..events.len() - MAX_ACTIVITY);
    }
    Ok(events)
}

/// Collapses burst duplicates: N processes observing one transition each
/// log it, and a repeated failing key logs the same error each press.
fn dedupe_repeats(events: &mut Vec<ActivityLine>) {
    let mut previous = HashMap::<(String, String, String), u64>::new();
    events.retain(|event| {
        let key = (
            event.channel.clone(),
            event.source.clone(),
            event.text.clone(),
        );
        if previous
            .get(&key)
            .is_some_and(|at_ms| event.at_ms.saturating_sub(*at_ms) <= REPEAT_WINDOW_MS)
        {
            false
        } else {
            previous.insert(key, event.at_ms);
            true
        }
    });
}

fn read_tail(path: &Path, limit: u64) -> anyhow::Result<String> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    let start = length.saturating_sub(limit);
    let read_start = start.saturating_sub(1);
    file.seek(SeekFrom::Start(read_start))?;
    let mut contents = Vec::with_capacity(length.saturating_sub(read_start) as usize);
    file.read_to_end(&mut contents)?;
    if start > 0 {
        let previous_byte = contents.first().copied();
        contents.drain(..1);
        if previous_byte != Some(b'\n')
            && let Some(newline) = contents.iter().position(|byte| *byte == b'\n')
        {
            contents.drain(..=newline);
        }
    }
    Ok(String::from_utf8_lossy(&contents).into_owned())
}

/// Parses one native log record into a feed line. The feeds show only
/// user-facing events, like the TypeScript `EVENT`/`ATTN` log tags:
/// explicit `event_*` records, plus errors raised by wt itself. Plain
/// INFO/WARN records (input latency, terminal lifecycle, scheduler
/// telemetry) stay in the file for diagnosis and never reach the panes.
fn parse_app_log_line(line: &str) -> Option<ActivityLine> {
    let value: Value = serde_json::from_str(line).ok()?;
    let fields = value.get("fields")?;
    let level = value.get("level")?.as_str()?.to_ascii_uppercase();
    let timestamp = value.get("timestamp")?.as_str()?;
    let at_ms = fields
        .get("event_at_ms")
        .and_then(Value::as_u64)
        .or_else(|| parse_timestamp_ms(timestamp))?;
    // An explicit event names its text, or marks itself with a channel and
    // carries the text as its message (plus any error field).
    let event_text = match fields.get("event_text").and_then(Value::as_str) {
        Some(text) => Some(text.to_owned()),
        None if fields.get("event_channel").is_some() => plain_error_text(fields),
        None => None,
    };
    let (source, text, channel) = match event_text {
        Some(text) => (
            fields
                .get("event_source")
                .and_then(Value::as_str)
                .filter(|source| !source.trim().is_empty())
                .unwrap_or("wt")
                .to_owned(),
            text,
            fields
                .get("event_channel")
                .and_then(Value::as_str)
                .unwrap_or("activity")
                .to_owned(),
        ),
        None => {
            let target = value.get("target").and_then(Value::as_str).unwrap_or("");
            // Warnings reach the feeds only when marked as events, since
            // most describe plumbing; wt's own errors always do.
            if level != "ERROR" || !is_user_facing_error_target(target) {
                return None;
            }
            (
                "wt".to_owned(),
                plain_error_text(fields)?,
                "activity".to_owned(),
            )
        }
    };
    Some(ActivityLine {
        at_ms,
        level,
        channel,
        source: wt_core::sanitize_terminal_text(&source),
        text: wt_core::sanitize_terminal_text(&text),
    })
}

/// wt's own errors reach the feeds; terminal-driver and dependency
/// records are diagnostics for the log file only.
fn is_user_facing_error_target(target: &str) -> bool {
    let crate_name = target.split("::").next().unwrap_or("");
    (crate_name == "wt" || crate_name.starts_with("wt_")) && !target.starts_with("wt_tui::terminal")
}

/// A logged failure names its cause in the `error` field; the feed shows
/// it, since "action failed" alone tells the reader nothing. The
/// controller's "TUI action failed" wording names an internal layer, so
/// the feed reads like the TypeScript `<verb> failed: <cause>` lines.
fn plain_error_text(fields: &Value) -> Option<String> {
    let message = fields.get("message").and_then(Value::as_str)?;
    let message = match message {
        "TUI action failed" => "action failed",
        message => message,
    };
    Some(match fields.get("error").and_then(Value::as_str) {
        Some(error) => format!("{message}: {error}"),
        None => message.to_owned(),
    })
}

/// Consecutive identical attention lines collapse into the most recent one,
/// so a failure that recurs reads with its latest time, not its first.
fn collapse_repeats(attention: &mut Vec<AttentionLine>) {
    let mut kept: Vec<AttentionLine> = Vec::with_capacity(attention.len());
    for line in attention.drain(..) {
        match kept.last_mut() {
            Some(last) if last.source == line.source && last.text == line.text => *last = line,
            _ => kept.push(line),
        }
    }
    *attention = kept;
}

fn attention_from_activity(activity: &[ActivityLine]) -> Vec<AttentionLine> {
    let mut attention = activity
        .iter()
        .filter(|event| event.channel == "attention" || event.level == "ERROR")
        .map(|event| AttentionLine {
            at_ms: event.at_ms,
            source: event.source.clone(),
            text: event.text.clone(),
        })
        .collect::<Vec<_>>();
    collapse_repeats(&mut attention);
    if attention.len() > MAX_ATTENTION {
        attention.drain(..attention.len() - MAX_ATTENTION);
    }
    attention
}

fn parse_timestamp_ms(timestamp: &str) -> Option<u64> {
    let parsed = OffsetDateTime::parse(timestamp, &Rfc3339).ok()?;
    u64::try_from(parsed.unix_timestamp_nanos() / 1_000_000).ok()
}

pub(crate) fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn native_watchers_drain_when_cancelled_during_startup() {
        let mut hosts = tokio::task::JoinSet::new();
        for _ in 0..4 {
            hosts.spawn(async {
                let root = tempfile::tempdir().unwrap();
                let scope = TaskScope::new();
                let (entered, started) = tokio::sync::oneshot::channel();
                let (release, held) = std::sync::mpsc::channel();
                let _source = start_with_watcher(
                    &scope,
                    root.path().join("cache"),
                    root.path().join("logs"),
                    true,
                    Some((entered, held)),
                );
                // This only bounds a broken test harness. The behavioral
                // deadline starts below, after native setup is held in flight.
                tokio::time::timeout(Duration::from_secs(10), started)
                    .await
                    .unwrap()
                    .unwrap();
                scope.cancel();
                release.send(()).unwrap();
                scope.shutdown(Duration::from_secs(5)).await.unwrap();
            });
        }
        while let Some(result) = hosts.join_next().await {
            result.unwrap();
        }
    }

    #[tokio::test]
    async fn log_append_refreshes_without_native_notifications() {
        let root = tempfile::tempdir().unwrap();
        let logs = root.path().join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        let log = logs.join("wt-native.fixture.log");
        std::fs::write(&log, "").unwrap();
        let scope = TaskScope::new();
        let source = start_with_watcher(&scope, root.path().join("cache"), logs, false, None);
        let mut updates = source.subscribe();
        tokio::time::timeout(
            Duration::from_secs(2),
            updates.wait_for(|snapshot| snapshot.data().is_some()),
        )
        .await
        .unwrap()
        .unwrap();
        // The append can occur in the same timestamp second as creation.
        std::fs::write(&log, concat!(r#"{"timestamp":"2026-10-09T12:00:00Z","level":"INFO","fields":{"event_at_ms":10,"event_channel":"attention","event_source":"fixture","event_text":"live append"},"target":"wt_attention"}"#, "\n")).unwrap();
        tokio::time::timeout(
            Duration::from_secs(3),
            updates.wait_for(|snapshot| {
                snapshot
                    .data()
                    .is_some_and(|data| data.activity.iter().any(|line| line.text == "live append"))
            }),
        )
        .await
        .unwrap()
        .unwrap();
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
    }

    #[test]
    fn a_logged_error_field_follows_the_message() {
        let line = r#"{"timestamp":"2026-10-09T12:00:00Z","level":"ERROR","fields":{"message":"TUI action failed","error":"no such worktree"},"target":"wt::controller"}"#;
        let event = parse_app_log_line(line).unwrap();
        assert_eq!(event.text, "action failed: no such worktree");
        assert_eq!(event.source, "wt");
    }

    #[test]
    fn internal_telemetry_never_reaches_the_feeds() {
        for line in [
            r#"{"timestamp":"2026-10-09T12:00:00Z","level":"INFO","fields":{"message":"input latency","ms":12},"target":"wt_tui::terminal"}"#,
            r#"{"timestamp":"2026-10-09T12:00:00Z","level":"INFO","fields":{"message":"terminal stopped"},"target":"wt_tui::terminal::driver"}"#,
            r#"{"timestamp":"2026-10-09T12:00:00Z","level":"ERROR","fields":{"message":"terminal stopped","error":"eof"},"target":"wt_tui::terminal"}"#,
            r#"{"timestamp":"2026-10-09T12:00:00Z","level":"WARN","fields":{"message":"activity feeds use refresh backstop"},"target":"wt::activity_source"}"#,
            r#"{"timestamp":"2026-10-09T12:00:00Z","level":"ERROR","fields":{"message":"connection reset"},"target":"hyper::proto"}"#,
            r#"{"timestamp":"2026-10-09T12:00:00Z","level":"WARN","fields":{"message":"filesystem watcher unavailable; using refresh backstop"},"target":"wt::freshness"}"#,
            r#"{"timestamp":"2026-10-09T12:00:00Z","level":"WARN","fields":{"message":"removed-worktree history refresh failed"},"target":"wt::history_source"}"#,
        ] {
            assert_eq!(parse_app_log_line(line), None, "{line}");
        }
    }

    #[test]
    fn a_background_failure_warning_reaches_the_activity_feed() {
        let line = r#"{"timestamp":"2026-10-09T12:00:00Z","level":"WARN","fields":{"message":"background merge-when-ready retry failed","error":"HTTP 502","event_channel":"activity"},"target":"wt::github_actions"}"#;
        let event = parse_app_log_line(line).unwrap();
        assert_eq!(
            event.text,
            "background merge-when-ready retry failed: HTTP 502"
        );
        assert_eq!(event.channel, "activity");
    }

    #[test]
    fn structured_events_never_show_a_module_path_source() {
        let line = r#"{"timestamp":"2026-10-09T12:00:00Z","level":"INFO","fields":{"event_text":"fetched git origin (87ms)"},"target":"wt::origin"}"#;
        let event = parse_app_log_line(line).unwrap();
        assert_eq!(event.source, "wt");
        assert_eq!(event.text, "fetched git origin (87ms)");
    }

    #[test]
    fn identical_consecutive_attention_lines_collapse() {
        let line = |at_ms, text: &str| ActivityLine {
            at_ms,
            level: "ERROR".into(),
            channel: "activity".into(),
            source: "wt".into(),
            text: text.into(),
        };
        let attention = attention_from_activity(&[
            line(1_000, "action failed: offline"),
            line(60_000, "action failed: offline"),
            line(61_000, "feature: ready"),
            line(90_000, "action failed: offline"),
        ]);
        assert_eq!(
            attention.iter().map(|line| line.at_ms).collect::<Vec<_>>(),
            [60_000, 61_000, 90_000],
            "a recurring failure keeps its latest time"
        );
        let mut board = wt_tui::Board::default();
        append_attention_at(&mut board, epoch_ms(), "wt", "same");
        board.attention.push(AttentionLine {
            at_ms: board.attention[0].at_ms + 60_000,
            source: "wt".into(),
            text: "same".into(),
        });
        bound_feeds(&mut board);
        assert_eq!(board.attention.len(), 1);
    }

    #[test]
    fn parses_structured_native_log_fields_and_error_attention() {
        let line = r#"{"timestamp":"2026-10-09T12:00:00Z","level":"INFO","fields":{"event_at_ms":10,"event_channel":"attention","event_source":"slug","event_text":"ready"},"target":"wt_attention"}"#;
        let parsed = parse_app_log_line(line).unwrap();
        assert_eq!(parsed.at_ms, 10);
        assert_eq!(parsed.source, "slug");
        assert_eq!(parsed.channel, "attention");
        let mut error = parsed.clone();
        error.level = "ERROR".into();
        error.channel = "activity".into();
        assert_eq!(attention_from_activity(&[error]).len(), 1);
    }

    #[test]
    fn bounded_feed_keeps_independent_attention_tail() {
        let activity = (0..MAX_ACTIVITY + 10)
            .map(|at_ms| ActivityLine {
                at_ms: at_ms as u64,
                level: if at_ms % 2 == 0 { "ERROR" } else { "INFO" }.into(),
                channel: "activity".into(),
                source: "test".into(),
                text: at_ms.to_string(),
            })
            .collect::<Vec<_>>();
        assert_eq!(activity.len(), MAX_ACTIVITY + 10);
        assert_eq!(attention_from_activity(&activity).len(), MAX_ATTENTION);
    }

    #[test]
    fn duplicate_attention_from_multiple_processes_collapses_only_nearby_lines() {
        let line = |at_ms| ActivityLine {
            at_ms,
            level: "INFO".into(),
            channel: "attention".into(),
            source: "wt".into(),
            text: "feature: ready".into(),
        };
        let mut events = vec![line(1_000), line(4_000), line(8_000)];
        dedupe_repeats(&mut events);
        assert_eq!(
            events.iter().map(|event| event.at_ms).collect::<Vec<_>>(),
            [1_000, 8_000]
        );
    }

    #[test]
    fn tail_cutoff_inside_unicode_scalar_skips_partial_record() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("wt-native.log");
        std::fs::write(&path, "12345678🙂discard\nretained\n").unwrap();
        let length = std::fs::metadata(&path).unwrap().len();
        let contents = read_tail(&path, length - 9).unwrap();
        assert_eq!(contents, "retained\n");
    }

    #[test]
    fn a_lasting_failure_keeps_its_first_time_until_a_compose_omits_it() {
        let mut clock = FailureClock::default();
        let compose = |clock: &mut FailureClock, failing: bool| {
            let mut board = wt_tui::Board::default();
            if failing {
                clock.attention(&mut board, "History", "History: database locked");
            }
            clock.sweep();
            board
        };
        let first = compose(&mut clock, true);
        std::thread::sleep(Duration::from_millis(3));
        // Recomposing an unchanged failure yields an identical board, so
        // `publish_if_changed` stays quiet and an acknowledgement sticks.
        assert_eq!(compose(&mut clock, true), first);
        compose(&mut clock, false);
        std::thread::sleep(Duration::from_millis(3));
        let recurred = compose(&mut clock, true);
        assert!(recurred.attention[0].at_ms > first.attention[0].at_ms);
    }
}
