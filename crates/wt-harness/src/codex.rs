use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use serde_json::Value;
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use wt_platform::process::{ProcessError, ProcessRunner};
use wt_tmux::{CreateSession, OptionScope, TmuxClient, TmuxError};

use crate::{
    DerivedState, DiscoveryRequest, HarnessExtras, HarnessSession, HarnessSpawnRequest,
    SpawnCommand,
    persist::{FileLock, atomic_write_json, read_object},
};

mod app_server;
mod events;
mod messaging;
mod usage;

pub use app_server::{
    CodexAppServerError, CodexAppServerFailureKind, CodexAppServerInfo, CodexQueueDelivery,
    CodexQueueSubmission, CodexThreadStatus,
};
pub use events::{CodexActivityBatch, CodexActivityTracker, CodexEvent, CodexEventLevel};
pub use messaging::{CodexMessageOutcome, CodexMessageTarget, CodexMessenger};
pub use usage::{CodexUsage, read_codex_usage};

const CODEX_SLOT_INFIX: &str = "-codex";
const CODEX_MANAGER_PROMPT: &str = "This is the dedicated wt manager session. Read $manager to initialize, then wait for a request.\n\n<!-- wt:codex-slot=manager:v1 -->";
const CODEX_MAIN_PROMPT: &str = "This is the wt main-clone coding session. Wait for a request.\n\n<!-- wt:codex-slot=main:v1 -->";
const SCAN_MAX_DAYS: usize = 30;
const MAX_PREFIX_BYTES: usize = 2 * 1024 * 1024;
const TAIL_START_BYTES: u64 = 64 * 1024;
const TAIL_MAX_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexPaths {
    pub home: PathBuf,
    pub codex_home: PathBuf,
    pub cache_dir: PathBuf,
    pub lock_dir: PathBuf,
}

impl CodexPaths {
    pub fn new(home: impl Into<PathBuf>, cache_dir: impl Into<PathBuf>) -> Self {
        let home = home.into();
        let cache_dir = cache_dir.into();
        Self {
            codex_home: home.join(".codex"),
            lock_dir: cache_dir.join("locks"),
            cache_dir,
            home,
        }
    }

    pub fn with_codex_home(mut self, codex_home: impl Into<PathBuf>) -> Self {
        self.codex_home = codex_home.into();
        self
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.codex_home.join("sessions")
    }
    pub fn names_file(&self) -> PathBuf {
        self.cache_dir.join("codex-sessions.json")
    }
    pub fn app_server_socket(&self) -> PathBuf {
        self.codex_home
            .join("app-server-control/app-server-control.sock")
    }
}

#[derive(Debug, Error)]
pub enum CodexHarnessError {
    #[error("Codex {operation}: {detail}")]
    Operation {
        operation: &'static str,
        detail: String,
    },
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Tmux(#[from] TmuxError),
    #[error(transparent)]
    Process(#[from] ProcessError),
    #[error(transparent)]
    AppServer(#[from] CodexAppServerError),
}

#[derive(Clone)]
pub struct CodexHarness {
    paths: CodexPaths,
    runner: ProcessRunner,
    tmux: TmuxClient,
}

impl CodexHarness {
    pub fn new(paths: CodexPaths, runner: ProcessRunner, tmux: TmuxClient) -> Self {
        Self {
            paths,
            runner,
            tmux,
        }
    }

    pub fn paths(&self) -> &CodexPaths {
        &self.paths
    }

    /// Read-only initialize handshake for `wt codex selftest`. This does not
    /// resume a thread, inspect a queue, or subscribe to live activity.
    pub async fn app_server_info(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<Option<CodexAppServerInfo>, CodexHarnessError> {
        let socket = self.paths.app_server_socket();
        if !socket.exists() {
            return Ok(None);
        }
        let client = app_server::CodexAppServerClient::connect(&socket, cancellation).await?;
        Ok(Some(client.info().clone()))
    }

    pub fn tmux_session_name(&self, slug: &str) -> String {
        format!("{slug}{CODEX_SLOT_INFIX}")
    }

    pub fn build_spawn_command(&self, request: &HarnessSpawnRequest) -> SpawnCommand {
        let args = if let Some(session) = &request.resume_session_id {
            vec!["resume".into(), session.clone()]
        } else if request.slug == "manager" {
            vec![CODEX_MANAGER_PROMPT.into()]
        } else if request.slug == "main" {
            vec![CODEX_MAIN_PROMPT.into()]
        } else {
            Vec::new()
        };
        SpawnCommand {
            program: PathBuf::from("codex"),
            args,
        }
    }

    /// Synchronous file discovery, intended to run on a dedicated blocking
    /// worker. Exact live ids get a full historical lookup because resumed
    /// Codex threads continue appending under their creation-day directory.
    pub fn discover_sync(
        &self,
        request: &DiscoveryRequest,
        live_session_id: Option<&str>,
    ) -> Result<Vec<HarnessSession>, CodexHarnessError> {
        let mut rollouts = scan_rollouts(
            &self.paths.sessions_dir(),
            &request.worktree_path,
            &request.slug,
        )?;
        if let Some(id) = live_session_id
            && !rollouts.iter().any(|rollout| rollout.session_id == id)
            && let Some(found) = find_rollout(
                &self.paths.sessions_dir(),
                &request.worktree_path,
                &request.slug,
                id,
            )?
        {
            rollouts.push(found);
        }
        rollouts.sort_by_key(|a| std::cmp::Reverse(a.mtime_ms));
        let ids = rollouts
            .iter()
            .map(|r| r.session_id.clone())
            .collect::<Vec<_>>();
        let names = reconcile_codex_names(&self.paths, &request.slug, &ids)?;
        Ok(rollouts
            .into_iter()
            .map(|rollout| {
                let managed = names
                    .get(&rollout.session_id)
                    .cloned()
                    .unwrap_or_else(|| short_id(&rollout.session_id));
                let tail = read_codex_tail(&rollout.path, rollout.mtime_ms, rollout.size)
                    .ok()
                    .flatten();
                HarnessSession {
                    display_name: managed.clone(),
                    session_id: rollout.session_id,
                    tmux_session_name: self.tmux_session_name(&request.slug),
                    last_active_ms: Some(rollout.mtime_ms),
                    is_live: false,
                    extras: HarnessExtras {
                        managed_name: Some(managed),
                        derived_state: tail.as_ref().map(derive_codex_state),
                        waiting_for: tail.as_ref().and_then(|t| match t.pending_interaction {
                            Some(PendingInteraction::Approval) => Some("approval prompt".into()),
                            Some(PendingInteraction::Question) => Some("question prompt".into()),
                            None => None,
                        }),
                        tail_ended_at: tail.as_ref().map(|t| t.last_event_ms),
                        ..HarnessExtras::default()
                    },
                }
            })
            .collect())
    }

    pub async fn discover(
        &self,
        request: &DiscoveryRequest,
        live_session_id: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<Vec<HarnessSession>, CodexHarnessError> {
        let harness = self.clone();
        let request = request.clone();
        let live_id = live_session_id.map(str::to_owned);
        let task = tokio::task::spawn_blocking(move || {
            harness.discover_sync(&request, live_id.as_deref())
        });
        let sessions = tokio::select! {
            _ = cancellation.cancelled() => return Err(CodexHarnessError::Operation { operation: "discover", detail: "cancelled".into() }),
            result = task => result.map_err(|e| CodexHarnessError::Operation { operation: "discover", detail: e.to_string() })??,
        };
        if sessions.is_empty() || cancellation.is_cancelled() {
            return Ok(sessions);
        }
        match self.read_native_snapshots(&sessions, cancellation).await {
            Ok(snapshots) => Ok(enrich_native_status(sessions, snapshots)),
            // Socket absence, busy, and old daemon schemas are optional; the
            // rollout is still a valid source for picker state.
            Err(_) if !cancellation.is_cancelled() => Ok(sessions),
            Err(_) => Err(CodexHarnessError::Operation {
                operation: "discover",
                detail: "cancelled".into(),
            }),
        }
    }

    async fn read_native_snapshots(
        &self,
        sessions: &[HarnessSession],
        cancellation: &CancellationToken,
    ) -> Result<HashMap<String, NativeSnapshot>, CodexAppServerError> {
        let mut client = app_server::CodexAppServerClient::connect(
            &self.paths.app_server_socket(),
            cancellation,
        )
        .await?;
        let mut out = HashMap::new();
        for session in sessions {
            let status = client
                .thread_status(&session.session_id, cancellation)
                .await?;
            let queued = client
                .queue_count(&session.session_id, cancellation)
                .await?;
            out.insert(
                session.session_id.clone(),
                NativeSnapshot { status, queued },
            );
        }
        Ok(out)
    }

    pub async fn start(
        &self,
        request: &HarnessSpawnRequest,
        cancellation: &CancellationToken,
    ) -> Result<bool, CodexHarnessError> {
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
                    name: name.clone(),
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
                    &OptionScope::Session(name),
                    "@wt-harness-session-id",
                    Some(session_id),
                    cancellation,
                )
                .await;
        }
        Ok(true)
    }

    pub async fn send_native(
        &self,
        thread_id: &str,
        text: &str,
        cancellation: &CancellationToken,
    ) -> Result<CodexQueueDelivery, CodexAppServerError> {
        let mut messenger =
            CodexMessenger::new(self.paths.clone(), self.runner.clone(), self.tmux.clone());
        messenger.send(thread_id, text, cancellation).await
    }

    pub async fn send_message(
        &self,
        target: &messaging::CodexMessageTarget,
        cancellation: &CancellationToken,
    ) -> Result<CodexMessageOutcome, CodexHarnessError> {
        let mut messenger =
            CodexMessenger::new(self.paths.clone(), self.runner.clone(), self.tmux.clone());
        messenger.send_target(target, cancellation).await
    }

    pub fn usage(&self) -> Option<CodexUsage> {
        read_codex_usage(&self.paths.sessions_dir())
    }

    pub fn poll_activity(
        &self,
        tracker: &mut CodexActivityTracker,
        active: &[(String, PathBuf, Option<String>)],
    ) -> CodexActivityBatch {
        events::poll_activity(&self.paths.sessions_dir(), tracker, active)
    }

    pub async fn poll_activity_async(
        &self,
        mut tracker: CodexActivityTracker,
        active: Vec<(String, PathBuf, Option<String>)>,
        cancellation: &CancellationToken,
    ) -> Result<(CodexActivityTracker, CodexActivityBatch), CodexHarnessError> {
        let harness = self.clone();
        let task = tokio::task::spawn_blocking(move || {
            let batch = harness.poll_activity(&mut tracker, &active);
            (tracker, batch)
        });
        tokio::select! {
            _ = cancellation.cancelled() => Err(CodexHarnessError::Operation { operation: "poll activity", detail: "cancelled".into() }),
            result = task => result.map_err(|error| CodexHarnessError::Operation { operation: "poll activity", detail: error.to_string() }),
        }
    }

    pub async fn usage_async(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<Option<CodexUsage>, CodexHarnessError> {
        let sessions = self.paths.sessions_dir();
        let task = tokio::task::spawn_blocking(move || read_codex_usage(&sessions));
        tokio::select! {
            _ = cancellation.cancelled() => Err(CodexHarnessError::Operation { operation: "read usage", detail: "cancelled".into() }),
            result = task => result.map_err(|error| CodexHarnessError::Operation { operation: "read usage", detail: error.to_string() }),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PendingInteraction {
    Question,
    Approval,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexTail {
    pub last_task_event: Option<String>,
    pub pending_interaction: Option<PendingInteraction>,
    pub parse_complete: bool,
    pub last_event_ms: i64,
}

fn derive_codex_state(tail: &CodexTail) -> DerivedState {
    match (tail.last_task_event.as_deref(), tail.pending_interaction) {
        (Some("task_started"), Some(_)) => DerivedState::Asking,
        (Some("task_started"), None) => DerivedState::Working,
        (Some("task_complete" | "turn_aborted"), _) => DerivedState::Waiting,
        (None, _) => DerivedState::Unknown,
        (Some(_), _) => DerivedState::Unknown,
    }
}

#[derive(Clone, Debug)]
struct Rollout {
    session_id: String,
    cwd: String,
    path: PathBuf,
    mtime_ms: i64,
    size: u64,
}

fn scan_rollouts(dir: &Path, cwd: &Path, slug: &str) -> Result<Vec<Rollout>, CodexHarnessError> {
    let mut days = Vec::new();
    let Ok(years) = fs::read_dir(dir) else {
        return Ok(Vec::new());
    };
    for year in years.flatten() {
        let Ok(months) = fs::read_dir(year.path()) else {
            continue;
        };
        for month in months.flatten() {
            let Ok(day_entries) = fs::read_dir(month.path()) else {
                continue;
            };
            for day in day_entries.flatten() {
                days.push(day.path());
            }
        }
    }
    days.sort_by(|a, b| b.cmp(a));
    let mut found = Vec::new();
    for day in days.into_iter().take(SCAN_MAX_DAYS) {
        let Ok(files) = fs::read_dir(day) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if !is_rollout_path(&path) {
                continue;
            }
            if let Some(rollout) = read_rollout(&path)?
                && rollout.cwd == cwd.to_string_lossy()
                && slot_owns(&path, rollout.size, slug)?
            {
                found.push(rollout);
            }
        }
    }
    Ok(found)
}

fn find_rollout(
    dir: &Path,
    cwd: &Path,
    slug: &str,
    id: &str,
) -> Result<Option<Rollout>, CodexHarnessError> {
    let mut stack = vec![dir.to_path_buf()];
    let mut best: Option<Rollout> = None;
    while let Some(current) = stack.pop() {
        let Ok(entries) = fs::read_dir(current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if !is_rollout_path(&path)
                || !path
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().contains(id))
            {
                continue;
            }
            let Some(rollout) = read_rollout(&path)? else {
                continue;
            };
            if rollout.session_id == id
                && rollout.cwd == cwd.to_string_lossy()
                && slot_owns(&path, rollout.size, slug)?
                && best
                    .as_ref()
                    .is_none_or(|prev| rollout.mtime_ms > prev.mtime_ms)
            {
                best = Some(rollout);
            }
        }
    }
    Ok(best)
}

fn is_rollout_path(path: &Path) -> bool {
    path.file_name().is_some_and(|name| {
        let name = name.to_string_lossy();
        name.starts_with("rollout-") && name.ends_with(".jsonl")
    })
}

fn read_rollout(path: &Path) -> Result<Option<Rollout>, CodexHarnessError> {
    let metadata = match fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return Ok(None),
    };
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return Ok(None),
    };
    let mut head = vec![0; metadata.len().min(64 * 1024) as usize];
    if file.read_exact(&mut head).is_err() {
        return Ok(None);
    }
    let Some(line) = head.split(|b| *b == b'\n').next() else {
        return Ok(None);
    };
    let Ok(event) = serde_json::from_slice::<Value>(line) else {
        return Ok(None);
    };
    if event.get("type").and_then(Value::as_str) != Some("session_meta") {
        return Ok(None);
    }
    let payload = &event["payload"];
    let (Some(id), Some(cwd), Some(originator), Some(source)) = (
        payload["id"].as_str(),
        payload["cwd"].as_str(),
        payload["originator"].as_str(),
        payload["thread_source"].as_str(),
    ) else {
        return Ok(None);
    };
    if !matches!(originator, "codex-tui" | "wt" | "Codex Desktop") || source != "user" {
        return Ok(None);
    }
    let modified = metadata
        .modified()
        .unwrap_or(std::time::UNIX_EPOCH)
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    Ok(Some(Rollout {
        session_id: id.to_owned(),
        cwd: cwd.to_owned(),
        path: path.to_path_buf(),
        mtime_ms: modified.as_millis().min(i64::MAX as u128) as i64,
        size: metadata.len(),
    }))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SlotOwner {
    Main,
    Manager,
}

fn slot_owns(path: &Path, size: u64, slug: &str) -> io::Result<bool> {
    if slug != "main" && slug != "manager" {
        return Ok(true);
    }
    let limit = size.min(MAX_PREFIX_BYTES as u64);
    let mut file = File::open(path)?;
    let mut bytes = vec![0; limit as usize];
    file.read_exact(&mut bytes)?;
    let mut owner = None;
    let mut lines = bytes.split(|b| *b == b'\n').collect::<Vec<_>>();
    if !bytes.ends_with(b"\n") {
        lines.pop();
    }
    for line in lines {
        let Ok(event) = serde_json::from_slice::<Value>(line) else {
            return Ok(false);
        };
        if event["type"] != "response_item" || event["payload"]["type"] != "message" {
            continue;
        }
        let role = event["payload"]["role"].as_str().unwrap_or_default();
        if role == "assistant" {
            owner = Some(SlotOwner::Main);
            break;
        }
        if role != "user" {
            continue;
        }
        if let Some(parts) = event["payload"]["content"].as_array() {
            if parts
                .iter()
                .any(|part| part["type"] == "input_text" && part["text"] == CODEX_MANAGER_PROMPT)
            {
                owner = Some(SlotOwner::Manager);
                break;
            }
            if parts
                .iter()
                .any(|part| part["type"] == "input_text" && part["text"] == CODEX_MAIN_PROMPT)
            {
                owner = Some(SlotOwner::Main);
                break;
            }
        }
    }
    Ok(matches!(
        (slug, owner),
        ("main", Some(SlotOwner::Main)) | ("manager", Some(SlotOwner::Manager))
    ))
}

pub fn read_codex_tail(path: &Path, mtime_ms: i64, size: u64) -> io::Result<Option<CodexTail>> {
    if size == 0 {
        return Ok(None);
    }
    let mut window = size.min(TAIL_START_BYTES);
    let (last_task, pending, parse_complete) = loop {
        let start = size.saturating_sub(window);
        let mut file = File::open(path)?;
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = vec![0; window as usize];
        file.read_exact(&mut bytes)?;
        let mut lines = bytes.split(|b| *b == b'\n').collect::<Vec<_>>();
        if start > 0 {
            lines.remove(0);
        }
        // Codex appends JSONL records in place. An incomplete trailing record
        // is not evidence of corrupt history and must not hide the last complete
        // task state while a write is in progress.
        if !bytes.ends_with(b"\n") {
            lines.pop();
        }
        let mut last_task: Option<(String, Option<i64>)> = None;
        let mut pending = None;
        let mut parse_complete = true;
        for line in lines {
            if line.is_empty() {
                continue;
            }
            let Ok(event) = serde_json::from_slice::<Value>(line) else {
                parse_complete = false;
                continue;
            };
            let typ = event["type"].as_str().unwrap_or_default();
            let payload = &event["payload"];
            if typ == "event_msg" {
                match payload["type"].as_str().unwrap_or_default() {
                    "task_started" | "task_complete" | "turn_aborted" => {
                        last_task = Some((
                            payload["type"].as_str().unwrap().to_owned(),
                            parse_ts(event["timestamp"].as_str()),
                        ));
                        pending = None;
                    }
                    "approval_request" => pending = Some(PendingInteraction::Approval),
                    "user_input_request" => pending = Some(PendingInteraction::Question),
                    _ => {}
                }
            }
        }
        if last_task.is_some() || window >= size || window >= TAIL_MAX_BYTES {
            break (last_task, pending, parse_complete);
        }
        window = size.min(window.saturating_mul(4).min(TAIL_MAX_BYTES));
    };
    Ok(Some(CodexTail {
        last_task_event: last_task.as_ref().map(|(kind, _)| kind.clone()),
        pending_interaction: pending,
        parse_complete,
        last_event_ms: last_task.and_then(|(_, ts)| ts).unwrap_or(mtime_ms),
    }))
}

fn parse_ts(value: Option<&str>) -> Option<i64> {
    let timestamp =
        time::OffsetDateTime::parse(value?, &time::format_description::well_known::Rfc3339).ok()?;
    Some(
        (timestamp.unix_timestamp_nanos() / 1_000_000).clamp(i64::MIN as i128, i64::MAX as i128)
            as i64,
    )
}

fn reconcile_codex_names(
    paths: &CodexPaths,
    slug: &str,
    ids: &[String],
) -> io::Result<BTreeMap<String, String>> {
    let lock = paths.lock_dir.join("__codex_names__.lock");
    let _guard = FileLock::acquire(&lock)?;
    let file = paths.names_file();
    let mut root = read_object(&file);
    let wanted = ids.iter().cloned().collect::<HashSet<_>>();
    let old = root
        .get(slug)
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut names = BTreeMap::new();
    let mut used = HashSet::new();
    for (id, name) in old.clone() {
        if !wanted.contains(&id) {
            continue;
        }
        let Some(name) = name.as_str() else {
            continue;
        };
        if (name == "primary" || name.parse::<u32>().is_ok_and(|n| n >= 2))
            && used.insert(name.to_owned())
        {
            names.insert(id, name.to_owned());
        }
    }
    if !ids.is_empty() && !used.contains("primary") {
        let first = &ids[0];
        if let Some(old) = names.remove(first) {
            used.remove(&old);
        }
        names.insert(first.clone(), "primary".into());
        used.insert("primary".into());
    }
    for id in ids {
        if names.contains_key(id) {
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
        names.insert(id.clone(), name);
    }
    let mut merged = old
        .into_iter()
        .filter(|(id, value)| !value.is_string() || wanted.contains(id))
        .collect::<serde_json::Map<_, _>>();
    for (id, name) in &names {
        merged.insert(id.clone(), Value::String(name.clone()));
    }
    let merged = Value::Object(merged);
    if root.get(slug) != Some(&merged) {
        root[slug] = merged;
        atomic_write_json(&file, &root)?;
    }
    Ok(names)
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

#[derive(Clone, Debug)]
struct NativeSnapshot {
    status: CodexThreadStatus,
    queued: u32,
}

fn enrich_native_status(
    sessions: Vec<HarnessSession>,
    map: HashMap<String, NativeSnapshot>,
) -> Vec<HarnessSession> {
    sessions
        .into_iter()
        .map(|mut session| {
            if let Some(native) = map.get(&session.session_id) {
                session.extras.queued = native.queued;
                let (state, waiting_for) = match &native.status {
                    CodexThreadStatus::NotLoaded | CodexThreadStatus::Idle => {
                        (DerivedState::Waiting, None)
                    }
                    CodexThreadStatus::SystemError => (DerivedState::Unknown, None),
                    CodexThreadStatus::Active(flags) => {
                        let approval = flags.iter().any(|f| f == "waitingOnApproval");
                        let question = flags.iter().any(|f| f == "waitingOnUserInput");
                        if approval || question {
                            (
                                DerivedState::Asking,
                                Some(
                                    match (approval, question) {
                                        (true, true) => "approval or question prompt",
                                        (true, false) => "approval prompt",
                                        (false, true) => "question prompt",
                                        (false, false) => unreachable!(),
                                    }
                                    .to_owned(),
                                ),
                            )
                        } else if flags.is_empty() {
                            (DerivedState::Working, None)
                        } else {
                            (DerivedState::Unknown, None)
                        }
                    }
                    CodexThreadStatus::Unknown(_) => (DerivedState::Unknown, None),
                };
                session.extras.derived_state = Some(state);
                session.extras.waiting_for = waiting_for;
            }
            session
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::num::NonZeroUsize;
    use wt_platform::process::ProcessRunner;
    use wt_tmux::{TmuxServer, TmuxSocket};

    fn harness(paths: CodexPaths, scratch: &Path) -> CodexHarness {
        let runner = ProcessRunner::new(NonZeroUsize::new(2).unwrap());
        let tmux = TmuxClient::new(
            runner.clone(),
            TmuxServer::new(TmuxSocket::Path(scratch.join("tmux.sock"))),
        );
        CodexHarness::new(paths, runner, tmux)
    }

    fn write_rollout(
        root: &Path,
        date: &str,
        id: &str,
        cwd: &Path,
        originator: &str,
        source: &str,
    ) -> PathBuf {
        let path = root.join(date).join(format!("rollout-{id}.jsonl"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let header = json!({"type":"session_meta","payload":{"id":id,"cwd":cwd.to_string_lossy(),"originator":originator,"thread_source":source}});
        let event = json!({"type":"event_msg","timestamp":"2026-10-01T00:00:00Z","payload":{"type":"task_complete"}});
        fs::write(&path, format!("{header}\n{event}\n")).unwrap();
        path
    }

    #[test]
    fn main_and_manager_names_require_the_exact_persisted_marker() {
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout-test.jsonl");
        let manager = json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":CODEX_MANAGER_PROMPT}]}});
        fs::write(&rollout, format!("{}\n", manager)).unwrap();
        assert!(slot_owns(&rollout, fs::metadata(&rollout).unwrap().len(), "manager").unwrap());
        assert!(!slot_owns(&rollout, fs::metadata(&rollout).unwrap().len(), "main").unwrap());
        let unmarked = json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"ordinary prompt"}]}});
        fs::write(&rollout, format!("{}\n", unmarked)).unwrap();
        assert!(!slot_owns(&rollout, fs::metadata(&rollout).unwrap().len(), "main").unwrap());
    }

    #[test]
    fn incomplete_trailing_jsonl_is_ignored_and_latest_task_state_survives() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let complete = json!({"type":"event_msg","timestamp":"2026-09-01T12:30:00.000Z","payload":{"type":"task_complete"}});
        fs::write(&path, format!("{}\n{{\"type\":", complete)).unwrap();
        let metadata = fs::metadata(&path).unwrap();
        let tail = read_codex_tail(&path, 1, metadata.len()).unwrap().unwrap();
        assert_eq!(tail.last_task_event.as_deref(), Some("task_complete"));
        assert!(tail.parse_complete);
        assert_eq!(
            tail.last_event_ms,
            parse_ts(Some("2026-09-01T12:30:00.000Z")).unwrap()
        );
        assert_eq!(
            parse_ts(Some("2026-09-01T14:30:00+02:00")),
            parse_ts(Some("2026-09-01T12:30:00Z"))
        );
    }

    #[test]
    fn names_reconcile_without_dropping_unknown_root_fields() {
        let dir = tempfile::tempdir().unwrap();
        let paths = CodexPaths::new(dir.path(), dir.path().join("cache"));
        fs::create_dir_all(&paths.cache_dir).unwrap();
        fs::write(
            paths.names_file(),
            r#"{"other":{"future":true},"slug":{"known":"primary","futureField":{"v":1}}}"#,
        )
        .unwrap();
        let names = reconcile_codex_names(&paths, "slug", &["known".into(), "new".into()]).unwrap();
        assert_eq!(names.get("known").map(String::as_str), Some("primary"));
        assert_eq!(names.get("new").map(String::as_str), Some("2"));
        let persisted: Value =
            serde_json::from_slice(&fs::read(paths.names_file()).unwrap()).unwrap();
        assert_eq!(persisted["other"]["future"], true);
        assert_eq!(persisted["slug"]["futureField"]["v"], 1);
    }

    #[test]
    fn discovery_filters_noninteractive_rollouts_and_includes_exact_old_live_uuid() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join(".codex/sessions");
        let cwd = dir.path().join("repo");
        let current = write_rollout(
            &sessions_dir,
            "2026/10/03",
            "current",
            &cwd,
            "codex-tui",
            "user",
        );
        write_rollout(
            &sessions_dir,
            "2026/10/02",
            "desktop",
            &cwd,
            "Codex Desktop",
            "user",
        );
        write_rollout(
            &sessions_dir,
            "2026/10/04",
            "automation",
            &cwd,
            "codex-tui",
            "automation",
        );
        let old = write_rollout(&sessions_dir, "2021/01/01", "old-live", &cwd, "wt", "user");
        for day in 5..=31 {
            fs::create_dir_all(sessions_dir.join("2026/10").join(format!("{day:02}"))).unwrap();
        }
        let paths = CodexPaths::new(dir.path(), dir.path().join("cache"));
        let harness = harness(paths, dir.path());
        let request = DiscoveryRequest {
            slug: "feature".into(),
            worktree_path: cwd,
            live_session_id: None,
        };
        let recent = harness.discover_sync(&request, None).unwrap();
        assert_eq!(recent.len(), 2);
        assert!(recent.iter().any(|s| s.session_id == "current"));
        assert!(recent.iter().any(|s| s.session_id == "desktop"));
        assert!(!recent.iter().any(|s| s.session_id == "automation"));
        assert!(!recent.iter().any(|s| s.session_id == "old-live"));
        let with_live = harness.discover_sync(&request, Some("old-live")).unwrap();
        assert!(with_live.iter().any(|s| s.session_id == "old-live"));
        assert!(current.exists() && old.exists());
    }
}
