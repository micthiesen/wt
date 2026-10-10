//! GitHub webhook verification, bounded event scheduling, and daemon state.
//!
//! The app owns repository/GitHub/remote facts and supplies them through
//! [`EventSource`]. This crate owns the HTTP boundary, event relevance and
//! debounce policy, and durable last-good snapshot files.

use std::{collections::BTreeMap, future::Future, path::PathBuf, pin::Pin, sync::Arc};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

pub type EventFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Runtime settings independent of the wt configuration implementation.
#[derive(Clone, Debug)]
pub struct EventDaemonConfig {
    pub host: String,
    pub port: u16,
    pub secret: String,
    pub events_dir: PathBuf,
    pub base_branch: String,
    pub writer_sha: Option<String>,
    pub debounce_ms: u64,
    pub min_fetch_interval_ms: u64,
    pub inventory_ttl_ms: u64,
    pub max_body_bytes: usize,
    pub max_concurrent_requests: usize,
    pub max_connections: usize,
    pub queue_capacity: usize,
}

impl EventDaemonConfig {
    pub fn new(
        host: impl Into<String>,
        port: u16,
        secret: impl Into<String>,
        events_dir: PathBuf,
        base_branch: impl Into<String>,
        writer_sha: Option<String>,
    ) -> Self {
        Self {
            host: host.into(),
            port,
            secret: secret.into(),
            events_dir,
            base_branch: base_branch.into(),
            writer_sha,
            debounce_ms: 1_500,
            min_fetch_interval_ms: 10_000,
            inventory_ttl_ms: 10_000,
            max_body_bytes: 5 * 1024 * 1024,
            max_concurrent_requests: 64,
            max_connections: 64,
            queue_capacity: 64,
        }
    }
}

/// The GitHub data maps are JSON envelopes to retain additive fields written
/// by newer GitHub API selections.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GithubSnapshot {
    pub updated_at: u64,
    pub branches: Vec<String>,
    #[serde(default)]
    pub prs: Value,
    #[serde(default)]
    pub merge_queue: Value,
    #[serde(default)]
    pub writer_sha: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EventDaemonState {
    pub pid: Option<u32>,
    pub port: Option<u16>,
    pub writer_sha: Option<String>,
    pub started_at: Option<u64>,
    pub last_event_at: Option<u64>,
    pub last_fetch_at: Option<u64>,
    pub event_count: u64,
    pub last_error: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default)]
pub struct EventGithubData {
    pub prs: Value,
    pub merge_queue: Value,
}

/// Supplies current worktree and GitHub facts. `remote_branches` returning
/// `Ok(None)` means no remote is configured; an error means unknown and the
/// daemon retains its last successful remote branch set.
pub trait EventSource: Send + Sync + 'static {
    fn local_branches<'a>(
        &'a self,
        cancel: &'a CancellationToken,
    ) -> EventFuture<'a, Result<Vec<String>, String>>;
    fn remote_branches<'a>(
        &'a self,
        cancel: &'a CancellationToken,
    ) -> EventFuture<'a, Result<Option<Vec<String>>, String>>;
    fn fetch_origin<'a>(
        &'a self,
        cancel: &'a CancellationToken,
    ) -> EventFuture<'a, Result<(), String>>;
    fn fetch_github<'a>(
        &'a self,
        branches: Vec<String>,
        cancel: &'a CancellationToken,
    ) -> EventFuture<'a, Result<EventGithubData, String>>;
}

#[derive(Debug, Error)]
pub enum EventDaemonError {
    #[error("invalid GitHub webhook configuration: {0}")]
    InvalidConfig(String),
    #[error("cannot bind GitHub events listener: {0}")]
    Bind(#[source] std::io::Error),
    #[error("GitHub events daemon I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("GitHub events state JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("GitHub events state file is larger than the {limit}-byte safety limit: {path}")]
    CacheTooLarge { path: PathBuf, limit: usize },
    #[error("GitHub events state parser failed: {0}")]
    Join(String),
}

pub struct EventsDaemon {
    local_addr: std::net::SocketAddr,
    shutdown: CancellationToken,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl EventsDaemon {
    pub fn local_addr(&self) -> std::net::SocketAddr {
        self.local_addr
    }

    /// Stop accepting events and wait for owned server/scheduler tasks.
    pub async fn shutdown(mut self) {
        self.shutdown.cancel();
        for task in self.tasks.drain(..) {
            let _ = task.await;
        }
    }
}

impl Drop for EventsDaemon {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// Starts a bounded HTTP daemon. The concrete implementation is below the API
/// boundary so the app can inject native repository/GitHub adapters.
pub async fn start_daemon(
    config: EventDaemonConfig,
    source: Arc<dyn EventSource>,
    cancel: CancellationToken,
) -> Result<EventsDaemon, EventDaemonError> {
    daemon::start(config, source, cancel).await
}

pub fn extract_event_branches(
    event: &str,
    payload: &Value,
    base_branch: &str,
) -> Option<Vec<String>> {
    daemon::extract_event_branches(event, payload, base_branch)
}

/// Returns the earliest non-starving fetch deadline, in milliseconds from the
/// same monotonic epoch as `now_ms` and `last_fetch_started_ms`.
pub fn next_fetch_at(
    now_ms: u64,
    last_fetch_started_ms: Option<u64>,
    pending_at_ms: Option<u64>,
    debounce_ms: u64,
    min_fetch_interval_ms: u64,
) -> Option<u64> {
    daemon::next_fetch_at(
        now_ms,
        last_fetch_started_ms,
        pending_at_ms,
        debounce_ms,
        min_fetch_interval_ms,
    )
}

pub fn verify_signature(secret: &[u8], signature_header: Option<&str>, body: &[u8]) -> bool {
    daemon::verify_signature(secret, signature_header, body)
}

pub async fn read_snapshot(
    events_dir: &std::path::Path,
) -> Result<Option<GithubSnapshot>, EventDaemonError> {
    match daemon::read_json(events_dir.join("github.json")).await {
        Ok(snapshot) => Ok(snapshot),
        Err(EventDaemonError::Json(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

pub async fn read_state(
    events_dir: &std::path::Path,
) -> Result<Option<EventDaemonState>, EventDaemonError> {
    daemon::read_json(events_dir.join("state.json")).await
}

mod daemon;
