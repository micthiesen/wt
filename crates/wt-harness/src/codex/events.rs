use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use serde_json::Value;

use super::{CodexHarnessError, Rollout, find_rollout, scan_rollouts};

const MAX_READ_PER_TICK: u64 = 32 * 1024;

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
}

#[derive(Clone, Default)]
pub struct CodexActivityTracker {
    by_slug: HashMap<String, RolloutCursor>,
}

pub(super) fn poll_activity(
    sessions_dir: &Path,
    tracker: &mut CodexActivityTracker,
    active: &[(String, PathBuf, Option<String>)],
) -> CodexActivityBatch {
    let active_slugs = active
        .iter()
        .map(|(slug, _, _)| slug.as_str())
        .collect::<HashSet<_>>();
    tracker
        .by_slug
        .retain(|slug, _| active_slugs.contains(slug.as_str()));
    let mut batch = CodexActivityBatch::default();
    for (slug, cwd, session_id) in active {
        if poll_slug(
            sessions_dir,
            tracker,
            slug,
            cwd,
            session_id.as_deref(),
            &mut batch,
        ) {
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
    let rollout = resolve_rollout(sessions_dir, cwd, slug, session_id);
    let Some(rollout) = rollout else {
        return false;
    };
    if tracker
        .by_slug
        .get(slug)
        .is_none_or(|state| state.path != rollout.path)
    {
        tracker.by_slug.insert(
            slug.to_owned(),
            RolloutCursor {
                path: rollout.path,
                offset: rollout.size,
                mtime_ms: rollout.mtime_ms,
            },
        );
        return true;
    }
    let Some(state) = tracker.by_slug.get_mut(slug) else {
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
    let Some(last_newline) = bytes.iter().rposition(|byte| *byte == b'\n') else {
        state.mtime_ms = rollout.mtime_ms;
        return true;
    };
    state.offset += (last_newline + 1) as u64;
    state.mtime_ms = rollout.mtime_ms;
    for line in bytes[..last_newline].split(|byte| *byte == b'\n') {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let Ok(event) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        emit_event(&event, slug, &mut batch.events);
    }
    true
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
