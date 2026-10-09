use std::{cell::RefCell, collections::HashMap, rc::Rc};

use serde_json::Value;

use super::transcript::{parse_timestamp_ms, strip_ansi};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActivityKind {
    Info,
    User,
    Assistant,
    Thinking,
    Tool,
    ToolOk,
    ToolError,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivityLine {
    pub id: u64,
    pub timestamp_ms: i64,
    pub kind: ActivityKind,
    pub text: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivityPatch {
    pub id: u64,
    pub line: ActivityLine,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ActivityDelta {
    pub append: Vec<ActivityLine>,
    pub patch: Vec<ActivityPatch>,
}

#[derive(Clone, Debug)]
struct PendingTool {
    id: u64,
    label: String,
    tool_name: String,
    started_at_ms: i64,
    batch: Option<Rc<RefCell<ToolBatch>>>,
}

#[derive(Debug)]
struct ToolBatch {
    line_id: u64,
    call_label: String,
    remaining: usize,
    total_duration_ms: i64,
    results: Vec<(String, u32, u32)>,
}

/// Stateful event conversion for an incremental transcript. Tool results patch
/// their earlier call lines while those ids remain in the consumer's buffer.
#[derive(Debug, Default)]
pub struct ClaudeEventParser {
    next_id: u64,
    tools: HashMap<String, PendingTool>,
}

impl ClaudeEventParser {
    pub fn new(first_id: u64) -> Self {
        Self {
            next_id: first_id,
            tools: HashMap::new(),
        }
    }

    pub fn parse_line(&mut self, line: &str) -> ActivityDelta {
        serde_json::from_str::<Value>(line)
            .map(|event| self.parse(&event))
            .unwrap_or_default()
    }

    pub fn parse(&mut self, event: &Value) -> ActivityDelta {
        let mut delta = ActivityDelta::default();
        let ts = event
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_timestamp_ms)
            .unwrap_or(0);
        match event.get("type").and_then(Value::as_str) {
            Some("assistant") => self.parse_message(event, true, ts, &mut delta),
            Some("user") => self.parse_message(event, false, ts, &mut delta),
            Some("system")
                if event.get("subtype").and_then(Value::as_str) == Some("compact_boundary") =>
            {
                let trigger = event
                    .pointer("/compactMetadata/trigger")
                    .and_then(Value::as_str)
                    .filter(|s| *s != "manual")
                    .map(|s| format!(" {s}"))
                    .unwrap_or_default();
                let tokens = match (
                    event
                        .pointer("/compactMetadata/preTokens")
                        .and_then(Value::as_u64),
                    event
                        .pointer("/compactMetadata/postTokens")
                        .and_then(Value::as_u64),
                ) {
                    (Some(a), Some(b)) => format!(" ({} → {})", format_tokens(a), format_tokens(b)),
                    _ => String::new(),
                };
                self.push(
                    &mut delta.append,
                    ts,
                    ActivityKind::Info,
                    format!("↘ compacted{trigger}{tokens}"),
                );
            }
            Some("system")
                if event.get("subtype").and_then(Value::as_str) == Some("away_summary") =>
            {
                if let Some(content) = event.get("content").and_then(Value::as_str) {
                    self.push_multiline(
                        &mut delta.append,
                        ts,
                        ActivityKind::Info,
                        &strip_recap_hint(content),
                        "─ ",
                        "  ",
                    );
                }
            }
            Some("attachment") => {
                let attachment = event.get("attachment");
                if attachment
                    .and_then(|a| a.get("type"))
                    .and_then(Value::as_str)
                    == Some("queued_command")
                    && attachment
                        .and_then(|a| a.get("commandMode"))
                        .and_then(Value::as_str)
                        == Some("prompt")
                    && let Some(prompt) = attachment
                        .and_then(|a| a.get("prompt"))
                        .and_then(Value::as_str)
                {
                    let prompt = compact(prompt, 200);
                    if !prompt.is_empty() {
                        self.push(
                            &mut delta.append,
                            ts,
                            ActivityKind::Info,
                            format!("⏎ queued: {prompt}"),
                        );
                    }
                }
            }
            _ => {}
        }
        delta
    }

    fn parse_message(
        &mut self,
        event: &Value,
        assistant: bool,
        ts: i64,
        delta: &mut ActivityDelta,
    ) {
        if !assistant && event.get("isCompactSummary").and_then(Value::as_bool) == Some(true) {
            return;
        }
        let Some(message) = event.get("message") else {
            return;
        };
        if assistant {
            self.parse_assistant(message, ts, delta);
            return;
        }
        match message.get("content") {
            Some(Value::String(text)) if !assistant => {
                self.append_user_text(&mut delta.append, ts, text)
            }
            Some(Value::Array(blocks)) => {
                for block in blocks {
                    match (assistant, block.get("type").and_then(Value::as_str)) {
                        (false, Some("text")) => {
                            if let Some(text) = block.get("text").and_then(Value::as_str) {
                                self.append_user_text(&mut delta.append, ts, text);
                            }
                        }
                        (false, Some("tool_result")) => self.tool_result(block, ts, delta),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn append_user_text(&mut self, target: &mut Vec<ActivityLine>, ts: i64, text: &str) {
        if text.starts_with("[Request interrupted") {
            self.push(target, ts, ActivityKind::Info, "! interrupted".into());
            return;
        }
        if text.starts_with("Base directory for this skill:")
            || text.trim_start().starts_with("# /")
            || text.trim_start().starts_with("<local-command-caveat>")
        {
            return;
        }
        if text.trim_start().starts_with("<local-command-stdout>") {
            if let Some(body) = between_tags(text, "local-command-stdout")
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                self.push_multiline(target, ts, ActivityKind::Info, body, "↳ ", "  ");
            }
            return;
        }
        if text.trim_start().starts_with("<task-notification>") {
            let summary = between_tags(text, "summary")
                .map(compact_task_summary)
                .unwrap_or_else(|| "task".into());
            self.push(target, ts, ActivityKind::Info, format!("◉ {summary}"));
            if let Some(event) = between_tags(text, "event")
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                self.push_multiline(target, ts, ActivityKind::Info, event, "  ", "  ");
            }
            return;
        }
        let mut prompt = text.to_owned();
        if text.trim_start().starts_with("<command-")
            && let Some(name) = between_tags(text, "command-name")
        {
            let args = between_tags(text, "command-args").unwrap_or("").trim();
            prompt = if args.is_empty() {
                name.trim().to_owned()
            } else {
                format!("{} {args}", name.trim())
            };
        }
        self.push_multiline(target, ts, ActivityKind::User, &prompt, "> ", "  ");
    }

    fn parse_assistant(&mut self, message: &Value, ts: i64, delta: &mut ActivityDelta) {
        let Some(blocks) = message.get("content").and_then(Value::as_array) else {
            return;
        };
        let mut bulk_names = Vec::new();
        let mut first_bulk = None;
        for (index, block) in blocks.iter().enumerate() {
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            let name = block.get("name").and_then(Value::as_str).unwrap_or("?");
            if is_detailed_tool(name) {
                continue;
            }
            bulk_names.push(name.to_owned());
            first_bulk.get_or_insert(index);
        }
        let batch = (bulk_names.len() >= 2).then(|| {
            Rc::new(RefCell::new(ToolBatch {
                line_id: self.next_id,
                call_label: format_batch_call(&bulk_names),
                remaining: blocks
                    .iter()
                    .filter(|b| {
                        b.get("type").and_then(Value::as_str) == Some("tool_use")
                            && b.get("name")
                                .and_then(Value::as_str)
                                .is_none_or(|n| !is_detailed_tool(n))
                            && b.get("id").and_then(Value::as_str).is_some()
                    })
                    .count(),
                total_duration_ms: 0,
                results: Vec::new(),
            }))
        });
        for (index, block) in blocks.iter().enumerate() {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        self.push_multiline(
                            &mut delta.append,
                            ts,
                            ActivityKind::Assistant,
                            text,
                            "  ",
                            "  ",
                        );
                    }
                }
                Some("thinking") => {
                    if let Some(text) = block.get("thinking").and_then(Value::as_str) {
                        let one_line = compact(text, 200);
                        self.push(
                            &mut delta.append,
                            ts,
                            ActivityKind::Thinking,
                            format!("· {one_line}"),
                        );
                    }
                }
                Some("tool_use") => {
                    let tool_name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("?")
                        .to_owned();
                    let tool_id = block.get("id").and_then(Value::as_str).unwrap_or_default();
                    let arg = tool_label(&tool_name, block.get("input"));
                    let detailed = is_detailed_tool(&tool_name);
                    if !detailed && let Some(shared) = &batch {
                        if !tool_id.is_empty() {
                            self.tools.insert(
                                tool_id.to_owned(),
                                PendingTool {
                                    id: shared.borrow().line_id,
                                    label: arg,
                                    tool_name,
                                    started_at_ms: ts,
                                    batch: Some(shared.clone()),
                                },
                            );
                        }
                        if Some(index) == first_bulk {
                            let label = shared.borrow().call_label.clone();
                            delta.append.push(self.make_line(
                                ts,
                                ActivityKind::Tool,
                                format!("  ⚒ {label}"),
                            ));
                        }
                    } else {
                        let line = self.make_line(ts, ActivityKind::Tool, format!("  ⚒ {arg}"));
                        if !tool_id.is_empty() {
                            self.tools.insert(
                                tool_id.to_owned(),
                                PendingTool {
                                    id: line.id,
                                    label: arg,
                                    tool_name,
                                    started_at_ms: ts,
                                    batch: None,
                                },
                            );
                        }
                        delta.append.push(line);
                    }
                }
                _ => {}
            }
        }
    }

    fn tool_result(&mut self, block: &Value, ts: i64, delta: &mut ActivityDelta) {
        let Some(tool_id) = block.get("tool_use_id").and_then(Value::as_str) else {
            return;
        };
        let start = self.tools.remove(tool_id);
        let is_error = block.get("is_error").and_then(Value::as_bool) == Some(true);
        let kind = if is_error {
            ActivityKind::ToolError
        } else {
            ActivityKind::ToolOk
        };
        let arrow = if is_error { "✗" } else { "✓" };
        let detail = if is_error {
            brief_result(block.get("content"))
        } else {
            None
        }
        .map(|s| format!(" err: {s}"))
        .unwrap_or_default();
        let Some(start) = start else {
            let arrow = if is_error { "✗" } else { "✓" };
            let detail = if is_error {
                brief_result(block.get("content"))
                    .map(|s| format!(" err: {s}"))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            let label = "(earlier call)";
            delta
                .append
                .push(self.make_line(ts, kind, format!("  {arrow} {label}{detail}")));
            return;
        };
        if let Some(batch) = start.batch {
            let mut batch = batch.borrow_mut();
            batch.remaining = batch.remaining.saturating_sub(1);
            batch.total_duration_ms = batch
                .total_duration_ms
                .saturating_add(ts.saturating_sub(start.started_at_ms));
            let entry = batch
                .results
                .iter_mut()
                .find(|(name, _, _)| *name == start.tool_name);
            if let Some((_, ok, err)) = entry {
                if is_error {
                    *err += 1;
                } else {
                    *ok += 1;
                }
            } else {
                batch.results.push((
                    start.tool_name,
                    if is_error { 0 } else { 1 },
                    if is_error { 1 } else { 0 },
                ));
            }
            if batch.remaining == 0 {
                let any_error = batch.results.iter().any(|(_, _, err)| *err > 0);
                let text = format_batch_result(&batch, any_error);
                delta.patch.push(ActivityPatch {
                    id: batch.line_id,
                    line: ActivityLine {
                        id: batch.line_id,
                        timestamp_ms: ts,
                        kind: if any_error {
                            ActivityKind::ToolError
                        } else {
                            ActivityKind::ToolOk
                        },
                        text: format!("  {text}"),
                    },
                });
            }
            return;
        }
        let elapsed = format_duration(ts.saturating_sub(start.started_at_ms));
        let line = ActivityLine {
            id: start.id,
            timestamp_ms: ts,
            kind,
            text: format!("  {arrow}{}{detail} {elapsed}", start.label),
        };
        delta.patch.push(ActivityPatch { id: start.id, line });
    }

    fn push(&mut self, target: &mut Vec<ActivityLine>, ts: i64, kind: ActivityKind, text: String) {
        let text = sanitize_activity_line(&text);
        if !text.is_empty() {
            target.push(self.make_line(ts, kind, text));
        }
    }

    fn push_multiline(
        &mut self,
        target: &mut Vec<ActivityLine>,
        ts: i64,
        kind: ActivityKind,
        text: &str,
        first: &str,
        rest: &str,
    ) {
        let pieces: Vec<_> = text
            .lines()
            .map(|line| sanitize_activity_line(line).trim_end().to_owned())
            .filter(|s| !s.is_empty())
            .collect();
        for (i, piece) in pieces.iter().take(50).enumerate() {
            target.push(self.make_line(
                ts,
                kind,
                format!("{}{}", if i == 0 { first } else { rest }, piece),
            ));
        }
        let truncated = pieces.len().saturating_sub(50);
        if truncated > 0 {
            target.push(self.make_line(
                ts,
                ActivityKind::Info,
                format!(
                    "  …{truncated} more line{} truncated",
                    if truncated == 1 { "" } else { "s" }
                ),
            ));
        }
    }

    fn make_line(&mut self, ts: i64, kind: ActivityKind, text: String) -> ActivityLine {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        ActivityLine {
            id,
            timestamp_ms: ts,
            kind,
            text,
        }
    }
}

fn tool_label(name: &str, input: Option<&Value>) -> String {
    let summary = input
        .and_then(Value::as_object)
        .and_then(|object| {
            [
                "command",
                "file_path",
                "path",
                "pattern",
                "query",
                "url",
                "subagent_type",
                "description",
            ]
            .iter()
            .find_map(|key| {
                object
                    .get(*key)
                    .and_then(Value::as_str)
                    .map(|s| compact(s, 120))
            })
        })
        .filter(|s| !s.is_empty());
    summary
        .map(|s| format!("{name}({s})"))
        .unwrap_or_else(|| name.to_owned())
}

fn is_detailed_tool(name: &str) -> bool {
    matches!(
        name,
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" | "Task"
    )
}

fn between_tags<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&close)? + start;
    Some(&text[start..end])
}

fn compact_task_summary(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Some(value) = trimmed
        .strip_prefix("Monitor event: \"")
        .and_then(|x| x.strip_suffix('\"'))
    {
        return format!("Monitor — {value}");
    }
    if let Some(rest) = trimmed.strip_prefix("Background command \"")
        && let Some((command, exit)) = rest.split_once("\" completed (exit code ")
        && let Some(code) = exit.strip_suffix(')')
    {
        return format!("Background — {command} (exit {code})");
    }
    trimmed.to_owned()
}

fn format_batch_call(names: &[String]) -> String {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for name in names {
        if let Some((_, count)) = counts.iter_mut().find(|(known, _)| *known == name) {
            *count += 1;
        } else {
            counts.push((name, 1));
        }
    }
    counts
        .into_iter()
        .map(|(name, count)| format!("{name}×{count}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_batch_result(batch: &ToolBatch, any_error: bool) -> String {
    let duration = format_duration(batch.total_duration_ms);
    if !any_error {
        return format!("✓ {} ({duration})", batch.call_label);
    }
    let parts = batch
        .results
        .iter()
        .flat_map(|(name, ok, err)| {
            let mut values = Vec::new();
            if *ok > 0 {
                values.push(format!("{name}×{ok} ok"));
            }
            if *err > 0 {
                values.push(format!("{name}×{err} err"));
            }
            values
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("✗ {parts} ({duration})")
}

fn brief_result(content: Option<&Value>) -> Option<String> {
    let pick = |text: &str| -> Option<String> {
        let line = text
            .lines()
            .map(|line| sanitize_activity_line(line).trim().to_owned())
            .find(|line| !line.is_empty())?;
        Some(if line.chars().count() > 120 {
            format!("{}…", line.chars().take(119).collect::<String>())
        } else {
            line
        })
    };
    match content? {
        Value::String(text) => pick(text),
        Value::Array(blocks) => blocks
            .iter()
            .find_map(|block| block.get("text").and_then(Value::as_str).and_then(pick)),
        _ => None,
    }
}

fn compact(text: &str, max: usize) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let clean = sanitize_activity_line(&joined);
    let clipped: String = clean.chars().take(max.saturating_sub(1)).collect();
    if clean.chars().count() > max {
        format!("{clipped}…")
    } else {
        clipped
    }
}

/// Match the activity parser's TS sanitizer: strip terminal control data,
/// retain intentional interior/leading spaces, and let each renderer decide
/// whether to trim a line's end.
fn sanitize_activity_line(input: &str) -> String {
    let mut value = strip_ansi(input);
    if let Some(last_cr) = value.rfind('\r') {
        value = value[last_cr + 1..].to_owned();
    }
    value
        .replace('\t', " ")
        .chars()
        .filter(|c| {
            let n = *c as u32;
            !((n <= 0x08) || (n == 0x0b) || (n == 0x0c) || (0x0e..=0x1f).contains(&n) || n == 0x7f)
        })
        .collect()
}

fn format_tokens(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

fn format_duration(ms: i64) -> String {
    let ms = ms.max(0);
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1_000.0)
    } else {
        format!("{}m{}s", ms / 60_000, (ms % 60_000) / 1_000)
    }
}

fn strip_recap_hint(s: &str) -> String {
    let lower = s.to_ascii_lowercase();
    lower
        .rfind("(disable recaps in ")
        .map(|i| s[..i].trim_end().to_owned())
        .unwrap_or_else(|| s.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_patches_tool_result_and_sanitizes_text() {
        let mut parser = ClaudeEventParser::new(40);
        let start = parser.parse_line(r#"{"type":"assistant","timestamp":"2026-08-09T00:40:00Z","message":{"content":[{"type":"text","text":"\u001b[31mhello\u001b[0m"},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"echo hi"}}]}}"#);
        assert_eq!(start.append[0].text, "  hello");
        assert_eq!(start.append[1].text, "  ⚒ Bash(echo hi)");
        let done = parser.parse_line(r#"{"type":"user","timestamp":"2026-08-09T00:40:01Z","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"done"}]}}"#);
        assert_eq!(done.patch[0].id, start.append[1].id);
        assert_eq!(done.patch[0].line.kind, ActivityKind::ToolOk);
    }

    #[test]
    fn batches_bulk_tool_calls_and_emits_orphan_results() {
        let mut parser = ClaudeEventParser::new(5);
        let start = parser.parse_line(r#"{"type":"assistant","timestamp":"2026-08-09T00:40:00Z","message":{"content":[{"type":"tool_use","id":"a","name":"Bash","input":{"command":"one"}},{"type":"tool_use","id":"b","name":"Read","input":{"file_path":"a.rs"}},{"type":"tool_use","id":"c","name":"Edit","input":{"file_path":"b.rs"}}]}}"#);
        assert_eq!(start.append.len(), 2);
        assert!(start.append[0].text.contains("Bash×1, Read×1"));
        assert_eq!(start.append[1].text, "  ⚒ Edit(b.rs)");
        let r1 = parser.parse_line(r#"{"type":"user","timestamp":"2026-08-09T00:40:01Z","message":{"content":[{"type":"tool_result","tool_use_id":"a"}]}}"#);
        assert!(r1.patch.is_empty());
        let r2 = parser.parse_line(r#"{"type":"user","timestamp":"2026-08-09T00:40:02Z","message":{"content":[{"type":"tool_result","tool_use_id":"b"}]}}"#);
        assert_eq!(r2.patch.len(), 1);
        assert!(r2.patch[0].line.text.contains("✓ Bash×1, Read×1"));
        let orphan = parser.parse_line(r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"outside"}]}}"#);
        assert_eq!(orphan.append[0].text, "  ✓ (earlier call)");
    }

    #[test]
    fn user_noise_commands_and_notifications_follow_visible_contract() {
        let mut parser = ClaudeEventParser::new(1);
        let prompt = parser.parse_line(r#"{"type":"user","message":{"content":"<command-name>/compact</command-name><command-args>now</command-args>"}}"#);
        assert_eq!(prompt.append[0].text, "> /compact now");
        let noise = parser.parse_line(
            r#"{"type":"user","message":{"content":"Base directory for this skill: /tmp"}}"#,
        );
        assert!(noise.append.is_empty());
        let notice = parser.parse_line(r#"{"type":"user","message":{"content":"<task-notification><summary>Monitor event: \"CI\"</summary><event>done\nnext</event></task-notification>"}}"#);
        assert_eq!(notice.append[0].text, "◉ Monitor — CI");
        assert_eq!(notice.append[1].text, "  done");
    }

    #[test]
    fn skips_compaction_summary_blob_and_surfaces_away_recap() {
        let mut parser = ClaudeEventParser::new(1);
        let ignored = parser.parse_line(
            r#"{"type":"user","isCompactSummary":true,"message":{"content":"ignore"}}"#,
        );
        assert!(ignored.append.is_empty());
        let recap = parser.parse_line(r#"{"type":"system","subtype":"away_summary","content":"done. (disable recaps in /config)"}"#);
        assert_eq!(recap.append[0].text, "─ done.");
    }
}
