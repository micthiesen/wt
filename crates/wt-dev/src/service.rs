use std::{
    collections::{BTreeSet, HashSet},
    io,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde_json::Value;
use thiserror::Error;
use tokio::{io::AsyncWriteExt, net::TcpStream};
use tokio_util::sync::CancellationToken;
use wt_config::DevServerConfig as Settings;
use wt_platform::{
    lock::{FileLock, LockError},
    process::{CommandSpec, ProcessError, ProcessRunner},
};
use wt_store::{RepositoryIdentity, Store, StoreError};
use wt_tmux::{
    CreateSession, OptionScope, PaneTarget, TmuxClient, TmuxError, WindowTarget, shell_quote,
};
use wt_vcs::{GitRepository, RepositoryError, WorktreeRecord};

use crate::{
    DevWaiter, QueueReport,
    queue::{self, QueueError},
    supervisor::SupervisorConfig,
};

const PORT_PROBE_TIMEOUT: Duration = Duration::from_millis(400);
const PORT_RELEASE_TIMEOUT: Duration = Duration::from_secs(3);
const READY_POLL: Duration = Duration::from_secs(2);
const STOP_COMMAND_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const HEALTH_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
const SESSION_SUFFIX: &str = "-dev";

#[derive(Clone)]
pub struct DevServerConfig {
    pub main_clone: PathBuf,
    pub dev_dir: PathBuf,
    pub lock_dir: PathBuf,
    pub state: StateConfig,
    pub settings: Settings,
    pub tmux: TmuxClient,
    pub executable: PathBuf,
    pub config_selector: Option<PathBuf>,
    pub home: PathBuf,
}

#[derive(Clone, Debug)]
pub struct StateConfig {
    pub path: PathBuf,
    pub identity: RepositoryIdentity,
}

#[derive(Clone, Debug)]
pub struct DevWorktree {
    pub slug: String,
    pub path: PathBuf,
    pub branch: String,
}

impl From<&WorktreeRecord> for DevWorktree {
    fn from(record: &WorktreeRecord) -> Self {
        Self {
            slug: record.target.slug().to_owned(),
            path: PathBuf::from(&record.target.path),
            branch: record.target.branch.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct DevStartOptions {
    /// Start normally when a slot is free, even if ordinary waiters exist.
    /// Promoted waiters still have priority.
    pub respect_priority: bool,
}

impl Default for DevStartOptions {
    fn default() -> Self {
        Self {
            respect_priority: true,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DevHealth {
    pub ok: bool,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestartStatus {
    pub count: i64,
    pub last_exit: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WaitingStatus {
    pub rank: i64,
    pub since: f64,
}

/// Wire-compatible with the remote JSON and UI board DTO. Health is kept
/// separate because it is an on-demand command that can take up to a minute.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DevServerStatus {
    pub running: bool,
    pub starting: bool,
    pub crashed: bool,
    pub port: Option<u16>,
    pub url: Option<String>,
    pub since: Option<f64>,
    pub waiting: Option<WaitingStatus>,
    pub rebased_since: Option<bool>,
    pub restarts: Option<RestartStatus>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HolderState {
    Up,
    Crashed,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DevSlotHolder {
    pub slug: String,
    pub state: HolderState,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DevSlotReport {
    pub limit: Option<u32>,
    pub free: Option<u32>,
    /// None means tmux inventory could not be read, not an empty fleet.
    pub holders: Option<Vec<DevSlotHolder>>,
    pub waiters: Vec<DevWaiter>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DevSlotDecision {
    pub ok: bool,
    pub limit: Option<u32>,
    pub free: Option<u32>,
    pub holders: Vec<DevSlotHolder>,
    pub yielding_to: Vec<DevWaiter>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DevStartOutcome {
    Started { port: u16, url: String },
    Adopted { port: u16, url: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReadyOutcome {
    Ready { health: Option<DevHealth> },
    Crashed,
    Timeout,
    Unhealthy(DevHealth),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WaitOutcome {
    Acquired,
    TimedOut,
}

#[derive(Debug, Error)]
pub enum DevServerError {
    #[error("development server is not configured")]
    NotConfigured,
    #[error("invalid worktree slug {0:?}")]
    InvalidSlug(String),
    #[error("development server operation cancelled")]
    Cancelled,
    #[error(transparent)]
    Supervisor(#[from] crate::SupervisorError),
    #[error("development server operation lock: {0}")]
    Lock(#[from] LockError),
    #[error("development server queue: {0}")]
    Queue(#[from] QueueError),
    #[error("development server process: {0}")]
    Process(#[from] ProcessError),
    #[error("development server tmux: {0}")]
    Tmux(#[from] TmuxError),
    #[error("development server state: {0}")]
    Store(#[from] StoreError),
    #[error("development server inventory: {0}")]
    Repository(#[from] RepositoryError),
    #[error("development server I/O at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("development server JSON at {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("dev-server slots full ({holders}/{limit}): {slugs}")]
    SlotFull {
        limit: u32,
        holders: u32,
        slugs: String,
        yielding_to: Vec<String>,
    },
    #[error(
        "stop_command failed for {slug}; external resources are unconfirmed, so reset_command was not run"
    )]
    StopBeforeResetFailed { slug: String, output: String },
    #[error("stop_command failed for {slug}; external resources may remain: {output}")]
    StopCommandFailed { slug: String, output: String },
    #[error("reset_command failed for {slug}: {output}")]
    ResetCommandFailed { slug: String, output: String },
    #[error("no free dev-server port in the configured range")]
    NoPort,
    #[error("tmux session {0} did not become available")]
    SessionUnavailable(String),
    #[error("dev-server status failed for {slug}: {message}")]
    StatusRow { slug: String, message: String },
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DevStatusRow {
    pub slug: String,
    pub status: Option<DevServerStatus>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DevStatusSnapshot {
    pub worktrees: Vec<DevStatusRow>,
    pub slots: DevSlotReport,
}

#[derive(Clone)]
pub struct DevServerService {
    config: DevServerConfig,
    repository: GitRepository,
    processes: ProcessRunner,
}

impl DevServerService {
    pub fn new(
        config: DevServerConfig,
        repository: GitRepository,
        processes: ProcessRunner,
    ) -> Self {
        Self {
            config,
            repository,
            processes,
        }
    }

    pub fn config(&self) -> &DevServerConfig {
        &self.config
    }

    pub async fn start(
        &self,
        worktree: &DevWorktree,
        options: DevStartOptions,
        cancellation: &CancellationToken,
    ) -> Result<DevStartOutcome, DevServerError> {
        let _worktree_lock = FileLock::acquire(
            &self.config.lock_dir,
            &worktree.slug,
            "start dev server",
            cancellation,
        )
        .await?;
        self.start_locked(worktree, options, cancellation, false)
            .await
    }

    async fn start_locked(
        &self,
        worktree: &DevWorktree,
        options: DevStartOptions,
        cancellation: &CancellationToken,
        slot_lock_held: bool,
    ) -> Result<DevStartOutcome, DevServerError> {
        validate_slug(&worktree.slug)?;
        let before = self
            .status(&worktree.slug, &worktree.path, cancellation)
            .await?;
        if before.starting
            && let Some(port) = before.port
        {
            return Ok(DevStartOutcome::Adopted {
                port,
                url: self.url(port),
            });
        }

        let _slots_lock = if self.config.settings.max_concurrent.is_some() && !slot_lock_held {
            Some(
                FileLock::acquire(
                    &self.config.lock_dir,
                    "dev-slots",
                    "allocate dev-server slot",
                    cancellation,
                )
                .await?,
            )
        } else {
            None
        };
        if let Some(limit) = self.config.settings.max_concurrent {
            let mut report = self.slot_report(cancellation).await?;
            let holders = report
                .holders
                .clone()
                .ok_or_else(|| DevServerError::SlotFull {
                    limit,
                    holders: 0,
                    slugs: "tmux session inventory unavailable".into(),
                    yielding_to: Vec::new(),
                })?;
            if !decide_slot(
                &worktree.slug,
                &holders,
                Some(limit),
                &report.waiters,
                options.respect_priority,
            )
            .ok
            {
                self.reclaim_orphan_sessions(&holders, cancellation).await?;
                report = self.slot_report(cancellation).await?;
            }
            let holders = report.holders.ok_or_else(|| DevServerError::SlotFull {
                limit,
                holders: 0,
                slugs: "tmux session inventory unavailable".into(),
                yielding_to: Vec::new(),
            })?;
            let decision = decide_slot(
                &worktree.slug,
                &holders,
                Some(limit),
                &report.waiters,
                options.respect_priority,
            );
            if !decision.ok {
                return Err(DevServerError::SlotFull {
                    limit,
                    holders: decision.holders.len() as u32,
                    slugs: decision
                        .holders
                        .iter()
                        .map(|holder| holder.slug.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                    yielding_to: decision
                        .yielding_to
                        .iter()
                        .map(|waiter| waiter.slug.clone())
                        .collect(),
                });
            }
        }

        let session = session_name(&worktree.slug);
        self.config
            .tmux
            .kill_session(&session, cancellation)
            .await?;
        if let Some(old_port) = self.read_port(&worktree.slug).await? {
            let deadline = tokio::time::Instant::now() + PORT_RELEASE_TIMEOUT;
            while tokio::time::Instant::now() < deadline
                && probe_port(old_port).await != PortProbe::Free
            {
                tokio::select! {
                    _ = cancellation.cancelled() => return Err(DevServerError::Cancelled),
                    _ = tokio::time::sleep(Duration::from_millis(150)) => {}
                }
            }
        }
        let port = self.allocate_port(&worktree.slug, cancellation).await?;
        let sha = self
            .git_text(&worktree.path, ["rev-parse", "HEAD"], cancellation)
            .await?;
        let mut command = vec![
            "env".to_owned(),
            format!("HOME={}", self.config.home.display()),
            "WT_AGENT=".to_owned(),
        ];
        if let Some(config_selector) = &self.config.config_selector {
            command.push(format!("WT_REPO_CONFIG={}", config_selector.display()));
        }
        command.extend([
            self.config.executable.to_string_lossy().into_owned(),
            "_dev-supervise".to_owned(),
            "--slug".to_owned(),
            worktree.slug.clone(),
            "--path".to_owned(),
            worktree.path.to_string_lossy().into_owned(),
            "--port".to_owned(),
            port.to_string(),
        ]);
        let launch = command
            .iter()
            .map(|argument| shell_quote(argument))
            .collect::<Vec<_>>()
            .join(" ");
        self.config
            .tmux
            .create_session(
                &CreateSession {
                    name: session.clone(),
                    cwd: worktree.path.clone(),
                    command: Vec::new(),
                    width: None,
                    height: None,
                },
                cancellation,
            )
            .await?;
        let pane = PaneTarget::active_session_pane(&session);
        let launch_result = async {
            self.config
                .tmux
                .set_option(
                    &OptionScope::Window(WindowTarget::active_session_window(&session)),
                    "remain-on-exit",
                    Some("on"),
                    cancellation,
                )
                .await?;
            self.config
                .tmux
                .send_literal(&pane, &launch, cancellation)
                .await?;
            self.config
                .tmux
                .send_keys(&pane, &["Enter"], cancellation)
                .await
        }
        .await;
        if let Err(error) = launch_result {
            let _ = self.config.tmux.kill_session(&session, cancellation).await;
            return Err(error.into());
        }
        if let Err(error) = self.write_marker(&worktree.slug, "running").await {
            let _ = self.config.tmux.kill_session(&session, cancellation).await;
            return Err(error);
        }
        if let Err(error) = self.write_started_sha(&worktree.slug, &sha).await {
            let _ = self.config.tmux.kill_session(&session, cancellation).await;
            let _ = self.write_marker(&worktree.slug, "stopped").await;
            return Err(error);
        }
        Ok(DevStartOutcome::Started {
            port,
            url: self.url(port),
        })
    }

    pub async fn stop(
        &self,
        worktree: &DevWorktree,
        cancellation: &CancellationToken,
    ) -> Result<(), DevServerError> {
        let _lock = FileLock::acquire(
            &self.config.lock_dir,
            &worktree.slug,
            "stop dev server",
            cancellation,
        )
        .await?;
        let outcome = self.stop_locked(worktree, cancellation).await?;
        if !outcome.success {
            return Err(DevServerError::StopCommandFailed {
                slug: worktree.slug.clone(),
                output: outcome.output,
            });
        }
        Ok(())
    }

    /// Stop helper for a lifecycle operation that already owns `<slug>.lock`.
    /// Passing that lock documents the lock-order contract and avoids a
    /// self-deadlock when `wt rm` tears down a dev server before removing a slug.
    pub async fn stop_under_lifecycle_lock(
        &self,
        worktree: &DevWorktree,
        _lifecycle_lock: &FileLock,
        cancellation: &CancellationToken,
    ) -> Result<(), DevServerError> {
        let outcome = self.stop_locked(worktree, cancellation).await?;
        if !outcome.success {
            return Err(DevServerError::StopCommandFailed {
                slug: worktree.slug.clone(),
                output: outcome.output,
            });
        }
        Ok(())
    }

    async fn stop_locked(
        &self,
        worktree: &DevWorktree,
        cancellation: &CancellationToken,
    ) -> Result<HookOutcome, DevServerError> {
        let session = session_name(&worktree.slug);
        self.config
            .tmux
            .kill_session(&session, cancellation)
            .await?;
        let _ = self.write_marker(&worktree.slug, "stopped").await;
        self.run_hook(
            "stop_command",
            self.config.settings.stop_command.as_deref(),
            &worktree.slug,
            &worktree.path,
            self.read_port(&worktree.slug).await?,
            cancellation,
        )
        .await
    }

    pub async fn reset(
        &self,
        worktree: &DevWorktree,
        cancellation: &CancellationToken,
    ) -> Result<DevStartOutcome, DevServerError> {
        let _lock = FileLock::acquire(
            &self.config.lock_dir,
            &worktree.slug,
            "reset dev server",
            cancellation,
        )
        .await?;
        let _slots_lock = if let Some(limit) = self.config.settings.max_concurrent {
            let lock = FileLock::acquire(
                &self.config.lock_dir,
                "dev-slots",
                "reserve dev-server slot for reset",
                cancellation,
            )
            .await?;
            let report = self.slot_report(cancellation).await?;
            let holders = report.holders.ok_or_else(|| DevServerError::SlotFull {
                limit,
                holders: 0,
                slugs: "tmux session inventory unavailable".into(),
                yielding_to: Vec::new(),
            })?;
            let decision =
                decide_slot(&worktree.slug, &holders, Some(limit), &report.waiters, true);
            if !decision.ok {
                return Err(DevServerError::SlotFull {
                    limit,
                    holders: decision.holders.len() as u32,
                    slugs: decision
                        .holders
                        .iter()
                        .map(|holder| holder.slug.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                    yielding_to: decision
                        .yielding_to
                        .into_iter()
                        .map(|waiter| waiter.slug)
                        .collect(),
                });
            }
            Some(lock)
        } else {
            None
        };
        let stopped = self.stop_locked(worktree, cancellation).await?;
        if !stopped.success {
            return Err(DevServerError::StopBeforeResetFailed {
                slug: worktree.slug.clone(),
                output: stopped.output,
            });
        }
        let port = self.read_port(&worktree.slug).await?;
        let reset = self
            .run_hook(
                "reset_command",
                self.config.settings.reset_command.as_deref(),
                &worktree.slug,
                &worktree.path,
                port,
                cancellation,
            )
            .await?;
        if !reset.success {
            return Err(DevServerError::ResetCommandFailed {
                slug: worktree.slug.clone(),
                output: reset.output,
            });
        }
        self.start_locked(worktree, DevStartOptions::default(), cancellation, true)
            .await
    }

    pub async fn status(
        &self,
        slug: &str,
        path: &Path,
        cancellation: &CancellationToken,
    ) -> Result<DevServerStatus, DevServerError> {
        let snapshot = self
            .status_all(
                &[DevWorktree {
                    slug: slug.to_owned(),
                    path: path.to_path_buf(),
                    branch: String::new(),
                }],
                cancellation,
            )
            .await?;
        let row = snapshot
            .worktrees
            .into_iter()
            .next()
            .expect("one requested status row");
        row.status.ok_or_else(|| DevServerError::StatusRow {
            slug: row.slug,
            message: row.error.unwrap_or_else(|| "unknown status error".into()),
        })
    }

    /// Read a fleet status snapshot with one read-only state connection, one
    /// tmux inventory and one queue scan. Per-row filesystem/git failures are
    /// returned alongside healthy rows so a missing checkout cannot hide the
    /// rest of the fleet. Row work is bounded to eight concurrent operations.
    pub async fn status_all(
        &self,
        worktrees: &[DevWorktree],
        cancellation: &CancellationToken,
    ) -> Result<DevStatusSnapshot, DevServerError> {
        if worktrees.is_empty() {
            let report = self.slot_report(cancellation).await?;
            return Ok(DevStatusSnapshot {
                worktrees: Vec::new(),
                slots: report,
            });
        }
        let state_config = self.config.state.clone();
        let state_path = state_config.path.clone();
        let state = tokio::task::spawn_blocking(move || {
            let mut store = Store::open_read_only(state_config.path, state_config.identity)?;
            store.read_wt_state()
        })
        .await
        .map_err(|error| DevServerError::Io {
            path: state_path,
            source: io::Error::other(error.to_string()),
        })??;
        let session_names = self
            .config
            .tmux
            .list_sessions(cancellation)
            .await?
            .into_iter()
            .map(|session| session.name)
            .collect::<Vec<_>>();
        let sessions = session_names.iter().cloned().collect::<HashSet<_>>();
        let waiters = queue::list(&self.config.dev_dir, &self.config.lock_dir, cancellation)
            .await?
            .waiters;
        let capacity = 8_usize;
        let mut next = 0;
        let mut tasks = tokio::task::JoinSet::new();
        let mut rows: Vec<Option<DevStatusRow>> = Vec::with_capacity(worktrees.len());
        while next < worktrees.len() || !tasks.is_empty() {
            while next < worktrees.len() && tasks.len() < capacity {
                let worktree = worktrees[next].clone();
                validate_slug(&worktree.slug)?;
                let row_state = state
                    .get("slugs")
                    .and_then(|slugs| slugs.get(&worktree.slug))
                    .cloned()
                    .unwrap_or(Value::Null);
                let has_session = sessions.contains(&session_name(&worktree.slug));
                let queue = waiters.clone();
                let service = self.clone();
                let cancellation = cancellation.clone();
                let index = next;
                tasks.spawn(async move {
                    let result = service
                        .status_from_snapshot(
                            &worktree.slug,
                            &worktree.path,
                            &row_state,
                            has_session,
                            &queue,
                            &cancellation,
                        )
                        .await;
                    (index, worktree.slug, result)
                });
                next += 1;
            }
            let Some(result) = tasks.join_next().await else {
                break;
            };
            let (index, slug, result) = result.map_err(|error| DevServerError::Io {
                path: self.config.dev_dir.clone(),
                source: io::Error::other(error.to_string()),
            })?;
            if rows.len() <= index {
                rows.resize_with(index + 1, || None);
            }
            rows[index] = Some(match result {
                Ok(status) => DevStatusRow {
                    slug,
                    status: Some(status),
                    error: None,
                },
                Err(error) => DevStatusRow {
                    slug,
                    status: None,
                    error: Some(error.to_string()),
                },
            });
        }
        let mut holders = Vec::new();
        for name in session_names {
            if let Some(slug) = name.strip_suffix(SESSION_SUFFIX) {
                let state = if self.read_marker(slug).await?.as_deref() == Some("crashed") {
                    HolderState::Crashed
                } else {
                    HolderState::Up
                };
                holders.push(DevSlotHolder {
                    slug: slug.to_owned(),
                    state,
                });
            }
        }
        holders.sort_by(|a, b| a.slug.cmp(&b.slug));
        let limit = self.config.settings.max_concurrent;
        let free = limit.map(|limit| limit.saturating_sub(holders.len() as u32));
        Ok(DevStatusSnapshot {
            worktrees: rows.into_iter().flatten().collect(),
            slots: DevSlotReport {
                limit,
                free,
                holders: Some(holders),
                waiters,
            },
        })
    }

    async fn status_from_snapshot(
        &self,
        slug: &str,
        path: &Path,
        state: &Value,
        session_exists: bool,
        waiters: &[DevWaiter],
        cancellation: &CancellationToken,
    ) -> Result<DevServerStatus, DevServerError> {
        let port = state
            .get("devPort")
            .and_then(Value::as_u64)
            .and_then(|port| u16::try_from(port).ok());
        let marker = self.read_marker(slug).await?;
        let attempts = self.read_attempts(slug).await?;
        let rank = waiters.iter().position(|waiter| waiter.slug == slug);
        let waiting = rank.map(|rank| WaitingStatus {
            rank: rank as i64,
            since: waiters[rank].since as f64,
        });
        let since = self.marker_mtime(slug).await?;
        let started_sha = state.get("devStartedSha").and_then(Value::as_str);
        // A stopped server has no running environment that can be stale. Avoid
        // a Git subprocess per idle worktree on every fleet snapshot.
        let rebased_since = if session_exists && let Some(started_sha) = started_sha {
            let output = self
                .processes
                .run(
                    CommandSpec::new("git")
                        .args(["merge-base", "--is-ancestor", started_sha, "HEAD"])
                        .cwd(path),
                    cancellation,
                )
                .await?;
            if output.status.success() {
                Some(false)
            } else if output.status.code() == Some(1) {
                Some(true)
            } else {
                None
            }
        } else {
            None
        };
        let base = DevServerStatus {
            running: false,
            starting: false,
            crashed: false,
            port,
            url: None,
            since,
            waiting,
            rebased_since,
            restarts: attempts,
        };
        if !session_exists {
            return Ok(if marker.as_deref() == Some("crashed") {
                DevServerStatus {
                    crashed: true,
                    ..base
                }
            } else {
                base
            });
        }
        let probe = match port {
            Some(port) => probe_port(port).await,
            None => PortProbe::Free,
        };
        if probe == PortProbe::Listening
            || (probe == PortProbe::Unknown && marker.as_deref() == Some("running"))
        {
            let port = port.ok_or(DevServerError::SessionUnavailable(slug.to_owned()))?;
            return Ok(DevServerStatus {
                running: true,
                url: Some(self.url(port)),
                ..base
            });
        }
        if marker.as_deref() == Some("crashed") {
            return Ok(DevServerStatus {
                crashed: true,
                ..base
            });
        }
        Ok(DevServerStatus {
            starting: marker.as_deref() == Some("running"),
            ..base
        })
    }

    pub async fn health(
        &self,
        worktree: &DevWorktree,
        cancellation: &CancellationToken,
    ) -> Result<Option<DevHealth>, DevServerError> {
        let Some(template) = self.config.settings.health_command.as_deref() else {
            return Ok(None);
        };
        let port = self.read_port(&worktree.slug).await?;
        let command = substitute(template, &worktree.slug, &worktree.path, port);
        let shell = std::env::var_os("SHELL").unwrap_or_else(|| "/bin/bash".into());
        let mut spec = CommandSpec::new(shell)
            .args(["-lc", command.as_str()])
            .cwd(&worktree.path);
        spec.timeout = HEALTH_COMMAND_TIMEOUT;
        if let Some(port) = port {
            spec.env
                .push(("PORT".into(), Some(port.to_string().into())));
        }
        let output = self.processes.run(spec, cancellation).await?;
        let message = first_nonempty(&output.stdout_text())
            .or_else(|| first_nonempty(&output.stderr_text()))
            .unwrap_or_else(|| {
                if output.status.success() {
                    "healthy".into()
                } else {
                    format!(
                        "health_command exited {}",
                        output
                            .status
                            .code()
                            .map_or("unknown".into(), |code| code.to_string())
                    )
                }
            });
        Ok(Some(DevHealth {
            ok: output.status.success(),
            message,
        }))
    }

    pub async fn wait_ready(
        &self,
        worktree: &DevWorktree,
        timeout: Duration,
        cancellation: &CancellationToken,
    ) -> Result<ReadyOutcome, DevServerError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if cancellation.is_cancelled() {
                return Err(DevServerError::Cancelled);
            }
            let marker = self.read_marker(&worktree.slug).await?;
            let session_exists = self
                .config
                .tmux
                .session_exists(&session_name(&worktree.slug), cancellation)
                .await?;
            if marker.as_deref() == Some("crashed") || !session_exists {
                return Ok(ReadyOutcome::Crashed);
            }
            if let Some(port) = self.read_port(&worktree.slug).await?
                && probe_port(port).await == PortProbe::Listening
            {
                return Ok(ReadyOutcome::Ready { health: None });
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(ReadyOutcome::Timeout);
            }
            tokio::select! {
                _ = cancellation.cancelled() => return Err(DevServerError::Cancelled),
                _ = tokio::time::sleep(READY_POLL) => {}
            }
        }
    }

    pub async fn wait_for_slot(
        &self,
        slug: &str,
        timeout: Duration,
        cancellation: &CancellationToken,
        mut on_wait: impl FnMut(usize),
    ) -> Result<WaitOutcome, DevServerError> {
        validate_slug(slug)?;
        let _waiter = queue::join(
            &self.config.dev_dir,
            &self.config.lock_dir,
            slug,
            cancellation,
        )
        .await?;
        let deadline = tokio::time::Instant::now() + timeout;
        let result = async {
            loop {
                if cancellation.is_cancelled() {
                    return Err(DevServerError::Cancelled);
                }
                let report = self.slot_report(cancellation).await?;
                let holders = report.holders.ok_or_else(|| DevServerError::SlotFull {
                    limit: report.limit.unwrap_or(1),
                    holders: 0,
                    slugs: "tmux session inventory unavailable".into(),
                    yielding_to: Vec::new(),
                })?;
                let waiters =
                    queue::list(&self.config.dev_dir, &self.config.lock_dir, cancellation)
                        .await?
                        .waiters;
                let rank = waiters
                    .iter()
                    .position(|waiter| waiter.slug == slug)
                    .unwrap_or(0);
                let decision = decide_slot(slug, &holders, report.limit, &waiters, true);
                if decision.ok
                    && (decision.free.is_none()
                        || (rank as u32) < decision.free.unwrap_or(u32::MAX))
                {
                    return Ok(WaitOutcome::Acquired);
                }
                if tokio::time::Instant::now() >= deadline {
                    return Ok(WaitOutcome::TimedOut);
                }
                on_wait(rank);
                tokio::select! {
                    _ = cancellation.cancelled() => return Err(DevServerError::Cancelled),
                    _ = tokio::time::sleep(Duration::from_secs(3)) => {}
                }
            }
        }
        .await;
        match result {
            Ok(WaitOutcome::Acquired) => Ok(WaitOutcome::Acquired),
            other => {
                queue::leave(&self.config.dev_dir, &self.config.lock_dir, slug).await?;
                other
            }
        }
    }

    pub async fn clear_waiter(&self, slug: &str) -> Result<(), DevServerError> {
        queue::leave(&self.config.dev_dir, &self.config.lock_dir, slug).await?;
        Ok(())
    }

    pub async fn queue_report(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<QueueReport, DevServerError> {
        Ok(queue::list(&self.config.dev_dir, &self.config.lock_dir, cancellation).await?)
    }

    pub async fn set_waiter_priority(
        &self,
        slug: &str,
        priority: i32,
        cancellation: &CancellationToken,
    ) -> Result<Option<DevWaiter>, DevServerError> {
        Ok(queue::set_priority(
            &self.config.dev_dir,
            &self.config.lock_dir,
            slug,
            priority,
            cancellation,
        )
        .await?)
    }

    pub async fn slot_report(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<DevSlotReport, DevServerError> {
        let sessions = self.config.tmux.list_sessions(cancellation).await?;
        let mut holders = Vec::new();
        for session in sessions {
            if let Some(slug) = session.name.strip_suffix(SESSION_SUFFIX) {
                let state = if self.read_marker(slug).await?.as_deref() == Some("crashed") {
                    HolderState::Crashed
                } else {
                    HolderState::Up
                };
                holders.push(DevSlotHolder {
                    slug: slug.to_owned(),
                    state,
                });
            }
        }
        holders.sort_by(|a, b| a.slug.cmp(&b.slug));
        let limit = self.config.settings.max_concurrent;
        let free = limit.map(|limit| limit.saturating_sub(holders.len() as u32));
        let waiters = queue::list(&self.config.dev_dir, &self.config.lock_dir, cancellation)
            .await?
            .waiters;
        Ok(DevSlotReport {
            limit,
            free,
            holders: Some(holders),
            waiters,
        })
    }

    pub async fn logs(
        &self,
        slug: &str,
        lines: u32,
        cancellation: &CancellationToken,
    ) -> Result<Option<String>, DevServerError> {
        validate_slug(slug)?;
        let session = session_name(slug);
        if self
            .config
            .tmux
            .session_exists(&session, cancellation)
            .await?
        {
            return Ok(Some(
                self.config
                    .tmux
                    .capture_pane(
                        &PaneTarget::active_session_pane(&session),
                        Some(lines),
                        cancellation,
                    )
                    .await?,
            ));
        }
        let path = self.config.dev_dir.join(format!("{slug}.crash.log"));
        match tokio::fs::read_to_string(&path).await {
            Ok(text) => Ok(Some(text)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(DevServerError::Io { path, source }),
        }
    }

    pub async fn run_supervisor(
        &self,
        worktree: &DevWorktree,
        port: u16,
        cancellation: CancellationToken,
    ) -> Result<crate::SupervisorExit, DevServerError> {
        crate::run_supervisor(
            SupervisorConfig {
                slug: worktree.slug.clone(),
                path: worktree.path.clone(),
                command: self
                    .config
                    .settings
                    .command
                    .replace("{{port}}", &port.to_string()),
                stop_command: self.config.settings.stop_command.clone(),
                port,
                dev_dir: self.config.dev_dir.clone(),
                tmux: self.config.tmux.clone(),
                processes: self.processes.clone(),
                shell: std::env::var_os("SHELL")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("/bin/bash")),
            },
            cancellation,
        )
        .await
        .map_err(DevServerError::from)
    }

    async fn reclaim_orphan_sessions(
        &self,
        holders: &[DevSlotHolder],
        cancellation: &CancellationToken,
    ) -> Result<(), DevServerError> {
        let live_rows = self.repository.inventory(cancellation).await?;
        let live = live_rows
            .into_iter()
            .map(|row| row.target.slug().to_owned())
            .collect::<BTreeSet<_>>();
        for holder in holders {
            if live.contains(&holder.slug) {
                continue;
            }
            let session = session_name(&holder.slug);
            if holder.state == HolderState::Crashed
                && let Ok(log) = self
                    .config
                    .tmux
                    .capture_pane(
                        &PaneTarget::active_session_pane(&session),
                        Some(2000),
                        cancellation,
                    )
                    .await
            {
                self.write_crash_log(&holder.slug, &log).await?;
            }
            self.config
                .tmux
                .kill_session(&session, cancellation)
                .await?;
            let _ = self
                .run_hook(
                    "stop_command",
                    self.config.settings.stop_command.as_deref(),
                    &holder.slug,
                    &self.config.main_clone,
                    self.read_port(&holder.slug).await?,
                    cancellation,
                )
                .await?;
        }
        Ok(())
    }

    async fn allocate_port(
        &self,
        slug: &str,
        cancellation: &CancellationToken,
    ) -> Result<u16, DevServerError> {
        let mut candidates = Vec::new();
        let state = self.read_state(slug).await?;
        let recorded = state
            .get("devPort")
            .and_then(Value::as_u64)
            .and_then(|value| u16::try_from(value).ok());
        let range_start = u32::from(self.config.settings.port_base);
        let range_end = range_start
            .saturating_add(self.config.settings.port_range)
            .min(u16::MAX as u32 + 1);
        if let Some(recorded) =
            recorded.filter(|port| u32::from(*port) >= range_start && u32::from(*port) < range_end)
            && probe_port(recorded).await == PortProbe::Free
        {
            candidates.push(recorded);
        }
        for value in range_start..range_end {
            let port = value as u16;
            if !candidates.contains(&port) && probe_port(port).await == PortProbe::Free {
                candidates.push(port);
                if candidates.len() >= 8 {
                    break;
                }
            }
            if cancellation.is_cancelled() {
                return Err(DevServerError::Cancelled);
            }
        }
        if cancellation.is_cancelled() {
            return Err(DevServerError::Cancelled);
        }
        let owned_slug = slug.to_owned();
        let port = self
            .mutate_store(move |store| store.claim_dev_port(&owned_slug, &candidates))
            .await?
            .ok_or(DevServerError::NoPort)?;
        Ok(port)
    }

    async fn read_port(&self, slug: &str) -> Result<Option<u16>, DevServerError> {
        let state = self.read_state(slug).await?;
        Ok(state
            .get("devPort")
            .and_then(Value::as_u64)
            .and_then(|port| u16::try_from(port).ok()))
    }

    async fn read_state(&self, slug: &str) -> Result<Value, DevServerError> {
        let config = self.config.state.clone();
        let slug = slug.to_owned();
        tokio::task::spawn_blocking(move || {
            let mut store = Store::open(config.path, config.identity)?;
            Ok::<_, StoreError>(store.read_slug_state(&slug)?.unwrap_or(Value::Null))
        })
        .await
        .map_err(|error| DevServerError::Io {
            path: self.config.state.path.clone(),
            source: io::Error::other(error.to_string()),
        })?
        .map_err(DevServerError::Store)
    }

    async fn mutate_store<R: Send + 'static>(
        &self,
        mutate: impl FnOnce(&mut Store) -> Result<R, StoreError> + Send + 'static,
    ) -> Result<R, DevServerError> {
        let config = self.config.state.clone();
        tokio::task::spawn_blocking(move || {
            let mut store = Store::open(config.path, config.identity)?;
            mutate(&mut store)
        })
        .await
        .map_err(|error| DevServerError::Io {
            path: self.config.state.path.clone(),
            source: io::Error::other(error.to_string()),
        })?
        .map_err(DevServerError::Store)
    }

    async fn write_started_sha(&self, slug: &str, sha: &str) -> Result<(), DevServerError> {
        let slug = slug.to_owned();
        let sha = sha.to_owned();
        self.mutate_store(move |store| store.set_slug_dev_started_sha(&slug, Some(&sha)))
            .await
    }

    async fn write_marker(&self, slug: &str, marker: &str) -> Result<(), DevServerError> {
        let path = self.config.dev_dir.join(format!("{slug}.state"));
        tokio::fs::create_dir_all(&self.config.dev_dir)
            .await
            .map_err(|source| DevServerError::Io {
                path: self.config.dev_dir.clone(),
                source,
            })?;
        tokio::fs::write(&path, marker)
            .await
            .map_err(|source| DevServerError::Io { path, source })
    }

    async fn read_marker(&self, slug: &str) -> Result<Option<String>, DevServerError> {
        let path = self.config.dev_dir.join(format!("{slug}.state"));
        match tokio::fs::read_to_string(&path).await {
            Ok(value) => Ok(match value.trim() {
                "running" | "stopped" | "crashed" => Some(value.trim().to_owned()),
                _ => None,
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(DevServerError::Io { path, source }),
        }
    }

    async fn read_attempts(&self, slug: &str) -> Result<Option<RestartStatus>, DevServerError> {
        let path = self.config.dev_dir.join(format!("{slug}.state.attempts"));
        match tokio::fs::read_to_string(&path).await {
            Ok(value) => {
                let mut parts = value.split_whitespace();
                let count = parts.next().and_then(|part| part.parse().ok()).unwrap_or(0);
                let last_exit = parts
                    .next()
                    .and_then(|part| part.parse().ok())
                    .unwrap_or(-1);
                Ok((count > 0).then_some(RestartStatus { count, last_exit }))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(DevServerError::Io { path, source }),
        }
    }

    async fn marker_mtime(&self, slug: &str) -> Result<Option<f64>, DevServerError> {
        let path = self.config.dev_dir.join(format!("{slug}.state"));
        match tokio::fs::metadata(&path).await {
            Ok(metadata) => {
                let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                Ok(Some(
                    modified
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as f64,
                ))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(DevServerError::Io { path, source }),
        }
    }

    async fn write_crash_log(&self, slug: &str, text: &str) -> Result<(), DevServerError> {
        let path = self.config.dev_dir.join(format!("{slug}.crash.log"));
        tokio::fs::create_dir_all(&self.config.dev_dir)
            .await
            .map_err(|source| DevServerError::Io {
                path: self.config.dev_dir.clone(),
                source,
            })?;
        let clipped = text
            .chars()
            .rev()
            .take(100_000)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>();
        tokio::fs::write(&path, clipped)
            .await
            .map_err(|source| DevServerError::Io { path, source })
    }

    async fn run_hook(
        &self,
        label: &'static str,
        template: Option<&str>,
        slug: &str,
        path: &Path,
        port: Option<u16>,
        cancellation: &CancellationToken,
    ) -> Result<HookOutcome, DevServerError> {
        let Some(template) = template else {
            return Ok(HookOutcome {
                success: true,
                output: String::new(),
            });
        };
        let command = substitute(template, slug, path, port);
        let shell = std::env::var_os("SHELL").unwrap_or_else(|| "/bin/bash".into());
        let mut spec = CommandSpec::new(shell)
            .args(["-lc", command.as_str()])
            .cwd(path);
        spec.timeout = STOP_COMMAND_TIMEOUT;
        if let Some(port) = port {
            spec.env
                .push(("PORT".into(), Some(port.to_string().into())));
        }
        let output = self.processes.run(spec, cancellation).await?;
        if output.status.success() {
            return Ok(HookOutcome {
                success: true,
                output: String::new(),
            });
        }
        let detail = [output.stdout_text(), output.stderr_text()]
            .into_iter()
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        tracing::warn!(slug, hook = label, %detail, "development-server teardown hook failed");
        Ok(HookOutcome {
            success: false,
            output: detail,
        })
    }

    async fn git_text<const N: usize>(
        &self,
        cwd: &Path,
        args: [&str; N],
        cancellation: &CancellationToken,
    ) -> Result<String, DevServerError> {
        let output = self
            .processes
            .run(CommandSpec::new("git").args(args).cwd(cwd), cancellation)
            .await?;
        if !output.status.success() {
            return Err(DevServerError::Process(ProcessError::Exit {
                program: "git".into(),
                code: output.status.code(),
                stdout: output.stdout_text(),
                stderr: output.stderr_text(),
            }));
        }
        Ok(output.stdout_text().trim().to_owned())
    }

    fn url(&self, port: u16) -> String {
        self.config
            .settings
            .url
            .replace("{{port}}", &port.to_string())
    }
}

#[derive(Debug)]
struct HookOutcome {
    success: bool,
    output: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PortProbe {
    Listening,
    Free,
    Unknown,
}

async fn probe_port(port: u16) -> PortProbe {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    match tokio::time::timeout(PORT_PROBE_TIMEOUT, TcpStream::connect(address)).await {
        Ok(Ok(mut stream)) => {
            let _ = stream.shutdown().await;
            PortProbe::Listening
        }
        Ok(Err(error))
            if error.kind() == io::ErrorKind::ConnectionRefused
                || error.kind() == io::ErrorKind::AddrNotAvailable =>
        {
            PortProbe::Free
        }
        Ok(Err(_)) | Err(_) => PortProbe::Unknown,
    }
}

pub fn decide_slot(
    slug: &str,
    holders: &[DevSlotHolder],
    limit: Option<u32>,
    waiters: &[DevWaiter],
    respect_priority: bool,
) -> DevSlotDecision {
    let holders = holders
        .iter()
        .filter(|holder| holder.slug != slug)
        .cloned()
        .collect::<Vec<_>>();
    let free = limit.map(|limit| limit.saturating_sub(holders.len() as u32));
    let mut yielding_to = Vec::new();
    if respect_priority && free.is_some_and(|free| free > 0) {
        let promoted = waiters
            .iter()
            .filter(|waiter| waiter.priority > 0 && waiter.slug != slug)
            .cloned()
            .collect::<Vec<_>>();
        let free_count = free.unwrap_or_default() as usize;
        let earlier_promoted = waiters
            .iter()
            .take_while(|waiter| waiter.slug != slug)
            .filter(|waiter| waiter.priority > 0)
            .count();
        // Only the earliest promoted waiters reserve currently free slots.
        // A promoted waiter within that reservation may claim its own slot;
        // later promoted waiters cannot mutually yield to one another.
        let candidate_promoted = waiters
            .iter()
            .any(|waiter| waiter.slug == slug && waiter.priority > 0);
        let should_yield = if candidate_promoted {
            earlier_promoted >= free_count
        } else {
            promoted.len() >= free_count
        };
        if should_yield {
            yielding_to.extend(promoted.into_iter().take(free_count));
        }
    }
    DevSlotDecision {
        ok: free.is_none() || (free.unwrap_or_default() > 0 && yielding_to.is_empty()),
        limit,
        free,
        holders,
        yielding_to,
    }
}

pub(crate) fn session_name(slug: &str) -> String {
    format!("{slug}{SESSION_SUFFIX}")
}

pub(crate) fn validate_slug(slug: &str) -> Result<(), DevServerError> {
    if slug.is_empty()
        || slug == "."
        || slug == ".."
        || slug.contains('/')
        || slug.contains('\\')
        || slug.contains(':')
        || slug.chars().any(char::is_control)
    {
        return Err(DevServerError::InvalidSlug(slug.to_owned()));
    }
    Ok(())
}

fn substitute(command: &str, slug: &str, path: &Path, port: Option<u16>) -> String {
    command
        .replace("{{path}}", &path.to_string_lossy())
        .replace("{{slug}}", slug)
        .replace(
            "{{port}}",
            &port.map_or_else(String::new, |port| port.to_string()),
        )
}

fn first_nonempty(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn holder(slug: &str) -> DevSlotHolder {
        DevSlotHolder {
            slug: slug.into(),
            state: HolderState::Up,
        }
    }

    fn waiter(slug: &str, priority: i32, since: u64) -> DevWaiter {
        DevWaiter {
            slug: slug.into(),
            pid: 1,
            since,
            priority,
        }
    }

    #[test]
    fn restarting_own_slot_does_not_count_it_twice() {
        let decision = decide_slot("one", &[holder("one")], Some(1), &[], true);
        assert!(decision.ok);
        assert_eq!(decision.free, Some(1));
        assert!(decision.holders.is_empty());
    }

    #[test]
    fn promoted_waiter_reserves_free_slots_from_unqueued_starts() {
        let promoted = vec![waiter("urgent", 1, 1)];
        let regular_start = decide_slot("new", &[], Some(1), &promoted, true);
        assert!(!regular_start.ok);
        assert_eq!(regular_start.yielding_to[0].slug, "urgent");
        let urgent_start = decide_slot("urgent", &[], Some(1), &promoted, true);
        assert!(urgent_start.ok);
    }

    #[test]
    fn promoted_waiters_reserve_distinct_slots_without_yielding_to_each_other() {
        let promoted = vec![waiter("urgent-a", 1, 1), waiter("urgent-b", 1, 2)];
        let first = decide_slot("urgent-a", &[], Some(2), &promoted, true);
        let second = decide_slot("urgent-b", &[], Some(2), &promoted, true);
        assert!(first.ok);
        assert!(second.ok);

        let overbooked = vec![
            waiter("urgent-a", 1, 1),
            waiter("urgent-b", 1, 2),
            waiter("urgent-c", 1, 3),
        ];
        assert!(decide_slot("urgent-a", &[], Some(2), &overbooked, true).ok);
        assert!(decide_slot("urgent-b", &[], Some(2), &overbooked, true).ok);
        assert!(!decide_slot("urgent-c", &[], Some(2), &overbooked, true).ok);
        assert!(!decide_slot("new", &[], Some(2), &overbooked, true).ok);

        let one_free = vec![waiter("urgent-a", 1, 1), waiter("urgent-b", 1, 2)];
        assert!(decide_slot("urgent-a", &[], Some(1), &one_free, true).ok);
        assert!(!decide_slot("urgent-b", &[], Some(1), &one_free, true).ok);
    }

    #[test]
    fn normal_waiters_do_not_block_an_unqueued_start_from_an_open_slot() {
        let decision = decide_slot("new", &[], Some(1), &[waiter("ordinary", 0, 1)], true);
        assert!(decision.ok);
        assert!(decision.yielding_to.is_empty());
    }
}
