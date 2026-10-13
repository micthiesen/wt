use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use serde_json::Value;

use super::{CodexHarnessError, Rollout, find_rollout, read_rollout, scan_rollouts, slot_owns};

const MAX_READ_PER_TICK: u64 = 32 * 1024;
// Activity previews do not need arbitrarily large tool payloads. Bound each
// session's unfinished record while still consuming through its terminator.
const MAX_RECORD_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodexEventLevel {
    Info,
    Dim,
    Ok,
    Warn,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexEvent {
    pub level: CodexEventLevel,
    pub text: String,
}

/// A response is produced for every poll, including baseline and empty polls.
/// `changed_slugs` also carries state changes without a displayable event.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CodexActivityBatch {
    pub events: Vec<CodexEvent>,
    pub changed_slugs: Vec<String>,
}

#[derive(Clone, Debug)]
struct RolloutCursor {
    path: PathBuf,
    offset: u64,
    mtime_ms: i64,
    partial: Vec<u8>,
    skipping_record: bool,
}

#[derive(Clone, Default)]
pub struct CodexActivityTracker {
    by_session: HashMap<(String, PathBuf, Option<String>), RolloutCursor>,
}

pub(super) fn poll_activity(
    sessions_dir: &Path,
    tracker: &mut CodexActivityTracker,
    active: &[(String, PathBuf, Option<String>)],
) -> CodexActivityBatch {
    let active_keys = active.iter().cloned().collect::<HashSet<_>>();
    tracker
        .by_session
        .retain(|key, _| active_keys.contains(key));
    let mut batch = CodexActivityBatch::default();
    for (slug, cwd, session_id) in active {
        if poll_slug(
            sessions_dir,
            tracker,
            slug,
            cwd,
            session_id.as_deref(),
            &mut batch,
        ) && !batch.changed_slugs.contains(slug)
        {
            batch.changed_slugs.push(slug.clone());
        }
    }
    batch
}

fn poll_slug(
    sessions_dir: &Path,
    tracker: &mut CodexActivityTracker,
    slug: &str,
    cwd: &Path,
    session_id: Option<&str>,
    batch: &mut CodexActivityBatch,
) -> bool {
    let key = (
        slug.to_owned(),
        cwd.to_owned(),
        session_id.map(str::to_owned),
    );
    // A live UUID is pinned. Revalidate its retained path instead of walking
    // the entire historical session tree on every activity tick.
    let retained = session_id.and_then(|id| {
        let state = tracker.by_session.get(&key)?;
        let rollout = read_rollout(&state.path).ok().flatten()?;
        (rollout.session_id == id
            && rollout.cwd == cwd.to_string_lossy()
            && slot_owns(&rollout.path, rollout.size, slug).ok()?)
        .then_some(rollout)
    });
    let rollout = retained.or_else(|| resolve_rollout(sessions_dir, cwd, slug, session_id));
    let Some(rollout) = rollout else {
        return false;
    };
    if tracker
        .by_session
        .get(&key)
        .is_none_or(|state| state.path != rollout.path)
    {
        tracker.by_session.insert(
            key.clone(),
            RolloutCursor {
                path: rollout.path,
                offset: rollout.size,
                mtime_ms: rollout.mtime_ms,
                partial: Vec::new(),
                skipping_record: false,
            },
        );
        return true;
    }
    let Some(state) = tracker.by_session.get_mut(&key) else {
        return false;
    };
    let current_size = match fs::metadata(&rollout.path) {
        Ok(metadata) => metadata.len(),
        Err(_) => return false,
    };
    if current_size <= state.offset {
        if current_size < state.offset {
            state.offset = current_size;
            state.mtime_ms = rollout.mtime_ms;
            state.partial.clear();
            state.skipping_record = false;
            return true;
        }
        state.mtime_ms = rollout.mtime_ms;
        return false;
    }
    let read_len = (current_size - state.offset).min(MAX_READ_PER_TICK) as usize;
    let mut file = match File::open(&rollout.path) {
        Ok(file) => file,
        Err(_) => return false,
    };
    if file.seek(SeekFrom::Start(state.offset)).is_err() {
        return false;
    }
    let mut bytes = vec![0; read_len];
    if file.read_exact(&mut bytes).is_err() {
        return false;
    }
    state.offset += read_len as u64;
    state.mtime_ms = rollout.mtime_ms;
    let mut completed = false;
    for chunk in bytes.split_inclusive(|byte| *byte == b'\n') {
        let complete = chunk.last() == Some(&b'\n');
        let fragment = if complete {
            &chunk[..chunk.len() - 1]
        } else {
            chunk
        };
        if !state.skipping_record {
            if state.partial.len() + fragment.len() > MAX_RECORD_BYTES {
                state.partial.clear();
                state.skipping_record = true;
            } else {
                state.partial.extend_from_slice(fragment);
            }
        }
        if complete {
            completed = true;
            if !state.skipping_record
                && let Ok(event) = serde_json::from_slice::<Value>(&state.partial)
            {
                emit_event(&event, slug, &mut batch.events);
            }
            state.partial.clear();
            state.skipping_record = false;
        }
    }
    // Consuming an unfinished fragment must not feed discovery back into this
    // poll. The regular activity tick will consume the next bounded fragment.
    completed
}

fn resolve_rollout(
    sessions_dir: &Path,
    cwd: &Path,
    slug: &str,
    session_id: Option<&str>,
) -> Option<Rollout> {
    let result: Result<Option<Rollout>, CodexHarnessError> = match session_id {
        Some(id) => find_rollout(sessions_dir, cwd, slug, id),
        None => scan_rollouts(sessions_dir, cwd, slug)
            .map(|rollouts| rollouts.into_iter().max_by_key(|rollout| rollout.mtime_ms)),
    };
    result.ok().flatten()
}

fn emit_event(event: &Value, slug: &str, events: &mut Vec<CodexEvent>) {
    match event["type"].as_str() {
        Some("event_msg") => {
            let payload = &event["payload"];
            let Some(kind) = payload["type"].as_str() else {
                return;
            };
            match kind {
                "task_started" => push(
                    events,
                    CodexEventLevel::Info,
                    format!("turn started · {slug}"),
                ),
                "task_complete" => {
                    let duration = payload["duration_ms"]
                        .as_u64()
                        .map(format_duration)
                        .unwrap_or_else(|| "?ms".into());
                    push(
                        events,
                        CodexEventLevel::Ok,
                        format!("turn done in {duration} · {slug}"),
                    );
                }
                "turn_aborted" => push(
                    events,
                    CodexEventLevel::Warn,
                    format!("turn interrupted · {slug}"),
                ),
                "user_message" => {
                    if let Some(message) = payload["message"].as_str().filter(|s| !s.is_empty()) {
                        push(
                            events,
                            CodexEventLevel::Dim,
                            format!("-> {} · {slug}", preview(message)),
                        );
                    }
                }
                "mcp_tool_call_end" => {
                    if payload["invocation"].is_object() {
                        let server = payload["invocation"]["server"].as_str().unwrap_or("?");
                        let tool = payload["invocation"]["tool"].as_str().unwrap_or("?");
                        push(
                            events,
                            CodexEventLevel::Info,
                            format!("mcp: {server}.{tool} · {slug}"),
                        );
                    }
                }
                "web_search_end" => {
                    if let Some(query) = payload["query"].as_str() {
                        push(
                            events,
                            CodexEventLevel::Info,
                            format!("web: {} · {slug}", preview(query)),
                        );
                    }
                }
                "token_count" if !payload["rate_limits"]["rate_limit_reached_type"].is_null() => {
                    push(
                        events,
                        CodexEventLevel::Warn,
                        format!("rate limit hit · {slug}"),
                    );
                }
                _ => (),
            }
        }
        Some("response_item") => {
            let payload = &event["payload"];
            if payload["type"] != "function_call" || payload["name"] != "exec_command" {
                return;
            }
            let args = payload["arguments"].as_str().unwrap_or_default();
            let command = serde_json::from_str::<Value>(args)
                .ok()
                .and_then(|args| {
                    args["cmd"]
                        .as_str()
                        .or(args["command"].as_str())
                        .map(str::to_owned)
                })
                .unwrap_or_else(|| {
                    if args.is_empty() {
                        "<command>".into()
                    } else {
                        args.to_owned()
                    }
                });
            push(
                events,
                CodexEventLevel::Info,
                format!("exec: {} · {slug}", preview(&command)),
            );
        }
        _ => (),
    }
}

fn push(events: &mut Vec<CodexEvent>, level: CodexEventLevel, text: String) {
    events.push(CodexEvent { level, text });
}

fn preview(value: &str) -> String {
    let mut chars = value.chars();
    let prefix = chars.by_ref().take(60).collect::<String>();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

fn format_duration(milliseconds: u64) -> String {
    if milliseconds < 1_000 {
        format!("{milliseconds}ms")
    } else if milliseconds < 60_000 {
        format!("{:.1}s", milliseconds as f64 / 1_000.0)
    } else {
        format!(
            "{}m {}s",
            milliseconds / 60_000,
            (milliseconds % 60_000) / 1_000
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn append(path: &Path, event: Value) {
        use std::io::Write;
        let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
        writeln!(file, "{event}").unwrap();
        file.sync_all().unwrap();
    }

    #[test]
    fn each_poll_returns_a_batch_and_baseline_changes_the_slot_without_events() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("repo");
        let path = dir.path().join("sessions/2026/10/09/rollout-thread.jsonl");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let header = json!({"type":"session_meta","payload":{"id":"thread","cwd":cwd.to_string_lossy(),"originator":"codex-tui","thread_source":"user"}});
        fs::write(&path, format!("{header}\n")).unwrap();
        let active = vec![("feature".to_owned(), cwd.clone(), Some("thread".to_owned()))];
        let mut tracker = CodexActivityTracker::default();
        let first = poll_activity(&dir.path().join("sessions"), &mut tracker, &active);
        assert!(first.events.is_empty());
        assert_eq!(first.changed_slugs, ["feature"]);
        let idle = poll_activity(&dir.path().join("sessions"), &mut tracker, &active);
        assert!(idle.events.is_empty());
        assert!(idle.changed_slugs.is_empty());
        append(
            &path,
            json!({"type":"event_msg","payload":{"type":"user_message","message":"hello"}}),
        );
        let update = poll_activity(&dir.path().join("sessions"), &mut tracker, &active);
        assert_eq!(update.changed_slugs, ["feature"]);
        assert_eq!(
            update.events,
            [CodexEvent {
                level: CodexEventLevel::Dim,
                text: "-> hello · feature".into()
            }]
        );
        let drained = poll_activity(&dir.path().join("sessions"), &mut tracker, &active);
        assert!(drained.events.is_empty());
        assert!(drained.changed_slugs.is_empty());
    }

    #[test]
    fn large_records_make_bounded_progress_and_emit_once() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("repo");
        let sessions = dir.path().join("sessions");
        let path = super::super::tests::write_rollout(
            &sessions,
            "2026/10/13",
            "thread",
            &cwd,
            "codex-tui",
            "user",
        );
        let active = vec![("feature".into(), cwd, Some("thread".into()))];
        let mut tracker = CodexActivityTracker::default();
        poll_activity(&sessions, &mut tracker, &active);
        let initial = tracker.by_session[&active[0]].offset;
        let event = json!({"type":"event_msg","payload":{"type":"user_message","message":"x".repeat(96 * 1024)}});
        let serialized = event.to_string();
        let mut writer = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writer.write_all(serialized.as_bytes()).unwrap();
        for _ in 0..5 {
            let before = tracker.by_session[&active[0]].offset;
            let batch = poll_activity(&sessions, &mut tracker, &active);
            assert!(batch.changed_slugs.is_empty());
            assert!(batch.events.is_empty());
            assert!(tracker.by_session[&active[0]].offset - before <= MAX_READ_PER_TICK);
        }
        assert_eq!(
            tracker.by_session[&active[0]].offset,
            initial + serialized.len() as u64
        );
        writer.write_all(b"\n").unwrap();
        let complete = poll_activity(&sessions, &mut tracker, &active);
        assert_eq!(complete.changed_slugs, ["feature"]);
        assert_eq!(complete.events.len(), 1);
        assert!(
            poll_activity(&sessions, &mut tracker, &active)
                .changed_slugs
                .is_empty()
        );
    }

    #[test]
    fn oversized_record_is_skipped_without_blocking_following_events() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("repo");
        let sessions = dir.path().join("sessions");
        let path = super::super::tests::write_rollout(
            &sessions,
            "2026/10/13",
            "thread",
            &cwd,
            "codex-tui",
            "user",
        );
        let active = vec![("feature".into(), cwd, Some("thread".into()))];
        let mut tracker = CodexActivityTracker::default();
        poll_activity(&sessions, &mut tracker, &active);
        append(
            &path,
            json!({"type":"event_msg","payload":{"type":"user_message","message":"x".repeat(MAX_RECORD_BYTES + 100)}}),
        );
        append(
            &path,
            json!({"type":"event_msg","payload":{"type":"user_message","message":"after oversized"}}),
        );
        let mut events = Vec::new();
        let mut changes = 0;
        for _ in 0..40 {
            let batch = poll_activity(&sessions, &mut tracker, &active);
            events.extend(batch.events);
            changes += batch.changed_slugs.len();
            assert!(tracker.by_session[&active[0]].partial.len() <= MAX_RECORD_BYTES);
        }
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].text, "-> after oversized · feature");
        assert_eq!(changes, 1);
        assert!(!tracker.by_session[&active[0]].skipping_record);
    }

    #[test]
    fn truncation_clears_partial_and_oversized_record_state() {
        use std::io::Write;
        for partial_size in [100, MAX_RECORD_BYTES + 100] {
            let dir = tempfile::tempdir().unwrap();
            let cwd = dir.path().join("repo");
            let sessions = dir.path().join("sessions");
            let path = super::super::tests::write_rollout(
                &sessions,
                "2026/10/13",
                "thread",
                &cwd,
                "codex-tui",
                "user",
            );
            let header = fs::read(&path).unwrap();
            let active = vec![("feature".into(), cwd, Some("thread".into()))];
            let mut tracker = CodexActivityTracker::default();
            poll_activity(&sessions, &mut tracker, &active);
            fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(&vec![b'x'; partial_size])
                .unwrap();
            for _ in 0..40 {
                assert!(
                    poll_activity(&sessions, &mut tracker, &active)
                        .changed_slugs
                        .is_empty()
                );
            }
            assert_eq!(
                tracker.by_session[&active[0]].skipping_record,
                partial_size > MAX_RECORD_BYTES
            );
            fs::write(&path, header).unwrap();
            assert_eq!(
                poll_activity(&sessions, &mut tracker, &active).changed_slugs,
                ["feature"]
            );
            assert!(tracker.by_session[&active[0]].partial.is_empty());
            assert!(!tracker.by_session[&active[0]].skipping_record);
            append(
                &path,
                json!({"type":"event_msg","payload":{"type":"user_message","message":"after truncation"}}),
            );
            let complete = poll_activity(&sessions, &mut tracker, &active);
            assert_eq!(complete.events.len(), 1);
            assert_eq!(complete.events[0].text, "-> after truncation · feature");
        }
    }

    #[test]
    fn unfinished_record_does_not_trigger_discovery_until_complete() {
        use std::io::Write;
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("repo");
        let sessions = root.path().join("sessions");
        let path = super::super::tests::write_rollout(
            &sessions,
            "2026/10/13",
            "thread",
            &cwd,
            "codex-tui",
            "user",
        );
        let active = vec![("feature".into(), cwd, Some("thread".into()))];
        let mut tracker = CodexActivityTracker::default();
        poll_activity(&sessions, &mut tracker, &active);
        let mut writer = fs::OpenOptions::new().append(true).open(&path).unwrap();
        write!(writer, "{{\"type\":\"event_msg\",\"payload\":").unwrap();
        writer.sync_all().unwrap();
        for _ in 0..5 {
            let batch = poll_activity(&sessions, &mut tracker, &active);
            assert!(batch.changed_slugs.is_empty());
            assert!(batch.events.is_empty());
        }
        writeln!(
            writer,
            "{{\"type\":\"user_message\",\"message\":\"hello\"}}}}"
        )
        .unwrap();
        writer.sync_all().unwrap();
        let complete = poll_activity(&sessions, &mut tracker, &active);
        assert_eq!(complete.changed_slugs, ["feature"]);
        assert_eq!(complete.events.len(), 1);
        assert!(
            poll_activity(&sessions, &mut tracker, &active)
                .changed_slugs
                .is_empty()
        );
    }

    #[test]
    fn multiple_sessions_in_one_slug_do_not_reset_each_others_activity_cursor() {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("repo");
        let sessions = root.path().join("sessions");
        let first = super::super::tests::write_rollout(
            &sessions,
            "2026/10/13",
            "first",
            &cwd,
            "codex-tui",
            "user",
        );
        super::super::tests::write_rollout(
            &sessions,
            "2026/10/13",
            "second",
            &cwd,
            "codex-tui",
            "user",
        );
        let mut active = vec![
            ("feature".into(), cwd.clone(), Some("first".into())),
            ("feature".into(), cwd, Some("second".into())),
        ];
        let mut tracker = CodexActivityTracker::default();
        assert_eq!(
            poll_activity(&sessions, &mut tracker, &active).changed_slugs,
            ["feature"]
        );
        for _ in 0..5 {
            active.reverse();
            let next = poll_activity(&sessions, &mut tracker, &active);
            assert!(
                next.changed_slugs.is_empty(),
                "history discovery must not feed another discovery forever"
            );
            assert!(next.events.is_empty());
        }
        append(
            &first,
            json!({"type":"event_msg","payload":{"type":"user_message","message":"new work"}}),
        );
        let update = poll_activity(&sessions, &mut tracker, &active);
        assert_eq!(update.changed_slugs, ["feature"]);
        assert_eq!(update.events.len(), 1);
        assert!(
            poll_activity(&sessions, &mut tracker, &active)
                .changed_slugs
                .is_empty()
        );
        active.truncate(1);
        poll_activity(&sessions, &mut tracker, &active);
        assert_eq!(tracker.by_session.len(), 1);
    }

    #[test]
    fn retained_activity_path_is_revalidated_when_live_identity_changes() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("repo");
        let sessions = dir.path().join("sessions");
        let first = super::super::tests::write_rollout(
            &sessions,
            "2026/10/13",
            "first",
            &cwd,
            "codex-tui",
            "user",
        );
        let second = super::super::tests::write_rollout(
            &sessions,
            "2026/10/13",
            "second",
            &cwd,
            "codex-tui",
            "user",
        );
        let mut tracker = CodexActivityTracker::default();
        let mut active = vec![("feature".into(), cwd, Some("first".into()))];
        poll_activity(&sessions, &mut tracker, &active);
        assert_eq!(tracker.by_session[&active[0]].path, first);
        active[0].2 = Some("second".into());
        poll_activity(&sessions, &mut tracker, &active);
        assert_eq!(tracker.by_session[&active[0]].path, second);
        append(
            &first,
            json!({"type":"event_msg","payload":{"type":"user_message","message":"wrong session"}}),
        );
        assert!(
            poll_activity(&sessions, &mut tracker, &active)
                .events
                .is_empty()
        );
        append(
            &second,
            json!({"type":"event_msg","payload":{"type":"user_message","message":"right session"}}),
        );
        assert_eq!(
            poll_activity(&sessions, &mut tracker, &active).events[0].text,
            "-> right session · feature"
        );
    }

    #[test]
    fn activity_tail_preserves_partial_jsonl_until_the_line_is_complete() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("repo");
        let path = dir.path().join("sessions/2026/10/09/rollout-thread.jsonl");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let header = json!({"type":"session_meta","payload":{"id":"thread","cwd":cwd.to_string_lossy(),"originator":"codex-tui","thread_source":"user"}});
        fs::write(&path, format!("{header}\n")).unwrap();
        let active = vec![("feature".to_owned(), cwd, Some("thread".to_owned()))];
        let mut tracker = CodexActivityTracker::default();
        poll_activity(&dir.path().join("sessions"), &mut tracker, &active);
        use std::io::Write;
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"type\":\"event_msg\",")
            .unwrap();
        let partial = poll_activity(&dir.path().join("sessions"), &mut tracker, &active);
        assert!(partial.events.is_empty());
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "\"payload\":{{\"type\":\"task_started\"}}}}").unwrap();
        let complete = poll_activity(&dir.path().join("sessions"), &mut tracker, &active);
        assert_eq!(complete.events.len(), 1);
        assert_eq!(complete.events[0].level, CodexEventLevel::Info);
    }
}
