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

use crate::types::{ActionMeta, ActionRun, ActionRunKind, ActionRunStatus};

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
    pub prompt: String,
    pub kind: ActionRunKind,
    pub command: Vec<String>,
    pub cwd: PathBuf,
    #[serde(default)]
    pub affects: Vec<EffectTag>,
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
    #[error("action job at {path} is invalid: {message}")]
    InvalidJob { path: PathBuf, message: String },
    #[error("action stream writer failed: {0}")]
    StreamWriter(String),
}

#[derive(Clone)]
pub struct ActionService {
    config: ActionServiceConfig,
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
        Self { config }
    }

    pub async fn start(
        &self,
        request: ActionRequest,
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
        {
            return Err(ActionServiceError::AlreadyRunning(session));
        }

        let run_id = new_run_id();
        let run_dir = self.config.log_dir.join("actions").join(&run_id);
        fs::create_dir_all(&run_dir)
            .await
            .map_err(|source| io_error("create action run directory", &run_dir, source))?;
        let run_dir = fs::canonicalize(&run_dir)
            .await
            .map_err(|source| io_error("resolve action run directory", &run_dir, source))?;
        let meta = ActionMeta {
            version: 1,
            slug: request.slug.clone(),
            worktree_ref: request.worktree_ref.clone(),
            run_id: run_id.clone(),
            kind: request.kind,
            action_id: request.action_id.clone(),
            action_name: request.action_name.clone(),
            prompt: request.prompt.clone(),
            affects: request.affects.clone(),
            external: request.external.then_some(true),
            auto_fire_keys: request.auto_fire_keys.clone(),
            started_at: now_ms(),
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
        write_json_atomic(&run_dir.join("meta.json"), &run.meta).await?;
        write_json_atomic(&run_dir.join("job.json"), &job).await?;

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
        if let Err(error) = self.config.tmux.create_session(&create, cancellation).await {
            let mut meta = run.meta;
            meta.status = ActionRunStatus::Failed;
            meta.ended_at = Some(now_ms());
            meta.extra
                .insert("startError".into(), error.to_string().into());
            write_json_atomic(&run_dir.join("meta.json"), &meta).await?;
            return Err(error.into());
        }
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
        if meta.status != ActionRunStatus::Running
            || fs::try_exists(run_dir.join("done.json"))
                .await
                .unwrap_or(false)
        {
            return Err(ActionServiceError::AlreadyFinished(meta.run_id));
        }
        meta.status = ActionRunStatus::Running;
        write_json_atomic(&run_dir.join("meta.json"), &meta).await?;

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
        let identity = scoped_identity(&self.config.log_dir, slug);
        let session = action_session_name(&identity);
        let lock_key = format!("action-{:016x}", stable_hash(&identity));
        let _lock = FileLock::acquire(
            &self.config.lock_dir,
            &lock_key,
            "kill action",
            cancellation,
        )
        .await?;
        if !self
            .config
            .tmux
            .session_exists(&session, cancellation)
            .await?
        {
            return Ok(false);
        }
        let runs = self.list_runs(20).await?;
        if let Some(mut run) = runs
            .into_iter()
            .find(|run| run.meta.slug == slug && run.meta.status == ActionRunStatus::Running)
        {
            run.meta.status = ActionRunStatus::Killed;
            run.meta.ended_at = Some(now_ms());
            run.meta
                .extra
                .insert("error".into(), "killed by user".into());
            write_json_atomic(&run.run_dir.join("meta.json"), &run.meta).await?;
        }
        self.config
            .tmux
            .kill_session(&session, cancellation)
            .await
            .map_err(Into::into)
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
            if runs.len() >= 2_000 {
                break;
            }
            if !entry.file_type().await.is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let run_dir = entry.path();
            let path = run_dir.join("meta.json");
            let Ok(meta) = read_json::<ActionMeta>(&path).await else {
                continue;
            };
            let Ok(job) = read_json::<ActionJob>(&run_dir.join("job.json")).await else {
                continue;
            };
            runs.push(ActionRun {
                meta,
                run_dir,
                command: job.run.command,
                cwd: job.run.cwd,
            });
        }
        runs.sort_by_key(|run| std::cmp::Reverse(run.meta.started_at));
        runs.truncate(limit.min(20));
        Ok(runs)
    }

    /// Reconcile persisted `running` records against one batched tmux session
    /// snapshot. A worker that died with its pane is terminally failed; it is
    /// never silently relaunched.
    pub async fn reconcile_runs(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<usize, ActionServiceError> {
        let sessions = self.config.tmux.list_sessions(cancellation).await?;
        let active: std::collections::HashSet<_> =
            sessions.into_iter().map(|session| session.name).collect();
        let runs = self.list_runs(20).await?;
        let mut reconciled = 0;
        for mut run in runs {
            if run.meta.status != ActionRunStatus::Running {
                continue;
            }
            let job: ActionJob = match read_json(&run.run_dir.join("job.json")).await {
                Ok(job) => job,
                Err(_) => continue,
            };
            let session = job.session;
            if active.contains(&session) {
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
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
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

#[cfg(test)]
mod tests {
    use super::*;
    use wt_platform::process::ProcessRunner;
    use wt_tmux::TmuxServer;

    fn request(cwd: &Path, command: &[&str]) -> ActionRequest {
        ActionRequest {
            action_key: "feature-1".into(),
            slug: "feature-1".into(),
            worktree_ref: None,
            action_id: "test".into(),
            action_name: "Test action".into(),
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
            version: 1,
            slug: request.slug.clone(),
            worktree_ref: None,
            run_id: run_dir.file_name().unwrap().to_string_lossy().into_owned(),
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
        let meta: ActionMeta = read_json(&run_dir.join("meta.json")).await.unwrap();
        assert_eq!(meta.status, ActionRunStatus::Succeeded);
        assert!(run_dir.join("done.json").exists());
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
}
