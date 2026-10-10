use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io,
    path::{Path, PathBuf},
};

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::Value;
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use wt_platform::process::ProcessRunner;
use wt_tmux::{CreateSession, OptionScope, PaneTarget, TmuxClient, TmuxError};

mod output;

pub use output::OpenCodeOutputTracker;

use crate::{
    DiscoveryRequest, HarnessExtras, HarnessSession, HarnessSpawnRequest, SpawnCommand,
    persist::{FileLock, atomic_write_json, read_object},
};

const SLOT_INFIX: &str = "-opencode";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenCodePaths {
    pub home: PathBuf,
    pub database: PathBuf,
    pub cache_dir: PathBuf,
    pub lock_dir: PathBuf,
}

impl OpenCodePaths {
    pub fn new(home: impl Into<PathBuf>, cache_dir: impl Into<PathBuf>) -> Self {
        let home = home.into();
        let cache_dir = cache_dir.into();
        Self {
            database: home.join(".local/share/opencode/opencode.db"),
            lock_dir: cache_dir.join("locks"),
            home,
            cache_dir,
        }
    }
    pub fn with_database(mut self, path: impl Into<PathBuf>) -> Self {
        self.database = path.into();
        self
    }
    pub fn names_file(&self) -> PathBuf {
        self.cache_dir.join("opencode-sessions.json")
    }
}

#[derive(Debug, Error)]
pub enum OpenCodeError {
    #[error("OpenCode {operation}: {detail}")]
    Operation {
        operation: &'static str,
        detail: String,
    },
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Tmux(#[from] TmuxError),
}

#[derive(Clone, Debug, PartialEq)]
pub struct OpenCodeCost {
    pub five_hour: f64,
    pub seven_day: f64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenCodeSendOutcome {
    pub cold_started: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenCodeEvent {
    pub slug: String,
    pub level: OpenCodeEventLevel,
    pub text: String,
    pub changed: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OpenCodeActivityBatch {
    pub events: Vec<OpenCodeEvent>,
    pub changed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenCodeEventLevel {
    Info,
    Dim,
    Ok,
    Warn,
}

#[derive(Clone, Debug, Default)]
struct OpenCodeSnapshot {
    title: String,
    latest_message_id: Option<String>,
    latest_role: Option<String>,
    latest_completed: Option<i64>,
    latest_created: Option<i64>,
    parts_seen: HashSet<String>,
}

#[derive(Clone, Default)]
pub struct OpenCodeActivityTracker {
    baseline: bool,
    known_sessions: HashMap<String, HashSet<String>>,
    snapshots: HashMap<(String, String), OpenCodeSnapshot>,
}

#[derive(Clone)]
pub struct OpenCodeHarness {
    paths: OpenCodePaths,
    _runner: ProcessRunner,
    tmux: TmuxClient,
}

impl OpenCodeHarness {
    pub fn new(paths: OpenCodePaths, runner: ProcessRunner, tmux: TmuxClient) -> Self {
        Self {
            paths,
            _runner: runner,
            tmux,
        }
    }
    pub fn paths(&self) -> &OpenCodePaths {
        &self.paths
    }
    pub fn tmux_session_name(&self, slug: &str) -> String {
        format!("{slug}{SLOT_INFIX}")
    }
    pub fn build_spawn_command(&self, request: &HarnessSpawnRequest) -> SpawnCommand {
        let mut args = Vec::new();
        if let Some(session) = &request.resume_session_id {
            args.extend(["-s".to_owned(), session.clone()]);
        }
        SpawnCommand {
            program: PathBuf::from("opencode"),
            args,
        }
    }

    pub fn discover_sync(
        &self,
        request: &DiscoveryRequest,
    ) -> Result<Vec<HarnessSession>, OpenCodeError> {
        if !self.paths.database.exists() {
            return Ok(Vec::new());
        }
        let connection = open_readonly(&self.paths.database)?;
        let mut statement = connection.prepare(
            "SELECT id, title, time_updated FROM session WHERE directory = ?1 AND time_archived IS NULL ORDER BY time_updated DESC LIMIT 50",
        )?;
        let rows = statement
            .query_map([request.worktree_path.to_string_lossy().as_ref()], |row| {
                Ok(SessionRow {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    updated: row.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let names = reconcile_opencode_names(
            &self.paths,
            &request.slug,
            &rows.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
        )?;
        let tmux = self.tmux_session_name(&request.slug);
        rows.into_iter()
            .map(|row| {
                let managed = names
                    .get(&row.id)
                    .cloned()
                    .unwrap_or_else(|| row.id.chars().take(8).collect());
                let last = read_last_message(&connection, &row.id)?;
                let state = derive_opencode_state(last.as_ref());
                Ok(HarnessSession {
                    display_name: opencode_display_title(Some(&row.title), &row.id)
                        .unwrap_or_else(|| managed.clone()),
                    session_id: row.id,
                    tmux_session_name: tmux.clone(),
                    last_active_ms: Some(row.updated),
                    is_live: false,
                    extras: HarnessExtras {
                        managed_name: Some(managed),
                        derived_state: state,
                        tail_ended_at: last.as_ref().map(|m| m.time_updated),
                        ..HarnessExtras::default()
                    },
                })
            })
            .collect()
    }

    pub async fn discover(
        &self,
        request: &DiscoveryRequest,
        cancellation: &CancellationToken,
    ) -> Result<Vec<HarnessSession>, OpenCodeError> {
        let this = self.clone();
        let request = request.clone();
        let task = tokio::task::spawn_blocking(move || this.discover_sync(&request));
        tokio::select! {
            _ = cancellation.cancelled() => Err(OpenCodeError::Operation { operation: "discover", detail: "cancelled".into() }),
            result = task => result.map_err(|e| OpenCodeError::Operation { operation: "discover", detail: e.to_string() })?,
        }
    }

    pub async fn start(
        &self,
        request: &HarnessSpawnRequest,
        cancellation: &CancellationToken,
    ) -> Result<bool, OpenCodeError> {
        let name = self.tmux_session_name(&request.slug);
        let _start_lock = FileLock::acquire_async(
            &self.paths.lock_dir.join(format!("__start__{name}.lock")),
            cancellation,
        )
        .await?;
        if self.tmux.session_exists(&name, cancellation).await? {
            return Ok(false);
        }
        let command = self.build_spawn_command(request);
        self.tmux
            .create_session(
                &CreateSession {
                    name,
                    cwd: request.worktree_path.clone(),
                    command: std::iter::once(command.program.to_string_lossy().into_owned())
                        .chain(command.args)
                        .collect(),
                    width: None,
                    height: None,
                },
                cancellation,
            )
            .await?;
        if let Some(session_id) = request.resume_session_id.as_deref() {
            let _ = self
                .tmux
                .set_option(
                    &OptionScope::Session(self.tmux_session_name(&request.slug)),
                    "@wt-harness-session-id",
                    Some(session_id),
                    cancellation,
                )
                .await;
        }
        Ok(true)
    }

    pub async fn send_terminal(
        &self,
        slug: &str,
        text: &str,
        cancellation: &CancellationToken,
    ) -> Result<(), OpenCodeError> {
        if text.trim().is_empty() {
            return Err(OpenCodeError::Operation {
                operation: "send",
                detail: "message is empty".into(),
            });
        }
        let name = self.tmux_session_name(slug);
        let _lock = FileLock::acquire_async(
            &self
                .paths
                .lock_dir
                .join(format!("__opencode_send_{name}.lock")),
            cancellation,
        )
        .await?;
        if !self.tmux.session_exists(&name, cancellation).await? {
            return Err(OpenCodeError::Operation {
                operation: "send",
                detail: format!("OpenCode slot {name} is not live"),
            });
        }
        let target = PaneTarget::active_session_pane(&name);
        self.tmux.send_literal(&target, text, cancellation).await?;
        self.tmux
            .send_keys(&target, &["Enter"], cancellation)
            .await?;
        Ok(())
    }

    /// Cold-start a slot when needed, wait for its first stable screen, then
    /// paste once and submit once. This deliberately reports no completion
    /// claim because OpenCode has no simple durable prompt receipt.
    pub async fn send_message(
        &self,
        slug: &str,
        cwd: &Path,
        text: &str,
        cancellation: &CancellationToken,
    ) -> Result<OpenCodeSendOutcome, OpenCodeError> {
        if text.trim().is_empty() {
            return Err(OpenCodeError::Operation {
                operation: "send",
                detail: "message is empty".into(),
            });
        }
        let name = self.tmux_session_name(slug);
        let _lock = FileLock::acquire_async(
            &self.paths.lock_dir.join(format!("__inject__{name}.lock")),
            cancellation,
        )
        .await?;
        let cold_started = self
            .start(
                &HarnessSpawnRequest {
                    worktree_path: cwd.to_path_buf(),
                    slug: slug.to_owned(),
                    managed_name: None,
                    resume_session_id: None,
                    display_label: None,
                },
                cancellation,
            )
            .await?;
        let target = PaneTarget::active_session_pane(&name);
        if cold_started {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
            let mut previous: Option<String> = None;
            loop {
                if cancellation.is_cancelled() {
                    return Err(OpenCodeError::Operation {
                        operation: "send",
                        detail: "cancelled while waiting for OpenCode startup".into(),
                    });
                }
                let current = self
                    .tmux
                    .capture_pane(&target, Some(80), cancellation)
                    .await?
                    .trim()
                    .to_owned();
                if !current.is_empty() && previous.as_deref() == Some(current.as_str()) {
                    break;
                }
                previous = Some(current);
                if tokio::time::Instant::now() >= deadline {
                    return Err(OpenCodeError::Operation {
                        operation: "send",
                        detail: "OpenCode did not reach a stable startup screen".into(),
                    });
                }
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        } else {
            tokio::select! {
                _ = cancellation.cancelled() => return Err(OpenCodeError::Operation { operation: "send", detail: "cancelled before terminal submission".into() }),
                _ = tokio::time::sleep(std::time::Duration::from_millis(300)) => (),
            }
        }
        if !self.tmux.session_exists(&name, cancellation).await? {
            return Err(OpenCodeError::Operation {
                operation: "send",
                detail: "OpenCode slot exited before terminal submission".into(),
            });
        }
        self.tmux.send_literal(&target, text, cancellation).await?;
        self.tmux
            .send_keys(&target, &["Enter"], cancellation)
            .await?;
        Ok(OpenCodeSendOutcome { cold_started })
    }

    pub fn cost(&self, now: i64) -> Result<Option<OpenCodeCost>, OpenCodeError> {
        if !self.paths.database.exists() {
            return Ok(None);
        }
        let connection = open_readonly(&self.paths.database)?;
        let cost_for = |after: i64| -> Result<f64, rusqlite::Error> {
            connection.query_row(
                "SELECT COALESCE(SUM(CAST(json_extract(data, '$.cost') AS REAL)), 0) FROM message WHERE json_extract(data, '$.role') = 'assistant' AND time_created > ?1",
                [after], |row| row.get(0),
            )
        };
        Ok(Some(OpenCodeCost {
            five_hour: cost_for(now - 5 * 60 * 60 * 1000)?,
            seven_day: cost_for(now - 7 * 24 * 60 * 60 * 1000)?,
        }))
    }

    pub fn poll_activity(
        &self,
        tracker: &mut OpenCodeActivityTracker,
        active: &[(String, PathBuf)],
    ) -> Result<OpenCodeActivityBatch, OpenCodeError> {
        if !self.paths.database.exists() {
            return Ok(OpenCodeActivityBatch::default());
        }
        let connection = open_readonly(&self.paths.database)?;
        let baseline = !tracker.baseline;
        let mut events = Vec::new();
        let mut changed = false;
        for (slug, cwd) in active {
            let sessions = list_sessions(&connection, cwd)?;
            let known = tracker.known_sessions.entry(slug.clone()).or_default();
            for session in sessions {
                let key = (slug.clone(), session.id.clone());
                let is_new = known.insert(session.id.clone());
                let last = read_last_message(&connection, &session.id)?;
                if baseline {
                    tracker
                        .snapshots
                        .insert(key, snapshot(&session, last.as_ref(), &connection)?);
                    continue;
                }
                if is_new {
                    changed = true;
                    events.push(OpenCodeEvent {
                        slug: slug.clone(),
                        level: OpenCodeEventLevel::Info,
                        text: format!("new session: {} · {slug}", short_id(&session.id)),
                        changed: true,
                    });
                    tracker
                        .snapshots
                        .insert(key, snapshot(&session, last.as_ref(), &connection)?);
                    continue;
                }
                let Some(snap) = tracker.snapshots.get_mut(&key) else {
                    tracker
                        .snapshots
                        .insert(key, snapshot(&session, last.as_ref(), &connection)?);
                    continue;
                };
                if !session.title.is_empty() && session.title != snap.title {
                    snap.title.clone_from(&session.title);
                    events.push(OpenCodeEvent {
                        slug: slug.clone(),
                        level: OpenCodeEventLevel::Dim,
                        text: format!("renamed: {} · {slug}", session.title),
                        changed: false,
                    });
                }
                if let Some(message) = &last {
                    if snap.latest_message_id.as_deref() != Some(message.id.as_str()) {
                        if message.role.as_deref() == Some("user") {
                            changed = true;
                            let prompt =
                                read_user_text(&connection, &message.id)?.unwrap_or_default();
                            let clipped = truncate_chars(&prompt, 60);
                            events.push(OpenCodeEvent {
                                slug: slug.clone(),
                                level: OpenCodeEventLevel::Dim,
                                text: format!(
                                    "→ {} · {slug}",
                                    if clipped.is_empty() {
                                        "(empty)".into()
                                    } else {
                                        clipped
                                    }
                                ),
                                changed: true,
                            });
                        } else if message.role.as_deref() == Some("assistant")
                            && message.completed.is_some()
                            && snap.latest_completed.is_none()
                            && snap.latest_role.as_deref() == Some("assistant")
                        {
                            changed = true;
                            events.push(response_done(
                                slug,
                                message.completed.unwrap_or_default(),
                                snap.latest_created,
                            ));
                        } else if message.role.as_deref() == Some("assistant") {
                            changed = true;
                        }
                        snap.latest_message_id = Some(message.id.clone());
                        snap.latest_role = message.role.clone();
                        snap.latest_completed = message.completed;
                        snap.latest_created = Some(message.time_created);
                    } else if message.role.as_deref() == Some("assistant")
                        && message.completed.is_some()
                        && snap.latest_completed.is_none()
                    {
                        changed = true;
                        events.push(response_done(
                            slug,
                            message.completed.unwrap_or_default(),
                            snap.latest_created,
                        ));
                        snap.latest_completed = message.completed;
                    }
                }
                let after = snap.latest_created.unwrap_or(0);
                for part in read_new_parts(&connection, &session.id, after)? {
                    if !snap.parts_seen.insert(part.id) || part.kind.as_deref() != Some("tool") {
                        continue;
                    }
                    let parsed = part
                        .data
                        .as_deref()
                        .and_then(|s| serde_json::from_str::<Value>(s).ok())
                        .unwrap_or(Value::Null);
                    let name = parsed
                        .get("tool")
                        .or_else(|| parsed.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    let error =
                        parsed.pointer("/state/status").and_then(Value::as_str) == Some("error");
                    events.push(OpenCodeEvent {
                        slug: slug.clone(),
                        level: if error {
                            OpenCodeEventLevel::Warn
                        } else {
                            OpenCodeEventLevel::Info
                        },
                        text: if error {
                            format!("tool error: {name} · {slug}")
                        } else {
                            format!("tool: {name} · {slug}")
                        },
                        changed: false,
                    });
                }
            }
        }
        while tracker.snapshots.len() > 256 {
            if let Some(key) = tracker.snapshots.keys().next().cloned() {
                tracker.snapshots.remove(&key);
            } else {
                break;
            }
        }
        tracker.baseline = true;
        Ok(OpenCodeActivityBatch { events, changed })
    }

    pub async fn poll_activity_async(
        &self,
        mut tracker: OpenCodeActivityTracker,
        active: Vec<(String, PathBuf)>,
        cancellation: &CancellationToken,
    ) -> Result<(OpenCodeActivityTracker, OpenCodeActivityBatch), OpenCodeError> {
        let harness = self.clone();
        let task = tokio::task::spawn_blocking(move || {
            let batch = harness.poll_activity(&mut tracker, &active)?;
            Ok::<_, OpenCodeError>((tracker, batch))
        });
        tokio::select! {
            _ = cancellation.cancelled() => Err(OpenCodeError::Operation { operation: "poll activity", detail: "cancelled".into() }),
            result = task => result.map_err(|error| OpenCodeError::Operation { operation: "poll activity", detail: error.to_string() })?,
        }
    }

    pub async fn cost_async(
        &self,
        now: i64,
        cancellation: &CancellationToken,
    ) -> Result<Option<OpenCodeCost>, OpenCodeError> {
        let harness = self.clone();
        let task = tokio::task::spawn_blocking(move || harness.cost(now));
        tokio::select! {
            _ = cancellation.cancelled() => Err(OpenCodeError::Operation { operation: "read cost", detail: "cancelled".into() }),
            result = task => result.map_err(|error| OpenCodeError::Operation { operation: "read cost", detail: error.to_string() })?,
        }
    }
}

#[derive(Clone, Debug)]
struct SessionRow {
    id: String,
    title: String,
    updated: i64,
}
#[derive(Clone, Debug)]
struct MessageRow {
    id: String,
    role: Option<String>,
    completed: Option<i64>,
    time_created: i64,
    time_updated: i64,
}
#[derive(Clone, Debug)]
struct PartRow {
    id: String,
    kind: Option<String>,
    data: Option<String>,
}

fn open_readonly(path: &Path) -> Result<Connection, rusqlite::Error> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(std::time::Duration::from_secs(2))?;
    Ok(conn)
}

fn list_sessions(conn: &Connection, cwd: &Path) -> Result<Vec<SessionRow>, rusqlite::Error> {
    let mut statement = conn.prepare("SELECT id, title, time_updated FROM session WHERE directory = ?1 AND time_archived IS NULL ORDER BY time_updated DESC LIMIT 50")?;
    statement
        .query_map([cwd.to_string_lossy().as_ref()], |row| {
            Ok(SessionRow {
                id: row.get(0)?,
                title: row.get(1)?,
                updated: row.get(2)?,
            })
        })?
        .collect()
}

fn read_last_message(conn: &Connection, id: &str) -> Result<Option<MessageRow>, rusqlite::Error> {
    conn.query_row("SELECT id, data, time_created, time_updated FROM message WHERE session_id = ?1 ORDER BY time_created DESC LIMIT 1", [id], |row| {
        let data: String = row.get(1)?;
        let json = serde_json::from_str::<Value>(&data).unwrap_or(Value::Null);
        Ok(MessageRow { id: row.get(0)?, role: json.get("role").and_then(Value::as_str).map(str::to_owned),
            completed: json.pointer("/time/completed").and_then(Value::as_i64), time_created: row.get(2)?, time_updated: row.get(3)? })
    }).optional()
}

fn read_user_text(conn: &Connection, id: &str) -> Result<Option<String>, rusqlite::Error> {
    conn.query_row("SELECT json_extract(data, '$.text') FROM part WHERE message_id = ?1 AND json_extract(data, '$.type') = 'text' ORDER BY time_created ASC LIMIT 1", [id], |row| row.get(0)).optional()
}

fn read_new_parts(
    conn: &Connection,
    session: &str,
    after: i64,
) -> Result<Vec<PartRow>, rusqlite::Error> {
    let mut stmt = conn.prepare("SELECT id, data FROM part WHERE session_id = ?1 AND time_created > ?2 ORDER BY time_created ASC")?;
    stmt.query_map((session, after), |row| {
        let data: Option<String> = row.get(1)?;
        let json = data
            .as_deref()
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
            .unwrap_or(Value::Null);
        Ok(PartRow {
            id: row.get(0)?,
            kind: json.get("type").and_then(Value::as_str).map(str::to_owned),
            data,
        })
    })?
    .collect()
}

fn derive_opencode_state(row: Option<&MessageRow>) -> Option<crate::DerivedState> {
    match row.and_then(|r| r.role.as_deref()) {
        Some("assistant") if row.is_some_and(|r| r.completed.is_none()) => {
            Some(crate::DerivedState::Working)
        }
        Some("assistant") => Some(crate::DerivedState::Waiting),
        Some("user") => Some(crate::DerivedState::Working),
        _ => None,
    }
}

pub fn opencode_display_title(title: Option<&str>, session_id: &str) -> Option<String> {
    let title = title?.trim();
    if title.is_empty()
        || title == session_id
        || title.starts_with("ses_")
        || title.to_ascii_lowercase().starts_with("new session")
    {
        None
    } else {
        Some(title.to_owned())
    }
}

fn reconcile_opencode_names(
    paths: &OpenCodePaths,
    slug: &str,
    ids: &[String],
) -> io::Result<BTreeMap<String, String>> {
    let _guard = FileLock::acquire(&paths.lock_dir.join("__opencode_names__.lock"))?;
    let file = paths.names_file();
    let mut root = read_object(&file);
    let old = root
        .get(slug)
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut out = old
        .iter()
        .filter_map(|(id, value)| {
            value
                .as_str()
                .filter(|name| !name.is_empty())
                .map(|name| (id.clone(), name.to_owned()))
        })
        .collect::<BTreeMap<_, _>>();
    let mut used = out.values().cloned().collect::<HashSet<_>>();
    for id in ids {
        if out.contains_key(id) {
            continue;
        }
        let name = if !used.contains("primary") {
            "primary".to_owned()
        } else {
            let mut n = 2;
            while used.contains(&n.to_string()) {
                n += 1;
            }
            n.to_string()
        };
        used.insert(name.clone());
        out.insert(id.clone(), name);
    }
    let mut merged = old;
    for (id, name) in &out {
        merged.insert(id.clone(), Value::String(name.clone()));
    }
    let merged = Value::Object(merged);
    if root.get(slug) != Some(&merged) {
        root[slug] = merged;
        atomic_write_json(&file, &root)?;
    }
    Ok(out)
}

fn snapshot(
    session: &SessionRow,
    last: Option<&MessageRow>,
    _conn: &Connection,
) -> Result<OpenCodeSnapshot, OpenCodeError> {
    Ok(OpenCodeSnapshot {
        title: session.title.clone(),
        latest_message_id: last.map(|r| r.id.clone()),
        latest_role: last.and_then(|r| r.role.clone()),
        latest_completed: last.and_then(|r| r.completed),
        latest_created: last.map(|r| r.time_created),
        parts_seen: HashSet::new(),
    })
}

fn response_done(slug: &str, completed: i64, started: Option<i64>) -> OpenCodeEvent {
    let elapsed = started
        .and_then(|start| (completed - start > 0).then_some((completed - start) as f64 / 1000.0));
    OpenCodeEvent {
        slug: slug.to_owned(),
        level: OpenCodeEventLevel::Ok,
        text: format!(
            "response done{} · {slug}",
            elapsed
                .map(|s| format!(" ({s:.1}s) "))
                .unwrap_or_default()
                .trim_end()
        ),
        changed: true,
    }
}

fn truncate_chars(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        value.to_owned()
    } else {
        format!("{}…", value.chars().take(max).collect::<String>())
    }
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::num::NonZeroUsize;
    use wt_platform::process::ProcessRunner;
    use wt_tmux::{TmuxServer, TmuxSocket};

    fn harness(paths: OpenCodePaths, scratch: &Path) -> OpenCodeHarness {
        let runner = ProcessRunner::new(NonZeroUsize::new(2).unwrap());
        let tmux = TmuxClient::new(
            runner.clone(),
            TmuxServer::new(TmuxSocket::Path(scratch.join("tmux.sock"))),
        );
        OpenCodeHarness::new(paths, runner, tmux)
    }

    fn create_db(path: &Path) -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch("CREATE TABLE session(id TEXT PRIMARY KEY, title TEXT, directory TEXT, time_updated INTEGER, time_archived INTEGER); CREATE TABLE message(id TEXT PRIMARY KEY, session_id TEXT, data TEXT, time_created INTEGER, time_updated INTEGER); CREATE TABLE part(id TEXT PRIMARY KEY, session_id TEXT, message_id TEXT, data TEXT, time_created INTEGER);").unwrap();
        conn
    }

    #[test]
    fn discovery_reads_only_unarchived_sessions_for_exact_directory_and_keeps_name_extensions() {
        let dir = tempfile::tempdir().unwrap();
        let paths = OpenCodePaths::new(dir.path(), dir.path().join("cache"))
            .with_database(dir.path().join("opencode.db"));
        fs::create_dir_all(&paths.cache_dir).unwrap();
        let conn = create_db(&paths.database);
        conn.execute("INSERT INTO session VALUES ('ses_one','Useful title','/repo/a',100,NULL), ('ses_old','Archived','/repo/a',200,1), ('ses_other','Wrong cwd','/repo/b',300,NULL)", []).unwrap();
        conn.execute("INSERT INTO message VALUES ('msg_one','ses_one','{\"role\":\"assistant\",\"time\":{\"completed\":90}}',80,95)", []).unwrap();
        drop(conn);
        fs::write(
            paths.names_file(),
            r#"{"slug":{"future":7},"other":{"keep":"yes"}}"#,
        )
        .unwrap();
        let service = harness(paths.clone(), dir.path());
        let found = service
            .discover_sync(&DiscoveryRequest {
                slug: "slug".into(),
                worktree_path: "/repo/a".into(),
                live_session_id: None,
            })
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].session_id, "ses_one");
        assert_eq!(found[0].display_name, "Useful title");
        assert_eq!(
            found[0].extras.derived_state,
            Some(crate::DerivedState::Waiting)
        );
        let names: Value = serde_json::from_slice(&fs::read(paths.names_file()).unwrap()).unwrap();
        assert_eq!(names["slug"]["future"], 7);
        assert_eq!(names["other"]["keep"], "yes");
    }

    #[test]
    fn baseline_activity_is_silent_then_reports_new_prompt_and_tool_without_repeating() {
        let dir = tempfile::tempdir().unwrap();
        let paths = OpenCodePaths::new(dir.path(), dir.path().join("cache"))
            .with_database(dir.path().join("opencode.db"));
        let conn = create_db(&paths.database);
        conn.execute(
            "INSERT INTO session VALUES ('ses_a','A','/repo',10,NULL)",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO message VALUES ('m0','ses_a','{\"role\":\"assistant\",\"time\":{\"completed\":9}}',8,9)", []).unwrap();
        drop(conn);
        let service = harness(paths.clone(), dir.path());
        let active = vec![("slug".to_owned(), PathBuf::from("/repo"))];
        let mut tracker = OpenCodeActivityTracker::default();
        assert!(
            !service
                .poll_activity(&mut tracker, &active)
                .unwrap()
                .changed
        );
        let conn = Connection::open(&paths.database).unwrap();
        conn.execute(
            "INSERT INTO message VALUES ('m1','ses_a','{\"role\":\"user\"}',11,11)",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO part VALUES ('p1','ses_a','m1','{\"type\":\"text\",\"text\":\"hello\"}',12), ('p2','ses_a','m1','{\"type\":\"tool\",\"tool\":\"Read\"}',13)", []).unwrap();
        drop(conn);
        let batch = service.poll_activity(&mut tracker, &active).unwrap();
        assert!(batch.changed);
        assert!(
            batch
                .events
                .iter()
                .any(|e| e.text.contains("hello") && e.changed)
        );
        assert!(batch.events.iter().any(|e| e.text.contains("tool: Read")));
        let empty = service.poll_activity(&mut tracker, &active).unwrap();
        assert!(!empty.changed);
        assert!(empty.events.is_empty());
    }

    #[test]
    fn cost_queries_are_read_only_and_use_assistant_cost_window() {
        let dir = tempfile::tempdir().unwrap();
        let paths = OpenCodePaths::new(dir.path(), dir.path().join("cache"))
            .with_database(dir.path().join("opencode.db"));
        let conn = create_db(&paths.database);
        conn.execute("INSERT INTO session VALUES ('s','S','/repo',1,NULL)", [])
            .unwrap();
        conn.execute("INSERT INTO message VALUES ('a','s','{\"role\":\"assistant\",\"cost\":1.25}',1000,1000), ('u','s','{\"role\":\"user\",\"cost\":99}',1000,1000)", []).unwrap();
        drop(conn);
        let service = harness(paths, dir.path());
        let cost = service.cost(2000).unwrap().unwrap();
        assert_eq!(cost.five_hour, 1.25);
        assert_eq!(cost.seven_day, 1.25);
    }
}
