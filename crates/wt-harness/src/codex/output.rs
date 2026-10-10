//! Bounded detailed output tails for exact Codex rollout identities.

use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::output::{one_line, text_lines};
use crate::{HarnessOutputKind, HarnessOutputLine, HarnessOutputTarget, HarnessOutputUpdate};

use super::{CodexHarness, CodexHarnessError, find_rollout_cancellable};

const SEED_BYTES: u64 = 48 * 1024;
const MAX_READ_PER_TICK: usize = 32 * 1024;
const MAX_BACKLOG_BYTES: u64 = 8 * 1024 * 1024;
const MAX_PENDING_LINE: usize = 256 * 1024;
const MAX_LINES_PER_POLL: usize = 256;

#[derive(Clone, Debug, Default)]
struct Cursor {
    path: Option<PathBuf>,
    offset: u64,
    pending: Vec<u8>,
    drop_first_fragment: bool,
    next_line_id: u64,
}

#[derive(Clone, Debug, Default)]
pub struct CodexOutputTracker {
    by_session: HashMap<(String, String), Cursor>,
}

impl CodexHarness {
    /// Read one bounded incremental output slice per target. The caller should
    /// run this on a blocking worker. Cancellation is checked between targets,
    /// directory entries, and before each file read.
    pub fn poll_output(
        &self,
        tracker: &mut CodexOutputTracker,
        targets: &[HarnessOutputTarget],
        cancellation: &CancellationToken,
    ) -> Result<Vec<HarnessOutputUpdate>, CodexHarnessError> {
        let active = targets
            .iter()
            .map(|target| (target.slug.as_str(), target.session_id.as_str()))
            .collect::<HashSet<_>>();
        tracker
            .by_session
            .retain(|(slug, session_id), _| active.contains(&(slug.as_str(), session_id.as_str())));

        let mut updates = Vec::new();
        for target in targets {
            if cancellation.is_cancelled() {
                return Err(cancelled());
            }
            let key = (target.slug.clone(), target.session_id.clone());
            let cursor = tracker.by_session.entry(key).or_default();
            if let Some(update) =
                read_target(&self.paths().sessions_dir(), cursor, target, cancellation)?
            {
                updates.push(update);
            }
        }
        Ok(updates)
    }

    /// Async adapter for callers that do not already own a blocking worker.
    /// It returns the updated tracker with the batch so errors never silently
    /// discard cursor state. The synchronous operation checks cancellation at
    /// bounded intervals and reads no more than 32 KiB per target per call.
    pub async fn poll_output_async(
        &self,
        mut tracker: CodexOutputTracker,
        targets: Vec<HarnessOutputTarget>,
        cancellation: &CancellationToken,
    ) -> (
        CodexOutputTracker,
        Result<Vec<HarnessOutputUpdate>, CodexHarnessError>,
    ) {
        let harness = self.clone();
        let cancel = cancellation.clone();
        let fallback = tracker.clone();
        let task = tokio::task::spawn_blocking(move || {
            let result = harness.poll_output(&mut tracker, &targets, &cancel);
            (tracker, result)
        });
        match task.await {
            Ok(result) => result,
            Err(error) => (
                fallback,
                Err(CodexHarnessError::Operation {
                    operation: "read output",
                    detail: error.to_string(),
                }),
            ),
        }
    }
}

impl CodexOutputTracker {
    /// Exact rollout files whose cursors are currently retained. Callers may
    /// watch these paths for append notifications while keeping a bounded
    /// polling fallback for missed filesystem events.
    pub fn watched_paths(&self) -> impl Iterator<Item = &std::path::Path> {
        self.by_session
            .values()
            .filter_map(|cursor| cursor.path.as_deref())
    }
}

fn read_target(
    sessions_dir: &std::path::Path,
    cursor: &mut Cursor,
    target: &HarnessOutputTarget,
    cancellation: &CancellationToken,
) -> Result<Option<HarnessOutputUpdate>, CodexHarnessError> {
    let mut reset = false;
    let mut path = cursor.path.clone();
    if path.is_none() {
        let rollout = find_rollout_cancellable(
            sessions_dir,
            &target.cwd,
            &target.slug,
            &target.session_id,
            Some(cancellation),
        )?;
        let Some(rollout) = rollout else {
            return Ok(None);
        };
        path = Some(rollout.path);
        cursor.offset = rollout.size.saturating_sub(SEED_BYTES);
        cursor.drop_first_fragment = cursor.offset > 0;
        reset = true;
    }
    let path = path.expect("cursor path assigned above");
    if cancellation.is_cancelled() {
        return Err(cancelled());
    }
    let metadata = fs::metadata(&path).map_err(|error| CodexHarnessError::Operation {
        operation: "read output",
        detail: format!("{}: {error}", path.display()),
    })?;
    if cursor
        .path
        .as_ref()
        .is_some_and(|previous| previous != &path)
    {
        cursor.offset = 0;
        cursor.pending.clear();
        cursor.drop_first_fragment = false;
        reset = true;
    }
    cursor.path = Some(path.clone());
    if metadata.len() < cursor.offset {
        cursor.offset = 0;
        cursor.pending.clear();
        cursor.drop_first_fragment = false;
        reset = true;
    }
    if metadata.len().saturating_sub(cursor.offset) > MAX_BACKLOG_BYTES {
        cursor.offset = metadata.len().saturating_sub(SEED_BYTES);
        cursor.pending.clear();
        cursor.drop_first_fragment = cursor.offset > 0;
        reset = true;
    }
    let read_len = metadata
        .len()
        .saturating_sub(cursor.offset)
        .min(MAX_READ_PER_TICK as u64) as usize;
    let mut lines = Vec::new();
    if read_len > 0 {
        if cancellation.is_cancelled() {
            return Err(cancelled());
        }
        let mut file = File::open(&path).map_err(|error| CodexHarnessError::Operation {
            operation: "read output",
            detail: format!("{}: {error}", path.display()),
        })?;
        file.seek(SeekFrom::Start(cursor.offset)).map_err(|error| {
            CodexHarnessError::Operation {
                operation: "seek output",
                detail: format!("{}: {error}", path.display()),
            }
        })?;
        let read_start = cursor.offset;
        let mut bytes = vec![0; read_len];
        let count = file
            .read(&mut bytes)
            .map_err(|error| CodexHarnessError::Operation {
                operation: "read output",
                detail: format!("{}: {error}", path.display()),
            })?;
        bytes.truncate(count);
        cursor.offset = cursor.offset.saturating_add(count as u64);
        let mut id = cursor.next_line_id;
        let mut next_id = || {
            id = id.saturating_add(1);
            id
        };
        for (index, byte) in bytes.into_iter().enumerate() {
            if byte == b'\n' {
                if !cursor.drop_first_fragment
                    && !cursor.pending.iter().all(u8::is_ascii_whitespace)
                    && let Ok(value) = serde_json::from_slice::<Value>(&cursor.pending)
                {
                    parse_event(&value, &mut next_id, &mut lines);
                }
                cursor.pending.clear();
                cursor.drop_first_fragment = false;
                if lines.len() >= MAX_LINES_PER_POLL {
                    cursor.offset = read_start.saturating_add(index as u64 + 1);
                    break;
                }
            } else if cursor.pending.len() < MAX_PENDING_LINE {
                cursor.pending.push(byte);
            } else {
                cursor.pending.clear();
                cursor.drop_first_fragment = true;
            }
        }
        cursor.next_line_id = id;
    }
    if !reset && lines.is_empty() {
        return Ok(None);
    }
    Ok(Some(HarnessOutputUpdate {
        slug: target.slug.clone(),
        session_id: target.session_id.clone(),
        reset,
        append: lines,
    }))
}

fn parse_event(
    event: &Value,
    next_id: &mut impl FnMut() -> u64,
    output: &mut Vec<HarnessOutputLine>,
) {
    let timestamp_ms = event["timestamp"]
        .as_str()
        .and_then(parse_timestamp_ms)
        .unwrap_or_else(now_ms);
    let payload = &event["payload"];
    match event["type"].as_str() {
        Some("event_msg") => match payload["type"].as_str() {
            Some("user_message") => {
                if let Some(message) = payload["message"].as_str() {
                    output.extend(text_lines(
                        message,
                        HarnessOutputKind::User,
                        timestamp_ms,
                        next_id,
                        "› ",
                    ));
                }
            }
            Some("agent_message") => {
                if let Some(message) = payload["message"].as_str() {
                    output.extend(text_lines(
                        message,
                        HarnessOutputKind::Assistant,
                        timestamp_ms,
                        next_id,
                        "",
                    ));
                }
            }
            Some("web_search_end") => {
                if let Some(query) = payload["query"].as_str() {
                    output.push(one_line(
                        &format!("⚒ web: {query}"),
                        HarnessOutputKind::Tool,
                        timestamp_ms,
                        next_id,
                    ));
                }
            }
            Some("turn_aborted") => output.push(one_line(
                "⊘ turn interrupted",
                HarnessOutputKind::Info,
                timestamp_ms,
                next_id,
            )),
            _ => {}
        },
        Some("response_item") => match payload["type"].as_str() {
            Some("function_call") => {
                let name = payload["name"].as_str().unwrap_or("tool");
                let label = match name {
                    "exec_command" | "shell" => extract_command(&payload["arguments"]),
                    "apply_patch" => "apply_patch".to_owned(),
                    _ => name.to_owned(),
                };
                output.push(one_line(
                    &format!("⚒ {label}"),
                    HarnessOutputKind::Tool,
                    timestamp_ms,
                    next_id,
                ));
            }
            Some("reasoning") => {
                if let Some(text) = payload["summary"]
                    .as_array()
                    .and_then(|items| items.first().and_then(|item| item["text"].as_str()))
                {
                    output.extend(
                        text_lines(
                            text,
                            HarnessOutputKind::Thinking,
                            timestamp_ms,
                            next_id,
                            "… ",
                        )
                        .into_iter()
                        .take(1),
                    );
                }
            }
            _ => {}
        },
        _ => {}
    }
}

fn extract_command(arguments: &Value) -> String {
    let parsed = match arguments {
        Value::String(text) => serde_json::from_str::<Value>(text).unwrap_or(Value::Null),
        value => value.clone(),
    };
    let command = parsed.get("cmd").or_else(|| parsed.get("command"));
    match command {
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| item.to_string())
            })
            .collect::<Vec<_>>()
            .join(" "),
        Some(Value::String(text)) => text.clone(),
        _ => match arguments {
            Value::String(text) if !text.is_empty() => text.clone(),
            _ => "<command>".into(),
        },
    }
}

fn parse_timestamp_ms(value: &str) -> Option<i64> {
    let parsed =
        time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339).ok()?;
    Some(
        (parsed.unix_timestamp_nanos() / 1_000_000).clamp(i64::MIN as i128, i64::MAX as i128)
            as i64,
    )
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn cancelled() -> CodexHarnessError {
    CodexHarnessError::Operation {
        operation: "read output",
        detail: "cancelled".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn rollout(root: &std::path::Path, id: &str, cwd: &str) -> PathBuf {
        let path = root
            .join("sessions/2021/01/02")
            .join(format!("rollout-2021-01-02T00-00-00-{id}.jsonl"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let header = serde_json::json!({
            "type":"session_meta", "timestamp":"2021-01-02T00:00:00Z",
            "payload":{"id":id,"cwd":cwd,"originator":"codex-tui","thread_source":"user"}
        });
        fs::write(&path, format!("{header}\n")).unwrap();
        path
    }

    #[test]
    fn exact_resumed_uuid_finds_old_day_and_carries_partial_jsonl_lines() {
        let dir = tempdir().unwrap();
        let id = "thread-resumed-uuid";
        let cwd = dir.path().join("repo");
        let path = rollout(dir.path(), id, cwd.to_str().unwrap());
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        use std::io::Write;
        writeln!(file, "{{\"type\":\"event_msg\",\"timestamp\":\"2021-01-02T00:00:01Z\",\"payload\":{{\"type\":\"user_message\",\"message\":\"first prompt\"}}}}").unwrap();
        let target = HarnessOutputTarget {
            slug: "feature".into(),
            cwd: cwd.clone(),
            session_id: id.into(),
        };
        let mut cursor = Cursor::default();
        let cancel = CancellationToken::new();
        let initial = read_target(&dir.path().join("sessions"), &mut cursor, &target, &cancel)
            .unwrap()
            .unwrap();
        assert!(initial.reset);
        assert_eq!(initial.session_id, id);
        let tracker = CodexOutputTracker {
            by_session: HashMap::from([(
                (target.slug.clone(), target.session_id.clone()),
                cursor.clone(),
            )]),
        };
        assert_eq!(
            tracker.watched_paths().collect::<Vec<_>>(),
            [path.as_path()]
        );
        assert!(
            initial
                .append
                .iter()
                .any(|line| line.text.contains("first prompt"))
        );

        let partial = br#"{"type":"event_msg","timestamp":"2021-01-02T00:00:02Z","payload":{"type":"agent_message","message":"second answer"}}"#;
        file.write_all(partial).unwrap();
        file.flush().unwrap();
        assert!(
            read_target(&dir.path().join("sessions"), &mut cursor, &target, &cancel)
                .unwrap()
                .is_none()
        );
        file.write_all(b"\n").unwrap();
        let next = read_target(&dir.path().join("sessions"), &mut cursor, &target, &cancel)
            .unwrap()
            .unwrap();
        assert!(!next.reset);
        assert_eq!(next.append.len(), 1);
        assert!(next.append[0].text.contains("second answer"));
    }

    #[test]
    fn truncation_resets_visible_tail_and_cancellation_is_reported() {
        let dir = tempdir().unwrap();
        let path = rollout(dir.path(), "thread-one", "/repo");
        use std::io::Write;
        writeln!(
            fs::OpenOptions::new().append(true).open(&path).unwrap(),
            "{}",
            serde_json::json!({
                "type":"event_msg","timestamp":"2021-01-02T00:00:01Z",
                "payload":{"type":"agent_message","message":"seed"}
            })
        )
        .unwrap();
        let target = HarnessOutputTarget {
            slug: "feature".into(),
            cwd: "/repo".into(),
            session_id: "thread-one".into(),
        };
        let mut cursor = Cursor::default();
        let cancel = CancellationToken::new();
        read_target(&dir.path().join("sessions"), &mut cursor, &target, &cancel).unwrap();
        fs::write(&path, b"\n").unwrap();
        let update = read_target(&dir.path().join("sessions"), &mut cursor, &target, &cancel)
            .unwrap()
            .unwrap();
        assert!(update.reset);

        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(
            read_target(
                &dir.path().join("sessions"),
                &mut cursor,
                &target,
                &cancelled
            )
            .is_err()
        );
    }
}
