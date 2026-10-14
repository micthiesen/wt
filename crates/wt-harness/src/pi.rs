//! Native Pi TUI adapter. The interactive process owns all model calls; a
//! bundled extension exposes bounded, identity-pinned control over a local socket.
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;
use wt_platform::process::{CommandSpec, ProcessRunner};
use wt_tmux::{CreateSession, OptionScope, TmuxClient, TmuxError};

use crate::{
    DerivedState, DiscoveryRequest, HarnessExtras, HarnessOutputKind, HarnessOutputLine,
    HarnessOutputTarget, HarnessOutputUpdate, HarnessSession, HarnessSpawnRequest, SpawnCommand,
    persist::FileLock,
};

const EXTENSION: &str = include_str!("pi/extension.ts");
pub const PI_VERSION: &str = "1.2.0";
const MAX_TAIL: u64 = 512 * 1024;
const MAX_REPLY: u64 = 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PiPaths {
    pub home: PathBuf,
    pub sessions: PathBuf,
    pub cache_dir: PathBuf,
    pub lock_dir: PathBuf,
}
impl PiPaths {
    pub fn new(home: impl Into<PathBuf>, cache_dir: impl Into<PathBuf>) -> Self {
        let home = home.into();
        let cache_dir = cache_dir.into();
        let sessions = home.join(".pi/agent/sessions");
        Self {
            home,
            sessions,
            lock_dir: cache_dir.join("locks"),
            cache_dir,
        }
    }
    pub fn with_sessions(mut self, path: impl Into<PathBuf>) -> Self {
        self.sessions = path.into();
        self
    }
    pub fn extension_path(&self) -> PathBuf {
        self.cache_dir
            .join("pi")
            .join(format!("extension-{}.ts", digest(EXTENSION.as_bytes())))
    }
    fn socket(&self, slug: &str) -> PathBuf {
        // macOS sockaddr_un is only 104 bytes. Never embed a checkout path.
        let scope = format!("{}:{slug}", self.cache_dir.display());
        #[cfg(unix)]
        let uid = unsafe { libc::geteuid() };
        #[cfg(not(unix))]
        let uid = 0;
        PathBuf::from(format!("/tmp/wt-pi-{uid}"))
            .join(format!("{}.sock", digest(scope.as_bytes())))
    }
}

#[derive(Debug, Error)]
pub enum PiError {
    #[error("Pi: {0}")]
    Operation(String),
    #[error("Pi request {request_id} may have been accepted; it was not resent: {detail}")]
    Uncertain { request_id: String, detail: String },
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Tmux(#[from] TmuxError),
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct PiStatus {
    pub session_id: String,
    pub cwd: PathBuf,
    pub state: String,
    pub queued: u32,
    pub compacting: bool,
    pub context_percent: Option<f64>,
    pub status_since: i64,
    pub session_file: Option<PathBuf>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PiSendOutcome {
    pub cold_started: bool,
}

#[derive(Clone)]
pub struct PiHarness {
    paths: PiPaths,
    tmux: TmuxClient,
    runner: ProcessRunner,
}
impl PiHarness {
    pub fn new(paths: PiPaths, runner: ProcessRunner, tmux: TmuxClient) -> Self {
        Self {
            paths,
            tmux,
            runner,
        }
    }
    pub fn paths(&self) -> &PiPaths {
        &self.paths
    }
    pub fn tmux_session_name(&self, slug: &str) -> String {
        format!("{slug}-pi")
    }

    pub fn prepare_extension(&self) -> Result<(), PiError> {
        let path = self.paths.extension_path();
        fs::create_dir_all(path.parent().expect("extension parent"))?;
        let temporary = path.with_extension(format!("{}.tmp", fresh_id()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(EXTENSION.as_bytes())?;
        drop(file);
        let installed = fs::hard_link(&temporary, &path);
        fs::remove_file(&temporary)?;
        match installed {
            Ok(()) => (),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                if fs::read(&path)? != EXTENSION.as_bytes() {
                    return Err(PiError::Operation("bundled extension was modified".into()));
                }
            }
            Err(error) => return Err(error.into()),
        }
        let socket = self.paths.socket("prepare");
        private_directory(socket.parent().expect("socket parent"))?;
        Ok(())
    }
    pub fn build_spawn_command(&self, request: &HarnessSpawnRequest) -> SpawnCommand {
        let mut args = vec![
            "--approve".into(),
            "--extension".into(),
            self.paths.extension_path().to_string_lossy().into_owned(),
            "--wt-control".into(),
            self.paths
                .socket(&request.slug)
                .to_string_lossy()
                .into_owned(),
            "--wt-tmux-session".into(),
            self.tmux_session_name(&request.slug),
            "--session-dir".into(),
            self.paths.sessions.to_string_lossy().into_owned(),
        ];
        if let Some(id) = &request.resume_session_id {
            args.extend(["--session".into(), id.clone()]);
        } else {
            args.extend(["--session-id".into(), fresh_id()]);
        }
        if let Some(name) = request
            .display_label
            .as_ref()
            .or(request.managed_name.as_ref())
        {
            args.extend(["--name".into(), name.clone()]);
        }
        SpawnCommand {
            program: "pi".into(),
            args,
        }
    }

    pub fn discover_sync(
        &self,
        request: &DiscoveryRequest,
    ) -> Result<Vec<HarnessSession>, PiError> {
        let mut found = Vec::new();
        for snapshot in recent_snapshots(
            session_files(&self.paths.sessions)?,
            &request.worktree_path,
            read_session,
        )? {
            let live = request.live_session_id.as_deref() == Some(snapshot.id.as_str());
            found.push(HarnessSession {
                display_name: snapshot
                    .name
                    .clone()
                    .unwrap_or_else(|| snapshot.id.chars().take(8).collect()),
                session_id: snapshot.id,
                tmux_session_name: self.tmux_session_name(&request.slug),
                last_active_ms: snapshot.modified,
                is_live: live,
                extras: HarnessExtras {
                    managed_name: snapshot.name,
                    session_summary: snapshot.summary,
                    derived_state: Some(if live {
                        DerivedState::Unknown
                    } else {
                        DerivedState::Idle
                    }),
                    ..HarnessExtras::default()
                },
            });
        }
        found.sort_by_key(|s| std::cmp::Reverse(s.last_active_ms));
        found.truncate(50);
        Ok(found)
    }
    pub async fn discover(
        &self,
        request: &DiscoveryRequest,
        cancel: &CancellationToken,
    ) -> Result<Vec<HarnessSession>, PiError> {
        let this = self.clone();
        let req = request.clone();
        let task = tokio::task::spawn_blocking(move || this.discover_sync(&req));
        let mut sessions = tokio::select! {
            _ = cancel.cancelled() => return Err(PiError::Operation("discovery cancelled".into())),
            result = task => result.map_err(|e| PiError::Operation(e.to_string()))??,
        };
        if let Some(expected) = request.live_session_id.as_ref() {
            let current = self
                .status_for(&request.slug, &request.worktree_path, expected, cancel)
                .await;
            if let Ok(status) = current {
                if let Some(row) = sessions.iter_mut().find(|row| row.session_id == *expected) {
                    row.extras.derived_state = Some(live_state(&status.state));
                    row.extras.queued = status.queued;
                    row.extras.status_since = Some(status.status_since);
                    row.extras.context_percent =
                        status.context_percent.map(|v| v.clamp(0.0, 100.0) as u8);
                } else {
                    sessions.insert(
                        0,
                        HarnessSession {
                            display_name: expected.chars().take(8).collect(),
                            session_id: expected.clone(),
                            tmux_session_name: self.tmux_session_name(&request.slug),
                            last_active_ms: Some(status.status_since),
                            is_live: true,
                            extras: HarnessExtras {
                                derived_state: Some(live_state(&status.state)),
                                queued: status.queued,
                                ..HarnessExtras::default()
                            },
                        },
                    );
                }
            }
        }
        Ok(sessions)
    }

    pub async fn start(
        &self,
        request: &HarnessSpawnRequest,
        cancel: &CancellationToken,
    ) -> Result<bool, PiError> {
        let name = self.tmux_session_name(&request.slug);
        let _guard = FileLock::acquire_async(
            &self.paths.lock_dir.join(format!("__start__{name}.lock")),
            cancel,
        )
        .await?;
        if self.tmux.session_exists(&name, cancel).await? {
            self.validate_live(request, cancel).await?;
            return Ok(false);
        }
        let mut version = CommandSpec::new("pi").args(["--version"]);
        version.timeout = Duration::from_secs(5);
        version.output_limit = 4096;
        let output = self
            .runner
            .run(version, cancel)
            .await
            .map_err(|e| PiError::Operation(format!("cannot verify Pi runtime: {e}")))?;
        if !output.status.success() || output.stdout_text().trim() != PI_VERSION {
            return Err(PiError::Operation("this wt integration requires Pi 1.2.0; install the supported runtime before launching".into()));
        }
        self.prepare_extension()?;
        let resume_path = match &request.resume_session_id {
            Some(id) => Some(
                exact_session_file(&self.paths.sessions, &request.worktree_path, id)?.ok_or_else(
                    || {
                        PiError::Operation(
                            "selected conversation no longer exists in this directory".into(),
                        )
                    },
                )?,
            ),
            None => None,
        };
        let socket = self.paths.socket(&request.slug);
        match fs::remove_file(&socket) {
            Ok(()) => (),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
        let mut command = self.build_spawn_command(request);
        let id = request.resume_session_id.clone().unwrap_or_else(|| {
            command
                .args
                .windows(2)
                .find(|pair| pair[0] == "--session-id")
                .map(|pair| pair[1].clone())
                .expect("new session identity")
        });
        // An ID found in a native project subdirectory is classified as global
        // by Pi and prompts to fork. This exact, cwd-validated path resumes it.
        if let Some(path) = resume_path {
            let index = command
                .args
                .iter()
                .position(|arg| arg == "--session")
                .expect("resume argument");
            command.args[index + 1] = path.to_string_lossy().into_owned();
        }
        self.tmux
            .create_session(
                &CreateSession {
                    name: name.clone(),
                    cwd: request.worktree_path.clone(),
                    command: std::iter::once("pi".into()).chain(command.args).collect(),
                    width: None,
                    height: None,
                },
                cancel,
            )
            .await?;
        self.tmux
            .set_option(
                &OptionScope::Session(name),
                "@wt-harness-session-id",
                Some(&id),
                cancel,
            )
            .await?;
        Ok(true)
    }

    async fn identity(&self, slug: &str, cancel: &CancellationToken) -> Result<String, PiError> {
        self.tmux
            .get_option(
                &OptionScope::Session(self.tmux_session_name(slug)),
                "@wt-harness-session-id",
                cancel,
            )
            .await?
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                PiError::Operation(
                    "live session identity is unavailable; reopen Pi through wt".into(),
                )
            })
    }
    /// A same-name tmux slot is not ownership evidence. Adoption requires its
    /// recorded UUID and the configuration-scoped extension socket to agree on
    /// the exact checkout and conversation, without changing either owner.
    pub async fn validate_live(
        &self,
        request: &HarnessSpawnRequest,
        cancel: &CancellationToken,
    ) -> Result<PiStatus, PiError> {
        let id = self.identity(&request.slug, cancel).await?;
        if request
            .resume_session_id
            .as_ref()
            .is_some_and(|wanted| wanted != &id)
        {
            return Err(PiError::Operation("the live Pi slot belongs to another conversation; select it explicitly or stop it before resuming".into()));
        }
        self.status_for(&request.slug, &request.worktree_path, &id, cancel)
            .await
    }
    pub async fn status(
        &self,
        slug: &str,
        cwd: &Path,
        cancel: &CancellationToken,
    ) -> Result<PiStatus, PiError> {
        let id = self.identity(slug, cancel).await?;
        self.status_for(slug, cwd, &id, cancel).await
    }
    pub async fn status_for(
        &self,
        slug: &str,
        cwd: &Path,
        id: &str,
        cancel: &CancellationToken,
    ) -> Result<PiStatus, PiError> {
        let status: PiStatus =
            serde_json::from_value(self.request(slug, cwd, id, "status", None, cancel).await?)
                .map_err(|e| PiError::Operation(e.to_string()))?;
        if status.session_id != id || status.cwd != cwd {
            return Err(PiError::Operation("Pi control reply belongs to another checkout or conversation; refusing to adopt its session".into()));
        }
        Ok(status)
    }
    pub async fn send_message(
        &self,
        slug: &str,
        cwd: &Path,
        text: &str,
        cancel: &CancellationToken,
    ) -> Result<PiSendOutcome, PiError> {
        self.send_to(
            &HarnessSpawnRequest {
                slug: slug.into(),
                worktree_path: cwd.into(),
                managed_name: None,
                resume_session_id: None,
                display_label: None,
            },
            text,
            cancel,
        )
        .await
    }
    pub async fn send_to(
        &self,
        request: &HarnessSpawnRequest,
        text: &str,
        cancel: &CancellationToken,
    ) -> Result<PiSendOutcome, PiError> {
        if text.trim().is_empty() || text.len() > MAX_REPLY as usize / 2 {
            return Err(PiError::Operation("message is empty or too large".into()));
        }
        let slug = request.slug.as_str();
        let cwd = request.worktree_path.as_path();
        let cold_started = self.start(request, cancel).await?;
        let id = self.identity(slug, cancel).await?;
        if cold_started {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
            loop {
                if self
                    .request(slug, cwd, &id, "status", None, cancel)
                    .await
                    .is_ok()
                {
                    break;
                }
                if cancel.is_cancelled() || tokio::time::Instant::now() >= deadline {
                    return Err(PiError::Operation(
                        "Pi control extension did not become ready".into(),
                    ));
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        if let Some(instructions) = text
            .trim()
            .strip_prefix("/compact")
            .filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
        {
            self.compact_with_instructions(slug, cwd, instructions.trim(), cancel)
                .await?;
        } else {
            self.request(slug, cwd, &id, "send", Some(text), cancel)
                .await?;
        }
        Ok(PiSendOutcome { cold_started })
    }
    pub async fn compact(
        &self,
        slug: &str,
        cwd: &Path,
        cancel: &CancellationToken,
    ) -> Result<(), PiError> {
        self.compact_with_instructions(slug, cwd, "", cancel).await
    }
    async fn compact_with_instructions(
        &self,
        slug: &str,
        cwd: &Path,
        instructions: &str,
        cancel: &CancellationToken,
    ) -> Result<(), PiError> {
        let id = self.identity(slug, cancel).await?;
        let operation_id = fresh_id();
        let request = json!({"id": operation_id, "session_id": id, "cwd": cwd, "op": "compact", "instructions": instructions});
        let socket = self.paths.socket(slug);
        let mut receipt = socket_request(&socket, request, true, cancel).await?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        loop {
            match receipt["status"].as_str() {
                Some("completed") => return Ok(()),
                Some("failed") => {
                    return Err(PiError::Operation(
                        receipt["error"]
                            .as_str()
                            .unwrap_or("compaction failed")
                            .into(),
                    ));
                }
                _ => (),
            }
            if cancel.is_cancelled() || tokio::time::Instant::now() >= deadline {
                return Err(PiError::Uncertain {
                    request_id: operation_id,
                    detail: "compaction was accepted but its completion is not confirmed".into(),
                });
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            receipt = socket_request(&socket, json!({"id":fresh_id(),"session_id":id,"cwd":cwd,"op":"receipt","request_id":operation_id}), false, cancel)
                .await.map_err(|error| PiError::Uncertain { request_id: operation_id.clone(), detail: error.to_string() })?;
        }
    }
    pub async fn poll_output_async(
        &self,
        mut tracker: PiOutputTracker,
        targets: Vec<HarnessOutputTarget>,
        cancel: &CancellationToken,
    ) -> (PiOutputTracker, Result<Vec<HarnessOutputUpdate>, PiError>) {
        let paths = self.paths.clone();
        let fallback = tracker.clone();
        let task = tokio::task::spawn_blocking(move || {
            let updates = tracker.poll(&paths, &targets);
            (tracker, updates)
        });
        tokio::select! {
            _ = cancel.cancelled() => (fallback, Err(PiError::Operation("output scan cancelled".into()))),
            result = task => result.unwrap_or_else(|e| (fallback, Err(PiError::Operation(e.to_string())))),
        }
    }
    async fn request(
        &self,
        slug: &str,
        cwd: &Path,
        session: &str,
        op: &str,
        text: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<Value, PiError> {
        let request_id = fresh_id();
        let request =
            json!({ "id": request_id, "session_id": session, "cwd": cwd, "op": op, "text": text });
        socket_request(&self.paths.socket(slug), request, op != "status", cancel).await
    }
}

async fn socket_request(
    path: &Path,
    request: Value,
    mutation: bool,
    cancel: &CancellationToken,
) -> Result<Value, PiError> {
    let id = request["id"].as_str().unwrap_or_default().to_owned();
    let mut sent = false;
    let operation = async {
        let mut socket = tokio::net::UnixStream::connect(path).await?;
        let mut bytes = serde_json::to_vec(&request).map_err(io::Error::other)?;
        bytes.push(b'\n');
        // Even a partial write is uncertain: do not retry a non-idempotent API.
        sent = true;
        socket.write_all(&bytes).await?;
        let mut line = Vec::new();
        let mut reader =
            tokio::io::BufReader::new(tokio::io::AsyncReadExt::take(socket, MAX_REPLY));
        reader.read_until(b'\n', &mut line).await?;
        if line.last() != Some(&b'\n') {
            return Err(io::Error::other("Pi reply is incomplete or too large"));
        }
        serde_json::from_slice::<Value>(&line).map_err(io::Error::other)
    };
    let result = tokio::select! {
        _ = cancel.cancelled() => Err(io::Error::other("cancelled")),
        result = tokio::time::timeout(Duration::from_secs(3), operation) => result.unwrap_or_else(|_| Err(io::Error::other("Pi control request timed out"))),
    };
    let reply = result.map_err(|error| {
        if sent && mutation {
            PiError::Uncertain {
                request_id: id.clone(),
                detail: error.to_string(),
            }
        } else {
            PiError::Io(error)
        }
    })?;
    if reply["id"].as_str() != Some(&id) {
        return Err(PiError::Uncertain {
            request_id: id,
            detail: "reply identity mismatch".into(),
        });
    }
    if reply["ok"] != true {
        return Err(PiError::Operation(
            reply["error"]
                .as_str()
                .unwrap_or("control request failed")
                .into(),
        ));
    }
    if mutation && reply["result"]["status"] == "uncertain" {
        return Err(PiError::Uncertain {
            request_id: id,
            detail: "delivery journal cannot prove acceptance".into(),
        });
    }
    if mutation && reply["result"]["status"] == "failed" {
        return Err(PiError::Operation(
            reply["result"]["error"]
                .as_str()
                .unwrap_or("operation failed")
                .into(),
        ));
    }
    Ok(reply["result"].clone())
}

fn digest(data: &[u8]) -> String {
    uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, data).to_string()
}
fn live_state(state: &str) -> DerivedState {
    match state {
        "working" => DerivedState::Working,
        "waiting" => DerivedState::Waiting,
        _ => DerivedState::Unknown,
    }
}
fn fresh_id() -> String {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    digest(
        format!(
            "{}:{:?}:{}",
            std::process::id(),
            SystemTime::now(),
            SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        )
        .as_bytes(),
    )
}
fn private_directory(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let meta = fs::symlink_metadata(path)?;
        if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } {
            return Err(io::Error::other(
                "Pi control directory has unexpected owner or type",
            ));
        }
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[derive(Default)]
struct Snapshot {
    id: String,
    name: Option<String>,
    summary: Option<String>,
    modified: Option<i64>,
    entries: Vec<Value>,
}
fn session_files(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    let mut remaining = 10_000usize;
    // Native defaults use one project directory; --session-dir stores flat.
    while let Some(dir) = dirs.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(v) => v,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        for entry in entries.take(10_000) {
            if remaining == 0 {
                return Err(io::Error::other(
                    "Pi session inventory exceeded 10000 entries",
                ));
            }
            remaining -= 1;
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() && dir == root {
                dirs.push(entry.path());
            } else if kind.is_file() && entry.path().extension().is_some_and(|e| e == "jsonl") {
                files.push(entry.path());
            }
        }
    }
    files.sort_by_cached_key(|path| {
        std::cmp::Reverse(fs::metadata(path).ok().and_then(|m| m.modified().ok()))
    });
    Ok(files)
}
fn read_session(path: &Path, cwd: &Path) -> io::Result<Option<Snapshot>> {
    let mut file = File::open(path)?;
    let mut header = String::new();
    BufReader::new(std::io::Read::by_ref(&mut file).take(16 * 1024)).read_line(&mut header)?;
    let Ok(header) = serde_json::from_str::<Value>(&header) else {
        return Ok(None);
    };
    if header["type"] != "session" || header["cwd"].as_str() != cwd.to_str() {
        return Ok(None);
    }
    let Some(id) = header["id"].as_str().filter(|id| !id.is_empty()) else {
        return Ok(None);
    };
    let meta = file.metadata()?;
    // Pi writes the selected display name near the beginning of a session.
    // Preserve it when the bounded tail has moved beyond that entry.
    file.seek(SeekFrom::Start(0))?;
    let mut prefix = Vec::new();
    std::io::Read::by_ref(&mut file)
        .take(64 * 1024)
        .read_to_end(&mut prefix)?;
    let initial_name = String::from_utf8_lossy(&prefix)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|entry| entry["type"] == "session_info")
        .filter_map(|entry| entry["name"].as_str().map(str::to_owned))
        .next_back();
    let start = meta.len().saturating_sub(MAX_TAIL);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(MAX_TAIL).read_to_end(&mut bytes)?;
    let tail = String::from_utf8_lossy(&bytes);
    let mut snapshot = Snapshot {
        id: id.into(),
        name: initial_name,
        modified: meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64),
        ..Snapshot::default()
    };
    for line in tail.lines().skip(usize::from(start > 0)) {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if entry["type"] == "session_info" {
            snapshot.name = entry["name"].as_str().map(str::to_owned);
        }
        if entry["type"] == "message" && entry["message"]["role"] == "user" {
            snapshot.summary = Some(
                message_text(&entry["message"]["content"])
                    .chars()
                    .take(240)
                    .collect(),
            );
        }
        snapshot.entries.push(entry);
    }
    Ok(Some(snapshot))
}
fn recent_snapshots(
    paths: Vec<PathBuf>,
    cwd: &Path,
    mut read: impl FnMut(&Path, &Path) -> io::Result<Option<Snapshot>>,
) -> io::Result<Vec<Snapshot>> {
    let mut found = Vec::new();
    for path in paths {
        if let Some(snapshot) = read(&path, cwd)? {
            found.push(snapshot);
            if found.len() == 50 {
                break;
            }
        }
    }
    Ok(found)
}
fn exact_session_file(root: &Path, cwd: &Path, id: &str) -> io::Result<Option<PathBuf>> {
    for path in session_files(root)? {
        if path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().contains(id))
            && read_session(&path, cwd)?.is_some_and(|snapshot| snapshot.id == id)
        {
            return Ok(Some(path));
        }
    }
    Ok(None)
}
fn message_text(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        return text.to_owned();
    }
    content
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

#[derive(Clone, Default)]
pub struct PiOutputTracker {
    seen: HashMap<String, (String, Vec<String>)>,
    files: HashMap<String, CachedOutputFile>,
    next_id: u64,
}
#[derive(Clone)]
struct CachedOutputFile {
    session: String,
    cwd: PathBuf,
    path: PathBuf,
    len: u64,
    modified: Option<SystemTime>,
}
impl PiOutputTracker {
    pub fn poll(
        &mut self,
        paths: &PiPaths,
        targets: &[HarnessOutputTarget],
    ) -> Result<Vec<HarnessOutputUpdate>, PiError> {
        let mut files = None;
        let mut updates = Vec::new();
        for target in targets {
            let cached = self
                .files
                .get(&target.slug)
                .filter(|file| file.session == target.session_id && file.cwd == target.cwd);
            if let Some(file) = cached
                && let Ok(meta) = fs::metadata(&file.path)
                && meta.len() == file.len
                && meta.modified().ok() == file.modified
            {
                continue;
            }
            let candidates = if let Some(file) = cached.filter(|file| file.path.exists()) {
                vec![file.path.clone()]
            } else {
                if files.is_none() {
                    files = Some(session_files(&paths.sessions)?);
                }
                files
                    .as_ref()
                    .expect("session inventory")
                    .iter()
                    .filter(|path| {
                        path.file_name()
                            .is_some_and(|name| name.to_string_lossy().contains(&target.session_id))
                    })
                    .cloned()
                    .collect()
            };
            let mut snapshot = None;
            for path in candidates {
                let meta = fs::metadata(&path)?;
                if let Some(found) = read_session(&path, &target.cwd)?
                    && found.id == target.session_id
                {
                    self.files.insert(
                        target.slug.clone(),
                        CachedOutputFile {
                            session: target.session_id.clone(),
                            cwd: target.cwd.clone(),
                            path,
                            len: meta.len(),
                            modified: meta.modified().ok(),
                        },
                    );
                    snapshot = Some(found);
                    break;
                }
            }
            let Some(snapshot) = snapshot else {
                continue;
            };
            let previous = self.seen.get(&target.slug);
            let reset = previous.is_none_or(|(id, _)| id != &target.session_id);
            let mut keys = Vec::new();
            let mut append = Vec::new();
            for entry in snapshot.entries.iter().filter(|e| e["type"] == "message") {
                let key = entry["id"].as_str().unwrap_or_default().to_owned();
                keys.push(key.clone());
                if !reset && previous.is_some_and(|(_, seen)| seen.contains(&key)) {
                    continue;
                }
                let message = &entry["message"];
                let kind = match message["role"].as_str() {
                    Some("user") => HarnessOutputKind::User,
                    Some("assistant") => HarnessOutputKind::Assistant,
                    Some("toolResult") => {
                        if message["isError"] == true {
                            HarnessOutputKind::ToolError
                        } else {
                            HarnessOutputKind::ToolOk
                        }
                    }
                    _ => continue,
                };
                let text = message_text(&message["content"]);
                for line in text.lines().filter(|l| !l.trim().is_empty()).take(8) {
                    self.next_id += 1;
                    append.push(HarnessOutputLine {
                        id: self.next_id,
                        timestamp_ms: message["timestamp"].as_i64().unwrap_or_default(),
                        kind,
                        text: line.chars().take(240).collect(),
                    });
                }
            }
            if append.len() > 80 {
                append.drain(..append.len() - 80);
            }
            self.seen
                .insert(target.slug.clone(), (target.session_id.clone(), keys));
            if reset || !append.is_empty() {
                updates.push(HarnessOutputUpdate {
                    slug: target.slug.clone(),
                    session_id: target.session_id.clone(),
                    reset,
                    append,
                });
            }
        }
        self.seen
            .retain(|slug, _| targets.iter().any(|t| &t.slug == slug));
        self.files
            .retain(|slug, _| targets.iter().any(|t| &t.slug == slug));
        Ok(updates)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pinned_status_and_empty_live_discovery_need_no_tmux_server() {
        let dir = tempfile::tempdir().unwrap();
        let paths = PiPaths::new(dir.path(), dir.path());
        let socket = paths.socket("fixture");
        private_directory(socket.parent().unwrap()).unwrap();
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let runner = ProcessRunner::new(std::num::NonZeroUsize::new(1).unwrap());
        let tmux = TmuxClient::new(
            runner.clone(),
            wt_tmux::TmuxServer::at(dir.path().join("missing-tmux")),
        );
        let harness = PiHarness::new(paths, runner, tmux);
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (stream, _) = listener.accept().await.unwrap();
                let mut reader = tokio::io::BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(request["session_id"], "live-new");
                assert_eq!(request["cwd"], "/fixture");
                let response = json!({"id":request["id"],"ok":true,"result":{
                    "session_id":"live-new","cwd":"/fixture","state":"waiting","queued":0,"compacting":false,
                    "context_percent":null,"status_since":42,"session_file":null}});
                reader
                    .get_mut()
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .unwrap();
            }
        });
        let cancel = CancellationToken::new();
        let status = harness
            .status_for("fixture", Path::new("/fixture"), "live-new", &cancel)
            .await
            .unwrap();
        assert_eq!(status.state, "waiting");
        let rows = harness
            .discover(
                &DiscoveryRequest {
                    slug: "fixture".into(),
                    worktree_path: "/fixture".into(),
                    live_session_id: Some("live-new".into()),
                },
                &cancel,
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].is_live);
        assert_eq!(rows[0].extras.derived_state, Some(DerivedState::Waiting));
        server.await.unwrap();
        fs::remove_file(socket).unwrap();
    }

    #[tokio::test]
    async fn existing_slot_requires_checkout_and_conversation_ownership_before_adoption() {
        let dir = tempfile::tempdir().unwrap();
        let runner = ProcessRunner::default();
        let tmux = TmuxClient::new(
            runner.clone(),
            wt_tmux::TmuxServer::at(dir.path().join("tmux.sock"))
                .with_cwd(dir.path())
                .with_config_file("/dev/null"),
        );
        let paths = PiPaths::new(dir.path(), dir.path());
        let socket = paths.socket("fixture");
        private_directory(socket.parent().unwrap()).unwrap();
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (stream, _) = listener.accept().await.unwrap();
                let mut reader = tokio::io::BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                // Even a syntactically successful reply must prove ownership.
                let response = json!({"id":request["id"],"ok":true,"result":{
                    "session_id":"actual-thread","cwd":"/actual-checkout","state":"waiting","queued":0,"compacting":false,
                    "context_percent":null,"status_since":42,"session_file":null}});
                reader
                    .get_mut()
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .unwrap();
            }
        });
        let cancel = CancellationToken::new();
        tmux.create_session(
            &CreateSession {
                name: "fixture-pi".into(),
                cwd: dir.path().into(),
                command: vec!["cat".into()],
                width: Some(80),
                height: Some(24),
            },
            &cancel,
        )
        .await
        .unwrap();
        tmux.set_option(
            &OptionScope::Session("fixture-pi".into()),
            "@wt-harness-session-id",
            Some("actual-thread"),
            &cancel,
        )
        .await
        .unwrap();
        let harness = PiHarness::new(paths, runner, tmux.clone());
        let request = HarnessSpawnRequest {
            slug: "fixture".into(),
            worktree_path: "/wrong-checkout".into(),
            managed_name: None,
            resume_session_id: None,
            display_label: None,
        };
        let wrong_checkout = harness.start(&request, &cancel).await;
        let wrong_thread = harness
            .start(
                &HarnessSpawnRequest {
                    worktree_path: "/actual-checkout".into(),
                    resume_session_id: Some("wrong-thread".into()),
                    ..request.clone()
                },
                &cancel,
            )
            .await;
        let correct = harness
            .start(
                &HarnessSpawnRequest {
                    worktree_path: "/actual-checkout".into(),
                    resume_session_id: Some("actual-thread".into()),
                    ..request
                },
                &cancel,
            )
            .await;
        let unchanged = tmux
            .get_option(
                &OptionScope::Session("fixture-pi".into()),
                "@wt-harness-session-id",
                &cancel,
            )
            .await;
        tmux.kill_server(&CancellationToken::new()).await.unwrap();
        server.await.unwrap();
        fs::remove_file(socket).unwrap();
        assert!(wrong_checkout.is_err());
        assert!(wrong_thread.is_err());
        assert!(!correct.unwrap());
        assert_eq!(unchanged.unwrap().as_deref(), Some("actual-thread"));
    }

    fn transcript(root: &Path, id: &str, cwd: &str, name: &str) -> PathBuf {
        let path = root.join(format!("{id}.jsonl"));
        fs::write(&path, format!("{}\n{}\n{}\n", json!({"type":"session","id":id,"cwd":cwd}),
            json!({"type":"session_info","name":name}),
            json!({"type":"message","id":"message-1","message":{"role":"user","content":"hello","timestamp":42}}))).unwrap();
        path
    }

    #[test]
    fn spawn_preserves_native_settings_and_trusts_only_this_launch() {
        let dir = tempfile::tempdir().unwrap();
        let runner = ProcessRunner::default();
        let harness = PiHarness::new(
            PiPaths::new(dir.path(), dir.path()),
            runner.clone(),
            TmuxClient::new(runner, wt_tmux::TmuxServer::at(dir.path().join("missing"))),
        );
        let command = harness.build_spawn_command(&HarnessSpawnRequest {
            worktree_path: "/fixture".into(),
            slug: "fixture".into(),
            managed_name: Some("named".into()),
            resume_session_id: Some("exact-id".into()),
            display_label: None,
        });
        assert!(command.args.contains(&"--approve".into()));
        assert!(!command.args.contains(&"--no-extensions".into()));
        assert!(
            command
                .args
                .windows(2)
                .any(|pair| pair == ["--session", "exact-id"])
        );
        assert!(
            command
                .args
                .windows(2)
                .any(|pair| pair == ["--name", "named"])
        );
    }

    #[test]
    fn transcript_discovery_uses_exact_cwd_and_native_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = transcript(dir.path(), "one", "/fixture", "named conversation");
        let snapshot = read_session(&path, Path::new("/fixture")).unwrap().unwrap();
        assert_eq!(snapshot.id, "one");
        assert_eq!(snapshot.name.as_deref(), Some("named conversation"));
        assert_eq!(snapshot.summary.as_deref(), Some("hello"));
        assert!(
            read_session(&path, Path::new("/fixture-other"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn exact_resume_is_independent_of_recent_history_limit() {
        let dir = tempfile::tempdir().unwrap();
        let old = transcript(dir.path(), "old", "/fixture", "old conversation");
        File::open(&old)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(UNIX_EPOCH))
            .unwrap();
        for number in 0..50 {
            transcript(
                dir.path(),
                &format!("new-{number}"),
                "/fixture",
                "new conversation",
            );
        }
        let runner = ProcessRunner::default();
        let harness = PiHarness::new(
            PiPaths::new(dir.path(), dir.path()).with_sessions(dir.path()),
            runner.clone(),
            TmuxClient::new(runner, wt_tmux::TmuxServer::at(dir.path().join("missing"))),
        );
        let rows = harness
            .discover_sync(&DiscoveryRequest {
                slug: "fixture".into(),
                worktree_path: "/fixture".into(),
                live_session_id: None,
            })
            .unwrap();
        assert_eq!(rows.len(), 50);
        assert!(rows.iter().all(|row| row.session_id != "old"));
        assert_eq!(
            exact_session_file(dir.path(), Path::new("/fixture"), "old").unwrap(),
            Some(old)
        );
        assert!(
            exact_session_file(dir.path(), Path::new("/foreign"), "old")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn history_limit_bounds_transcript_reads_not_only_returned_rows() {
        let paths = (0..100)
            .map(|index| PathBuf::from(index.to_string()))
            .collect();
        let mut reads = 0;
        let snapshots = recent_snapshots(paths, Path::new("/fixture"), |_, _| {
            reads += 1;
            assert!(
                reads <= 50,
                "older transcripts must not be read after the visible history is full"
            );
            Ok(Some(Snapshot {
                id: reads.to_string(),
                ..Snapshot::default()
            }))
        })
        .unwrap();
        assert_eq!(snapshots.len(), 50);
        assert_eq!(reads, 50);
    }

    #[test]
    fn oversized_lines_and_truncated_utf8_do_not_hide_recent_messages() {
        let dir = tempfile::tempdir().unwrap();
        let path = transcript(dir.path(), "one", "/fixture", "name");
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "{}", "🦀".repeat(MAX_TAIL as usize)).unwrap();
        writeln!(
            file,
            "{}",
            json!({"type":"message","id":"last","message":{"role":"user","content":"latest"}})
        )
        .unwrap();
        let snapshot = read_session(&path, Path::new("/fixture")).unwrap().unwrap();
        assert_eq!(snapshot.summary.as_deref(), Some("latest"));
        assert!(snapshot.entries.len() < 3);
    }

    #[test]
    fn output_is_bounded_incremental_and_exact_session_scoped() {
        let dir = tempfile::tempdir().unwrap();
        let path = transcript(dir.path(), "one", "/fixture", "name");
        transcript(dir.path(), "other", "/other", "private");
        let paths = PiPaths::new(dir.path(), dir.path()).with_sessions(dir.path());
        let mut tracker = PiOutputTracker::default();
        let target = HarnessOutputTarget {
            slug: "fixture".into(),
            cwd: "/fixture".into(),
            session_id: "one".into(),
        };
        let first = tracker.poll(&paths, std::slice::from_ref(&target)).unwrap();
        assert_eq!(first.len(), 1);
        assert!(first[0].reset);
        assert_eq!(first[0].append[0].text, "hello");
        assert!(
            tracker
                .poll(&paths, std::slice::from_ref(&target))
                .unwrap()
                .is_empty()
        );
        let mut file = OpenOptions::new().append(true).open(path).unwrap();
        writeln!(file, "{}", json!({"type":"message","id":"message-2","message":{"role":"assistant","content":"fresh reply","timestamp":43}})).unwrap();
        let updates = tracker.poll(&paths, &[target]).unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].append.len(), 1);
        assert_eq!(updates[0].append[0].text, "fresh reply");
    }

    #[tokio::test]
    async fn lost_mutation_reply_is_uncertain_and_never_replayed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("socket");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = tokio::io::BufReader::new(stream);
            let mut request = String::new();
            reader.read_line(&mut request).await.unwrap();
            assert!(request.contains("original"));
            // Drop after receiving a complete command, before acknowledging.
        });
        let error = socket_request(
            &path,
            json!({"id":"original","op":"send"}),
            true,
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, PiError::Uncertain { request_id, .. } if request_id == "original"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn missing_socket_is_safe_pre_dispatch_failure() {
        let dir = tempfile::tempdir().unwrap();
        let error = socket_request(
            &dir.path().join("missing"),
            json!({"id":"one"}),
            true,
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, PiError::Io(_)));
    }
}
