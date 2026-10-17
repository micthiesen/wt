use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io,
    path::{Path, PathBuf},
};

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::Value;
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use wt_platform::process::{CommandSpec, ProcessRunner};
use wt_tmux::{CreateSession, OptionScope, TmuxClient, TmuxError};

mod output;
pub mod service;

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
            database: std::env::var_os("XDG_DATA_HOME")
                .filter(|_| {
                    std::env::var_os("HOME").is_some_and(|current| Path::new(&current) == home)
                })
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .unwrap_or_else(|| home.join(".local/share"))
                .join("opencode/opencode.db"),
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
    fn service_registration(&self) -> PathBuf {
        std::env::var_os("XDG_STATE_HOME")
            .filter(|_| std::env::var_os("HOME").is_some_and(|home| Path::new(&home) == self.home))
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .unwrap_or_else(|| self.home.join(".local/state"))
            .join("opencode/service.json")
    }
}

#[derive(Debug, Error)]
pub enum OpenCodeError {
    #[error("OpenCode {operation}: {detail}")]
    Operation {
        operation: &'static str,
        detail: String,
    },
    #[error("OpenCode V2 database: {0}; wt requires OpenCode 2.x and its current session schema")]
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
    program: PathBuf,
    paths: OpenCodePaths,
    runner: ProcessRunner,
    tmux: TmuxClient,
}

impl OpenCodeHarness {
    pub fn new(paths: OpenCodePaths, runner: ProcessRunner, tmux: TmuxClient) -> Self {
        Self {
            program: "opencode".into(),
            paths,
            runner,
            tmux,
        }
    }
    pub fn paths(&self) -> &OpenCodePaths {
        &self.paths
    }
    fn write_launch_auth(&self, password: &str) -> Result<PathBuf, OpenCodeError> {
        let quoted = password.replace('\'', "'\\''");
        Ok(crate::persist::write_private_launch_file(
            &self.paths.cache_dir.join("opencode-launch-auth"),
            &format!("{}.sh", unique_id()),
            format!("export OPENCODE_PASSWORD='{quoted}'\n").as_bytes(),
        )?)
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
            program: self.program.clone(),
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
            "SELECT id, COALESCE(title, slug), time_updated FROM session_v2 WHERE (directory = ?1 OR directory = ?3) AND time_archived IS NULL AND parent_id IS NULL ORDER BY CASE WHEN id=?2 THEN 0 ELSE 1 END, time_updated DESC LIMIT 50",
        )?;
        let rows = statement
            .query_map(
                rusqlite::params![
                    request.worktree_path.to_string_lossy(),
                    request.live_session_id,
                    canonical_directory(&request.worktree_path).to_string_lossy()
                ],
                |row| {
                    Ok(SessionRow {
                        id: row.get(0)?,
                        title: row.get(1)?,
                        updated: row.get(2)?,
                    })
                },
            )?
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
                let state = session_state(&connection, &row.id)?;
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
        if let Some(id) = &request.resume_session_id {
            self.validate_session_sync(&request.worktree_path, id)?;
        }
        self.prepare_location(&request.worktree_path, cancellation)
            .await?;
        let session_id = match &request.resume_session_id {
            Some(id) => id.clone(),
            None => {
                self.create_session(
                    &request.worktree_path,
                    request.display_label.as_deref(),
                    cancellation,
                )
                .await?
            }
        };
        // Serialize identity adoption across slots, not only starts of one slot.
        // Hash the external ID rather than using it as a filesystem component.
        let identity_key = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, session_id.as_bytes());
        let _identity_lock = FileLock::acquire_async(
            &self
                .paths
                .database
                .with_file_name("wt-session-locks")
                .join(format!("{identity_key}.lock")),
            cancellation,
        )
        .await?;
        self.validate_session_sync(&request.worktree_path, &session_id)?;
        ensure_unique_owner(
            &session_id,
            &name,
            &self.tmux.list_sessions(cancellation).await?,
        )?;
        self.reserve_owner(&session_id, &name, cancellation).await?;
        let mut request = request.clone();
        request.resume_session_id = Some(session_id.clone());
        let command = self.build_spawn_command(&request);
        let service = self.api_service(cancellation).await?;
        let auth_file = self.write_launch_auth(&service.password)?;
        // Consume this launch's private source before exec. Keep tmux's own
        // configuration source and selectors intact, and secrets out of argv.
        let launch = [
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            ". \"$1\" || exit; /bin/rm -f -- \"$1\"; shift; exec \"$@\"".to_owned(),
            "sh".to_owned(),
            auth_file.to_string_lossy().into_owned(),
            command.program.to_string_lossy().into_owned(),
            "--server".to_owned(),
            service.url,
        ]
        .into_iter()
        .chain(command.args)
        .collect();

        let result = self
            .tmux
            .create_session_with_options(
                &CreateSession {
                    name: name.clone(),
                    cwd: request.worktree_path.clone(),
                    command: launch,
                    width: None,
                    height: None,
                },
                &[("@wt-harness-session-id", session_id.as_str())],
                cancellation,
            )
            .await;
        // A lost tmux reply may still have launched the pane. Do not remove its
        // unread source unless the exact slot is confirmed absent.
        if result.is_err()
            && matches!(
                self.tmux.session_exists(&name, cancellation).await,
                Ok(false)
            )
        {
            let _ = std::fs::remove_file(&auth_file);
        }
        result?;
        Ok(true)
    }

    /// Agent and skill discovery wait for V2's location services to initialize.
    /// Do this before opening a TUI: its service-health recovery otherwise can
    /// restart a healthy server while synchronous native watchers are starting,
    /// severing the first prompt before durable admission. Existing slots skip
    /// this path. These reads never submit a prompt or retry a mutation.
    async fn prepare_location(
        &self,
        cwd: &Path,
        cancel: &CancellationToken,
    ) -> Result<(), OpenCodeError> {
        for operation in ["agent.list", "skill.list"] {
            let command = CommandSpec::new(self.program.clone())
                .args([
                    "api",
                    operation,
                    "--param",
                    &format!("location[directory]={}", cwd.display()),
                ])
                .cwd(cwd);
            let response = self.run_api_command(operation, command, cancel).await?;
            if !same_directory(
                response
                    .pointer("/location/directory")
                    .and_then(Value::as_str),
                cwd,
            ) || !response["data"].is_array()
            {
                return Err(operation_error(
                    "prepare location",
                    format!(
                        "{operation} did not confirm readiness for {}",
                        cwd.display()
                    ),
                ));
            }
        }
        Ok(())
    }

    // Called only while holding the database-wide session identity lock.
    async fn reserve_owner(
        &self,
        id: &str,
        slot: &str,
        cancel: &CancellationToken,
    ) -> Result<(), OpenCodeError> {
        let key = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, id.as_bytes());
        let path = self
            .paths
            .database
            .with_file_name("wt-session-locks")
            .join(format!("{key}.json"));
        match std::fs::read(&path) {
            Ok(bytes) => {
                let owner: OpenCodeOwner = serde_json::from_slice(&bytes).map_err(|error| {
                    operation_error(
                        "resume",
                        format!("invalid ownership record {}: {error}", path.display()),
                    )
                })?;
                if owner.session_id != id || owner.version != 1 {
                    return Err(operation_error(
                        "resume",
                        "unrecognized session ownership record",
                    ));
                }
                if matches!(owner.socket, OwnerSocket::Name(_))
                    && owner.tmux_tmpdir != std::env::var_os("TMUX_TMPDIR").map(PathBuf::from)
                {
                    return Err(operation_error(
                        "resume",
                        "the recorded tmux server uses a different TMUX_TMPDIR; its ownership cannot be confirmed",
                    ));
                }
                let socket = match owner.socket {
                    OwnerSocket::Name(name) => wt_tmux::TmuxSocket::Name(name),
                    OwnerSocket::Path(path) => wt_tmux::TmuxSocket::Path(path),
                };
                let probe = TmuxClient::new(
                    self.runner.clone(),
                    wt_tmux::TmuxServer::new(socket).with_cwd(&self.paths.home),
                );
                if probe.session_exists(&owner.slot, cancel).await? {
                    let live = probe
                        .get_option(
                            &OptionScope::Session(owner.slot.clone()),
                            "@wt-harness-session-id",
                            cancel,
                        )
                        .await?;
                    let replaced = live.as_deref().is_some_and(|live| {
                        live.starts_with("ses_") && live.len() > 4 && live != id
                    });
                    if !replaced {
                        return Err(operation_error(
                            "resume",
                            format!(
                                "session {id} still belongs to live or unconfirmed slot {} on its recorded tmux server",
                                owner.slot
                            ),
                        ));
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let socket = match &self.tmux.server().socket {
            wt_tmux::TmuxSocket::Name(name) => OwnerSocket::Name(name.clone()),
            wt_tmux::TmuxSocket::Path(path) => OwnerSocket::Path(path.clone()),
        };
        let owner = OpenCodeOwner {
            version: 1,
            session_id: id.into(),
            slot: slot.into(),
            socket,
            tmux_tmpdir: std::env::var_os("TMUX_TMPDIR").map(PathBuf::from),
        };
        // Write intent before starting tmux. A lost start reply cannot leave an
        // unrecorded owner; a confirmed absent slot permits the next claimant.
        atomic_write_json(
            &path,
            &serde_json::to_value(owner).map_err(io::Error::other)?,
        )?;
        Ok(())
    }

    /// Native V2 API admission. Never paste text into an interactive prompt.
    pub async fn api(
        &self,
        operation: &str,
        cwd: &Path,
        session: Option<&str>,
        body: Option<Value>,
        cancel: &CancellationToken,
    ) -> Result<Value, OpenCodeError> {
        let mut command = CommandSpec::new(self.program.clone())
            .args(["api", operation])
            .cwd(cwd);
        if let Some(id) = session {
            command = command.args(["--param", &format!("sessionID={id}")]);
        }
        if let Some(body) = body {
            command = command.args(["--data", &body.to_string()]);
        }
        self.run_api_command(operation, command, cancel).await
    }

    async fn run_api_command(
        &self,
        operation: &str,
        mut command: CommandSpec,
        cancel: &CancellationToken,
    ) -> Result<Value, OpenCodeError> {
        let registration = self.api_service(cancel).await?;
        registration.pin_command(&mut command);
        command.output_limit = 2 * 1024 * 1024;
        command.preserve_children_on_success = true;
        let result = self
            .runner
            .run(command, cancel)
            .await
            .map_err(|e| operation_error("API", e.to_string()))?
            .checked("opencode")
            .map_err(|e| operation_error("API", e.to_string()))?;
        let response: Value = serde_json::from_slice(&result.stdout).map_err(|e| {
            operation_error("API", format!("invalid V2 response for {operation}: {e}"))
        })?;
        if response.get("data").is_none() {
            return Err(operation_error(
                "API",
                format!(
                    "{operation} returned no success envelope: {}",
                    truncate_chars(&response.to_string(), 512)
                ),
            ));
        }
        Ok(response)
    }

    async fn api_service(
        &self,
        cancel: &CancellationToken,
    ) -> Result<service::OpenCodeConnection, OpenCodeError> {
        service::OpenCodeService::for_home(&self.paths.home)
            .connect(self.program.as_os_str(), &self.runner, cancel)
            .await
    }

    /// Pending approvals and forms are location-scoped in V2. The caller
    /// batches this per distinct active checkout, never per historical session.
    pub async fn pending_inputs(
        &self,
        cwd: &Path,
        cancel: &CancellationToken,
    ) -> Result<std::collections::BTreeSet<String>, OpenCodeError> {
        let query = |operation| {
            let command = CommandSpec::new(self.program.clone())
                .args([
                    "api",
                    operation,
                    "--param",
                    &format!("location[directory]={}", cwd.display()),
                ])
                .cwd(cwd);
            self.run_api_command(operation, command, cancel)
        };
        let (permissions, forms) =
            tokio::try_join!(query("permission.request.list"), query("form.list"))?;
        let mut pending = std::collections::BTreeSet::new();
        for response in [permissions, forms] {
            if !same_directory(
                response
                    .pointer("/location/directory")
                    .and_then(Value::as_str),
                cwd,
            ) {
                return Err(operation_error(
                    "pending inputs",
                    "server returned a different location",
                ));
            }
            let entries = response["data"]
                .as_array()
                .ok_or_else(|| operation_error("pending inputs", "invalid pending input list"))?;
            for entry in entries {
                let id = entry["sessionID"]
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| {
                        operation_error("pending inputs", "pending input has no session identity")
                    })?;
                pending.insert(id.to_owned());
            }
        }
        Ok(pending)
    }

    pub async fn create_session(
        &self,
        cwd: &Path,
        title: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<String, OpenCodeError> {
        let id = format!("ses_{}", unique_id());
        let body = serde_json::json!({"id": id, "title": title, "location": {"directory": cwd}});
        let response = self
            .api("session.create", cwd, None, Some(body), cancel)
            .await;
        match response {
            Ok(reply) if reply.pointer("/data/id").and_then(Value::as_str) == Some(&id) => {}
            result => {
                // A reply can be lost after the server has committed creation.
                // Recover this exact generated identity, not the newest row.
                if self.validate_session_sync(cwd, &id).is_err() {
                    let detail = result
                        .err()
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| "server returned a different session identity".into());
                    return Err(operation_error(
                        "create",
                        format!("{detail}; session {id} creation is unconfirmed"),
                    ));
                }
            }
        }

        Ok(id)
    }

    pub fn validate_session_sync(&self, cwd: &Path, id: &str) -> Result<(), OpenCodeError> {
        let conn = open_readonly(&self.paths.database)?;
        let valid: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM session_v2 WHERE id=?1 AND (directory=?2 OR directory=?3) AND time_archived IS NULL AND parent_id IS NULL)", rusqlite::params![id,cwd.to_string_lossy(),canonical_directory(cwd).to_string_lossy()], |r| r.get(0))?;
        if !valid {
            return Err(operation_error(
                "session",
                "session identity does not belong to this checkout or is archived",
            ));
        }
        Ok(())
    }

    async fn live_identity(
        &self,
        slug: &str,
        cancel: &CancellationToken,
    ) -> Result<String, OpenCodeError> {
        self.tmux
            .get_option(
                &OptionScope::Session(self.tmux_session_name(slug)),
                "@wt-harness-session-id",
                cancel,
            )
            .await?
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                operation_error(
                    "session",
                    "live slot has no proven OpenCode session identity; reopen it",
                )
            })
    }

    async fn submit(
        &self,
        id: &str,
        cwd: &Path,
        text: &str,
        cancel: &CancellationToken,
    ) -> Result<(), OpenCodeError> {
        if text.trim().is_empty() {
            return Err(operation_error("send", "message is empty"));
        }
        self.validate_session_sync(cwd, id)?;
        let message_id = format!("msg_{}", unique_id());
        let body = prompt_body(&message_id, text);
        let result = self
            .api("session.prompt", cwd, Some(id), Some(body), cancel)
            .await;
        match result {
            Ok(reply) if reply.pointer("/data/id").and_then(Value::as_str) == Some(&message_id) => {
            }
            reply => {
                // Admission and delivery are durable V2 facts. A lost reply is
                // reconciled against this exact new identity, never resent.
                let accepted = open_readonly(&self.paths.database)
                    .and_then(|conn| prompt_is_recorded(&conn, id, &message_id));
                if !matches!(accepted, Ok(true)) {
                    let detail = match reply {
                        Err(error) => error.to_string(),
                        Ok(_) => "unexpected admission receipt".into(),
                    };
                    return Err(operation_error(
                        "send",
                        format!(
                            "{detail}; message {message_id} admission is unconfirmed; do not resend blindly"
                        ),
                    ));
                }
            }
        }

        Ok(())
    }

    pub async fn compact(
        &self,
        slug: &str,
        cwd: &Path,
        cancel: &CancellationToken,
    ) -> Result<(), OpenCodeError> {
        let id = self.live_identity(slug, cancel).await?;
        self.validate_session_sync(cwd, &id)?;
        let message_id = format!("msg_{}", unique_id());
        let reply = self.api(
            "session.compact", cwd, Some(&id),
            Some(serde_json::json!({"id": message_id, "delivery": "queue"})), cancel,
        ).await.map_err(|error| operation_error("compact", format!("{error}; control {message_id} admission is unconfirmed; do not retry blindly")))?;
        if reply.pointer("/data/id").and_then(Value::as_str) != Some(&message_id) {
            return Err(operation_error(
                "compact",
                format!(
                    "control {message_id} returned an unexpected receipt; admission is unconfirmed"
                ),
            ));
        }

        Ok(())
    }

    pub async fn send_message(
        &self,
        slug: &str,
        cwd: &Path,
        text: &str,
        cancellation: &CancellationToken,
    ) -> Result<OpenCodeSendOutcome, OpenCodeError> {
        if text.trim().is_empty() {
            return Err(operation_error("send", "message is empty"));
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
                    worktree_path: cwd.into(),
                    slug: slug.into(),
                    managed_name: None,
                    resume_session_id: None,
                    display_label: None,
                },
                cancellation,
            )
            .await?;
        let id = self.live_identity(slug, cancellation).await?;
        self.validate_session_sync(cwd, &id)?;
        if text.trim() == "/compact" || text.trim_start().starts_with("/compact ") {
            // V2 has no custom compaction prompt. Match the native Codex path:
            // execute the compaction control, never submit its suffix as a turn.
            self.compact(slug, cwd, cancellation).await?;
        } else if text.trim_start().starts_with('/') {
            let input = text.trim_start().trim_start_matches('/');
            let (name, arguments) = input.split_once(char::is_whitespace).unwrap_or((input, ""));
            self.api(
                "session.command",
                cwd,
                Some(&id),
                Some(serde_json::json!({"name":name,"text":arguments,"delivery":"queue"})),
                cancellation,
            )
            .await
            .map_err(|error| {
                operation_error(
                    "command",
                    format!("{error}; command admission is unconfirmed; do not retry blindly"),
                )
            })?;
        } else {
            self.submit(&id, cwd, text, cancellation).await?;
        }
        Ok(OpenCodeSendOutcome { cold_started })
    }

    pub fn cost(&self, now: i64) -> Result<Option<OpenCodeCost>, OpenCodeError> {
        if !self.paths.database.exists() {
            return Ok(None);
        }
        let connection = open_readonly(&self.paths.database)?;
        let cost_for = |after: i64| -> Result<f64, rusqlite::Error> {
            connection.query_row(
                "SELECT COALESCE(SUM(CAST(json_extract(data, '$.cost') AS REAL)), 0) FROM session_message WHERE type = 'assistant' AND time_created > ?1",
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
    let mut statement = conn.prepare("SELECT id, COALESCE(title, slug), time_updated FROM session_v2 WHERE (directory = ?1 OR directory = ?2) AND time_archived IS NULL AND parent_id IS NULL ORDER BY time_updated DESC LIMIT 50")?;
    statement
        .query_map(
            rusqlite::params![
                cwd.to_string_lossy(),
                canonical_directory(cwd).to_string_lossy()
            ],
            |row| {
                Ok(SessionRow {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    updated: row.get(2)?,
                })
            },
        )?
        .collect()
}

fn read_last_message(conn: &Connection, id: &str) -> Result<Option<MessageRow>, rusqlite::Error> {
    conn.query_row("SELECT id,json_extract(data,'$.time.completed'),time_created,time_updated,type FROM session_message WHERE session_id=?1 AND type IN ('user','assistant') ORDER BY seq DESC LIMIT 1", [id], |row| {
        Ok(MessageRow { id: row.get(0)?, completed: row.get(1)?, time_created: row.get(2)?, time_updated: row.get(3)?, role: row.get(4)? })
    }).optional()
}

fn read_user_text(conn: &Connection, id: &str) -> Result<Option<String>, rusqlite::Error> {
    conn.query_row("SELECT substr(json_extract(data, '$.text'),1,4096) FROM session_message WHERE id=?1 AND type='user'", [id], |r| r.get(0)).optional()
}

fn read_new_parts(
    conn: &Connection,
    session: &str,
    after: i64,
) -> Result<Vec<PartRow>, rusqlite::Error> {
    let mut statement = conn.prepare("SELECT m.id || ':' || j.key, j.value FROM session_message m, json_each(m.data, '$.content') j WHERE m.session_id=?1 AND m.time_created>=?2 AND json_extract(j.value,'$.type')='tool' ORDER BY m.seq,j.key LIMIT 256")?;
    statement
        .query_map((session, after), |r| {
            Ok(PartRow {
                id: r.get(0)?,
                kind: Some("tool".into()),
                data: r.get(1)?,
            })
        })?
        .collect()
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
enum OwnerSocket {
    Name(String),
    Path(PathBuf),
}

#[derive(serde::Serialize, serde::Deserialize)]
struct OpenCodeOwner {
    version: u8,
    session_id: String,
    slot: String,
    socket: OwnerSocket,
    tmux_tmpdir: Option<PathBuf>,
}

fn ensure_unique_owner(
    id: &str,
    target: &str,
    sessions: &[wt_tmux::SessionInfo],
) -> Result<(), OpenCodeError> {
    if let Some(owner) = sessions
        .iter()
        .find(|session| session.name != target && session.harness_session_id.as_deref() == Some(id))
    {
        return Err(operation_error(
            "resume",
            format!(
                "session {id} already belongs to live slot {}; attach there or start a new session",
                owner.name
            ),
        ));
    }
    Ok(())
}

fn prompt_body(id: &str, text: &str) -> Value {
    let mut body = serde_json::json!({"id":id,"text":text,"delivery":"queue"});
    let command = text.trim_start();
    let command = command
        .strip_prefix("[re: ")
        .and_then(|rest| rest.split_once("] "))
        .map_or(command, |(_, command)| command.trim_start());
    if let Some(token) = command
        .strip_prefix('@')
        .and_then(|text| text.split_whitespace().next())
        && !token.is_empty()
        && token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        body["skills"] = serde_json::json!([{"id":token}]);
    }
    body
}

fn prompt_is_recorded(
    conn: &Connection,
    session: &str,
    message: &str,
) -> Result<bool, rusqlite::Error> {
    conn.query_row("SELECT EXISTS(SELECT 1 FROM session_inbox WHERE id=?1 AND session_id=?2 AND type='user' UNION ALL SELECT 1 FROM session_message WHERE id=?1 AND session_id=?2 AND type='user')", (message,session), |row| row.get(0))
}

fn unique_id() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nonce = format!(
        "{}:{:?}:{}",
        std::process::id(),
        std::time::SystemTime::now(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, nonce.as_bytes())
        .simple()
        .to_string()
}

fn operation_error(operation: &'static str, detail: impl Into<String>) -> OpenCodeError {
    OpenCodeError::Operation {
        operation,
        detail: detail.into(),
    }
}

pub(super) fn canonical_directory(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn same_directory(observed: Option<&str>, expected: &Path) -> bool {
    observed.is_some_and(|observed| {
        Path::new(observed) == expected
            || std::fs::canonicalize(observed)
                .ok()
                .zip(std::fs::canonicalize(expected).ok())
                .is_some_and(|(a, b)| a == b)
    })
}

fn session_state(
    conn: &Connection,
    id: &str,
) -> Result<Option<crate::DerivedState>, rusqlite::Error> {
    conn.query_row(
        "SELECT time_idle,time_compacting,idle_outcome,time_suspended FROM session_v2 WHERE id=?1",
        [id],
        |r| {
            let idle: Option<i64> = r.get(0)?;
            let compacting: Option<i64> = r.get(1)?;
            let outcome: Option<String> = r.get(2)?;
            let suspended: Option<i64> = r.get(3)?;
            Ok(Some(if suspended.is_some() {
                crate::DerivedState::Unknown
            } else if idle.is_some() && compacting.is_none() {
                if outcome.as_deref() == Some("failed") {
                    crate::DerivedState::Asking
                } else {
                    crate::DerivedState::Waiting
                }
            } else {
                crate::DerivedState::Working
            }))
        },
    )
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
    use std::num::NonZeroUsize;
    pub(super) fn fixture() -> (tempfile::TempDir, OpenCodeHarness, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let paths = OpenCodePaths::new(dir.path(), dir.path().join("cache"))
            .with_database(dir.path().join("opencode.db"));
        let registration = paths.service_registration();
        std::fs::create_dir_all(registration.parent().unwrap()).unwrap();
        std::fs::write(
            registration,
            r#"{"url":"http://127.0.0.1:12345","password":"isolated-fixture"}"#,
        )
        .unwrap();
        let db = Connection::open(&paths.database).unwrap();
        db.execute_batch("CREATE TABLE session_v2(id TEXT PRIMARY KEY,slug TEXT,title TEXT,directory TEXT,parent_id TEXT,time_updated INTEGER,time_archived INTEGER,time_idle INTEGER,time_compacting INTEGER,idle_outcome TEXT,time_suspended INTEGER); CREATE TABLE session_message(id TEXT PRIMARY KEY,session_id TEXT,type TEXT,seq INTEGER,time_created INTEGER,time_updated INTEGER,data TEXT);").unwrap();
        let runner = ProcessRunner::new(NonZeroUsize::new(2).unwrap());
        let tmux = TmuxClient::new(runner.clone(), wt_tmux::TmuxServer::named("oc-v2-fixture"));
        (dir, OpenCodeHarness::new(paths, runner, tmux), db)
    }
    #[test]
    fn v2_discovery_uses_exact_checkout_root_sessions_and_idle_projection() {
        let (_dir, harness, db) = fixture();
        db.execute_batch("INSERT INTO session_v2(id,slug,title,directory,parent_id,time_updated,time_archived,time_idle,time_compacting) VALUES ('ses_a','a','A','/repo',NULL,10,NULL,10,NULL),('ses_child','c','child','/repo','ses_a',20,NULL,NULL,NULL),('ses_other','o','Other','/else',NULL,30,NULL,NULL,NULL),('ses_archived','z','Old','/repo',NULL,40,40,NULL,NULL);").unwrap();
        let found = harness
            .discover_sync(&DiscoveryRequest {
                slug: "r".into(),
                worktree_path: "/repo".into(),
                live_session_id: None,
            })
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].session_id, "ses_a");
        assert_eq!(
            found[0].extras.derived_state,
            Some(crate::DerivedState::Waiting)
        );
        assert!(
            harness
                .validate_session_sync(Path::new("/else"), "ses_a")
                .is_err()
        );
        assert!(
            harness
                .validate_session_sync(Path::new("/repo"), "ses_child")
                .is_err()
        );
        assert!(
            harness
                .validate_session_sync(Path::new("/repo"), "ses_archived")
                .is_err()
        );
        assert!(
            harness
                .validate_session_sync(Path::new("/repo"), "ses_a")
                .is_ok()
        );
    }
    #[test]
    fn v2_cost_reads_assistant_messages_only() {
        let (_dir, harness, db) = fixture();
        db.execute_batch("INSERT INTO session_message VALUES ('msg_a','ses_a','assistant',1,100,100,'{\"cost\":1.5}'),('msg_u','ses_a','user',2,100,100,'{\"cost\":99}');").unwrap();
        assert_eq!(harness.cost(200).unwrap().unwrap().five_hour, 1.5);
    }
    #[test]
    fn prompt_receipt_reconciliation_requires_exact_session_message_and_type() {
        let (_dir, _harness, db) = fixture();
        db.execute_batch("CREATE TABLE session_inbox(id TEXT,session_id TEXT,type TEXT); INSERT INTO session_inbox VALUES ('msg_pending','ses_a','user'); INSERT INTO session_message VALUES ('msg_done','ses_a','user',1,1,1,'{}'),('msg_tool','ses_a','assistant',2,2,2,'{}');").unwrap();
        assert!(prompt_is_recorded(&db, "ses_a", "msg_pending").unwrap());
        assert!(prompt_is_recorded(&db, "ses_a", "msg_done").unwrap());
        assert!(!prompt_is_recorded(&db, "ses_other", "msg_pending").unwrap());
        assert!(!prompt_is_recorded(&db, "ses_a", "msg_tool").unwrap());
        assert!(!prompt_is_recorded(&db, "ses_a", "msg_absent").unwrap());
    }
    #[test]
    fn native_skill_invocations_attach_only_explicit_leading_skill_names() {
        assert_eq!(
            prompt_body("msg_a", "@start do the task")["skills"][0]["id"],
            "start"
        );
        assert_eq!(
            prompt_body("msg_a", "[re: task-1] @start do the task")["skills"][0]["id"],
            "start"
        );
        assert_eq!(
            prompt_body("msg_a", " @cz-test")["skills"][0]["id"],
            "cz-test"
        );
        assert!(prompt_body("msg_a", "Ask @someone").get("skills").is_none());
        assert!(
            prompt_body("msg_a", "@someone@example.org")
                .get("skills")
                .is_none()
        );
    }
    #[test]
    fn same_checkout_does_not_authorize_adopting_another_live_slot_identity() {
        let sessions = vec![wt_tmux::SessionInfo {
            name: "manager-opencode".into(),
            id: "$1".into(),
            created_at: 1,
            attached_clients: 0,
            window_count: 1,
            harness_session_id: Some("ses_manager".into()),
        }];
        assert!(ensure_unique_owner("ses_manager", "main-opencode", &sessions).is_err());
        assert!(ensure_unique_owner("ses_new", "main-opencode", &sessions).is_ok());
        assert!(ensure_unique_owner("ses_manager", "manager-opencode", &sessions).is_ok());
    }
    #[test]
    fn failed_and_suspended_native_sessions_do_not_look_busy_or_successful() {
        let (_dir, _harness, db) = fixture();
        db.execute("INSERT INTO session_v2(id,slug,directory,time_idle,idle_outcome) VALUES ('ses_a','a','/repo',10,'failed')",[]).unwrap();
        assert_eq!(
            session_state(&db, "ses_a").unwrap(),
            Some(crate::DerivedState::Asking)
        );
        db.execute("UPDATE session_v2 SET time_suspended=11", [])
            .unwrap();
        assert_eq!(
            session_state(&db, "ses_a").unwrap(),
            Some(crate::DerivedState::Unknown)
        );
        db.execute(
            "UPDATE session_v2 SET time_suspended=NULL,idle_outcome='interrupted'",
            [],
        )
        .unwrap();
        assert_eq!(
            session_state(&db, "ses_a").unwrap(),
            Some(crate::DerivedState::Waiting)
        );
    }
    #[tokio::test]
    async fn durable_owner_prevents_cross_server_adoption_until_original_slot_is_absent() {
        let (dir, base, _db) = fixture();
        let cancel = CancellationToken::new();
        let make = |name: &str| {
            let tmux = TmuxClient::new(
                base.runner.clone(),
                wt_tmux::TmuxServer::at(dir.path().join(name)).with_config_file("/dev/null"),
            );
            OpenCodeHarness::new(base.paths.clone(), base.runner.clone(), tmux)
        };
        let first = make("first.sock");
        let second = make("second.sock");
        let outcome = async {
            first
                .reserve_owner("ses_owner", "manager-opencode", &cancel)
                .await?;
            first
                .tmux
                .create_session_with_options(
                    &CreateSession {
                        name: "manager-opencode".into(),
                        cwd: dir.path().into(),
                        command: vec!["sleep".into(), "60".into()],
                        width: None,
                        height: None,
                    },
                    &[("@wt-harness-session-id", "ses_owner")],
                    &cancel,
                )
                .await?;
            let denied = second
                .reserve_owner("ses_owner", "main-opencode", &cancel)
                .await;
            if denied.is_ok() {
                return Err(operation_error(
                    "test",
                    "second server adopted a live identity",
                ));
            }
            first
                .tmux
                .set_option(
                    &OptionScope::Session("manager-opencode".into()),
                    "@wt-harness-session-id",
                    None,
                    &cancel,
                )
                .await?;
            if second
                .reserve_owner("ses_owner", "main-opencode", &cancel)
                .await
                .is_ok()
            {
                return Err(operation_error(
                    "test",
                    "unknown owner identity was overwritten",
                ));
            }
            first
                .tmux
                .set_option(
                    &OptionScope::Session("manager-opencode".into()),
                    "@wt-harness-session-id",
                    Some("ses_replacement"),
                    &cancel,
                )
                .await?;
            second
                .reserve_owner("ses_owner", "main-opencode", &cancel)
                .await?;
            // The second recorded intent has no slot: reclaim confirmed absence.
            first
                .reserve_owner("ses_owner", "other-opencode", &cancel)
                .await?;
            Ok::<_, OpenCodeError>(())
        }
        .await;
        let _ = first.tmux.kill_server(&cancel).await;
        let _ = second.tmux.kill_server(&cancel).await;
        outcome.unwrap();
    }
    #[tokio::test]
    async fn cold_launch_prepares_native_services_before_tui_and_existing_slot_skips_reads() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, mut harness, db) = fixture();
        harness.tmux = TmuxClient::new(
            harness.runner.clone(),
            wt_tmux::TmuxServer::at(dir.path().join("ready.sock")).with_config_file("/dev/null"),
        );
        let program = dir.path().join("opencode-fixture");
        let events = dir.path().join("events");
        let cwd = dir.path().to_string_lossy().to_string();
        let script = format!(
            r##"#!/usr/bin/env python3
import json,os,sys,time
from pathlib import Path
p=Path({events})
if sys.argv[1] == 'api':
    with p.open('a') as f:f.write(sys.argv[2]+'\n')
    print(json.dumps({{"location":{{"directory":{cwd}}},"data":[]}}))
else:
    assert sys.argv[sys.argv.index('--server')+1] == 'http://127.0.0.1:12345'
    assert os.environ['OPENCODE_PASSWORD'] == 'isolated-fixture'
    assert 'isolated-fixture' not in sys.argv
    previous=p.read_text().splitlines()
    with p.open('a') as f:f.write('tui-ready\n' if previous==['agent.list','skill.list'] else 'tui-too-early\n')
    time.sleep(60)
"##,
            events = serde_json::to_string(&events).unwrap(),
            cwd = serde_json::to_string(&cwd).unwrap()
        );
        std::fs::write(&program, script).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        harness.program = program;
        db.execute(
            "INSERT INTO session_v2(id,slug,directory) VALUES ('ses_ready','ready',?1)",
            [&cwd],
        )
        .unwrap();
        let request = HarnessSpawnRequest {
            worktree_path: dir.path().into(),
            slug: "ready".into(),
            resume_session_id: Some("ses_ready".into()),
            managed_name: None,
            display_label: None,
        };
        let cancel = CancellationToken::new();
        let result = async {
            assert!(harness.start(&request, &cancel).await?);
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
            loop {
                if std::fs::read_to_string(&events)?.contains("tui-ready") {
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(operation_error(
                        "test",
                        "TUI did not observe completed readiness",
                    ));
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert!(!harness.start(&request, &cancel).await?);
            assert_eq!(
                std::fs::read_to_string(&events)?,
                "agent.list\nskill.list\ntui-ready\n"
            );
            assert_eq!(
                std::fs::read_dir(harness.paths.cache_dir.join("opencode-launch-auth"))?.count(),
                0
            );
            Ok::<_, OpenCodeError>(())
        }
        .await;
        let _ = harness.tmux.kill_server(&cancel).await;
        result.unwrap();
    }

    #[tokio::test]
    async fn rejected_location_readiness_never_starts_tui_or_creates_history() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, mut harness, db) = fixture();
        harness.tmux = TmuxClient::new(
            harness.runner.clone(),
            wt_tmux::TmuxServer::at(dir.path().join("rejected.sock")).with_config_file("/dev/null"),
        );
        let program = dir.path().join("opencode-fixture");
        std::fs::write(
            &program,
            "#!/bin/sh\nprintf '%s' '{\"location\":{\"directory\":\"/wrong\"},\"data\":[]}'\n",
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        harness.program = program;
        let request = HarnessSpawnRequest {
            worktree_path: dir.path().into(),
            slug: "rejected".into(),
            resume_session_id: None,
            managed_name: None,
            display_label: None,
        };
        let cancel = CancellationToken::new();
        assert!(
            harness
                .start(&request, &cancel)
                .await
                .unwrap_err()
                .to_string()
                .contains("did not confirm readiness")
        );
        assert!(
            !harness
                .tmux
                .session_exists("rejected-opencode", &cancel)
                .await
                .unwrap()
        );
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM session_v2", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
    #[test]
    fn directory_aliases_match_the_same_checkout_but_never_another_real_directory() {
        let (dir, harness, db) = fixture();
        let real = dir.path().join("real");
        let other = dir.path().join("other");
        let alias = dir.path().join("alias");
        std::fs::create_dir(&real).unwrap();
        std::fs::create_dir(&other).unwrap();
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        assert!(same_directory(real.to_str(), &alias));
        assert!(!same_directory(other.to_str(), &alias));
        db.execute(
            "INSERT INTO session_v2(id,slug,directory) VALUES ('ses_alias','alias',?1)",
            [canonical_directory(&real).to_string_lossy().as_ref()],
        )
        .unwrap();
        assert!(harness.validate_session_sync(&alias, "ses_alias").is_ok());
        assert!(harness.validate_session_sync(&other, "ses_alias").is_err());
    }
    #[tokio::test]
    async fn invalid_resume_refuses_locally_before_any_native_readiness_call() {
        let (dir, mut harness, _db) = fixture();
        harness.tmux = TmuxClient::new(
            harness.runner.clone(),
            wt_tmux::TmuxServer::at(dir.path().join("invalid.sock")).with_config_file("/dev/null"),
        );
        // If readiness is attempted, execution fails instead of returning the
        // local identity error. No executable or API process exists here.
        harness.program = dir.path().join("must-not-execute");
        let request = HarnessSpawnRequest {
            worktree_path: dir.path().into(),
            slug: "invalid".into(),
            resume_session_id: Some("ses_missing".into()),
            managed_name: None,
            display_label: None,
        };
        let cancel = CancellationToken::new();
        let error = harness
            .start(&request, &cancel)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("does not belong to this checkout"),
            "{error}"
        );
        assert!(
            !harness
                .tmux
                .session_exists("invalid-opencode", &cancel)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn launch_auth_is_private_quoted_and_does_not_execute_secret_text() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, harness, _db) = fixture();
        let secret = "a'\"$(exit 9)\nvalue";
        let file = harness.write_launch_auth(secret).unwrap();
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(file.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let result = harness
            .runner
            .run(
                CommandSpec::new("sh").args([
                    "-c",
                    ". \"$1\"; printf %s \"$OPENCODE_PASSWORD\"",
                    "sh",
                    file.to_str().unwrap(),
                ]),
                &CancellationToken::new(),
            )
            .await
            .unwrap()
            .checked("fixture")
            .unwrap();
        assert_eq!(result.stdout_text(), secret);
    }

    #[tokio::test]
    async fn concurrent_missing_service_bootstraps_once_before_explicit_connections() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, mut harness, _db) = fixture();
        let registration = harness.paths.service_registration();
        std::fs::remove_file(&registration).unwrap();
        let events = dir.path().join("bootstrap-events");
        let program = dir.path().join("bootstrap-api");
        let script = format!(
            r##"#!/usr/bin/env python3
import json,os,sys,time
from pathlib import Path
if sys.argv[1:3] == ['service','start']:
    with Path({events}).open('a') as f: f.write('start\n')
    time.sleep(0.05)
    Path({registration}).write_text(json.dumps({{"url":"http://127.0.0.1:12345","password":"isolated-fixture"}}))
else:
    assert '--server' in sys.argv
    assert os.environ['OPENCODE_PASSWORD'] == 'isolated-fixture'
    print('{{"data":[]}}')
"##,
            events = serde_json::to_string(&events).unwrap(),
            registration = serde_json::to_string(&registration).unwrap()
        );
        std::fs::write(&program, script).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        harness.program = program;
        let cancel = CancellationToken::new();
        let call = || harness.api("skill.list", dir.path(), None, None, &cancel);
        let (first, second) = tokio::join!(call(), call());
        first.unwrap();
        second.unwrap();
        assert_eq!(std::fs::read_to_string(events).unwrap(), "start\n");
    }

    #[tokio::test]
    async fn concurrent_readiness_and_pending_probes_pin_service_without_recovery() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, mut harness, _db) = fixture();
        let program = dir.path().join("explicit-api");
        let script = format!(
            r##"#!/usr/bin/env python3
import json,os,sys,time
assert sys.argv[1] == 'api', 'must never invoke service recovery'
assert sys.argv[sys.argv.index('--server')+1] == 'http://127.0.0.1:12345'
assert os.environ['OPENCODE_PASSWORD'] == 'isolated-fixture'
assert 'isolated-fixture' not in sys.argv
if sys.argv[2] == 'agent.list': time.sleep(0.1)
print(json.dumps({{"location":{{"directory":{cwd}}},"data":[]}}))
"##,
            cwd = serde_json::to_string(dir.path()).unwrap()
        );
        std::fs::write(&program, script).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        harness.program = program;
        let cancel = CancellationToken::new();
        let (ready, pending) = tokio::join!(
            harness.prepare_location(dir.path(), &cancel),
            harness.pending_inputs(dir.path(), &cancel),
        );
        ready.unwrap();
        assert!(pending.unwrap().is_empty());
    }

    #[tokio::test]
    async fn malformed_readonly_response_does_not_claim_mutation_uncertainty() {
        let (_dir, harness, _db) = fixture();
        let error = harness
            .run_api_command(
                "skill.list",
                CommandSpec::new("sh").args(["-c", "printf invalid-json"]),
                &CancellationToken::new(),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid V2 response for skill.list"));
        assert!(!error.contains("admission"));
        assert!(!error.contains("retry"));
    }
}
