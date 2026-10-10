//! Bounded detailed output tails for exact OpenCode session identities.

use std::collections::HashMap;

use rusqlite::{Connection, OpenFlags, params};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::output::{one_line, text_lines};
use crate::{HarnessOutputKind, HarnessOutputTarget, HarnessOutputUpdate};

use super::{OpenCodeError, OpenCodeHarness};

const SEED_PARTS: i64 = 120;
const MAX_PARTS_PER_TICK: i64 = 256;
const MAX_PART_DATA_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, Default)]
struct Cursor {
    session_id: String,
    after_time: i64,
    after_id: String,
    next_line_id: u64,
    seeded: bool,
}

#[derive(Clone, Debug, Default)]
pub struct OpenCodeOutputTracker {
    by_session: HashMap<(String, String), Cursor>,
}

#[derive(Debug)]
struct PartRow {
    id: String,
    created: i64,
    kind: Option<String>,
    data: Option<String>,
    role: Option<String>,
}

impl OpenCodeHarness {
    /// Read at most 120 seed parts or 256 incremental parts per target. The
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
        let connection = Connection::open_with_flags(
            &self.paths().database,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let mut updates = Vec::new();
        for target in targets {
            if cancellation.is_cancelled() {
                return Err(cancelled());
            }
            let key = (target.slug.clone(), target.session_id.clone());
            let cursor = tracker.by_session.entry(key).or_default();
            let is_new = cursor.session_id != target.session_id || !cursor.seeded;
            if !session_matches(&connection, target)? {
                continue;
            }
            let rows = if is_new {
                seed_parts(&connection, &target.session_id)?
            } else {
                new_parts(&connection, &target.session_id, cursor)?
            };
            if cancellation.is_cancelled() {
                return Err(cancelled());
            }
            let reset = is_new;
            if is_new {
                cursor.session_id.clone_from(&target.session_id);
                cursor.after_time = 0;
                cursor.after_id.clear();
                cursor.seeded = true;
            }
            let mut lines = Vec::new();
            let mut id = cursor.next_line_id;
            let mut next_id = || {
                id = id.saturating_add(1);
                id
            };
            for row in rows {
                cursor.after_time = row.created;
                cursor.after_id.clone_from(&row.id);
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
        "SELECT EXISTS(SELECT 1 FROM session WHERE id = ?1 AND directory = ?2 AND time_archived IS NULL)",
        params![target.session_id, target.cwd.to_string_lossy()],
        |row| row.get::<_, bool>(0),
    )?)
}

fn seed_parts(connection: &Connection, session_id: &str) -> Result<Vec<PartRow>, OpenCodeError> {
    let mut statement = connection.prepare(
        "SELECT p.id, p.time_created, json_extract(p.data, '$.type'), substr(p.data, 1, ?2), json_extract(m.data, '$.role') FROM part p JOIN message m ON m.id = p.message_id WHERE p.session_id = ?1 ORDER BY p.time_created DESC, p.id DESC LIMIT ?3",
    )?;
    let mut rows = statement
        .query(params![
            session_id,
            MAX_PART_DATA_BYTES as i64 + 1,
            SEED_PARTS
        ])?
        .mapped(part_row)
        .collect::<Result<Vec<_>, _>>()?;
    rows.reverse();
    Ok(rows)
}

fn new_parts(
    connection: &Connection,
    session_id: &str,
    cursor: &Cursor,
) -> Result<Vec<PartRow>, OpenCodeError> {
    let mut statement = connection.prepare(
        "SELECT p.id, p.time_created, json_extract(p.data, '$.type'), substr(p.data, 1, ?4), json_extract(m.data, '$.role') FROM part p JOIN message m ON m.id = p.message_id WHERE p.session_id = ?1 AND (p.time_created > ?2 OR (p.time_created = ?2 AND p.id > ?3)) ORDER BY p.time_created ASC, p.id ASC LIMIT ?5",
    )?;
    let rows = statement
        .query(params![
            session_id,
            cursor.after_time,
            cursor.after_id,
            MAX_PART_DATA_BYTES as i64 + 1,
            MAX_PARTS_PER_TICK
        ])?
        .mapped(part_row)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn part_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PartRow> {
    Ok(PartRow {
        id: row.get(0)?,
        created: row.get(1)?,
        kind: row.get(2)?,
        data: row.get(3)?,
        role: row.get(4)?,
    })
}

fn parse_part(
    row: &PartRow,
    next_id: &mut impl FnMut() -> u64,
    lines: &mut Vec<crate::HarnessOutputLine>,
) {
    let Some(raw) = row.data.as_deref() else {
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
        Some("text") => {
            let Some(text) = data["text"].as_str() else {
                return;
            };
            let (kind, prefix) = if row.role.as_deref() == Some("user") {
                (HarnessOutputKind::User, "› ")
            } else {
                (HarnessOutputKind::Assistant, "")
            };
            lines.extend(text_lines(text, kind, row.created, next_id, prefix));
        }
        Some("reasoning") => {
            if let Some(text) = data["text"].as_str() {
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
            let tool = data["tool"]
                .as_str()
                .or_else(|| data["name"].as_str())
                .unwrap_or("tool");
            let state = &data["state"];
            let status = state["status"].as_str();
            let title = state["title"]
                .as_str()
                .map(|title| format!(" {title}"))
                .unwrap_or_default();
            let kind = match status {
                Some("error") => HarnessOutputKind::ToolError,
                Some("completed") => HarnessOutputKind::ToolOk,
                _ => HarnessOutputKind::Tool,
            };
            lines.push(one_line(
                &format!("⚒ {tool}{title}"),
                kind,
                row.created,
                next_id,
            ));
        }
        Some("patch") => lines.push(one_line(
            "⚒ patch",
            HarnessOutputKind::Tool,
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
    use std::num::NonZeroUsize;
    use tempfile::tempdir;
    use wt_platform::process::ProcessRunner;
    use wt_tmux::TmuxServer;

    fn setup() -> (tempfile::TempDir, OpenCodeHarness, Connection) {
        let dir = tempdir().unwrap();
        let paths = super::super::OpenCodePaths::new(dir.path(), dir.path().join("cache"))
            .with_database(dir.path().join("opencode.db"));
        let db = Connection::open(&paths.database).unwrap();
        db.execute_batch(
            "CREATE TABLE session(id TEXT PRIMARY KEY, title TEXT, directory TEXT, time_updated INTEGER, time_archived INTEGER); \
             CREATE TABLE message(id TEXT PRIMARY KEY, session_id TEXT, data TEXT, time_created INTEGER, time_updated INTEGER); \
             CREATE TABLE part(id TEXT PRIMARY KEY, session_id TEXT, message_id TEXT, data TEXT, time_created INTEGER);",
        ).unwrap();
        let harness = OpenCodeHarness::new(
            paths,
            ProcessRunner::new(NonZeroUsize::new(1).unwrap()),
            wt_tmux::TmuxClient::new(
                ProcessRunner::new(NonZeroUsize::new(1).unwrap()),
                TmuxServer::named("opencode-output-fixture"),
            ),
        );
        (dir, harness, db)
    }

    #[test]
    fn tails_exact_session_seeds_text_and_tools_then_reads_equal_timestamp_parts_once() {
        let (_dir, harness, db) = setup();
        db.execute("INSERT INTO session VALUES ('ses-old','Old','/repo',100,NULL), ('ses-new','New','/repo',200,NULL)", []).unwrap();
        db.execute("INSERT INTO message VALUES ('m-old-user','ses-old','{\"role\":\"user\"}',10,10), ('m-old-assistant','ses-old','{\"role\":\"assistant\"}',20,20), ('m-new','ses-new','{\"role\":\"assistant\"}',30,30)", []).unwrap();
        db.execute("INSERT INTO part VALUES ('p-user','ses-old','m-old-user','{\"type\":\"text\",\"text\":\"hello from old session\"}',11), ('p-tool','ses-old','m-old-assistant','{\"type\":\"tool\",\"tool\":\"Read\",\"state\":{\"status\":\"completed\"}}',12), ('p-new','ses-new','m-new','{\"type\":\"text\",\"text\":\"wrong session\"}',31)", []).unwrap();
        let target = HarnessOutputTarget {
            slug: "slug".into(),
            cwd: "/repo".into(),
            session_id: "ses-old".into(),
        };
        let mut tracker = OpenCodeOutputTracker::default();
        let cancel = CancellationToken::new();
        let seeded = harness
            .poll_output(&mut tracker, std::slice::from_ref(&target), &cancel)
            .unwrap();
        assert_eq!(seeded.len(), 1);
        assert!(seeded[0].reset);
        assert!(
            seeded[0]
                .append
                .iter()
                .any(|line| line.text.contains("hello from old session"))
        );
        assert!(
            seeded[0]
                .append
                .iter()
                .any(|line| line.kind == HarnessOutputKind::ToolOk)
        );
        assert!(
            !seeded[0]
                .append
                .iter()
                .any(|line| line.text.contains("wrong session"))
        );

        db.execute(
            "INSERT INTO message VALUES ('m-delta','ses-old','{\"role\":\"assistant\"}',40,40)",
            [],
        )
        .unwrap();
        db.execute("INSERT INTO part VALUES ('p2','ses-old','m-delta','{\"type\":\"text\",\"text\":\"second answer\"}',50), ('p3','ses-old','m-delta','{\"type\":\"patch\"}',50)", []).unwrap();
        let delta = harness
            .poll_output(&mut tracker, std::slice::from_ref(&target), &cancel)
            .unwrap();
        assert_eq!(delta.len(), 1);
        assert!(!delta[0].reset);
        assert!(
            delta[0]
                .append
                .iter()
                .any(|line| line.text.contains("second answer"))
        );
        assert!(delta[0].append.iter().any(|line| line.text == "⚒ patch"));
        assert!(
            harness
                .poll_output(&mut tracker, &[target], &cancel)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn cancelled_output_poll_fails_without_mutating_cursor() {
        let (_dir, harness, db) = setup();
        db.execute("INSERT INTO session VALUES ('ses','S','/repo',1,NULL)", [])
            .unwrap();
        let target = HarnessOutputTarget {
            slug: "slug".into(),
            cwd: "/repo".into(),
            session_id: "ses".into(),
        };
        let mut tracker = OpenCodeOutputTracker::default();
        let before = tracker.clone();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(
            harness
                .poll_output(&mut tracker, &[target], &cancel)
                .is_err()
        );
        assert_eq!(tracker.by_session.len(), before.by_session.len());
    }
}
