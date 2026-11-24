use std::{
    collections::HashMap,
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
    time::SystemTime,
};

use serde_json::Value;

use super::identity::session_jsonl_path;
use super::transcript::strip_ansi;
use crate::{SessionSummary, SessionSummarySource};

const MAX_SCAN_BYTES: u64 = 4 * 1024 * 1024;
const TITLE_LIMIT: usize = 240;
const AWAY_LIMIT: usize = 800;
const MAX_CACHED_SUMMARIES: usize = 1024;

/// Discovery asks for every known session of a slug whenever one of them
/// moves, and most transcripts are megabytes that did not change.
static SUMMARIES: LazyLock<Mutex<SummaryCache>> = LazyLock::new(Mutex::default);

#[derive(Clone, Copy, PartialEq, Eq)]
struct TranscriptStamp {
    len: u64,
    modified: SystemTime,
}

impl TranscriptStamp {
    fn of(metadata: &std::fs::Metadata) -> Option<Self> {
        Some(Self {
            len: metadata.len(),
            modified: metadata.modified().ok()?,
        })
    }
}

#[derive(Default)]
struct SummaryCache {
    entries: HashMap<PathBuf, (TranscriptStamp, Option<SessionSummary>)>,
    #[cfg(test)]
    scans: usize,
}

impl SummaryCache {
    fn get(&self, path: &Path, stamp: TranscriptStamp) -> Option<Option<SessionSummary>> {
        let (cached, summary) = self.entries.get(path)?;
        (*cached == stamp).then(|| summary.clone())
    }

    fn insert(&mut self, path: &Path, stamp: TranscriptStamp, summary: Option<SessionSummary>) {
        if self.entries.len() >= MAX_CACHED_SUMMARIES
            && !self.entries.contains_key(path)
            && let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, (stamp, _))| stamp.modified)
                .map(|(path, _)| path.clone())
        {
            self.entries.remove(&oldest);
        }
        self.entries.insert(path.to_owned(), (stamp, summary));
    }
}

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
                    read_summary(&SUMMARIES, &session_jsonl_path(home, worktree_path, id))
                } else {
                    None
                },
            )
        })
        .collect()
}

fn read_summary(cache: &Mutex<SummaryCache>, path: &Path) -> Option<SessionSummary> {
    let seen = std::fs::metadata(path).ok()?;
    if seen.len() == 0 {
        return None;
    }
    // Transcripts are append-only, so an unchanged length and mtime means
    // unchanged bytes.
    if let Some(seen) = TranscriptStamp::of(&seen)
        && let Ok(cache) = cache.lock()
        && let Some(summary) = cache.get(path, seen)
    {
        return summary;
    }
    let mut file = File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    let size = metadata.len();
    if size == 0 {
        return None;
    }
    let start = size.saturating_sub(MAX_SCAN_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::with_capacity((size - start) as usize);
    file.read_to_end(&mut bytes).ok()?;
    let summary = scan_summary(&bytes, start > 0);
    // The stamp predates the read, so bytes appended during it are scanned
    // again next time instead of being hidden behind a newer stamp.
    if let Ok(mut cache) = cache.lock() {
        #[cfg(test)]
        {
            cache.scans += 1;
        }
        if let Some(stamp) = TranscriptStamp::of(&metadata) {
            cache.insert(path, stamp, summary.clone());
        }
    }
    summary
}

/// The newest AI title in `bytes`, else the newest away summary, else the
/// newest last prompt. `partial_first_line` drops the line a tail read cut.
fn scan_summary(bytes: &[u8], partial_first_line: bool) -> Option<SessionSummary> {
    let complete = if partial_first_line {
        let end = bytes.iter().position(|byte| *byte == b'\n')?;
        &bytes[end + 1..]
    } else {
        bytes
    };
    let mut away = None;
    let mut prompt = None;
    for line in complete.rsplit(|byte| *byte == b'\n') {
        if !may_carry_summary(line) {
            continue;
        }
        let Some(summary) = row_summary(&String::from_utf8_lossy(line)) else {
            continue;
        };
        match summary.source {
            SessionSummarySource::AiTitle => return Some(summary),
            SessionSummarySource::AwaySummary if away.is_none() => away = Some(summary),
            SessionSummarySource::LastPrompt if prompt.is_none() => prompt = Some(summary),
            _ => {}
        }
    }
    away.or(prompt)
}

/// Whether `line` holds one of the three strings `row_summary` matches on
/// (`ai-title`, `last-prompt`, `away_summary`). Nearly every transcript row
/// is a message that holds none, and parsing megabytes of those was most of
/// discovery's cost. A row spelling these with JSON `\u` escapes would be
/// missed; Claude Code writes them plainly.
fn may_carry_summary(line: &[u8]) -> bool {
    let at = |index: usize, offset: usize, needle: &[u8]| {
        index
            .checked_sub(offset)
            .and_then(|start| line.get(start..start + needle.len()))
            == Some(needle)
    };
    line.iter().enumerate().any(|(index, byte)| match byte {
        b'-' => at(index, 2, b"ai-title") || at(index, 4, b"last-prompt"),
        b'_' => at(index, 4, b"away_summary"),
        _ => false,
    })
}

fn row_summary(line: &str) -> Option<SessionSummary> {
    let row = serde_json::from_str::<Value>(line).ok()?;
    if row.get("type").and_then(Value::as_str) == Some("ai-title") {
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
    }
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

    /// The scan as it was before the byte prefilter: every line parsed.
    fn scan_every_line(bytes: &[u8], partial_first_line: bool) -> Option<SessionSummary> {
        let text = String::from_utf8_lossy(bytes);
        let mut away = None;
        let mut prompt = None;
        for line in text
            .lines()
            .skip(usize::from(partial_first_line))
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            let Some(summary) = row_summary(line) else {
                continue;
            };
            match summary.source {
                SessionSummarySource::AiTitle => return Some(summary),
                SessionSummarySource::AwaySummary if away.is_none() => away = Some(summary),
                SessionSummarySource::LastPrompt if prompt.is_none() => prompt = Some(summary),
                _ => {}
            }
        }
        away.or(prompt)
    }

    fn rows() -> Vec<Vec<u8>> {
        let json = |value: Value| value.to_string().into_bytes();
        vec![
            json(serde_json::json!({"type":"ai-title","aiTitle":"Fix rollout"})),
            json(serde_json::json!({
                "type":"system","subtype":"away_summary",
                "content":"Was fixing rollout (disable recaps in /config)"
            })),
            json(serde_json::json!({"type":"last-prompt","lastPrompt":"please fix it"})),
            // Every consumed marker, but only as text inside a message body.
            json(serde_json::json!({"type":"user","message":{"content":
                "{\"type\":\"ai-title\",\"aiTitle\":\"x\"} last-prompt away_summary"}})),
            json(serde_json::json!({"type":"assistant","message":{"content":"a-b_c ai-titl"}})),
            b"{\"type\":\"ai-title\",\"aiTitle\":\"bad \xff byte\"}".to_vec(),
            b"{\"type\":\"last-prompt\",\"lastPrompt\":\"crlf\"}\r".to_vec(),
            b"{\"type\":\"last-prompt\",\"lastPro".to_vec(),
        ]
    }

    #[test]
    fn prefiltered_scan_matches_parsing_every_line() {
        let rows = rows();
        for mask in 1_u32..(1 << rows.len()) {
            let mut picked: Vec<&[u8]> = rows
                .iter()
                .enumerate()
                .filter(|(index, _)| mask & (1 << index) != 0)
                .map(|(_, row)| row.as_slice())
                .collect();
            for _ in 0..2 {
                for ending in ["\n", ""] {
                    let bytes = [picked.join(&b'\n'), ending.as_bytes().to_vec()].concat();
                    for partial in [false, true] {
                        assert_eq!(
                            scan_summary(&bytes, partial),
                            scan_every_line(&bytes, partial),
                            "mask {mask:#b} partial {partial} ending {ending:?}"
                        );
                    }
                }
                picked.reverse();
            }
        }
    }

    #[test]
    fn scan_handles_lookalikes_partial_lines_and_invalid_utf8() {
        let rows = rows();
        let text = |summary: Option<SessionSummary>| summary.map(|summary| summary.text);
        assert_eq!(scan_summary(&rows[3], false), None);
        let prompt_then_lookalike = [rows[2].as_slice(), rows[3].as_slice()].join(&b'\n');
        assert_eq!(
            text(scan_summary(&prompt_then_lookalike, false)).as_deref(),
            Some("please fix it")
        );
        assert_eq!(
            text(scan_summary(&rows[1], false)).as_deref(),
            Some("Was fixing rollout")
        );
        assert_eq!(
            text(scan_summary(&rows[5], false)).as_deref(),
            Some("bad \u{fffd} byte")
        );
        // A tail read that starts inside the title row must not use it.
        let cut_title = [&rows[0][3..], rows[2].as_slice()].join(&b'\n');
        assert_eq!(
            text(scan_summary(&cut_title, true)).as_deref(),
            Some("please fix it")
        );
        assert_eq!(scan_summary(&rows[0], true), None);
    }

    #[test]
    fn unchanged_transcript_is_scanned_once_and_a_changed_one_again() {
        use std::io::Write;
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("session.jsonl");
        let cache = Mutex::new(SummaryCache::default());
        let scans = || cache.lock().unwrap().scans;
        assert_eq!(read_summary(&cache, &path), None);
        fs::write(
            &path,
            format!(
                "{}\n",
                serde_json::json!({"type":"last-prompt","lastPrompt":"first"})
            ),
        )
        .unwrap();
        for _ in 0..3 {
            assert_eq!(read_summary(&cache, &path).unwrap().text, "first");
        }
        assert_eq!(scans(), 1);

        writeln!(
            fs::OpenOptions::new().append(true).open(&path).unwrap(),
            "{}",
            serde_json::json!({"type":"ai-title","aiTitle":"Titled"})
        )
        .unwrap();
        for _ in 0..3 {
            assert_eq!(read_summary(&cache, &path).unwrap().text, "Titled");
        }
        assert_eq!(scans(), 2);

        // A transcript with no summary row is remembered as such.
        let plain = tmp.path().join("plain.jsonl");
        fs::write(&plain, "{\"type\":\"user\"}\n").unwrap();
        assert_eq!(read_summary(&cache, &plain), None);
        assert_eq!(read_summary(&cache, &plain), None);
        assert_eq!(scans(), 3);
    }

    #[test]
    fn summary_cache_is_bounded_and_evicts_the_oldest_transcript() {
        let mut cache = SummaryCache::default();
        let stamp = |seconds: u64| TranscriptStamp {
            len: 1,
            modified: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(seconds),
        };
        for index in 0..MAX_CACHED_SUMMARIES as u64 + 10 {
            cache.insert(Path::new(&format!("/t/{index}")), stamp(index + 1), None);
        }
        assert_eq!(cache.entries.len(), MAX_CACHED_SUMMARIES);
        assert!(cache.get(Path::new("/t/0"), stamp(1)).is_none());
        let newest = MAX_CACHED_SUMMARIES as u64 + 9;
        assert!(
            cache
                .get(Path::new(&format!("/t/{newest}")), stamp(newest + 1))
                .is_some()
        );
    }
}
