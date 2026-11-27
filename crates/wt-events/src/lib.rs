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
    pub writer_version: Option<String>,
    /// Repository identity written into every snapshot, so a reader for
    /// another repository or configuration rejects it.
    pub repository: Option<String>,
    pub debounce_ms: u64,
    pub min_fetch_interval_ms: u64,
    /// The floor between fetch starts for a check or status delivery that is
    /// not a completion.
    pub progress_fetch_interval_ms: u64,
    /// The longest a read of only the branches a check or status delivery
    /// names may stand in for a read of every branch. Readers stop using a
    /// snapshot whose whole-fleet read is 90 seconds old, so this stays
    /// well under that with a slow read on top.
    pub full_sweep_interval_ms: u64,
    pub inventory_ttl_ms: u64,
    pub max_body_bytes: usize,
    pub max_concurrent_requests: usize,
    pub max_connections: usize,
    pub queue_capacity: usize,
}

impl EventDaemonConfig {
    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.writer_version = Some(version.into());
        self
    }

    pub fn with_repository(mut self, repository: impl Into<String>) -> Self {
        self.repository = Some(repository.into());
        self
    }

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
            writer_version: None,
            repository: None,
            debounce_ms: 1_500,
            min_fetch_interval_ms: 10_000,
            progress_fetch_interval_ms: 45_000,
            full_sweep_interval_ms: 45_000,
            inventory_ttl_ms: 10_000,
            max_body_bytes: 5 * 1024 * 1024,
            max_concurrent_requests: 64,
            max_connections: 64,
            queue_capacity: 64,
        }
    }
}

/// GitHub's GraphQL point budget as one read left it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateBudget {
    pub limit: u64,
    pub remaining: u64,
    /// Points the read that reported this spent.
    pub cost: u64,
    pub reset_at_ms: u64,
}

/// The budget readings a poller has seen, one per window still open.
///
/// GitHub counts one token's GraphQL requests against more than one window
/// at a time, each with its own reset, and a reply reports only the window
/// that counted it. A read can therefore report plenty left while most
/// requests are being counted against a window that is nearly spent. A
/// window's figure only falls until it resets, so an older reading of it is
/// an upper bound that still holds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BudgetWindows {
    by_reset: BTreeMap<u64, RateBudget>,
}

impl BudgetWindows {
    /// Record what a read reported and return the budget to pace by: the
    /// open window with the least left, at that read's cost.
    pub fn observe(&mut self, read: RateBudget, now_ms: u64) -> Option<RateBudget> {
        self.by_reset.retain(|reset, _| *reset > now_ms);
        if read.reset_at_ms > now_ms {
            self.by_reset
                .entry(read.reset_at_ms)
                .and_modify(|seen| {
                    seen.remaining = seen.remaining.min(read.remaining);
                    seen.limit = read.limit;
                })
                .or_insert(read);
        }
        self.lowest(now_ms).map(|lowest| RateBudget {
            cost: read.cost,
            ..lowest
        })
    }

    /// The open window with the least left.
    pub fn lowest(&self, now_ms: u64) -> Option<RateBudget> {
        self.open(now_ms)
            .into_iter()
            .min_by_key(|budget| (budget.remaining, budget.reset_at_ms))
    }

    /// Every window still open, soonest reset first.
    pub fn open(&self, now_ms: u64) -> Vec<RateBudget> {
        self.by_reset
            .values()
            .filter(|budget| budget.reset_at_ms > now_ms)
            .copied()
            .collect()
    }
}

/// What a snapshot's fields mean: this envelope, and the pull-request and
/// merge-queue JSON the writer's `wt_github` types serialize into it. A
/// reader that only displays the data uses a snapshot whose schema is its
/// own, whichever release wrote it, so a session on an older wt still reads
/// the daemon's file after an update. Bump this whenever a field here, in
/// `wt_github::PullRequest` or in a merge-queue entry changes meaning, is
/// removed or renamed, or becomes required. An added field needs no bump
/// only if both directions stay true: an older reader may ignore it, and a
/// newer reader that finds it missing in an older writer's file reads that
/// as "unknown", never as a fact (an absent `queueRemoval` must not be
/// taken to prove the pull request was never removed).
pub const SNAPSHOT_SCHEMA: u32 = 1;

/// The GitHub data maps are JSON envelopes to retain additive fields written
/// by newer GitHub API selections.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GithubSnapshot {
    pub updated_at: u64,
    /// Wall-clock epoch ms when the GitHub read that produced this snapshot
    /// started. Older writers omit it; readers then cannot order the data
    /// against their own writes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetch_started_at_ms: Option<u64>,
    /// When the last read of every branch finished. `updated_at` moves on
    /// each write, and a write after a read of a few branches leaves the
    /// rest as old as this. Absent when every write is a whole read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swept_at_ms: Option<u64>,
    /// Branches read again since `fetch_started_at_ms`, with the start of
    /// the read that last covered each. Only these are newer than the rest.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub branch_reads: BTreeMap<String, u64>,
    pub branches: Vec<String>,
    #[serde(default)]
    pub prs: Value,
    #[serde(default)]
    pub merge_queue: Value,
    /// The budget after the read that produced this snapshot, when GitHub
    /// reported one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateBudget>,
    /// Every budget window the daemon has a reading of, when there is more
    /// than the one in `rate_limit` (the lowest).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rate_windows: Vec<RateBudget>,
    /// Wall-clock epoch ms before which the daemon starts no background
    /// read of every branch: the last such read's start plus the interval
    /// its budget asks for. A reader that only displays the data may use it
    /// until then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_fetch_at_ms: Option<u64>,
    #[serde(default)]
    pub writer_sha: Option<String>,
    /// The writer's [`SNAPSHOT_SCHEMA`]. Absent from writers that predate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<u32>,
    /// Older daemons preserve unknown fields; bind the schema to its writer
    /// so a rollback cannot advertise a leftover schema from a newer daemon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_writer_sha: Option<String>,
    /// The writer's repository identity (see `EventDaemonConfig::repository`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl GithubSnapshot {
    /// Whether this build reads the fields as the writer meant them.
    pub fn schema_current(&self) -> bool {
        self.schema == Some(SNAPSHOT_SCHEMA)
            && self.writer_sha.is_some()
            && self.schema_writer_sha == self.writer_sha
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EventDaemonState {
    pub pid: Option<u32>,
    pub port: Option<u16>,
    pub writer_sha: Option<String>,
    /// Version reported by the daemon itself, absent in older state files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writer_version: Option<String>,
    /// Older daemons preserve unknown fields; bind the version to its writer
    /// so a rollback cannot advertise a leftover version from a newer daemon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writer_version_sha: Option<String>,
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
    pub budget: Option<RateBudget>,
}

/// A failed GitHub read: what it said and, for a rate limit whose reply
/// named one, when the limit resets in epoch milliseconds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GithubFetchError {
    pub message: String,
    pub rate_limit_reset_ms: Option<u64>,
}

impl From<String> for GithubFetchError {
    fn from(message: String) -> Self {
        Self {
            message,
            rate_limit_reset_ms: None,
        }
    }
}

impl From<&str> for GithubFetchError {
    fn from(message: &str) -> Self {
        message.to_owned().into()
    }
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
    ) -> EventFuture<'a, Result<EventGithubData, GithubFetchError>>;
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

pub use daemon::{
    MAX_PACED_WAIT_MS, backoff_interval_ms, budget_low, failure_wait_ms, paced_interval_ms,
    rate_limited,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshots_without_a_fetch_start_still_parse_and_round_trip() {
        let old: GithubSnapshot = serde_json::from_value(serde_json::json!({
            "updatedAt": 5, "branches": ["feature"], "writerSha": "build"
        }))
        .unwrap();
        assert_eq!(old.fetch_started_at_ms, None);
        assert!(
            !serde_json::to_value(&old)
                .unwrap()
                .as_object()
                .unwrap()
                .contains_key("fetchStartedAtMs")
        );
        let new = GithubSnapshot {
            fetch_started_at_ms: Some(3),
            ..old
        };
        let json = serde_json::to_value(&new).unwrap();
        assert_eq!(json["fetchStartedAtMs"], 3);
        assert_eq!(serde_json::from_value::<GithubSnapshot>(json).unwrap(), new);
    }
}
