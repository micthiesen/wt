//! Bounded detailed output tails for exact OpenCode session identities.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use rusqlite::{Connection, OpenFlags, params};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::output::{one_line, text_lines};
use crate::{HarnessOutputKind, HarnessOutputTarget, HarnessOutputUpdate};

use super::{OpenCodeError, OpenCodeHarness};

// A selected tail has a hard 1 MiB payload budget even when sibling sessions
// commit continuously. Older messages remain available in native history.
const SEED_PARTS: i64 = 16;

const MAX_PART_DATA_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Default)]
struct Cursor {
    session_id: String,
    data_version: i64,
    next_line_id: u64,
    seeded: bool,
}

#[derive(Clone, Debug, Default)]
pub struct OpenCodeOutputTracker {
    by_session: HashMap<(String, String), Cursor>,
    database: Arc<Mutex<Option<(PathBuf, Connection)>>>,
}

#[derive(Debug)]
struct PartRow {
    created: i64,
    kind: Option<String>,
    data: Option<String>,
}

impl OpenCodeHarness {
    /// Read at most 16 V2 messages (1 MiB total payload) per target. The
    /// caller should run this on a blocking worker. SQLite rows are read-only,
    /// exact-session scoped, and JSON payloads are capped before Rust receives
    /// them.
    pub fn poll_output(
        &self,
        tracker: &mut OpenCodeOutputTracker,
        targets: &[HarnessOutputTarget],
        cancellation: &CancellationToken,
    ) -> Result<Vec<HarnessOutputUpdate>, OpenCodeError> {
        let active = targets
            .iter()
            .map(|target| (target.slug.as_str(), target.session_id.as_str()))
            .collect::<std::collections::HashSet<_>>();
        tracker
            .by_session
            .retain(|(slug, session_id), _| active.contains(&(slug.as_str(), session_id.as_str())));
        if !self.paths().database.exists() || targets.is_empty() {
            return Ok(Vec::new());
        }
        let mut database = tracker
            .database
            .lock()
            .map_err(|_| OpenCodeError::Operation {
                operation: "read output",
                detail: "database reader lock poisoned".into(),
            })?;
        if database
            .as_ref()
            .is_none_or(|(path, _)| path != &self.paths().database)
        {
            let connection = Connection::open_with_flags(
                &self.paths().database,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            connection.busy_timeout(std::time::Duration::from_secs(2))?;
            *database = Some((self.paths().database.clone(), connection));
        }
        let connection = &database.as_ref().expect("database initialized").1;
        // data_version changes for commits by other connections, including
        // in-place streaming edits with equal timestamps and lengths.
        let data_version: i64 =
            connection.query_row("PRAGMA data_version", [], |row| row.get(0))?;
        let mut updates = Vec::new();
        for target in targets {
            if cancellation.is_cancelled() {
                return Err(cancelled());
            }
            let key = (target.slug.clone(), target.session_id.clone());
            let cursor = tracker.by_session.entry(key).or_default();
            let is_new = cursor.session_id != target.session_id || !cursor.seeded;
            if !session_matches(connection, target)? {
                continue;
            }
            if !is_new && cursor.data_version == data_version {
                continue;
            }
            // Decode only after a SQLite commit, never hash whole transcripts
            // on each poll. Replacement keeps in-place streaming updates intact.
            let rows = seed_parts(connection, &target.session_id)?;
            if cancellation.is_cancelled() {
                return Err(cancelled());
            }
            cursor.data_version = data_version;
            let reset = true;
            cursor.session_id.clone_from(&target.session_id);
            cursor.seeded = true;
            let mut lines = Vec::new();
            let mut id = cursor.next_line_id;
            let mut next_id = || {
                id = id.saturating_add(1);
                id
            };
            for row in rows {
                parse_part(&row, &mut next_id, &mut lines);
            }
            cursor.next_line_id = id;
            if reset || !lines.is_empty() {
                updates.push(HarnessOutputUpdate {
                    slug: target.slug.clone(),
                    session_id: target.session_id.clone(),
                    reset,
                    append: lines,
                });
            }
        }
        Ok(updates)
    }

    /// Async adapter retaining updated cursor state on read errors. The SQL
    /// queries have fixed row and payload bounds; cancellation is checked
    /// between targets and after each bounded query.
    pub async fn poll_output_async(
        &self,
        mut tracker: OpenCodeOutputTracker,
        targets: Vec<HarnessOutputTarget>,
        cancellation: &CancellationToken,
    ) -> (
        OpenCodeOutputTracker,
        Result<Vec<HarnessOutputUpdate>, OpenCodeError>,
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
                Err(OpenCodeError::Operation {
                    operation: "read output",
                    detail: error.to_string(),
                }),
            ),
        }
    }
}

fn session_matches(
    connection: &Connection,
    target: &HarnessOutputTarget,
) -> Result<bool, OpenCodeError> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM session_v2 WHERE id = ?1 AND (directory = ?2 OR directory = ?3) AND time_archived IS NULL)",
        params![target.session_id, target.cwd.to_string_lossy(),super::canonical_directory(&target.cwd).to_string_lossy()],
        |row| row.get::<_, bool>(0),
    )?)
}

fn seed_parts(connection: &Connection, session_id: &str) -> Result<Vec<PartRow>, OpenCodeError> {
    let mut statement = connection.prepare("SELECT id,time_created,time_updated,type,CASE WHEN length(CAST(data AS BLOB)) > ?2 THEN NULL ELSE data END FROM session_message WHERE session_id=?1 ORDER BY seq DESC LIMIT ?3")?;
    let mut rows = statement
        .query_map(
            params![session_id, MAX_PART_DATA_BYTES as i64, SEED_PARTS],
            |row| {
                Ok(PartRow {
                    created: row.get(1)?,
                    kind: row.get(3)?,
                    data: row.get(4)?,
                })
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;
    rows.reverse();
    Ok(rows)
}

fn parse_part(
    row: &PartRow,
    next_id: &mut impl FnMut() -> u64,
    lines: &mut Vec<crate::HarnessOutputLine>,
) {
    let Some(raw) = row.data.as_deref() else {
        lines.push(one_line(
            "[output part truncated]",
            HarnessOutputKind::Info,
            row.created,
            next_id,
        ));
        return;
    };
    if raw.len() > MAX_PART_DATA_BYTES {
        lines.push(one_line(
            "[output part truncated]",
            HarnessOutputKind::Info,
            row.created,
            next_id,
        ));
        return;
    }
    let Ok(data) = serde_json::from_str::<Value>(raw) else {
        return;
    };
    match row.kind.as_deref() {
        Some("user" | "synthetic" | "system") => {
            if let Some(text) = data["text"].as_str() {
                lines.extend(text_lines(
                    text,
                    HarnessOutputKind::User,
                    row.created,
                    next_id,
                    "› ",
                ));
            }
        }
        Some("assistant") => {
            if let Some(content) = data["content"].as_array() {
                for part in content {
                    match part["type"].as_str() {
                        Some("text") => {
                            if let Some(text) = part["text"].as_str() {
                                lines.extend(text_lines(
                                    text,
                                    HarnessOutputKind::Assistant,
                                    row.created,
                                    next_id,
                                    "",
                                ));
                            }
                        }
                        Some("reasoning") => {
                            if let Some(text) = part["text"].as_str() {
                                lines.extend(
                                    text_lines(
                                        text,
                                        HarnessOutputKind::Thinking,
                                        row.created,
                                        next_id,
                                        "… ",
                                    )
                                    .into_iter()
                                    .take(1),
                                );
                            }
                        }
                        Some("tool") => {
                            let name = part["name"]
                                .as_str()
                                .or_else(|| part["tool"].as_str())
                                .unwrap_or("tool");
                            let status = part["status"]
                                .as_str()
                                .or_else(|| part["state"]["status"].as_str());
                            let kind = match status {
                                Some("completed" | "succeeded") => HarnessOutputKind::ToolOk,
                                Some("error" | "failed") => HarnessOutputKind::ToolError,
                                _ => HarnessOutputKind::Tool,
                            };
                            lines.push(one_line(&format!("⚒ {name}"), kind, row.created, next_id));
                        }
                        _ => {}
                    }
                }
            }
            if let Some(error) = data.get("error").filter(|e| !e.is_null()) {
                lines.push(one_line(
                    &error.to_string(),
                    HarnessOutputKind::ToolError,
                    row.created,
                    next_id,
                ));
            }
        }
        Some("compaction") => lines.push(one_line(
            "Context compacted",
            HarnessOutputKind::Info,
            row.created,
            next_id,
        )),
        _ => {}
    }
}

fn cancelled() -> OpenCodeError {
    OpenCodeError::Operation {
        operation: "read output",
        detail: "cancelled".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn v2_streaming_updates_existing_message_and_never_reads_another_checkout() {
        let (_dir, harness, db) = super::super::tests::fixture();
        db.execute_batch("INSERT INTO session_v2(id,slug,title,directory,parent_id,time_updated,time_archived,time_idle,time_compacting) VALUES ('ses_a','a','A','/repo',NULL,10,NULL,NULL,NULL); INSERT INTO session_message VALUES ('msg_a','ses_a','assistant',1,10,10,'{\"content\":[{\"type\":\"text\",\"text\":\"first\"}]}');").unwrap();
        let target = HarnessOutputTarget {
            slug: "r".into(),
            cwd: "/repo".into(),
            session_id: "ses_a".into(),
        };
        let mut tracker = OpenCodeOutputTracker::default();
        let cancel = CancellationToken::new();
        let first = harness
            .poll_output(&mut tracker, std::slice::from_ref(&target), &cancel)
            .unwrap();
        assert!(first[0].append.iter().any(|l| l.text == "first"));
        assert!(
            harness
                .poll_output(&mut tracker, std::slice::from_ref(&target), &cancel)
                .unwrap()
                .is_empty()
        );
        db.execute("UPDATE session_message SET data='{\"content\":[{\"type\":\"text\",\"text\":\"other\"}]}'",[]).unwrap();
        let changed = harness
            .poll_output(&mut tracker, std::slice::from_ref(&target), &cancel)
            .unwrap();
        assert!(changed[0].reset);
        assert!(changed[0].append.iter().any(|l| l.text == "other"));
        let wrong = HarnessOutputTarget {
            cwd: "/wrong".into(),
            ..target
        };
        assert!(
            harness
                .poll_output(&mut tracker, &[wrong], &cancel)
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn output_payload_budget_is_independent_of_transcript_length() {
        let (_dir, _harness, db) = super::super::tests::fixture();
        let data=serde_json::json!({"content":[{"type":"text","text":"x".repeat(MAX_PART_DATA_BYTES*2)}]}).to_string();
        for seq in 0..20 {
            db.execute(
                "INSERT INTO session_message VALUES (?1,'ses_a','assistant',?2,10,10,?3)",
                params![format!("msg_{seq}"), seq, data],
            )
            .unwrap();
        }
        let rows = seed_parts(&db, "ses_a").unwrap();
        assert_eq!(rows.len(), SEED_PARTS as usize);
        assert!(
            rows.iter()
                .map(|row| row.data.as_ref().map_or(0, String::len))
                .sum::<usize>()
                <= (MAX_PART_DATA_BYTES + 1) * SEED_PARTS as usize
        );
        let mut lines = Vec::new();
        parse_part(&rows[0], &mut || 1, &mut lines);
        assert_eq!(lines[0].text, "[output part truncated]");
    }
}
