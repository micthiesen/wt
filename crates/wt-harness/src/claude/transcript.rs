use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use serde_json::Value;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LastEntryKind {
    ToolUse,
    ToolResult,
    Paused,
    EndTurn,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionTail {
    pub name: Option<String>,
    pub has_jsonl: bool,
    pub last_entry_ms: Option<i64>,
    pub last_entry_kind: Option<LastEntryKind>,
    pub queued: u32,
    pub pending_ask: Option<String>,
    pub last_assistant_text: Option<String>,
    pub session_summary: Option<String>,
    pub context_usage: Option<SessionContextUsage>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionContextUsage {
    /// Prompt tokens currently occupying the conversation window.
    pub tokens: u64,
    pub model: Option<String>,
}

impl SessionContextUsage {
    pub fn percent(&self) -> u8 {
        let window = self
            .model
            .as_deref()
            .filter(|model| model.to_ascii_lowercase().contains("haiku"))
            .map_or(1_000_000u128, |_| 200_000u128);
        let scaled = (u128::from(self.tokens) * 100 + window / 2) / window;
        scaled.min(100) as u8
    }
}

impl SessionTail {
    pub fn empty(name: Option<String>) -> Self {
        Self {
            name,
            has_jsonl: false,
            last_entry_ms: None,
            last_entry_kind: None,
            queued: 0,
            pending_ask: None,
            last_assistant_text: None,
            session_summary: None,
            context_usage: None,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ClaudeStatus {
    pub sessions: Vec<SessionTail>,
}

/// Incremental, bounded-memory JSONL reader for live Claude transcripts.
/// Partial final records are retained until a later poll completes the line.
#[derive(Debug, Default)]
pub struct TranscriptFollower {
    path: Option<PathBuf>,
    offset: u64,
    pending: Vec<u8>,
    max_line_bytes: usize,
    dropping_oversized_line: bool,
}

impl TranscriptFollower {
    pub fn new(max_line_bytes: usize) -> Self {
        Self {
            path: None,
            offset: 0,
            pending: Vec::new(),
            max_line_bytes: max_line_bytes.max(1024),
            dropping_oversized_line: false,
        }
    }

    pub fn read_new(&mut self, path: &Path) -> std::io::Result<Vec<Value>> {
        let meta = match std::fs::metadata(path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                self.reset(Some(path.to_owned()));
                return Ok(Vec::new());
            }
            Err(err) => return Err(err),
        };
        if self.path.as_deref() != Some(path) || meta.len() < self.offset {
            self.reset(Some(path.to_owned()));
        }
        let mut file = File::open(path)?;
        file.seek(SeekFrom::Start(self.offset))?;
        let mut fresh = Vec::new();
        file.read_to_end(&mut fresh)?;
        self.offset = self.offset.saturating_add(fresh.len() as u64);
        let mut entries = Vec::new();
        for byte in fresh {
            if byte == b'\n' {
                if !self.dropping_oversized_line
                    && let Ok(value) = serde_json::from_slice::<Value>(&self.pending)
                {
                    entries.push(value);
                }
                self.pending.clear();
                self.dropping_oversized_line = false;
            } else if !self.dropping_oversized_line {
                if self.pending.len() < self.max_line_bytes {
                    self.pending.push(byte);
                } else {
                    self.pending.clear();
                    self.dropping_oversized_line = true;
                }
            }
        }
        Ok(entries)
    }

    fn reset(&mut self, path: Option<PathBuf>) {
        self.path = path;
        self.offset = 0;
        self.pending.clear();
        self.dropping_oversized_line = false;
    }
}

const TAIL_BYTES: u64 = 64 * 1024;
const AWAY_HINT: &str = " (disable recaps in ";

pub fn read_session_tail(path: &Path, name: Option<String>) -> SessionTail {
    let Ok(mut file) = File::open(path) else {
        return SessionTail::empty(name);
    };
    let Ok(meta) = file.metadata() else {
        return SessionTail::empty(name);
    };
    if !meta.is_file() {
        return SessionTail::empty(name);
    }
    let size = meta.len();
    let start = size.saturating_sub(TAIL_BYTES);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return SessionTail::empty(name);
    }
    let mut bytes = Vec::with_capacity((size - start) as usize);
    if file.read_to_end(&mut bytes).is_err() {
        return SessionTail::empty(name);
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut lines = text.lines();
    if start > 0 {
        let _ = lines.next();
    }
    let entries: Vec<Value> = lines
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let mut tail = SessionTail {
        has_jsonl: true,
        name,
        ..SessionTail::empty(None)
    };
    let mut enqueued = 0i64;
    let mut dequeued = 0i64;
    for entry in &entries {
        if entry.get("type").and_then(Value::as_str) == Some("queue-operation") {
            match entry.get("operation").and_then(Value::as_str) {
                Some("enqueue") => enqueued += 1,
                Some("dequeue") => dequeued += 1,
                _ => {}
            }
        }
    }
    tail.queued = (enqueued - dequeued).max(0) as u32;
    tail.last_entry_kind = classify(&entries, &mut tail.last_entry_ms);
    tail.pending_ask = pending_ask(&entries);
    tail.last_assistant_text = last_assistant_text(&entries);
    tail.session_summary = current_summary(&entries);
    tail.context_usage = context_usage(&entries);
    tail
}

fn context_usage(entries: &[Value]) -> Option<SessionContextUsage> {
    let mut latest = None;
    for entry in entries {
        if entry.get("type").and_then(Value::as_str) == Some("system")
            && entry.get("subtype").and_then(Value::as_str) == Some("compact_boundary")
        {
            latest = None;
            continue;
        }
        if entry.get("type").and_then(Value::as_str) != Some("assistant")
            || entry.get("isSidechain").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        let Some(message) = entry.get("message").and_then(Value::as_object) else {
            continue;
        };
        let Some(usage) = message.get("usage").and_then(Value::as_object) else {
            continue;
        };
        let Some(input_tokens) = usage.get("input_tokens").and_then(Value::as_u64) else {
            continue;
        };
        let cache_read = usage
            .get("cache_read_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        let cache_create = usage
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        latest = Some(SessionContextUsage {
            tokens: input_tokens
                .saturating_add(cache_read)
                .saturating_add(cache_create),
            model: message
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_owned),
        });
    }
    latest
}

fn classify(entries: &[Value], timestamp: &mut Option<i64>) -> Option<LastEntryKind> {
    for e in entries.iter().rev() {
        let Some(ty) = e.get("type").and_then(Value::as_str) else {
            continue;
        };
        if ty != "assistant" && ty != "user" {
            continue;
        }
        *timestamp = e
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_timestamp_ms);
        if ty == "assistant" {
            return Some(
                match e.pointer("/message/stop_reason").and_then(Value::as_str) {
                    Some("tool_use") => LastEntryKind::ToolUse,
                    Some("pause_turn") => LastEntryKind::Paused,
                    Some("end_turn" | "max_tokens" | "stop_sequence" | "refusal") => {
                        LastEntryKind::EndTurn
                    }
                    Some(_) => LastEntryKind::EndTurn,
                    None => LastEntryKind::Other,
                },
            );
        }
        let tool_result = e
            .pointer("/message/content")
            .and_then(Value::as_array)
            .is_some_and(|blocks| {
                blocks
                    .iter()
                    .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
            });
        return Some(if tool_result {
            LastEntryKind::ToolResult
        } else {
            LastEntryKind::Other
        });
    }
    None
}

fn current_summary(entries: &[Value]) -> Option<String> {
    for e in entries.iter().rev() {
        let Some(ty) = e.get("type").and_then(Value::as_str) else {
            continue;
        };
        if ty == "user" || ty == "assistant" {
            return None;
        }
        let raw = if ty == "summary" {
            e.get("summary").and_then(Value::as_str)
        } else if ty == "system" && e.get("subtype").and_then(Value::as_str) == Some("away_summary")
        {
            e.get("content").and_then(Value::as_str)
        } else {
            continue;
        };
        let mut line = raw?
            .lines()
            .map(str::trim)
            .find(|s| !s.is_empty())?
            .to_owned();
        if let Some(i) = line.to_ascii_lowercase().rfind(AWAY_HINT) {
            line.truncate(i);
        }
        let clean = sanitize_line(&line);
        return (!clean.is_empty()).then_some(clean);
    }
    None
}

fn last_assistant_text(entries: &[Value]) -> Option<String> {
    for e in entries.iter().rev() {
        if e.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let content = e.pointer("/message/content")?.as_array()?;
        for block in content.iter().rev() {
            if block.get("type").and_then(Value::as_str) == Some("text") {
                let text = block.get("text")?.as_str()?;
                return text
                    .lines()
                    .map(str::trim)
                    .find(|s| !s.is_empty())
                    .map(sanitize_line)
                    .filter(|s| !s.is_empty());
            }
        }
    }
    None
}

fn pending_ask(entries: &[Value]) -> Option<String> {
    let assistant = entries
        .iter()
        .rev()
        .find(|e| e.get("type").and_then(Value::as_str) == Some("assistant"))?;
    if assistant
        .pointer("/message/stop_reason")
        .and_then(Value::as_str)
        != Some("tool_use")
    {
        return None;
    }
    let blocks = assistant.pointer("/message/content")?.as_array()?;
    let block = blocks
        .iter()
        .rev()
        .find(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))?;
    let name = block.get("name")?.as_str()?;
    let input = block.get("input")?;
    let text = match name {
        "ExitPlanMode" => input
            .get("plan")
            .and_then(Value::as_str)
            .and_then(|s| s.lines().map(str::trim).find(|s| !s.is_empty()))
            .map(|s| format!("approve plan: {}", s.trim_start_matches('#').trim()))
            .unwrap_or_else(|| "approve plan".to_owned()),
        "AskUserQuestion" => input
            .pointer("/questions/0/question")
            .or_else(|| input.get("question"))
            .and_then(Value::as_str)
            .map(|s| format!("question: {s}"))?,
        _ => {
            let mut pieces = Vec::new();
            if let Some(v) = input.as_object() {
                for key in ["command", "description", "pattern", "file_path", "query"] {
                    if let Some(s) = v.get(key).and_then(Value::as_str) {
                        pieces.push(format!("{key}: {}", compact(s)));
                    }
                }
            }
            if pieces.is_empty() {
                format!("allow {name}")
            } else {
                format!("allow {name}: {}", pieces.join(", "))
            }
        }
    };
    let clean = sanitize_line(&text);
    Some(clean.chars().take(180).collect())
}

fn compact(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(80)
        .collect()
}

pub fn sanitize_line(s: &str) -> String {
    strip_ansi(s)
        .chars()
        .filter(|c| !c.is_control() || *c == '\t')
        .collect::<String>()
        .replace('\t', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                let mut esc = false;
                for c in chars.by_ref() {
                    if c == '\u{7}' || (esc && c == '\\') {
                        break;
                    }
                    esc = c == '\u{1b}';
                }
            }
            Some(_) => {}
            None => {}
        }
    }
    out
}

/// Parse an RFC3339 timestamp into epoch milliseconds without a time crate.
pub(crate) fn parse_timestamp_ms(s: &str) -> Option<i64> {
    let bytes = s.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    let n = |a: usize, b: usize| s.get(a..b)?.parse::<i64>().ok();
    let (year, month, day, hour, minute, second) = (
        n(0, 4)?,
        n(5, 7)?,
        n(8, 10)?,
        n(11, 13)?,
        n(14, 16)?,
        n(17, 19)?,
    );
    let days = days_from_civil(year, month, day)?;
    let mut millis = ((days * 24 + hour) * 60 + minute) * 60_000 + second * 1000;
    let mut i = 19;
    if bytes.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while bytes.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        let frac = s.get(start..i).unwrap_or("");
        let padded = format!("{frac:0<3}");
        millis += padded.get(..3)?.parse::<i64>().ok()?;
    }
    match bytes.get(i).copied()? {
        b'Z' | b'z' => {}
        sign @ (b'+' | b'-') => {
            let zone = s.get(i + 1..i + 6)?;
            let zh = zone.get(..2)?.parse::<i64>().ok()?;
            let zm = zone.get(3..5)?.parse::<i64>().ok()?;
            let off = (zh * 60 + zm) * 60_000;
            millis += if sign == b'+' { -off } else { off };
        }
        _ => return None,
    }
    Some(millis)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> Option<i64> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = y - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = m + if m > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146097 + doe - 719468)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn reads_bounded_tail_classification_queue_and_current_recap() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("s.jsonl");
        fs::write(&path, concat!(
            "{\"type\":\"assistant\",\"timestamp\":\"2026-08-09T00:40:00.172Z\",\"message\":{\"stop_reason\":\"tool_use\"}}\n",
            "{\"type\":\"queue-operation\",\"operation\":\"enqueue\"}\n",
            "{\"type\":\"queue-operation\",\"operation\":\"dequeue\"}\n",
            "{\"type\":\"system\",\"subtype\":\"away_summary\",\"content\":\"Done. (disable recaps in /config)\"}\n"
        )).unwrap();
        let tail = read_session_tail(&path, None);
        assert!(tail.has_jsonl);
        assert_eq!(tail.last_entry_kind, Some(LastEntryKind::ToolUse));
        assert_eq!(tail.queued, 0);
        assert_eq!(tail.session_summary.as_deref(), Some("Done."));
        assert_eq!(tail.last_entry_ms, Some(1786236000172));
    }

    #[test]
    fn context_usage_includes_cache_buckets_ignores_sidechains_and_resets_on_compact() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("context.jsonl");
        fs::write(
            &path,
            concat!(
                "{\"type\":\"assistant\",\"message\":{\"model\":\"claude-sonnet\",\"usage\":{\"input_tokens\":400000,\"cache_read_input_tokens\":100000,\"cache_creation_input_tokens\":200000}}}\n",
                "{\"type\":\"assistant\",\"isSidechain\":true,\"message\":{\"model\":\"claude-haiku\",\"usage\":{\"input_tokens\":1}}}\n",
                "{\"type\":\"assistant\",\"message\":{\"model\":\"claude-haiku\",\"usage\":{\"input_tokens\":100000,\"cache_read_input_tokens\":20000}}}\n"
            ),
        )
        .unwrap();
        let tail = read_session_tail(&path, None);
        let usage = tail.context_usage.unwrap();
        assert_eq!(usage.tokens, 120_000);
        assert_eq!(usage.model.as_deref(), Some("claude-haiku"));
        assert_eq!(usage.percent(), 60);

        fs::write(
            &path,
            concat!(
                "{\"type\":\"assistant\",\"message\":{\"model\":\"claude-sonnet\",\"usage\":{\"input_tokens\":400000,\"cache_read_input_tokens\":100000,\"cache_creation_input_tokens\":200000}}}\n",
                "{\"type\":\"system\",\"subtype\":\"compact_boundary\"}\n"
            ),
        )
        .unwrap();
        assert!(read_session_tail(&path, None).context_usage.is_none());
    }

    #[test]
    fn timestamp_accounts_for_time_zone() {
        assert_eq!(parse_timestamp_ms("1970-01-01T01:00:00+01:00"), Some(0));
    }

    #[test]
    fn follower_keeps_partial_records_and_restarts_after_truncation() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("live.jsonl");
        let mut f = File::create(&path).unwrap();
        f.write_all(b"{\"n\":1}\n{\"n\":").unwrap();
        let mut follower = TranscriptFollower::new(1024);
        assert_eq!(
            follower.read_new(&path).unwrap(),
            vec![serde_json::json!({"n":1})]
        );
        f.write_all(b"2}\n").unwrap();
        assert_eq!(
            follower.read_new(&path).unwrap(),
            vec![serde_json::json!({"n":2})]
        );
        std::fs::write(&path, b"{\"fresh\":true}\n").unwrap();
        assert_eq!(
            follower.read_new(&path).unwrap(),
            vec![serde_json::json!({"fresh":true})]
        );
    }
}
