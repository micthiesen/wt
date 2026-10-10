//! Bounded app-log backfill and manager reports for the output feeds.
use std::{
    collections::HashMap,
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use notify::Watcher;
use serde_json::Value;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use wt_runtime::{RefreshPolicy, SourceHandle, TaskScope, start_source};
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

pub(crate) fn append_activity(board: &mut wt_tui::Board, level: &str, source: &str, text: &str) {
    let event = ActivityLine {
        at_ms: epoch_ms(),
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

pub(crate) fn append_attention(board: &mut wt_tui::Board, source: &str, text: &str) {
    let at_ms = epoch_ms();
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
    board
        .attention
        .dedup_by(|later, earlier| later.source == earlier.source && later.text == earlier.text);
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
) -> SourceHandle<ActivitySnapshot> {
    let manager_dir = cache_root.join("manager");
    let reports = manager_dir.join("reports.jsonl");
    let watched_log_dir = app_log_dir.clone();
    let source = start_source(
        scope,
        RefreshPolicy {
            debounce: Duration::from_millis(50),
            minimum_interval: Duration::from_millis(100),
        },
        move |_| {
            let reports = reports.clone();
            let app_log_dir = app_log_dir.clone();
            async move {
                tokio::task::spawn_blocking(move || read_snapshot(&reports, &app_log_dir)).await?
            }
        },
    );
    let observed = source.clone();
    let cancellation = scope.token();
    scope.spawn(async move {
        let callback = observed.clone();
        let watcher = tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&manager_dir)?;
            std::fs::create_dir_all(&watched_log_dir)?;
            let mut watcher = notify::recommended_watcher(
                move |event: notify::Result<notify::Event>| match event {
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
                },
            )?;
            watcher.watch(&manager_dir, notify::RecursiveMode::NonRecursive)?;
            watcher.watch(&watched_log_dir, notify::RecursiveMode::NonRecursive)?;
            Ok::<_, anyhow::Error>(watcher)
        })
        .await;
        let _watcher = match watcher {
            Ok(Ok(watcher)) => Some(watcher),
            error => {
                tracing::warn!(?error, "activity feeds use refresh backstop");
                None
            }
        };
        observed.refresh();
        let period = Duration::from_secs(30);
        let mut backstop = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        backstop.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = cancellation.cancelled() => break,
                _ = backstop.tick() => { observed.refresh(); },
            }
        }
    });
    source
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
    // carries the text as its message.
    let event_text = fields
        .get("event_text")
        .and_then(Value::as_str)
        .or_else(|| {
            fields
                .get("event_channel")
                .and(fields.get("message"))
                .and_then(Value::as_str)
        });
    let (source, text, channel) = match event_text {
        Some(text) => (
            fields
                .get("event_source")
                .and_then(Value::as_str)
                .filter(|source| !source.trim().is_empty())
                .unwrap_or("wt")
                .to_owned(),
            text.to_owned(),
            fields
                .get("event_channel")
                .and_then(Value::as_str)
                .unwrap_or("activity")
                .to_owned(),
        ),
        None => {
            let target = value.get("target").and_then(Value::as_str).unwrap_or("");
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
    attention
        .dedup_by(|later, earlier| later.source == earlier.source && later.text == earlier.text);
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
        ] {
            assert_eq!(parse_app_log_line(line), None, "{line}");
        }
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
            [1_000, 61_000, 90_000]
        );
        let mut board = wt_tui::Board::default();
        append_attention(&mut board, "wt", "same");
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
}
