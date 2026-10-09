use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

use serde_json::Value;

use super::identity::session_jsonl_path;
use super::transcript::strip_ansi;
use crate::{SessionSummary, SessionSummarySource};

const MAX_SCAN_BYTES: u64 = 4 * 1024 * 1024;
const TITLE_LIMIT: usize = 240;
const AWAY_LIMIT: usize = 800;

/// Read the best current summary for each session id. Invalid identifiers are
/// ignored before they can be used as path components.
pub fn read_session_summaries(
    home: &Path,
    worktree_path: &Path,
    session_ids: &[String],
) -> Vec<(String, Option<SessionSummary>)> {
    session_ids
        .iter()
        .map(|id| {
            let valid = id.len() == 36
                && id.bytes().enumerate().all(|(i, b)| match i {
                    8 | 13 | 18 | 23 => b == b'-',
                    _ => b.is_ascii_hexdigit(),
                });
            (
                id.clone(),
                if valid {
                    read_summary(&session_jsonl_path(home, worktree_path, id))
                } else {
                    None
                },
            )
        })
        .collect()
}

fn read_summary(path: &Path) -> Option<SessionSummary> {
    let mut file = File::open(path).ok()?;
    let size = file.metadata().ok()?.len();
    if size == 0 {
        return None;
    }
    let start = size.saturating_sub(MAX_SCAN_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::with_capacity((size - start) as usize);
    file.read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<_> = text.lines().skip(usize::from(start > 0)).collect();
    let mut away = None;
    let mut prompt = None;
    for line in lines.into_iter().rev() {
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let summary = if row.get("type").and_then(Value::as_str) == Some("ai-title") {
            row.get("aiTitle")
                .and_then(Value::as_str)
                .and_then(|s| clean_text(s, TITLE_LIMIT))
                .map(|text| SessionSummary {
                    text,
                    source: SessionSummarySource::AiTitle,
                })
        } else if row.get("type").and_then(Value::as_str) == Some("system")
            && row.get("subtype").and_then(Value::as_str) == Some("away_summary")
        {
            row.get("content")
                .and_then(Value::as_str)
                .and_then(|s| {
                    let without_hint = strip_recap_hint(s);
                    clean_text(&without_hint, AWAY_LIMIT)
                })
                .map(|text| SessionSummary {
                    text,
                    source: SessionSummarySource::AwaySummary,
                })
        } else if row.get("type").and_then(Value::as_str) == Some("last-prompt") {
            row.get("lastPrompt")
                .and_then(Value::as_str)
                .and_then(|s| clean_text(s, TITLE_LIMIT))
                .map(|text| SessionSummary {
                    text,
                    source: SessionSummarySource::LastPrompt,
                })
        } else {
            None
        };
        let Some(summary) = summary else { continue };
        match summary.source {
            SessionSummarySource::AiTitle => return Some(summary),
            SessionSummarySource::AwaySummary if away.is_none() => away = Some(summary),
            SessionSummarySource::LastPrompt if prompt.is_none() => prompt = Some(summary),
            _ => {}
        }
    }
    away.or(prompt)
}

fn strip_recap_hint(s: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let needle = "(disable recaps in ";
    lower
        .rfind(needle)
        .map(|i| s[..i].trim_end().to_owned())
        .unwrap_or_else(|| s.to_owned())
}

fn clean_text(s: &str, max: usize) -> Option<String> {
    let clean: String = strip_ansi(s)
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect();
    let clean = clean.trim();
    if clean.is_empty() {
        return None;
    }
    let clipped: String = clean.chars().take(max).collect();
    Some(if clean.chars().count() > max {
        format!("{clipped}…")
    } else {
        clipped
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;
    use uuid::Uuid;

    #[test]
    fn summary_priority_and_control_sanitization_match_contract() {
        let tmp = tempdir().unwrap();
        let home = tmp.path().join("home");
        let cwd = Path::new("/tmp/wt/demo");
        let id = Uuid::new_v5(&Uuid::NAMESPACE_OID, b"summary fixture").to_string();
        let file = session_jsonl_path(&home, cwd, &id);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        let fixture = format!(
            "{}\n{}\n",
            serde_json::json!({"type":"ai-title","aiTitle":"\u{1b}[31mFix rollout\u{1b}[0m"}),
            serde_json::json!({"type":"system","subtype":"away_summary","content":"Lower priority"})
        );
        fs::write(&file, fixture).unwrap();
        let summaries = read_session_summaries(&home, cwd, &[id.clone(), "../../escape".into()]);
        assert_eq!(summaries[0].1.as_ref().unwrap().text, "Fix rollout");
        assert_eq!(
            summaries[0].1.as_ref().unwrap().source,
            SessionSummarySource::AiTitle
        );
        assert!(summaries[1].1.is_none());
    }
}
