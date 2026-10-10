use std::sync::atomic::AtomicU64 as RunCounter;
use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::OffsetDateTime;
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    sync::mpsc,
};
use tokio_util::sync::CancellationToken;
use wt_config::EffectTag;
use wt_core::WorktreeRef;
use wt_platform::{
    lock::{FileLock, LockError},
    process::{CommandSpec, ProcessError, ProcessRunner, ProcessStream},
};
use wt_tmux::{CreateSession, TmuxClient, TmuxError};

use crate::types::{ActionArgHistory, ActionMeta, ActionRun, ActionRunKind, ActionRunStatus};

const ACTION_CAPTURE_LIMIT: usize = 2 * 1024 * 1024;
const ACTION_LOG_LIMIT: u64 = 64 * 1024 * 1024;
const STREAM_QUEUE_CHUNKS: usize = 64;
const STREAM_CHUNK_LIMIT: usize = 16 * 1024;
const MAX_LOG_READ: u64 = 1024 * 1024;
static RUN_COUNTER: RunCounter = RunCounter::new(0);

#[derive(Clone)]
pub struct ActionServiceConfig {
    pub log_dir: PathBuf,
    pub lock_dir: PathBuf,
    pub executable: PathBuf,
    pub runner: ProcessRunner,
    pub tmux: TmuxClient,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ActionRequest {
    pub action_key: String,
    pub slug: String,
    #[serde(default)]
    pub worktree_ref: Option<WorktreeRef>,
    pub action_id: String,
    pub action_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arg_history: Option<ActionArgHistory>,
    pub prompt: String,
    pub kind: ActionRunKind,
    pub command: Vec<String>,
    pub cwd: PathBuf,
    #[serde(default)]
    pub affects: Vec<EffectTag>,
    #[serde(default)]
    pub issue_status: Option<crate::IssueStatusExpectation>,
    #[serde(default)]
    pub external: bool,
    #[serde(default)]
    pub auto_fire_keys: Vec<String>,
    /// Absolute configuration selectors are carried unchanged into the durable
    /// job so a worker launched from another cwd can reconstruct its context.
    #[serde(default)]
    pub config_selectors: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionStart {
    pub run_id: String,
    pub run_dir: PathBuf,
    pub session: String,
}

#[derive(Debug, Error)]
pub enum ActionServiceError {
    #[error("action {operation} at {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Lock(#[from] LockError),
    #[error(transparent)]
    Tmux(#[from] TmuxError),
    #[error(transparent)]
    Process(#[from] ProcessError),
    #[error("invalid action run id {0:?}")]
    InvalidRunId(String),
    #[error("action command is empty")]
    EmptyCommand,
    #[error("action session {0:?} is already running")]
    AlreadyRunning(String),
    #[error("action run {0:?} has already started or completed")]
    AlreadyFinished(String),
    #[error("action start for run {run_id:?} in tmux session {session:?} is ambiguous: {reason}")]
    StartAmbiguous {
        run_id: String,
        session: String,
        reason: String,
    },
    #[error("action job at {path} is invalid: {message}")]
    InvalidJob { path: PathBuf, message: String },
    #[error("action stream writer failed: {0}")]
    StreamWriter(String),
}

#[derive(Clone)]
pub struct ActionService {
    config: ActionServiceConfig,
    history_path: Option<PathBuf>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActionJob {
    version: u32,
    request: ActionRequest,
    run: ActionRun,
    session: String,
}

#[derive(Debug)]
struct OutputChunk {
    stream: ProcessStream,
    bytes: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionDone {
    pub run_id: String,
    pub status: ActionRunStatus,
    pub exit_code: Option<i32>,
    pub ended_at: u64,
    #[serde(default)]
    pub dropped_output_bytes: u64,
    #[serde(default)]
    pub stdout_truncated: bool,
    #[serde(default)]
    pub stderr_truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ActionService {
    pub fn new(config: ActionServiceConfig) -> Self {
        Self {
            config,
            history_path: None,
        }
    }

    /// Enable successful-run argument label refinement in the shared picker
    /// history file. The worker treats this as best-effort durable metadata.
    pub fn with_history_path(mut self, path: PathBuf) -> Self {
        self.history_path = Some(path);
        self
    }

    async fn refine_history(&self, run_dir: &Path) -> Result<bool, ActionServiceError> {
        let initial: ActionMeta = read_json(&run_dir.join("meta.json")).await?;
        let _state_lock = self.state_lock(&initial.run_id).await?;
        let mut meta: ActionMeta = read_json(&run_dir.join("meta.json")).await?;
        if meta.status != ActionRunStatus::Succeeded || meta.history_refined == Some(true) {
            return Ok(false);
        }
        let Some(history) = meta.arg_history.clone() else {
            return Ok(false);
        };
        let Some(history_path) = self.history_path.as_deref() else {
            return Ok(false);
        };
        if let Some(pattern) = history.label_extract.as_deref()
            && let Ok(regex) = regex::Regex::new(pattern)
            && let Some(label) = extract_output_label(run_dir, &regex).await?
        {
            let _ = crate::history::refine_value(
                history_path,
                &meta.action_id,
                &history.value,
                &label,
                i64::try_from(meta.started_at).unwrap_or(i64::MAX),
                history.launch_token.as_deref(),
                &CancellationToken::new(),
            )
            .await?;
        }
        // Invalid/missing extractors and no-match runs keep the launch-time
        // raw value. Mark them done to avoid rescanning large logs forever.
        meta.history_refined = Some(true);
        write_json_atomic(&run_dir.join("meta.json"), &meta).await?;
        Ok(true)
    }

    pub async fn start(
        &self,
        mut request: ActionRequest,
        cancellation: &CancellationToken,
    ) -> Result<ActionStart, ActionServiceError> {
        if request.command.is_empty() {
            return Err(ActionServiceError::EmptyCommand);
        }
        let identity = scoped_identity(&self.config.log_dir, &request.slug);
        let lock_key = format!("action-{:016x}", stable_hash(&identity));
        let _lock = FileLock::acquire(
            &self.config.lock_dir,
            &lock_key,
            "start action",
            cancellation,
        )
        .await?;
        let session = action_session_name(&identity);
        if self
            .config
            .tmux
            .session_exists(&session, cancellation)
            .await?
            || self
                .config
                .tmux
                .session_exists(&format!("{}-action", request.slug), cancellation)
                .await?
        {
            return Err(ActionServiceError::AlreadyRunning(session));
        }

        let run_id = new_run_id();
        if let Some(history) = request.arg_history.as_mut() {
            history.launch_token = Some(run_id.clone());
        }
        let started_at = now_ms();
        if let (Some(path), Some(history)) = (&self.history_path, request.arg_history.as_ref()) {
            let _ = crate::history::record_value_for_run(
                path,
                &request.action_id,
                &history.value,
                None,
                i64::try_from(started_at).unwrap_or(i64::MAX),
                &run_id,
                cancellation,
            )
            .await;
        }
        let run_dir = self.config.log_dir.join("actions").join(&run_id);
        fs::create_dir_all(&run_dir)
            .await
            .map_err(|source| io_error("create action run directory", &run_dir, source))?;
        let run_dir = fs::canonicalize(&run_dir)
            .await
            .map_err(|source| io_error("resolve action run directory", &run_dir, source))?;
        let meta = ActionMeta {
            issue_status: request.issue_status.clone(),
            arg_history: request.arg_history.clone(),
            history_refined: None,
            version: 1,
            slug: request.slug.clone(),
            worktree_ref: request.worktree_ref.clone(),
            run_id: run_id.clone(),
            action_key: request.action_key.clone(),
            kind: request.kind,
            action_id: request.action_id.clone(),
            action_name: request.action_name.clone(),
            prompt: request.prompt.clone(),
            affects: request.affects.clone(),
            external: request.external.then_some(true),
            auto_fire_keys: request.auto_fire_keys.clone(),
            started_at,
            ended_at: None,
            exit_code: None,
            status: ActionRunStatus::Running,
            extra: Default::default(),
        };
        let run = ActionRun {
            meta,
            run_dir: run_dir.clone(),
            command: request.command.clone(),
            cwd: request.cwd.clone(),
        };
        let job = ActionJob {
            version: 1,
            request,
            run: run.clone(),
            session: session.clone(),
        };
        let mut starting_meta = run.meta.clone();
        // Persist uncertainty before asking tmux to create the session. If the
        // request reply is lost, boot must not treat the start as a safe retry.
        starting_meta.status = ActionRunStatus::Ambiguous;
        starting_meta
            .extra
            .insert("startState".into(), "creating".into());
        let mut command = vec!["env".to_owned()];
        for (name, value) in &job.request.config_selectors {
            if !valid_env_name(name) || value.contains('\0') {
                return Err(ActionServiceError::InvalidJob {
                    path: run_dir.join("job.json"),
                    message: format!("invalid environment selector {name:?}"),
                });
            }
            command.push(format!("{name}={value}"));
        }
        command.extend([
            self.config.executable.to_string_lossy().into_owned(),
            "_action-worker".into(),
            "--job".into(),
            run_dir.join("job.json").to_string_lossy().into_owned(),
        ]);
        let create = CreateSession {
            name: session.clone(),
            cwd: run.cwd.clone(),
            command,
            width: None,
            height: None,
        };
        let worker_lock_key = format!("action-run-{run_id}");
        let worker_lock = FileLock::acquire(
            &self.config.lock_dir,
            &worker_lock_key,
            "start action worker",
            cancellation,
        )
        .await?;
        write_json_atomic(&run_dir.join("job.json"), &job).await?;
        write_json_atomic(&run_dir.join("meta.json"), &starting_meta).await?;
        match self.config.tmux.create_session(&create, cancellation).await {
            Ok(()) => {
                let mut meta: ActionMeta = read_json(&run_dir.join("meta.json"))
                    .await
                    .map_err(|error| start_ambiguous_error(&run_id, &session, error))?;
                if meta.status == ActionRunStatus::Ambiguous {
                    meta.status = ActionRunStatus::Running;
                    meta.extra.remove("startState");
                    write_json_atomic(&run_dir.join("meta.json"), &meta)
                        .await
                        .map_err(|error| start_ambiguous_error(&run_id, &session, error))?;
                }
            }
            Err(error) => {
                let probe = self
                    .config
                    .tmux
                    .session_exists(&session, &CancellationToken::new())
                    .await;
                let mut meta: ActionMeta =
                    read_json(&run_dir.join("meta.json"))
                        .await
                        .map_err(|metadata_error| {
                            start_ambiguous_error(
                                &run_id,
                                &session,
                                format!(
                                    "{error}; could not re-read start record: {metadata_error}"
                                ),
                            )
                        })?;
                match classify_start_error(&error, &probe) {
                    StartErrorDisposition::Active => {
                        if meta.status == ActionRunStatus::Ambiguous {
                            meta.status = ActionRunStatus::Running;
                            meta.extra.remove("startState");
                            write_json_atomic(&run_dir.join("meta.json"), &meta)
                                .await
                                .map_err(|metadata_error| {
                                    start_ambiguous_error(
                                        &run_id,
                                        &session,
                                        format!("{error}; could not record active session: {metadata_error}"),
                                    )
                                })?;
                        }
                        drop(worker_lock);
                        return Ok(ActionStart {
                            run_id,
                            run_dir,
                            session,
                        });
                    }
                    StartErrorDisposition::Rejected => {
                        if meta.status == ActionRunStatus::Ambiguous {
                            meta.status = ActionRunStatus::Failed;
                            meta.ended_at = Some(now_ms());
                            meta.extra.remove("startState");
                            meta.extra
                                .insert("startError".into(), error.to_string().into());
                            write_json_atomic(&run_dir.join("meta.json"), &meta)
                                .await
                                .map_err(|metadata_error| {
                                    start_ambiguous_error(
                                        &run_id,
                                        &session,
                                        format!("{error}; could not record rejected start: {metadata_error}"),
                                    )
                                })?;
                        }
                        drop(worker_lock);
                        return Err(error.into());
                    }
                    StartErrorDisposition::Ambiguous => {
                        if meta.status == ActionRunStatus::Ambiguous {
                            meta.extra.insert("startState".into(), "ambiguous".into());
                            meta.extra.insert(
                                "startError".into(),
                                format!("{error}; session probe: {}", probe_error(&probe)).into(),
                            );
                            write_json_atomic(&run_dir.join("meta.json"), &meta)
                                .await
                                .map_err(|metadata_error| {
                                    start_ambiguous_error(
                                        &run_id,
                                        &session,
                                        format!("{error}; could not record ambiguous start: {metadata_error}"),
                                    )
                                })?;
                        }
                        drop(worker_lock);
                        return Err(ActionServiceError::StartAmbiguous {
                            run_id,
                            session,
                            reason: format!("{error}; {}", probe_error(&probe)),
                        });
                    }
                }
            }
        }
        drop(worker_lock);
        Ok(ActionStart {
            run_id,
            run_dir,
            session,
        })
    }

    /// Entry point for the hidden native worker. The caller passes the
    /// persisted job path; the worker validates it remains under logDir/actions
    /// before executing any command.
    pub async fn run_worker(
        &self,
        job_path: &Path,
        cancellation: &CancellationToken,
    ) -> Result<ActionDone, ActionServiceError> {
        let job_path = tokio::fs::canonicalize(job_path)
            .await
            .map_err(|source| io_error("resolve action job", job_path, source))?;
        let actions_root = tokio::fs::canonicalize(self.config.log_dir.join("actions"))
            .await
            .map_err(|source| {
                io_error("resolve actions directory", &self.config.log_dir, source)
            })?;
        if !job_path.starts_with(&actions_root)
            || job_path.file_name().is_none_or(|n| n != "job.json")
        {
            return Err(ActionServiceError::InvalidJob {
                path: job_path,
                message: "job path is outside the owned actions directory".into(),
            });
        }
        let job: ActionJob = read_json(&job_path).await?;
        if job.version != 1 || job.run.meta.run_id.is_empty() || job.request.command.is_empty() {
            return Err(ActionServiceError::InvalidJob {
                path: job_path,
                message: "unsupported version or missing run identity/command".into(),
            });
        }
        validate_run_id(&job.run.meta.run_id)?;
        let worker_lock_key = format!("action-run-{}", job.run.meta.run_id);
        let _worker_lock = FileLock::acquire(
            &self.config.lock_dir,
            &worker_lock_key,
            "run action worker",
            cancellation,
        )
        .await?;
        if job.run.run_dir != job_path.parent().unwrap_or(Path::new("")) {
            return Err(ActionServiceError::InvalidJob {
                path: job_path,
                message: "run directory does not match the job location".into(),
            });
        }
        let run_dir = job.run.run_dir.clone();
        let mut meta: ActionMeta = read_json(&run_dir.join("meta.json")).await?;
        let _state_lock = self.state_lock(&meta.run_id).await?;
        meta = read_json(&run_dir.join("meta.json")).await?;
        let done_exists = match fs::try_exists(run_dir.join("done.json")).await {
            Ok(exists) => exists,
            Err(source) => {
                return Err(io_error(
                    "inspect completion marker",
                    &run_dir.join("done.json"),
                    source,
                ));
            }
        };
        if !matches!(
            meta.status,
            ActionRunStatus::Running | ActionRunStatus::Ambiguous
        ) || done_exists
        {
            return Err(ActionServiceError::AlreadyFinished(meta.run_id));
        }
        meta.status = ActionRunStatus::Running;
        write_json_atomic(&run_dir.join("meta.json"), &meta).await?;
        drop(_state_lock);

        let (sender, receiver) = mpsc::channel(STREAM_QUEUE_CHUNKS);
        let dropped_bytes = Arc::new(AtomicU64::new(0));
        let writer = tokio::spawn(write_streams(run_dir.clone(), receiver));
        let dropped = dropped_bytes.clone();
        let mut command = CommandSpec::new(OsString::from(&job.request.command[0]));
        command.args = job
            .request
            .command
            .iter()
            .skip(1)
            .map(OsString::from)
            .collect();
        command.cwd = Some(job.request.cwd.clone());
        command.env = job
            .request
            .config_selectors
            .iter()
            .map(|(name, value)| (OsString::from(name), Some(OsString::from(value))))
            .collect();
        command.timeout = Duration::from_secs(24 * 60 * 60);
        command.output_limit = ACTION_CAPTURE_LIMIT;
        let sender_for_observer = sender.clone();
        let result = self
            .config
            .runner
            .run_streaming(command, cancellation, move |stream, bytes| {
                for part in bytes.chunks(STREAM_CHUNK_LIMIT) {
                    match sender_for_observer.try_send(OutputChunk {
                        stream,
                        bytes: part.to_vec(),
                    }) {
                        Ok(()) => {}
                        Err(_) => {
                            dropped.fetch_add(part.len() as u64, Ordering::Relaxed);
                        }
                    }
                }
            })
            .await;
        drop(sender);
        let writer_result = writer
            .await
            .map_err(|error| ActionServiceError::StreamWriter(error.to_string()))
            .and_then(|result| result);
        let writer_error = writer_result.as_ref().err().map(ToString::to_string);
        let writer_failed = writer_error.is_some();
        let writer_dropped = writer_result.unwrap_or(0);
        let dropped_output_bytes = dropped_bytes
            .load(Ordering::Relaxed)
            .saturating_add(writer_dropped);
        let (mut status, exit_code, stdout_truncated, stderr_truncated, mut error) = match result {
            Ok(output) => (
                if cancellation.is_cancelled() {
                    ActionRunStatus::Killed
                } else if output.status.success() {
                    ActionRunStatus::Succeeded
                } else {
                    ActionRunStatus::Failed
                },
                output.status.code(),
                output.stdout_truncated,
                output.stderr_truncated,
                None,
            ),
            Err(ProcessError::Cancelled { .. }) => (
                ActionRunStatus::Killed,
                None,
                false,
                false,
                Some("cancelled".into()),
            ),
            Err(error) => (
                ActionRunStatus::Failed,
                None,
                false,
                false,
                Some(error.to_string()),
            ),
        };
        if let Some(writer_error) = writer_error {
            status = ActionRunStatus::Failed;
            error = Some(writer_error);
        }
        // A kill can be asserted while the process is draining. Serialize the
        // terminal write with that assertion and retain its outcome/fields.
        let _state_lock = self.state_lock(&meta.run_id).await?;
        meta = read_json(&run_dir.join("meta.json")).await?;
        if meta.status == ActionRunStatus::Killed {
            status = ActionRunStatus::Killed;
            error = Some("killed by user".into());
        }
        let done = ActionDone {
            run_id: meta.run_id.clone(),
            status,
            exit_code,
            ended_at: now_ms(),
            dropped_output_bytes,
            stdout_truncated,
            stderr_truncated,
            error: error.clone(),
        };
        meta.status = status;
        meta.ended_at = Some(done.ended_at);
        meta.exit_code = exit_code;
        if dropped_output_bytes > 0 {
            meta.extra
                .insert("droppedOutputBytes".into(), dropped_output_bytes.into());
        }
        if stdout_truncated || stderr_truncated {
            meta.extra.insert("captureTruncated".into(), true.into());
        }
        if let Some(error) = error {
            meta.extra.insert("error".into(), error.into());
        }
        write_json_atomic(&run_dir.join("done.json"), &done).await?;
        write_json_atomic(&run_dir.join("meta.json"), &meta).await?;
        drop(_state_lock);
        if status == ActionRunStatus::Succeeded {
            let _ = self.refine_history(&run_dir).await;
        }
        if writer_failed {
            return Err(ActionServiceError::StreamWriter(
                done.error.clone().unwrap_or_default(),
            ));
        }
        Ok(done)
    }

    pub async fn kill(
        &self,
        slug: &str,
        cancellation: &CancellationToken,
    ) -> Result<bool, ActionServiceError> {
        let Some(run) = self.list_runs(usize::MAX).await?.into_iter().find(|run| {
            run.meta.slug == slug
                && matches!(
                    run.meta.status,
                    ActionRunStatus::Running | ActionRunStatus::Ambiguous
                )
        }) else {
            return Ok(false);
        };
        let action_key = if run.meta.action_key.is_empty() {
            run.meta.slug.as_str()
        } else {
            run.meta.action_key.as_str()
        };
        self.kill_run(action_key, &run.meta.run_id, cancellation)
            .await
    }

    /// Kill only the exact active run the caller confirmed. A replacement run
    /// cannot be killed by a delayed confirmation because the slug start/kill
    /// lock and per-run state lock are rechecked before changing metadata.
    pub async fn kill_run(
        &self,
        action_key: &str,
        expected_run_id: &str,
        cancellation: &CancellationToken,
    ) -> Result<bool, ActionServiceError> {
        validate_run_id(expected_run_id)?;
        let Some(candidate) = self
            .list_runs(usize::MAX)
            .await?
            .into_iter()
            .find(|run| run.meta.run_id == expected_run_id)
        else {
            return Ok(false);
        };
        let candidate_key = if candidate.meta.action_key.is_empty() {
            candidate.meta.slug.as_str()
        } else {
            candidate.meta.action_key.as_str()
        };
        if candidate_key != action_key
            || !matches!(
                candidate.meta.status,
                ActionRunStatus::Running | ActionRunStatus::Ambiguous
            )
        {
            return Ok(false);
        }
        let identity = scoped_identity(&self.config.log_dir, &candidate.meta.slug);
        let lock_key = format!("action-{:016x}", stable_hash(&identity));
        let _lock = FileLock::acquire(
            &self.config.lock_dir,
            &lock_key,
            "kill action",
            cancellation,
        )
        .await?;
        let latest_for_slot = self
            .list_runs(usize::MAX)
            .await?
            .into_iter()
            .filter(|run| run.meta.slug == candidate.meta.slug)
            .max_by(|left, right| {
                left.meta
                    .started_at
                    .cmp(&right.meta.started_at)
                    .then_with(|| left.meta.run_id.cmp(&right.meta.run_id))
            });
        let current_run = latest_for_slot
            .as_ref()
            .is_some_and(|latest| latest.meta.run_id == expected_run_id);
        let _state_lock = self.state_lock(expected_run_id).await?;
        let mut meta: ActionMeta = read_json(&candidate.run_dir.join("meta.json")).await?;
        let meta_key = if meta.action_key.is_empty() {
            meta.slug.as_str()
        } else {
            meta.action_key.as_str()
        };
        if meta.run_id != expected_run_id
            || meta_key != action_key
            || !matches!(
                meta.status,
                ActionRunStatus::Running | ActionRunStatus::Ambiguous
            )
        {
            return Ok(false);
        }
        meta.status = ActionRunStatus::Killed;
        meta.ended_at = Some(now_ms());
        meta.extra.insert("error".into(), "killed by user".into());
        write_json_atomic(&candidate.run_dir.join("meta.json"), &meta).await?;

        // An orphaned older run may still have ambiguous/running metadata.
        // Tombstone that exact run so a delayed worker refuses to execute,
        // but never signal the shared tmux name now owned by a replacement.
        if !current_run {
            return Ok(true);
        }

        let session = action_session_name(&identity);
        let legacy_session = format!("{}-action", candidate.meta.slug);
        for session in [session, legacy_session] {
            if self
                .config
                .tmux
                .session_exists(&session, cancellation)
                .await?
            {
                self.config
                    .tmux
                    .kill_session(&session, cancellation)
                    .await?;
                break;
            }
        }
        Ok(true)
    }

    pub async fn list_runs(&self, limit: usize) -> Result<Vec<ActionRun>, ActionServiceError> {
        let root = self.config.log_dir.join("actions");
        let mut dirs = match fs::read_dir(&root).await {
            Ok(dirs) => dirs,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(io_error("list action runs", &root, source)),
        };
        let mut runs = Vec::new();
        while let Some(entry) = dirs
            .next_entry()
            .await
            .map_err(|source| io_error("read action run entry", &root, source))?
        {
            if !entry.file_type().await.is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let run_dir = entry.path();
            let path = run_dir.join("meta.json");
            let Ok(meta) = read_json::<ActionMeta>(&path).await else {
                continue;
            };
            if meta.version != 1 || meta.run_id != entry.file_name().to_string_lossy() {
                continue;
            }
            // TypeScript workers have metadata and logs but no native job.
            // Preserve their history without ever attempting to execute it.
            let job = match read_json::<ActionJob>(&run_dir.join("job.json")).await {
                Ok(job) => Some(job),
                Err(ActionServiceError::Io { source, .. })
                    if source.kind() == std::io::ErrorKind::NotFound =>
                {
                    None
                }
                Err(error) => return Err(error),
            };
            runs.push(ActionRun {
                meta,
                run_dir,
                command: job
                    .as_ref()
                    .map(|job| job.run.command.clone())
                    .unwrap_or_default(),
                cwd: job.map(|job| job.run.cwd).unwrap_or_default(),
            });
        }
        runs.sort_by_key(|run| std::cmp::Reverse(run.meta.started_at));
        runs.truncate(limit);
        Ok(runs)
    }

    /// Return recent history while always retaining every active or ambiguous
    /// run. The output is bounded by `limit` terminal runs plus active runs.
    /// Only metadata is inspected for discarded history; job files are read
    /// for the retained records.
    pub async fn list_runs_bounded(
        &self,
        terminal_limit: usize,
    ) -> Result<Vec<ActionRun>, ActionServiceError> {
        let root = self.config.log_dir.join("actions");
        let mut dirs = match fs::read_dir(&root).await {
            Ok(dirs) => dirs,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(io_error("list action runs", &root, source)),
        };
        let mut records = Vec::new();
        while let Some(entry) = dirs
            .next_entry()
            .await
            .map_err(|source| io_error("read action run entry", &root, source))?
        {
            if !entry.file_type().await.is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let run_dir = entry.path();
            let Ok(meta) = read_json::<ActionMeta>(&run_dir.join("meta.json")).await else {
                continue;
            };
            if meta.version == 1 && meta.run_id == entry.file_name().to_string_lossy() {
                records.push((meta, run_dir));
            }
        }
        records.sort_by_key(|record| std::cmp::Reverse(record.0.started_at));
        let mut retained = Vec::new();
        let mut terminal_count = 0usize;
        for (meta, run_dir) in records {
            let active = matches!(
                meta.status,
                ActionRunStatus::Running | ActionRunStatus::Ambiguous
            );
            if active || terminal_count < terminal_limit {
                if !active {
                    terminal_count += 1;
                }
                retained.push((meta, run_dir));
            }
        }
        let mut runs = Vec::with_capacity(retained.len());
        for (meta, run_dir) in retained {
            let job = match read_json::<ActionJob>(&run_dir.join("job.json")).await {
                Ok(job) => Some(job),
                Err(ActionServiceError::Io { source, .. })
                    if source.kind() == std::io::ErrorKind::NotFound =>
                {
                    None
                }
                Err(error) => return Err(error),
            };
            runs.push(ActionRun {
                meta,
                run_dir,
                command: job
                    .as_ref()
                    .map(|job| job.run.command.clone())
                    .unwrap_or_default(),
                cwd: job.map(|job| job.run.cwd).unwrap_or_default(),
            });
        }
        Ok(runs)
    }

    /// Reconcile persisted `running` records against one batched tmux session
    /// snapshot. A worker that died with its pane is terminally failed; it is
    /// never silently relaunched.
    pub async fn reconcile_runs(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<usize, ActionServiceError> {
        // Keep every active run visible, but bound terminal reconciliation so
        // startup work does not scale with the full action-log archive.
        let runs = self.list_runs_bounded(120).await?;
        let sessions = self.config.tmux.list_sessions(cancellation).await?;
        let active: std::collections::HashSet<_> =
            sessions.into_iter().map(|session| session.name).collect();
        let mut reconciled = 0;
        for mut run in runs {
            if run.meta.status == ActionRunStatus::Succeeded
                && run.meta.arg_history.is_some()
                && run.meta.history_refined != Some(true)
            {
                let _ = self.refine_history(&run.run_dir).await;
                continue;
            }
            if run.meta.status != ActionRunStatus::Running {
                continue;
            }
            let session = match read_json::<ActionJob>(&run.run_dir.join("job.json")).await {
                Ok(job) => job.session,
                Err(ActionServiceError::Io { source, .. })
                    if source.kind() == std::io::ErrorKind::NotFound =>
                {
                    format!("{}-action", run.meta.slug)
                }
                Err(error) => return Err(error),
            };
            if active.contains(&session) {
                continue;
            }
            let identity = scoped_identity(&self.config.log_dir, &run.meta.slug);
            let Some(_action_lock) = FileLock::try_acquire(
                &self.config.lock_dir,
                &format!("action-{:016x}", stable_hash(&identity)),
                "reconcile action",
            )
            .await?
            else {
                continue;
            };
            let Some(_worker_lock) = FileLock::try_acquire(
                &self.config.lock_dir,
                &format!("action-run-{}", run.meta.run_id),
                "reconcile action worker",
            )
            .await?
            else {
                continue;
            };
            let _state_lock = self.state_lock(&run.meta.run_id).await?;
            run.meta = read_json(&run.run_dir.join("meta.json")).await?;
            if run.meta.status != ActionRunStatus::Running {
                continue;
            }
            if self.restore_completion(&mut run).await? {
                reconciled += 1;
                continue;
            }
            // The batched observation may predate a concurrent start. Repeat
            // only this orphan candidate's probe under the ownership locks.
            if self
                .config
                .tmux
                .session_exists(&session, cancellation)
                .await?
            {
                continue;
            }
            let ended_at = now_ms();
            run.meta.status = ActionRunStatus::Failed;
            run.meta.ended_at = Some(ended_at);
            run.meta.extra.insert(
                "error".into(),
                "worker exited before writing completion metadata".into(),
            );
            write_json_atomic(
                &run.run_dir.join("done.json"),
                &ActionDone {
                    run_id: run.meta.run_id.clone(),
                    status: ActionRunStatus::Failed,
                    exit_code: None,
                    ended_at,
                    dropped_output_bytes: 0,
                    stdout_truncated: false,
                    stderr_truncated: false,
                    error: Some("worker exited before writing completion metadata".into()),
                },
            )
            .await?;
            write_json_atomic(&run.run_dir.join("meta.json"), &run.meta).await?;
            reconciled += 1;
        }
        Ok(reconciled)
    }

    async fn state_lock(&self, run_id: &str) -> Result<FileLock, ActionServiceError> {
        validate_run_id(run_id)?;
        Ok(FileLock::acquire(
            &self.config.lock_dir,
            &format!("action-state-{run_id}"),
            "write action outcome",
            &CancellationToken::new(),
        )
        .await?)
    }

    async fn restore_completion(&self, run: &mut ActionRun) -> Result<bool, ActionServiceError> {
        let path = run.run_dir.join("done.json");
        let value: serde_json::Value = match read_json(&path).await {
            Ok(value) => value,
            Err(ActionServiceError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                return Ok(false);
            }
            Err(error) => return Err(error),
        };
        let (status, ended_at, exit_code) = if value.get("runId").is_some() {
            let done: ActionDone =
                serde_json::from_value(value).map_err(|error| ActionServiceError::InvalidJob {
                    path: path.clone(),
                    message: error.to_string(),
                })?;
            if done.run_id != run.meta.run_id
                || matches!(
                    done.status,
                    ActionRunStatus::Running | ActionRunStatus::Ambiguous
                )
            {
                return Err(ActionServiceError::InvalidJob {
                    path,
                    message: "completion does not identify a terminal result for this run".into(),
                });
            }
            (done.status, done.ended_at, done.exit_code)
        } else {
            let code = value
                .get("exitCode")
                .and_then(serde_json::Value::as_i64)
                .and_then(|code| i32::try_from(code).ok())
                .ok_or_else(|| ActionServiceError::InvalidJob {
                    path: path.clone(),
                    message: "legacy completion has no exitCode".into(),
                })?;
            let ended = fs::metadata(&path)
                .await
                .map_err(|source| io_error("stat completion", &path, source))?
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_millis() as u64)
                .unwrap_or_else(now_ms);
            (
                if code == 0 {
                    ActionRunStatus::Succeeded
                } else {
                    ActionRunStatus::Failed
                },
                ended,
                Some(code),
            )
        };
        run.meta.status = status;
        run.meta.ended_at = Some(ended_at);
        run.meta.exit_code = exit_code;
        write_json_atomic(&run.run_dir.join("meta.json"), &run.meta).await?;
        Ok(true)
    }

    pub async fn read_log(
        &self,
        run_id: &str,
        stream: ProcessStream,
    ) -> Result<Vec<u8>, ActionServiceError> {
        validate_run_id(run_id)?;
        let filename = match stream {
            ProcessStream::Stdout => "stream.log",
            ProcessStream::Stderr => "stderr.log",
        };
        let path = self
            .config
            .log_dir
            .join("actions")
            .join(run_id)
            .join(filename);
        let mut file = fs::File::open(&path)
            .await
            .map_err(|source| io_error("open action log", &path, source))?;
        let metadata = file
            .metadata()
            .await
            .map_err(|source| io_error("stat action log", &path, source))?;
        let start = metadata.len().saturating_sub(MAX_LOG_READ);
        file.seek(std::io::SeekFrom::Start(start))
            .await
            .map_err(|source| io_error("seek action log", &path, source))?;
        let mut bytes = Vec::with_capacity(metadata.len().saturating_sub(start) as usize);
        file.take(MAX_LOG_READ)
            .read_to_end(&mut bytes)
            .await
            .map_err(|source| io_error("read action log", &path, source))?;
        Ok(bytes)
    }

    /// Read one bounded incremental window from a run log. With no offset,
    /// seed a tail from the end; subsequent calls resume at the returned end.
    pub async fn read_log_chunk(
        &self,
        run_id: &str,
        stream: ProcessStream,
        offset: Option<u64>,
        limit: usize,
    ) -> Result<(u64, Vec<u8>), ActionServiceError> {
        validate_run_id(run_id)?;
        let filename = match stream {
            ProcessStream::Stdout => "stream.log",
            ProcessStream::Stderr => "stderr.log",
        };
        let path = self
            .config
            .log_dir
            .join("actions")
            .join(run_id)
            .join(filename);
        let mut file = fs::File::open(&path)
            .await
            .map_err(|source| io_error("open action log", &path, source))?;
        let len = file
            .metadata()
            .await
            .map_err(|source| io_error("stat action log", &path, source))?
            .len();
        let limit = limit.clamp(1, 64 * 1024);
        let start = offset
            .filter(|offset| *offset <= len)
            .unwrap_or_else(|| len.saturating_sub(limit as u64));
        file.seek(std::io::SeekFrom::Start(start))
            .await
            .map_err(|source| io_error("seek action log", &path, source))?;
        let mut bytes = Vec::with_capacity(limit);
        file.take(limit as u64)
            .read_to_end(&mut bytes)
            .await
            .map_err(|source| io_error("read action log", &path, source))?;
        Ok((start + bytes.len() as u64, bytes))
    }
}

async fn write_streams(
    run_dir: PathBuf,
    mut receiver: mpsc::Receiver<OutputChunk>,
) -> Result<u64, ActionServiceError> {
    let stdout_path = run_dir.join("stream.log");
    let stderr_path = run_dir.join("stderr.log");
    let mut stdout = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&stdout_path)
        .await
        .map_err(|source| io_error("open action stdout log", &stdout_path, source))?;
    let mut stderr = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&stderr_path)
        .await
        .map_err(|source| io_error("open action stderr log", &stderr_path, source))?;
    let mut written = 0_u64;
    let mut dropped = 0_u64;
    while let Some(chunk) = receiver.recv().await {
        let (file, path) = match chunk.stream {
            ProcessStream::Stdout => (&mut stdout, &stdout_path),
            ProcessStream::Stderr => (&mut stderr, &stderr_path),
        };
        let remaining = ACTION_LOG_LIMIT.saturating_sub(written) as usize;
        let keep = remaining.min(chunk.bytes.len());
        if keep > 0 {
            file.write_all(&chunk.bytes[..keep])
                .await
                .map_err(|source| io_error("append action log", path, source))?;
            written += keep as u64;
        }
        dropped = dropped.saturating_add((chunk.bytes.len() - keep) as u64);
    }
    stdout
        .flush()
        .await
        .map_err(|source| io_error("flush action stdout log", &stdout_path, source))?;
    stderr
        .flush()
        .await
        .map_err(|source| io_error("flush action stderr log", &stderr_path, source))?;
    Ok(dropped)
}

/// Scan only a bounded tail of each captured stream. The native writer stores
/// stdout and stderr separately, so within-stream order is exact; when both
/// contain matches, stderr is treated as later because their cross-stream
/// byte ordering is not persisted.
async fn extract_output_label(
    run_dir: &Path,
    regex: &regex::Regex,
) -> Result<Option<String>, ActionServiceError> {
    let mut found = None;
    for name in ["stream.log", "stderr.log"] {
        let path = run_dir.join(name);
        let mut file = match fs::File::open(&path).await {
            Ok(file) => file,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => return Err(io_error("open action output for label", &path, source)),
        };
        let metadata = file
            .metadata()
            .await
            .map_err(|source| io_error("stat action output for label", &path, source))?;
        let start = metadata.len().saturating_sub(MAX_LOG_READ);
        file.seek(std::io::SeekFrom::Start(start))
            .await
            .map_err(|source| io_error("seek action output for label", &path, source))?;
        let mut bytes = Vec::with_capacity(metadata.len().saturating_sub(start) as usize);
        file.take(MAX_LOG_READ)
            .read_to_end(&mut bytes)
            .await
            .map_err(|source| io_error("read action output for label", &path, source))?;
        if start > 0 {
            if let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') {
                bytes.drain(..=newline);
            } else {
                continue;
            }
        }
        for line in String::from_utf8_lossy(&bytes).lines() {
            if let Some(captures) = regex.captures(line) {
                let value = captures
                    .get(1)
                    .or_else(|| captures.get(0))
                    .map(|matched| matched.as_str().trim())
                    .unwrap_or_default();
                found = (!value.is_empty()).then(|| value.to_owned());
            }
        }
    }
    Ok(found)
}

async fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), ActionServiceError> {
    let parent = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent)
        .await
        .map_err(|source| io_error("create metadata directory", parent, source))?;
    let temp = path.with_extension(format!("tmp-{}", std::process::id()));
    let bytes =
        serde_json::to_vec_pretty(value).map_err(|source| ActionServiceError::InvalidJob {
            path: path.to_path_buf(),
            message: source.to_string(),
        })?;
    let mut file = fs::File::create(&temp)
        .await
        .map_err(|source| io_error("create temporary metadata", &temp, source))?;
    file.write_all(&bytes)
        .await
        .map_err(|source| io_error("write metadata", &temp, source))?;
    file.sync_all()
        .await
        .map_err(|source| io_error("sync metadata", &temp, source))?;
    drop(file);
    fs::rename(&temp, path)
        .await
        .map_err(|source| io_error("replace metadata", path, source))?;
    Ok(())
}

async fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, ActionServiceError> {
    let bytes = fs::read(path)
        .await
        .map_err(|source| io_error("read action metadata", path, source))?;
    serde_json::from_slice(&bytes).map_err(|source| ActionServiceError::InvalidJob {
        path: path.to_path_buf(),
        message: source.to_string(),
    })
}

fn validate_run_id(run_id: &str) -> Result<(), ActionServiceError> {
    if run_id.is_empty()
        || run_id == "."
        || run_id == ".."
        || !run_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(ActionServiceError::InvalidRunId(run_id.to_owned()));
    }
    Ok(())
}

fn action_session_name(key: &str) -> String {
    format!("wt-action-{:016x}", stable_hash(key))
}

fn scoped_identity(log_dir: &Path, slug: &str) -> String {
    format!("{}\0{slug}", log_dir.to_string_lossy())
}

fn valid_env_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().enumerate().all(|(index, byte)| {
            byte == b'_' || byte.is_ascii_alphabetic() || (index > 0 && byte.is_ascii_digit())
        })
}

fn stable_hash(value: &str) -> u64 {
    value.bytes().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    })
}

fn new_run_id() -> String {
    format!(
        "{}-{}-{}",
        now_ms(),
        std::process::id(),
        RUN_COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn now_ms() -> u64 {
    (OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000).max(0) as u64
}

fn io_error(operation: &'static str, path: &Path, source: std::io::Error) -> ActionServiceError {
    ActionServiceError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

fn tmux_start_rejected(error: &TmuxError) -> bool {
    // A completed non-zero tmux command is a server response. Timeout,
    // cancellation, spawn, and transport failures do not prove the request was
    // rejected and therefore leave its durable start claim ambiguous.
    matches!(error, TmuxError::Command { .. })
}

fn start_ambiguous_error(
    run_id: &str,
    session: &str,
    reason: impl std::fmt::Display,
) -> ActionServiceError {
    ActionServiceError::StartAmbiguous {
        run_id: run_id.to_owned(),
        session: session.to_owned(),
        reason: reason.to_string(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StartErrorDisposition {
    Active,
    Rejected,
    Ambiguous,
}

fn classify_start_error(
    error: &TmuxError,
    probe: &Result<bool, TmuxError>,
) -> StartErrorDisposition {
    match probe {
        Ok(true) => StartErrorDisposition::Active,
        Ok(false) if tmux_start_rejected(error) => StartErrorDisposition::Rejected,
        Ok(false) | Err(_) => StartErrorDisposition::Ambiguous,
    }
}

fn probe_error(probe: &Result<bool, TmuxError>) -> String {
    match probe {
        Ok(true) => "session is active".into(),
        Ok(false) => "session is absent, but the create reply was lost".into(),
        Err(error) => format!("session state is unknown ({error})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_platform::process::ProcessRunner;
    use wt_tmux::TmuxServer;

    #[test]
    fn only_explicit_tmux_rejection_and_confirmed_absence_is_safe_failure() {
        let rejected = TmuxError::Command {
            operation: "new-session",
            code: Some(1),
            stderr: "invalid session name".into(),
            stdout: String::new(),
        };
        assert_eq!(
            classify_start_error(&rejected, &Ok(false)),
            StartErrorDisposition::Rejected
        );
        assert_eq!(
            classify_start_error(&rejected, &Ok(true)),
            StartErrorDisposition::Active
        );
        let cancelled = TmuxError::Process {
            operation: "new-session",
            source: ProcessError::Cancelled {
                program: "tmux".into(),
            },
        };
        assert_eq!(
            classify_start_error(&cancelled, &Ok(false)),
            StartErrorDisposition::Ambiguous
        );
        assert_eq!(
            classify_start_error(
                &cancelled,
                &Err(TmuxError::MalformedOutput {
                    operation: "has-session",
                    line: "unparseable".into(),
                })
            ),
            StartErrorDisposition::Ambiguous
        );
    }

    fn request(cwd: &Path, command: &[&str]) -> ActionRequest {
        ActionRequest {
            issue_status: None,
            action_key: "feature-1".into(),
            slug: "feature-1".into(),
            worktree_ref: None,
            action_id: "test".into(),
            action_name: "Test action".into(),
            arg_history: None,
            prompt: "fixture".into(),
            kind: ActionRunKind::Shell,
            command: command.iter().map(|part| (*part).into()).collect(),
            cwd: cwd.to_path_buf(),
            affects: Vec::new(),
            external: false,
            auto_fire_keys: Vec::new(),
            config_selectors: BTreeMap::new(),
        }
    }

    fn service(log_dir: &Path) -> ActionService {
        ActionService::new(ActionServiceConfig {
            log_dir: log_dir.to_path_buf(),
            lock_dir: log_dir.join("locks"),
            executable: PathBuf::from("/bin/true"),
            runner: ProcessRunner::default(),
            tmux: TmuxClient::new(
                ProcessRunner::default(),
                TmuxServer::at(log_dir.join("tmux.sock")),
            ),
        })
    }

    async fn prepared_job(log_dir: &Path, request: ActionRequest) -> PathBuf {
        let run_id = new_run_id();
        let run_dir = log_dir.join("actions").join(run_id);
        fs::create_dir_all(&run_dir).await.unwrap();
        let run_dir = fs::canonicalize(run_dir).await.unwrap();
        let meta = ActionMeta {
            issue_status: request.issue_status.clone(),
            arg_history: request.arg_history.clone(),
            history_refined: None,
            version: 1,
            slug: request.slug.clone(),
            worktree_ref: None,
            run_id: run_dir.file_name().unwrap().to_string_lossy().into_owned(),
            action_key: request.action_key.clone(),
            kind: request.kind,
            action_id: request.action_id.clone(),
            action_name: request.action_name.clone(),
            prompt: request.prompt.clone(),
            affects: request.affects.clone(),
            external: None,
            auto_fire_keys: request.auto_fire_keys.clone(),
            started_at: now_ms(),
            ended_at: None,
            exit_code: None,
            status: ActionRunStatus::Running,
            extra: Default::default(),
        };
        let run = ActionRun {
            meta: meta.clone(),
            run_dir: run_dir.clone(),
            command: request.command.clone(),
            cwd: request.cwd.clone(),
        };
        write_json_atomic(&run_dir.join("meta.json"), &meta)
            .await
            .unwrap();
        write_json_atomic(
            &run_dir.join("job.json"),
            &ActionJob {
                version: 1,
                request,
                run,
                session: action_session_name("feature-1"),
            },
        )
        .await
        .unwrap();
        run_dir.join("job.json")
    }

    #[tokio::test]
    async fn history_honors_limits_and_retains_legacy_metadata_without_native_jobs() {
        let temp = tempfile::tempdir().unwrap();
        for index in 0..32 {
            let job = prepared_job(temp.path(), request(temp.path(), &["true"])).await;
            let path = job.parent().unwrap().join("meta.json");
            let mut meta: ActionMeta = read_json(&path).await.unwrap();
            meta.started_at = index;
            write_json_atomic(&path, &meta).await.unwrap();
            if index == 31 {
                fs::remove_file(job).await.unwrap();
            }
        }
        let all = service(temp.path()).list_runs(100).await.unwrap();
        assert_eq!(all.len(), 32);
        assert_eq!(all[0].meta.started_at, 31);
        assert!(all[0].command.is_empty());
        assert_eq!(service(temp.path()).list_runs(3).await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn recovery_uses_durable_completion_and_respects_an_owned_worker_lock() {
        let temp = tempfile::tempdir().unwrap();
        let job = prepared_job(temp.path(), request(temp.path(), &["true"])).await;
        let run_dir = job.parent().unwrap();
        let meta: ActionMeta = read_json(&run_dir.join("meta.json")).await.unwrap();
        let worker_lock = FileLock::acquire(
            &temp.path().join("locks"),
            &format!("action-run-{}", meta.run_id),
            "fixture live worker",
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            service(temp.path())
                .reconcile_runs(&CancellationToken::new())
                .await
                .unwrap(),
            0
        );
        drop(worker_lock);
        // Legacy workers can leave only this sentinel before the TUI exits.
        fs::write(run_dir.join("done.json"), br#"{"exitCode":0}"#)
            .await
            .unwrap();
        assert_eq!(
            service(temp.path())
                .reconcile_runs(&CancellationToken::new())
                .await
                .unwrap(),
            1
        );
        let meta: ActionMeta = read_json(&run_dir.join("meta.json")).await.unwrap();
        assert_eq!(meta.status, ActionRunStatus::Succeeded);
        assert_eq!(meta.exit_code, Some(0));
        assert_eq!(
            service(temp.path())
                .reconcile_runs(&CancellationToken::new())
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn stale_kill_confirmation_cannot_kill_replacement_run() {
        let temp = tempfile::tempdir().unwrap();
        let old_job = prepared_job(temp.path(), request(temp.path(), &["true"])).await;
        let new_job = prepared_job(temp.path(), request(temp.path(), &["true"])).await;
        let old_dir = old_job.parent().unwrap();
        let old_meta: ActionMeta = read_json(&old_dir.join("meta.json")).await.unwrap();
        // Simulate a previous action whose session vanished without updating
        // its metadata, followed by a newer run in the shared tmux slot.
        let new_dir = new_job.parent().unwrap();
        let new_meta: ActionMeta = read_json(&new_dir.join("meta.json")).await.unwrap();
        assert!(
            service(temp.path())
                .kill_run("feature-1", &old_meta.run_id, &CancellationToken::new())
                .await
                .unwrap()
        );
        let tombstoned: ActionMeta = read_json(&old_dir.join("meta.json")).await.unwrap();
        assert_eq!(tombstoned.status, ActionRunStatus::Killed);
        let still_running: ActionMeta = read_json(&new_dir.join("meta.json")).await.unwrap();
        assert_eq!(still_running.run_id, new_meta.run_id);
        assert_eq!(still_running.status, ActionRunStatus::Running);
    }

    #[tokio::test]
    async fn worker_streams_logs_and_commits_completion_after_flush() {
        let temp = tempfile::tempdir().unwrap();
        let job = prepared_job(
            temp.path(),
            request(
                temp.path(),
                &["/bin/sh", "-c", "printf out; printf err >&2"],
            ),
        )
        .await;
        let done = service(temp.path())
            .run_worker(&job, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(done.status, ActionRunStatus::Succeeded);
        let run_dir = job.parent().unwrap();
        assert_eq!(fs::read(run_dir.join("stream.log")).await.unwrap(), b"out");
        assert_eq!(fs::read(run_dir.join("stderr.log")).await.unwrap(), b"err");
        let run_id = run_dir.file_name().unwrap().to_string_lossy();
        let service = service(temp.path());
        let (offset, tail) = service
            .read_log_chunk(&run_id, ProcessStream::Stdout, None, 2)
            .await
            .unwrap();
        assert_eq!(tail, b"ut");
        assert_eq!(offset, 3);
        // Tokio file writes finish on a blocking thread; flush before the
        // read or it can race the append and see nothing.
        let mut log = fs::OpenOptions::new()
            .append(true)
            .open(run_dir.join("stream.log"))
            .await
            .unwrap();
        log.write_all(b"more").await.unwrap();
        log.flush().await.unwrap();
        drop(log);
        let (offset, delta) = service
            .read_log_chunk(&run_id, ProcessStream::Stdout, Some(offset), 2)
            .await
            .unwrap();
        assert_eq!(delta, b"mo");
        assert_eq!(offset, 5);
        let meta: ActionMeta = read_json(&run_dir.join("meta.json")).await.unwrap();
        assert_eq!(meta.status, ActionRunStatus::Succeeded);
        assert!(run_dir.join("done.json").exists());
    }

    #[tokio::test]
    async fn successful_worker_refines_arg_history_but_failed_worker_does_not() {
        let temp = tempfile::tempdir().unwrap();
        let history_path = temp.path().join("cache/action-history.json");
        let cancel = CancellationToken::new();
        let mut success_request = request(
            temp.path(),
            &["/bin/sh", "-c", "printf 'Resolved: Friendly name\\n'"],
        );
        success_request.arg_history = Some(ActionArgHistory {
            value: "lookup-key".into(),
            label_extract: Some("Resolved: (.*)".into()),
            launch_token: None,
        });
        let success_job = prepared_job(temp.path(), success_request).await;
        let success_meta: ActionMeta = read_json(&success_job.parent().unwrap().join("meta.json"))
            .await
            .unwrap();
        crate::history::record_value(
            &history_path,
            "test",
            "lookup-key",
            None,
            i64::try_from(success_meta.started_at).unwrap(),
            &cancel,
        )
        .await
        .unwrap();
        let meta_path = success_job.parent().unwrap().join("meta.json");
        let mut raw: serde_json::Value = read_json(&meta_path).await.unwrap();
        raw["futureField"] = "preserved".into();
        write_json_atomic(&meta_path, &raw).await.unwrap();
        let done = service(temp.path())
            .with_history_path(history_path.clone())
            .run_worker(&success_job, &cancel)
            .await
            .unwrap();
        assert_eq!(done.status, ActionRunStatus::Succeeded);
        assert_eq!(
            crate::history::recent_values(&history_path, "test").await[0]
                .label
                .as_deref(),
            Some("Friendly name")
        );
        let raw: serde_json::Value = read_json(&meta_path).await.unwrap();
        assert_eq!(raw["futureField"], "preserved");
        assert_eq!(raw["historyRefined"], true);

        let mut failed_request = request(
            temp.path(),
            &["/bin/sh", "-c", "printf 'Resolved: Wrong name\\n'; exit 1"],
        );
        failed_request.action_id = "failed".into();
        failed_request.arg_history = Some(ActionArgHistory {
            value: "failed-key".into(),
            label_extract: Some("Resolved: (.*)".into()),
            launch_token: None,
        });
        let failed_job = prepared_job(temp.path(), failed_request).await;
        let failed_meta: ActionMeta = read_json(&failed_job.parent().unwrap().join("meta.json"))
            .await
            .unwrap();
        crate::history::record_value(
            &history_path,
            "failed",
            "failed-key",
            None,
            i64::try_from(failed_meta.started_at).unwrap(),
            &cancel,
        )
        .await
        .unwrap();
        let failed_done = service(temp.path())
            .with_history_path(history_path.clone())
            .run_worker(&failed_job, &cancel)
            .await
            .unwrap();
        assert_eq!(failed_done.status, ActionRunStatus::Failed);
        assert_eq!(
            crate::history::recent_values(&history_path, "failed").await[0].label,
            None
        );
    }

    #[tokio::test]
    async fn worker_surfaces_capture_truncation_and_invalid_job_paths_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let job = prepared_job(
            temp.path(),
            request(temp.path(), &["/bin/sh", "-c", "head -c 3000000 /dev/zero"]),
        )
        .await;
        let done = service(temp.path())
            .run_worker(&job, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(done.status, ActionRunStatus::Succeeded);
        assert!(done.stdout_truncated);
        assert!(done.dropped_output_bytes > 0);
        let outside = temp.path().join("job.json");
        assert!(matches!(
            service(temp.path())
                .run_worker(&outside, &CancellationToken::new())
                .await,
            Err(ActionServiceError::Io { .. })
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn worker_fails_closed_when_done_marker_state_is_unreadable() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let job = prepared_job(temp.path(), request(temp.path(), &["/bin/true"])).await;
        let marker = job.parent().unwrap().join("done.json");
        symlink(&marker, &marker).unwrap();
        let error = service(temp.path())
            .run_worker(&job, &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ActionServiceError::Io {
                operation: "inspect completion marker",
                ..
            }
        ));
        assert!(
            !fs::read(job.parent().unwrap().join("stream.log"))
                .await
                .is_ok()
        );
    }
}
