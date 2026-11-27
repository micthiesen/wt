use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::State,
    http::{Request, StatusCode, header::CONTENT_LENGTH},
    response::IntoResponse,
    routing::{get, post},
};
use hmac::{Hmac, Mac};
use hyper::server::conn::http1;
use hyper_util::{
    rt::{TokioIo, TokioTimer},
    service::TowerToHyperService,
};
use serde::de::DeserializeOwned;
use serde_json::Value;
use sha2::Sha256;
use tokio::{
    io::AsyncReadExt,
    net::TcpListener,
    sync::{Mutex, Semaphore, mpsc},
    task::JoinSet,
    time::Instant,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, warn};

const MAX_HTTP_HEADERS: usize = 32;
const MAX_HTTP_HEADER_BYTES: usize = 16 * 1024;
const HTTP_HEADER_TIMEOUT: Duration = Duration::from_secs(5);
const HTTP_CONNECTION_TIMEOUT: Duration = Duration::from_secs(30);

use crate::{
    EventDaemonConfig, EventDaemonError, EventDaemonState, EventSource, EventsDaemon,
    GithubSnapshot, RateBudget,
};

#[derive(Clone)]
struct HttpState {
    secret: Arc<[u8]>,
    max_body_bytes: usize,
    requests: Arc<Semaphore>,
    events: mpsc::Sender<PendingEvent>,
    coalesced_refresh: mpsc::Sender<()>,
}

struct PendingEvent {
    name: String,
    payload: Value,
    received_at: u64,
}

struct DurableState {
    dir: PathBuf,
    state: Mutex<EventDaemonState>,
}

pub(super) async fn start(
    config: EventDaemonConfig,
    source: Arc<dyn EventSource>,
    parent_cancel: CancellationToken,
) -> Result<EventsDaemon, EventDaemonError> {
    validate(&config)?;
    tokio::fs::create_dir_all(&config.events_dir).await?;
    let bind_addr = format!("{}:{}", config.host, config.port);
    let listener = TcpListener::bind(&bind_addr)
        .await
        .map_err(EventDaemonError::Bind)?;
    let local_addr = listener.local_addr()?;
    let shutdown = parent_cancel.child_token();
    let (tx, rx) = mpsc::channel(config.queue_capacity);
    let (coalesced_tx, coalesced_rx) = mpsc::channel(1);
    let http = HttpState {
        secret: Arc::from(config.secret.as_bytes()),
        max_body_bytes: config.max_body_bytes,
        requests: Arc::new(Semaphore::new(config.max_concurrent_requests)),
        events: tx,
        coalesced_refresh: coalesced_tx,
    };
    let durable = Arc::new(DurableState {
        dir: config.events_dir.clone(),
        state: Mutex::new(EventDaemonState {
            pid: Some(std::process::id()),
            port: Some(local_addr.port()),
            writer_sha: config.writer_sha.clone(),
            writer_version: config.writer_version.clone(),
            writer_version_sha: config.writer_sha.clone(),
            started_at: Some(now_millis()),
            extra: read_json::<EventDaemonState>(config.events_dir.join("state.json"))
                .await
                .ok()
                .flatten()
                .map(|state| state.extra)
                .unwrap_or_default(),
            ..EventDaemonState::default()
        }),
    });
    durable.persist_state().await?;

    let app = Router::new()
        .route("/health", get(health))
        .route("/webhook", post(webhook))
        .with_state(http);
    let server_shutdown = shutdown.clone();
    let max_connections = config.max_connections;
    let server = tokio::spawn(async move {
        let connection_slots = Arc::new(Semaphore::new(max_connections));
        let mut connections = JoinSet::new();
        loop {
            let accepted = tokio::select! {
                _ = server_shutdown.cancelled() => break,
                accepted = listener.accept() => accepted,
            };
            let (stream, _) = match accepted {
                Ok(accepted) => accepted,
                Err(error) => {
                    warn!(error = %error, "cannot accept GitHub events HTTP connection");
                    tokio::select! {
                        _ = server_shutdown.cancelled() => break,
                        _ = tokio::time::sleep(Duration::from_millis(100)) => continue,
                    }
                }
            };

            while connections.try_join_next().is_some() {}
            let Ok(connection_slot) = connection_slots.clone().try_acquire_owned() else {
                drop(stream);
                continue;
            };
            let app = app.clone();
            connections.spawn(async move {
                let _connection_slot = connection_slot;
                let mut builder = http1::Builder::new();
                builder
                    .timer(TokioTimer::new())
                    .header_read_timeout(Some(HTTP_HEADER_TIMEOUT))
                    .max_headers(MAX_HTTP_HEADERS)
                    .max_header_size(MAX_HTTP_HEADER_BYTES)
                    .keep_alive(false);
                let connection =
                    builder.serve_connection(TokioIo::new(stream), TowerToHyperService::new(app));
                match tokio::time::timeout(HTTP_CONNECTION_TIMEOUT, connection).await {
                    Err(_) => warn!("GitHub events HTTP connection exceeded its lifetime"),
                    Ok(Err(error)) => {
                        warn!(error = %error, "GitHub events HTTP connection failed")
                    }
                    Ok(Ok(())) => {}
                }
            });
        }
        connections.abort_all();
        while connections.join_next().await.is_some() {}
    });

    let scheduler_shutdown = shutdown.clone();
    let scheduler_durable = durable.clone();
    let scheduler_config = config.clone();
    let scheduler = tokio::spawn(async move {
        scheduler(
            scheduler_config,
            source,
            rx,
            coalesced_rx,
            scheduler_durable,
            scheduler_shutdown,
        )
        .await;
    });

    Ok(EventsDaemon {
        local_addr,
        shutdown,
        tasks: vec![server, scheduler],
    })
}

fn validate(config: &EventDaemonConfig) -> Result<(), EventDaemonError> {
    if config.secret.is_empty() {
        return Err(EventDaemonError::InvalidConfig("secret is empty".into()));
    }
    if config.max_body_bytes == 0
        || config.max_concurrent_requests == 0
        || config.max_connections == 0
        || config.queue_capacity == 0
    {
        return Err(EventDaemonError::InvalidConfig(
            "body, concurrency, and queue limits must be positive".into(),
        ));
    }
    Ok(())
}

async fn health() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

async fn webhook(State(state): State<HttpState>, request: Request<Body>) -> impl IntoResponse {
    let _permit = match state.requests.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE,
    };
    if request
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .is_some_and(|length| length > state.max_body_bytes as u64)
    {
        return StatusCode::PAYLOAD_TOO_LARGE;
    }
    let (parts, body) = request.into_parts();
    let bytes = match tokio::time::timeout(
        Duration::from_secs(10),
        to_bytes(body, state.max_body_bytes),
    )
    .await
    {
        Err(_) => return StatusCode::REQUEST_TIMEOUT,
        Ok(Ok(bytes)) => bytes,
        Ok(Err(_)) => return StatusCode::PAYLOAD_TOO_LARGE,
    };
    if !verify_signature(
        &state.secret,
        parts
            .headers
            .get("x-hub-signature-256")
            .and_then(|h| h.to_str().ok()),
        &bytes,
    ) {
        return StatusCode::UNAUTHORIZED;
    }
    let event_name = parts
        .headers
        .get("x-github-event")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_owned();
    if event_name == "ping" {
        return StatusCode::OK;
    }
    if !matches!(
        event_name.as_str(),
        "pull_request"
            | "pull_request_review"
            | "pull_request_review_thread"
            | "issue_comment"
            | "check_suite"
            | "check_run"
            | "status"
            | "merge_group"
            | "push"
    ) {
        return StatusCode::OK;
    }
    let payload: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) => return StatusCode::OK,
    };
    match state.events.try_send(PendingEvent {
        name: event_name,
        payload,
        received_at: now_millis(),
    }) {
        Ok(()) => StatusCode::OK,
        Err(mpsc::error::TrySendError::Full(_)) => match state.coalesced_refresh.try_send(()) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(())) => StatusCode::OK,
            Err(mpsc::error::TrySendError::Closed(())) => StatusCode::SERVICE_UNAVAILABLE,
        },
        Err(mpsc::error::TrySendError::Closed(_)) => StatusCode::SERVICE_UNAVAILABLE,
    }
}

pub(super) fn verify_signature(secret: &[u8], signature_header: Option<&str>, body: &[u8]) -> bool {
    let Some(value) = signature_header.and_then(|v| v.strip_prefix("sha256=")) else {
        return false;
    };
    if value.len() != 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return false;
    }
    let Ok(signature) = decode_hex(value) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&signature).is_ok()
}

fn decode_hex(text: &str) -> Result<Vec<u8>, ()> {
    let mut out = Vec::with_capacity(text.len() / 2);
    for pair in text.as_bytes().as_chunks::<2>().0 {
        let nibble = |b: u8| match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            _ => None,
        };
        out.push((nibble(pair[0]).ok_or(())? << 4) | nibble(pair[1]).ok_or(())?);
    }
    Ok(out)
}

/// The most branches a `status` payload names.
const STATUS_BRANCH_LIMIT: usize = 10;

/// The branches an event concerns: `None` when that is unknown (it may
/// concern any), empty when it concerns none.
pub(super) fn extract_event_branches(
    event: &str,
    payload: &Value,
    base_branch: &str,
) -> Option<Vec<String>> {
    let unscoped = || None;
    match event {
        "pull_request" | "pull_request_review" | "pull_request_review_thread" => {
            // Entering or leaving the merge queue moves every queued PR's
            // position, a local one's included.
            if matches!(
                payload.get("action").and_then(Value::as_str),
                Some("enqueued" | "dequeued")
            ) {
                return unscoped();
            }
            let head = branch_at(payload, &["pull_request", "head", "ref"]);
            let base = branch_at(payload, &["pull_request", "base", "ref"]);
            let (Some(head), Some(base)) = (head, base) else {
                return unscoped();
            };
            if head.is_empty() || base.is_empty() {
                return unscoped();
            }
            // A PR stacked on a local branch concerns that branch. Nearly
            // every PR targets the trunk, so the trunk itself scopes nothing.
            let retargeted_from = payload
                .pointer("/changes/base/ref/from")
                .and_then(Value::as_str);
            Some(
                std::iter::once(head)
                    .chain(
                        [Some(base), retargeted_from]
                            .into_iter()
                            .flatten()
                            .filter(|branch| *branch != base_branch),
                    )
                    .map(str::to_owned)
                    .collect(),
            )
        }
        "status" => {
            let Some(branches) = payload.get("branches").and_then(Value::as_array) else {
                return unscoped();
            };
            let names: Option<Vec<String>> = branches
                .iter()
                .map(|branch| branch_at(branch, &["name"]).map(str::to_owned))
                .collect();
            match names {
                // GitHub lists at most ten branches containing the commit, so
                // a full list may omit a local one.
                Some(names)
                    if !names.is_empty()
                        && names.len() < STATUS_BRANCH_LIMIT
                        && names.iter().all(|name| name != base_branch) =>
                {
                    Some(names)
                }
                _ => unscoped(),
            }
        }
        "merge_group" => unscoped(),
        "check_suite" => branch_at(payload, &["check_suite", "head_branch"])
            .map(|v| vec![v.to_owned()])
            .or_else(unscoped),
        "check_run" => branch_at(payload, &["check_run", "check_suite", "head_branch"])
            .map(|v| vec![v.to_owned()])
            .or_else(unscoped),
        "issue_comment" => {
            if payload.pointer("/issue/pull_request").is_some() {
                unscoped()
            } else {
                Some(Vec::new())
            }
        }
        "push" => {
            let Some(refname) = payload.get("ref").and_then(Value::as_str) else {
                return unscoped();
            };
            let Some(branch) = refname.strip_prefix("refs/heads/") else {
                return unscoped();
            };
            if branch.is_empty() || branch == base_branch {
                unscoped()
            } else {
                Some(vec![branch.to_owned()])
            }
        }
        _ => Some(Vec::new()),
    }
}

/// Whether an event can move origin's refs, so the refresh it schedules
/// should fetch origin first. Check, status, review and comment events only
/// change GitHub state the batched query reads. An unknown pull request
/// action counts as ref-changing.
pub(super) fn changes_refs(event: &str, payload: &Value) -> bool {
    match event {
        "push" | "merge_group" | "create" | "delete" => true,
        "pull_request" => !matches!(
            payload.get("action").and_then(Value::as_str),
            Some(
                "labeled"
                    | "unlabeled"
                    | "assigned"
                    | "unassigned"
                    | "review_requested"
                    | "review_request_removed"
                    | "ready_for_review"
                    | "converted_to_draft"
                    | "locked"
                    | "unlocked"
                    | "milestoned"
                    | "demilestoned"
                    | "auto_merge_enabled"
                    | "auto_merge_disabled"
                    | "enqueued"
                    | "dequeued"
            )
        ),
        _ => false,
    }
}

/// Whether a delivery only reports CI that has not finished: a check run or
/// suite that was created or requested again, or a pending status. Such
/// deliveries far outnumber results and change nothing but a progress badge,
/// so they wait behind a longer floor. An action this build does not know
/// is not assumed to be progress.
pub(super) fn ci_progress(event: &str, payload: &Value) -> bool {
    let field = |key: &str| payload.get(key).and_then(Value::as_str);
    match event {
        "check_run" => matches!(
            field("action"),
            Some("created" | "rerequested" | "requested_action")
        ),
        "check_suite" => matches!(field("action"), Some("requested" | "rerequested")),
        "status" => field("state") == Some("pending"),
        _ => false,
    }
}

fn branch_at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a str> {
    path.iter()
        .try_fold(value, |node, key| node.get(*key))?
        .as_str()
}

pub(super) fn next_fetch_at(
    now_ms: u64,
    last_fetch_started_ms: Option<u64>,
    pending_at_ms: Option<u64>,
    debounce_ms: u64,
    min_fetch_interval_ms: u64,
) -> Option<u64> {
    let debounced = now_ms.saturating_add(debounce_ms);
    let floored = last_fetch_started_ms
        .map(|last| last.saturating_add(min_fetch_interval_ms))
        .unwrap_or(0);
    let next = debounced.max(floored);
    if pending_at_ms.is_some_and(|pending| pending <= next) {
        None
    } else {
        Some(next)
    }
}

/// Longest wait between GitHub reads while they keep failing. GitHub asks
/// clients that hit a secondary rate limit to wait minutes, not seconds.
const MAX_FAILURE_BACKOFF_MS: u64 = 5 * 60 * 1000;

/// The floor between reads after `failures` consecutive failed ones: the
/// normal interval, doubled per failure up to [`MAX_FAILURE_BACKOFF_MS`]. A
/// rate limit waits the longest at once; retrying sooner extends it.
pub fn backoff_interval_ms(min_fetch_interval_ms: u64, failures: u32, rate_limited: bool) -> u64 {
    if rate_limited {
        return MAX_FAILURE_BACKOFF_MS.max(min_fetch_interval_ms);
    }
    if failures == 0 {
        return min_fetch_interval_ms;
    }
    min_fetch_interval_ms
        .saturating_mul(1u64 << failures.min(16))
        .min(MAX_FAILURE_BACKOFF_MS)
        .max(min_fetch_interval_ms)
}

/// Clocks differ; a read sent exactly at the reset can still be refused.
const RESET_SLACK_MS: u64 = 5 * 1000;
/// GitHub's primary limits reset within the hour. A reset further away than
/// this is not believed.
const MAX_RESET_WAIT_MS: u64 = 65 * 60 * 1000;

/// How long to wait before the next GitHub read after a failed one. Every
/// poller of GitHub spaces its retries with this. A rate limit whose reset
/// time is known waits until just past it; otherwise
/// [`backoff_interval_ms`] decides.
pub fn failure_wait_ms(
    min_fetch_interval_ms: u64,
    failures: u32,
    rate_limited: bool,
    reset_at_ms: Option<u64>,
    now_ms: u64,
) -> u64 {
    if rate_limited
        && let Some(wait) = reset_at_ms
            .and_then(|reset| reset.checked_sub(now_ms))
            .filter(|wait| *wait > 0 && *wait <= MAX_RESET_WAIT_MS)
    {
        return wait
            .saturating_add(RESET_SLACK_MS)
            .max(min_fetch_interval_ms);
    }
    backoff_interval_ms(min_fetch_interval_ms, failures, rate_limited)
}

/// The share of the hourly budget background reads leave alone: agents
/// working through the same token spend about that much.
const RESERVE_DIVISOR: u64 = 4;
/// Background reads plan to use their share at half speed, because they are
/// not its only spender and learn what others spent only at their next read.
const PACE_SAFETY: u64 = 2;

/// The floor between starts of background GitHub reads, given the budget
/// the last one left: `floor_ms`, stretched so that reads costing what the
/// last one did would use the unreserved budget evenly until it resets, and
/// all the way to the reset once only the reserve is left. A budget that is
/// unknown, already reset, or resetting implausibly far away leaves the
/// floor as it is. User-initiated reads do not ask this.
pub fn paced_interval_ms(floor_ms: u64, budget: Option<RateBudget>, now_ms: u64) -> u64 {
    let Some(budget) = budget else {
        return floor_ms;
    };
    let Some(to_reset) = budget
        .reset_at_ms
        .checked_sub(now_ms)
        .filter(|wait| *wait > 0 && *wait <= MAX_RESET_WAIT_MS)
    else {
        return floor_ms;
    };
    let until_reset = to_reset.saturating_add(RESET_SLACK_MS);
    let spendable = budget
        .remaining
        .saturating_sub(budget.limit / RESERVE_DIVISOR);
    if spendable == 0 {
        return until_reset.max(floor_ms);
    }
    let paced = budget
        .cost
        .max(1)
        .saturating_mul(to_reset)
        .saturating_mul(PACE_SAFETY)
        / spendable;
    paced.min(until_reset).max(floor_ms)
}

async fn scheduler(
    config: EventDaemonConfig,
    source: Arc<dyn EventSource>,
    mut rx: mpsc::Receiver<PendingEvent>,
    mut coalesced_rx: mpsc::Receiver<()>,
    durable: Arc<DurableState>,
    cancel: CancellationToken,
) {
    let started = Instant::now();
    let mut last_fetch_start: Option<Instant> = None;
    let mut fetch_interval_ms = config.min_fetch_interval_ms;
    let mut failures = 0u32;
    let mut pending_deadline: Option<Instant> = Some(started);
    let mut cached_inventory_at: Option<Instant> = None;
    let mut local_branches = BTreeSet::new();
    let mut remote_branches = BTreeSet::new();
    let mut local_known = false;
    let mut remote_known = false;
    // What the next refresh fetches origin for; `None` skips the fetch. The
    // first refresh, and a coalesced overflow whose events are unknown,
    // always fetch.
    let mut origin_trigger: Option<(String, Option<String>)> = Some(("startup".into(), None));
    loop {
        let sleep = async {
            match pending_deadline {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => futures_util::future::pending().await,
            }
        };
        tokio::select! {
            _ = cancel.cancelled() => break,
            maybe_event = rx.recv() => {
                let Some(event) = maybe_event else { break; };
                let now = Instant::now();
                let ttl = Duration::from_millis(config.inventory_ttl_ms);
                if cached_inventory_at.is_none_or(|at| now.duration_since(at) >= ttl) {
                    let local = source.local_branches(&cancel).await;
                    match local {
                        Ok(branches) => { local_branches = branches.into_iter().collect(); local_known = true; }
                        Err(error_text) => { local_known = false; warn!(error = %error_text, "could not refresh local event branch inventory"); }
                    }
                    match source.remote_branches(&cancel).await {
                        Ok(Some(branches)) => { remote_branches = branches.into_iter().collect(); remote_known = true; }
                        Ok(None) => { remote_branches.clear(); remote_known = true; }
                        Err(error_text) => {
                            remote_known = false;
                            warn!(error = %error_text, "remote event branch inventory is unknown; retaining last good set");
                        }
                    }
                    cached_inventory_at = Some(now);
                }
                let candidates = extract_event_branches(&event.name, &event.payload, &config.base_branch);
                let (relevant, matched) = match candidates {
                    Some(names) if names.is_empty() => (false, None),
                    None => (true, None),
                    Some(_) if !(local_known && remote_known) => (true, None),
                    Some(names) => {
                        let matched = names.into_iter().find(|branch| {
                            local_branches.contains(branch) || remote_branches.contains(branch)
                        });
                        (matched.is_some(), matched)
                    }
                };
                if relevant {
                    let floor_ms = if ci_progress(&event.name, &event.payload) {
                        fetch_interval_ms.max(config.progress_fetch_interval_ms)
                    } else {
                        fetch_interval_ms
                    };
                    if changes_refs(&event.name, &event.payload) {
                        origin_trigger.get_or_insert_with(|| (event.name.clone(), matched));
                    }
                    {
                        let mut state = durable.state.lock().await;
                        state.last_event_at = Some(event.received_at);
                        state.event_count = state.event_count.saturating_add(1);
                        let _ = durable.write_state_locked(&state).await;
                    }
                    let now_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
                    let last_ms = last_fetch_start.map(|instant| instant.duration_since(started).as_millis().min(u64::MAX as u128) as u64);
                    let pending_ms = pending_deadline.map(|instant| instant.duration_since(started).as_millis().min(u64::MAX as u128) as u64);
                    if let Some(deadline) = next_fetch_at(now_ms, last_ms, pending_ms, config.debounce_ms, floor_ms) {
                        pending_deadline = Some(started + Duration::from_millis(deadline));
                    }
                }
            }
            _ = coalesced_rx.recv() => {
                origin_trigger.get_or_insert_with(|| ("coalesced overflow".into(), None));
                let now_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
                let last_ms = last_fetch_start.map(|instant| instant.duration_since(started).as_millis().min(u64::MAX as u128) as u64);
                let pending_ms = pending_deadline.map(|instant| instant.duration_since(started).as_millis().min(u64::MAX as u128) as u64);
                if let Some(deadline) = next_fetch_at(now_ms, last_ms, pending_ms, config.debounce_ms, fetch_interval_ms) {
                    pending_deadline = Some(started + Duration::from_millis(deadline));
                }
            }
            _ = sleep => {
                pending_deadline = None;
                let now = Instant::now();
                last_fetch_start = Some(now);
                let branch_refresh = async {
                    source.local_branches(&cancel).await
                };
                let local = match branch_refresh.await {
                    Ok(branches) => { local_known = true; branches },
                    Err(e) if cancel.is_cancelled() => {
                        debug!(error = %e, "inventory read interrupted by shutdown");
                        break;
                    }
                    Err(e) => {
                        local_known = false;
                        failures = failures.saturating_add(1);
                        fetch_interval_ms = backoff_interval_ms(config.min_fetch_interval_ms, failures, false);
                        pending_deadline = Some(now + Duration::from_millis(fetch_interval_ms));
                        set_error(&durable, format!("local worktree inventory failed: {e}"), fetch_interval_ms).await;
                        continue;
                    }
                };
                local_branches = local.iter().cloned().collect();
                let mut fetch_branches: BTreeSet<String> = local.into_iter().collect();
                if let Some(at) = cached_inventory_at {
                    if now.duration_since(at) < Duration::from_millis(config.inventory_ttl_ms) {
                        fetch_branches.extend(remote_branches.iter().cloned());
                    } else {
                        match source.remote_branches(&cancel).await {
                            Ok(Some(branches)) => { remote_branches = branches.into_iter().collect(); remote_known = true; }
                            Ok(None) => { remote_branches.clear(); remote_known = true; }
                            Err(e) => {
                                remote_known = false;
                                warn!(error = %e, "remote branch inventory unavailable; using last known set");
                            }
                        }
                        fetch_branches.extend(remote_branches.iter().cloned());
                    }
                } else {
                    match source.remote_branches(&cancel).await {
                        Ok(Some(branches)) => { remote_branches = branches.into_iter().collect(); remote_known = true; }
                        Ok(None) => { remote_known = true; }
                        Err(e) => {
                            remote_known = false;
                            warn!(error = %e, "remote branch inventory unavailable");
                        }
                    }
                    fetch_branches.extend(remote_branches.iter().cloned());
                }
                cached_inventory_at = Some(now);
                if let Some((trigger, branch)) = origin_trigger.take() {
                    let branch_name = branch.as_deref().unwrap_or("");
                    let origin_started = Instant::now();
                    let result = source.fetch_origin(&cancel).await;
                    let elapsed_ms = origin_started.elapsed().as_millis() as u64;
                    match result {
                        // The fetch itself logs its trigger and duration at info.
                        Ok(()) => debug!(event = %trigger, branch = branch_name, elapsed_ms, "fetched origin before GitHub refresh"),
                        Err(e) => {
                            warn!(event = %trigger, branch = branch_name, elapsed_ms, error = %e, "git fetch origin failed before GitHub refresh");
                            // Keep the request so the next refresh retries it.
                            origin_trigger.get_or_insert((trigger, branch));
                        }
                    }
                }
                // Readers order this observation by when the GitHub read
                // began: a write accepted after this moment may be missing.
                let fetch_started_at_ms = now_millis();
                let fetched = source.fetch_github(fetch_branches.iter().cloned().collect(), &cancel).await;
                let mut budget = None;
                let mut reset_at_ms = None;
                let failure = match fetched {
                    Ok(data) => {
                        budget = data.budget;
                        let snapshot = GithubSnapshot {
                            updated_at: now_millis(), fetch_started_at_ms: Some(fetch_started_at_ms),
                            branches: fetch_branches.into_iter().collect(), prs: data.prs,
                            merge_queue: data.merge_queue, rate_limit: data.budget,
                            writer_sha: config.writer_sha.clone(),
                            repository: config.repository.clone(),
                            extra: read_json::<GithubSnapshot>(config.events_dir.join("github.json")).await.ok().flatten().map(|snapshot| snapshot.extra).unwrap_or_default(),
                        };
                        match durable.write_snapshot(&snapshot).await {
                            Ok(()) => {
                                let mut state = durable.state.lock().await;
                                state.last_fetch_at = Some(snapshot.updated_at);
                                state.last_error = None;
                                let _ = durable.write_state_locked(&state).await;
                                if let Err(e) = atomic_write(&config.events_dir.join("github.touch"), snapshot.updated_at.to_string().as_bytes()).await { warn!(error = %e, "cannot write GitHub snapshot marker"); }
                                None
                            }
                            Err(e) => Some(format!("cannot persist GitHub snapshot: {e}")),
                        }
                    }
                    Err(e) => {
                        reset_at_ms = e.rate_limit_reset_ms;
                        Some(format!("GitHub refresh failed: {}", e.message))
                    }
                };
                match failure {
                    // A busy hour stretches the floor instead of running the
                    // budget out; the floor still counts from this fetch's start.
                    None => (failures, fetch_interval_ms) = (0, paced_interval_ms(config.min_fetch_interval_ms, budget, now_millis())),
                    // A read cut short by shutdown says nothing about GitHub.
                    Some(message) if cancel.is_cancelled() => {
                        debug!(error = %message, "GitHub refresh interrupted by shutdown");
                        break;
                    }
                    Some(message) => {
                        failures = failures.saturating_add(1);
                        let limited = rate_limited(&message);
                        fetch_interval_ms = failure_wait_ms(config.min_fetch_interval_ms, failures, limited, reset_at_ms, now_millis());
                        // No webhook may follow the one this refresh served
                        // (or the startup read), so the retry is scheduled
                        // here, on the same floor an arriving event would get.
                        pending_deadline = Some(now + Duration::from_millis(fetch_interval_ms));
                        set_error(&durable, bounded_error(&message), fetch_interval_ms).await;
                    }
                }
            }
        }
    }
}

/// GitHub read errors arrive as text; its rate-limit replies say so.
pub fn rate_limited(error: &str) -> bool {
    let lower = error.to_lowercase();
    ["rate limit", "rate_limited", "abuse detection"]
        .iter()
        .any(|sign| lower.contains(sign))
}

const MAX_ERROR_CHARS: usize = 500;

/// A refresh error fit to log and to keep in the state file. A failed
/// GraphQL read can quote its whole reply, and one reply can repeat an
/// error for every branch asked about: only the `errors[]` messages of a
/// quoted reply are kept, each distinct line once, capped.
fn bounded_error(message: &str) -> String {
    let text = quoted_reply_errors(message).unwrap_or_else(|| message.to_owned());
    let mut lines: Vec<&str> = Vec::new();
    for line in text.lines().map(str::trim) {
        if !line.is_empty() && !lines.contains(&line) {
            lines.push(line);
        }
    }
    let joined = lines.join("; ");
    match joined.char_indices().nth(MAX_ERROR_CHARS) {
        Some((end, _)) => format!("{}...", &joined[..end]),
        None => joined,
    }
}

/// The text before a JSON reply quoted in `message`, then that reply's
/// `errors[]` messages. `None` when `message` quotes no such reply.
fn quoted_reply_errors(message: &str) -> Option<String> {
    let start = message.find('{')?;
    let reply: serde_json::Value = serde_json::from_str(message[start..].trim()).ok()?;
    let errors = reply.get("errors")?.as_array()?;
    let mut said: Vec<String> = Vec::new();
    for error in errors {
        let Some(text) = error
            .get("message")
            .and_then(serde_json::Value::as_str)
            .or_else(|| error.as_str())
        else {
            continue;
        };
        let line = match error.get("type").and_then(serde_json::Value::as_str) {
            Some(kind) => format!("{kind}: {text}"),
            None => text.to_owned(),
        };
        if !said.contains(&line) {
            said.push(line);
        }
    }
    Some(format!(
        "{} {}",
        message[..start].trim_end(),
        said.join("\n")
    ))
}

/// Record the failure, and log it when it is not the one already recorded:
/// a rate limit or an outage fails every retry the same way.
async fn set_error(durable: &DurableState, message: String, retry_in_ms: u64) {
    let mut state = durable.state.lock().await;
    if state.last_error.as_deref() == Some(message.as_str()) {
        return;
    }
    error!(error = %message, retry_in_seconds = retry_in_ms / 1000, "GitHub event refresh failed");
    state.last_error = Some(message);
    let _ = durable.write_state_locked(&state).await;
}

impl DurableState {
    async fn persist_state(&self) -> Result<(), EventDaemonError> {
        let state = self.state.lock().await;
        self.write_state_locked(&state).await?;
        Ok(())
    }

    async fn write_state_locked(&self, state: &EventDaemonState) -> Result<(), std::io::Error> {
        let bytes = serde_json::to_vec(state).map_err(std::io::Error::other)?;
        atomic_write(&self.dir.join("state.json"), &bytes).await
    }

    async fn write_snapshot(&self, snapshot: &GithubSnapshot) -> Result<(), std::io::Error> {
        let bytes = serde_json::to_vec(snapshot).map_err(std::io::Error::other)?;
        atomic_write(&self.dir.join("github.json"), &bytes).await
    }
}

async fn atomic_write(path: &Path, data: &[u8]) -> Result<(), std::io::Error> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("output path has no parent"))?;
    tokio::fs::create_dir_all(parent).await?;
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("events");
    let tmp = parent.join(format!(
        ".{filename}.{}.{}.tmp",
        std::process::id(),
        unique_nonce()
    ));
    tokio::fs::write(&tmp, data).await?;
    if let Err(error) = tokio::fs::rename(&tmp, path).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(error);
    }
    Ok(())
}

fn unique_nonce() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

pub(super) async fn read_json<T: DeserializeOwned + Send + 'static>(
    path: PathBuf,
) -> Result<Option<T>, EventDaemonError> {
    const MAX_STATE_BYTES: usize = 16 * 1024 * 1024;
    let mut file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    let mut bounded = (&mut file).take(MAX_STATE_BYTES as u64 + 1);
    bounded.read_to_end(&mut bytes).await?;
    if bytes.len() > MAX_STATE_BYTES {
        return Err(EventDaemonError::CacheTooLarge {
            path,
            limit: MAX_STATE_BYTES,
        });
    }
    let parsed = tokio::task::spawn_blocking(move || serde_json::from_slice(&bytes))
        .await
        .map_err(|error| EventDaemonError::Join(error.to_string()))??;
    Ok(Some(parsed))
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EventFuture, EventGithubData, GithubFetchError, read_snapshot, read_state};
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tempfile::TempDir;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpStream,
    };

    #[derive(Default)]
    struct MockSource {
        /// The GitHub read waits for cancellation and then fails, as a
        /// request in flight at shutdown does.
        hang_until_cancelled: AtomicBool,
        fail: AtomicBool,
        /// The budget each successful read reports.
        budget: Option<RateBudget>,
        fetches: AtomicUsize,
        origin_fetches: AtomicUsize,
        remote_calls: AtomicUsize,
        remote_failure_after: Option<usize>,
    }

    impl EventSource for MockSource {
        fn local_branches<'a>(
            &'a self,
            _: &'a CancellationToken,
        ) -> EventFuture<'a, Result<Vec<String>, String>> {
            Box::pin(async { Ok(vec!["feature-x".to_owned()]) })
        }
        fn remote_branches<'a>(
            &'a self,
            _: &'a CancellationToken,
        ) -> EventFuture<'a, Result<Option<Vec<String>>, String>> {
            let call = self.remote_calls.fetch_add(1, Ordering::SeqCst);
            let should_fail = self
                .remote_failure_after
                .is_some_and(|threshold| call >= threshold);
            Box::pin(async move {
                if should_fail {
                    Err("fixture remote inventory failure".into())
                } else {
                    Ok(Some(vec!["remote-only".to_owned()]))
                }
            })
        }
        fn fetch_origin<'a>(
            &'a self,
            _: &'a CancellationToken,
        ) -> EventFuture<'a, Result<(), String>> {
            self.origin_fetches.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
        fn fetch_github<'a>(
            &'a self,
            branches: Vec<String>,
            cancel: &'a CancellationToken,
        ) -> EventFuture<'a, Result<EventGithubData, GithubFetchError>> {
            self.fetches.fetch_add(1, Ordering::SeqCst);
            let budget = self.budget;
            let fail = self.fail.load(Ordering::SeqCst);
            let hang = self.hang_until_cancelled.load(Ordering::SeqCst);
            Box::pin(async move {
                if hang {
                    cancel.cancelled().await;
                    return Err("transient GitHub failure: GitHub request cancelled".into());
                }
                if fail {
                    return Err("fixture failure".into());
                }
                Ok(EventGithubData {
                    prs: json!({"branches": branches}),
                    merge_queue: json!({}),
                    budget,
                })
            })
        }
    }

    fn test_config(dir: &TempDir) -> EventDaemonConfig {
        let mut config = EventDaemonConfig::new(
            "127.0.0.1",
            0,
            "test-secret",
            dir.path().to_owned(),
            "main",
            Some("build-current".into()),
        );
        config.debounce_ms = 3;
        config.min_fetch_interval_ms = 15;
        config.progress_fetch_interval_ms = 15;
        config.inventory_ttl_ms = 50;
        config
    }

    async fn http_request(
        addr: std::net::SocketAddr,
        path: &str,
        headers: &str,
        body: &[u8],
    ) -> u16 {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let request = format!(
            "POST {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n",
            body.len()
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        stream.write_all(body).await.unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        String::from_utf8_lossy(&response)
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap()
    }

    #[test]
    fn signature_verification_checks_exact_lowercase_sha256_mac() {
        let body = br#"{"zen":"hi"}"#;
        let mut mac = Hmac::<Sha256>::new_from_slice(b"secret").unwrap();
        mac.update(body);
        let signature = format!("sha256={}", hex(&mac.finalize().into_bytes()));
        assert!(verify_signature(b"secret", Some(&signature), body));
        assert!(!verify_signature(b"wrong", Some(&signature), body));
        assert!(!verify_signature(
            b"secret",
            Some(&signature.to_uppercase()),
            body
        ));
        assert!(!verify_signature(b"secret", None, body));
    }

    #[test]
    fn branch_extraction_preserves_unscoped_and_irrelevant_distinction() {
        assert_eq!(
            extract_event_branches("pull_request", &json!({}), "main"),
            None
        );
        assert_eq!(
            extract_event_branches("push", &json!({"ref":"refs/heads/feature-a"}), "main"),
            Some(vec!["feature-a".into()])
        );
        assert_eq!(
            extract_event_branches("push", &json!({"ref":"refs/heads/main"}), "main"),
            None
        );
        assert_eq!(
            extract_event_branches("issue_comment", &json!({"issue":{}}), "main"),
            Some(vec![])
        );
        assert_eq!(
            extract_event_branches(
                "issue_comment",
                &json!({"issue":{"pull_request":{}}}),
                "main"
            ),
            None
        );
        assert_eq!(
            extract_event_branches(
                "check_run",
                &json!({"check_run":{"check_suite":{"head_branch":"topic"}}}),
                "main"
            ),
            Some(vec!["topic".into()])
        );
    }

    #[test]
    fn pull_request_events_are_scoped_to_their_head_and_base() {
        let pr = |head: &str, base: &str| json!({"action":"synchronize","pull_request":{"head":{"ref":head},"base":{"ref":base}}});
        for event in [
            "pull_request",
            "pull_request_review",
            "pull_request_review_thread",
        ] {
            // Someone else's PR into the trunk names only its own head.
            assert_eq!(
                extract_event_branches(event, &pr("theirs", "main"), "main"),
                Some(vec!["theirs".into()]),
                "{event}"
            );
            // A PR stacked on another branch concerns that branch too.
            assert_eq!(
                extract_event_branches(event, &pr("child", "feature-x"), "main"),
                Some(vec!["child".into(), "feature-x".into()]),
                "{event}"
            );
            for partial in [
                json!({"pull_request":{"head":{"ref":"theirs"}}}),
                json!({"pull_request":{"base":{"ref":"main"}}}),
                json!({"pull_request":{"head":{"ref":""},"base":{"ref":"main"}}}),
                json!({"pull_request":{"head":{"ref":7},"base":{"ref":"main"}}}),
            ] {
                assert_eq!(
                    extract_event_branches(event, &partial, "main"),
                    None,
                    "{event} {partial}"
                );
            }
        }
        // Retargeted off a local branch: its old base lost a child.
        let mut retargeted = pr("child", "main");
        retargeted["changes"] = json!({"base":{"ref":{"from":"feature-x"}}});
        assert_eq!(
            extract_event_branches("pull_request", &retargeted, "main"),
            Some(vec!["child".into(), "feature-x".into()])
        );
        for action in ["enqueued", "dequeued"] {
            let mut queued = pr("theirs", "main");
            queued["action"] = json!(action);
            assert_eq!(
                extract_event_branches("pull_request", &queued, "main"),
                None
            );
        }
        assert_eq!(
            extract_event_branches("merge_group", &json!({}), "main"),
            None
        );
    }

    #[test]
    fn status_events_are_scoped_to_the_branches_they_name() {
        let status = |names: &[&str]| json!({"branches": names.iter().map(|name| json!({"name": name})).collect::<Vec<_>>()});
        assert_eq!(
            extract_event_branches("status", &status(&["theirs", "other"]), "main"),
            Some(vec!["theirs".into(), "other".into()])
        );
        assert_eq!(extract_event_branches("status", &json!({}), "main"), None);
        assert_eq!(extract_event_branches("status", &status(&[]), "main"), None);
        assert_eq!(
            extract_event_branches("status", &status(&["theirs", "main"]), "main"),
            None
        );
        assert_eq!(
            extract_event_branches("status", &json!({"branches":[{"name":"a"},{}]}), "main"),
            None
        );
        let full: Vec<String> = (0..STATUS_BRANCH_LIMIT).map(|n| format!("b{n}")).collect();
        let full: Vec<&str> = full.iter().map(String::as_str).collect();
        assert_eq!(
            extract_event_branches("status", &status(&full), "main"),
            None
        );
    }

    #[test]
    fn failed_reads_back_off_and_rate_limits_wait_longest() {
        assert_eq!(backoff_interval_ms(10_000, 0, false), 10_000);
        assert_eq!(backoff_interval_ms(10_000, 1, false), 20_000);
        assert_eq!(backoff_interval_ms(10_000, 3, false), 80_000);
        assert_eq!(
            backoff_interval_ms(10_000, 40, false),
            MAX_FAILURE_BACKOFF_MS
        );
        assert_eq!(backoff_interval_ms(10_000, 1, true), MAX_FAILURE_BACKOFF_MS);
        // A known reset is waited out once, with slack.
        let now = 1_000_000;
        assert_eq!(
            failure_wait_ms(10_000, 1, true, Some(now + 40 * 60_000), now),
            40 * 60_000 + RESET_SLACK_MS
        );
        // Not for another kind of failure, a reset already past, or one too
        // far off to be a rate-limit window.
        assert_eq!(
            failure_wait_ms(10_000, 1, false, Some(now + 40 * 60_000), now),
            20_000
        );
        for reset in [None, Some(now - 1), Some(now + 3 * 60 * 60_000)] {
            assert_eq!(
                failure_wait_ms(10_000, 1, true, reset, now),
                MAX_FAILURE_BACKOFF_MS
            );
        }
        assert!(rate_limited(
            "GitHub refresh failed: cannot resolve GitHub repository: HTTP 403: You have exceeded a secondary rate limit."
        ));
        assert!(!rate_limited("transient GitHub failure: gh: HTTP 504"));
    }

    #[test]
    fn a_refresh_error_keeps_only_bounded_error_messages() {
        let reply = serde_json::json!({
            "data": {"repository": {"wt_0": {"nodes": [{"title": "x".repeat(70_000)}]}}},
            "errors": (0..300)
                .map(|alias| serde_json::json!({
                    "type": "INTERNAL",
                    "path": ["repository", format!("wt_{alias}")],
                    "message": "Something went wrong while executing your query.",
                }))
                .collect::<Vec<_>>(),
        });
        assert_eq!(
            bounded_error(&format!(
                "GitHub refresh failed: GitHub command failed: {reply}"
            )),
            "GitHub refresh failed: GitHub command failed: INTERNAL: Something went wrong while executing your query."
        );
        // Messages already extracted, one per branch.
        let repeated = vec!["Something went wrong"; 300].join("\n");
        assert_eq!(
            bounded_error(&format!("GitHub refresh failed: {repeated}")),
            "GitHub refresh failed: Something went wrong; Something went wrong"
        );
        let long = bounded_error(&"é".repeat(2_000));
        assert_eq!(long.chars().count(), MAX_ERROR_CHARS + 3);
        assert_eq!(bounded_error("gh: HTTP 504"), "gh: HTTP 504");
    }

    #[test]
    fn continuous_events_do_not_starve_and_floor_is_from_fetch_start() {
        assert_eq!(
            next_fetch_at(1_000, Some(0), None, 1_500, 10_000),
            Some(10_000)
        );
        assert_eq!(
            next_fetch_at(9_000, Some(0), Some(10_000), 1_500, 10_000),
            None
        );
        assert_eq!(
            next_fetch_at(10_000, Some(0), Some(10_000), 1_500, 10_000),
            None
        );
    }

    #[tokio::test]
    async fn loopback_hmac_daemon_rejects_bad_signature_and_preserves_last_good_snapshot() {
        let dir = TempDir::new().unwrap();
        let source = Arc::new(MockSource {
            fail: AtomicBool::new(false),
            fetches: AtomicUsize::new(0),
            remote_calls: AtomicUsize::new(0),
            remote_failure_after: None,
            ..MockSource::default()
        });
        let cancel = CancellationToken::new();
        let daemon = start(test_config(&dir), source.clone(), cancel.clone())
            .await
            .unwrap();
        let initial_snapshot = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(snapshot) = read_snapshot(dir.path()).await.unwrap() {
                    break snapshot;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let initial_updated_at = initial_snapshot.updated_at;
        let mut health = TcpStream::connect(daemon.local_addr()).await.unwrap();
        health
            .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut health_response = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(2),
            health.read_to_end(&mut health_response),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(String::from_utf8_lossy(&health_response).starts_with("HTTP/1.1 200"));

        let body = br#"{"ref":"refs/heads/feature-x"}"#;
        let bad = http_request(
            daemon.local_addr(),
            "/webhook",
            "x-github-event: push\r\nx-hub-signature-256: sha256=00\r\n",
            body,
        )
        .await;
        assert_eq!(bad, StatusCode::UNAUTHORIZED.as_u16());
        let mut mac = Hmac::<Sha256>::new_from_slice(b"test-secret").unwrap();
        mac.update(body);
        let signature = format!("sha256={}", hex(&mac.finalize().into_bytes()));

        let plain_issue = br#"{"issue":{}}"#;
        let mut plain_mac = Hmac::<Sha256>::new_from_slice(b"test-secret").unwrap();
        plain_mac.update(plain_issue);
        let plain_signature = format!("sha256={}", hex(&plain_mac.finalize().into_bytes()));
        let response = http_request(
            daemon.local_addr(),
            "/webhook",
            &format!("x-github-event: issue_comment\r\nx-hub-signature-256: {plain_signature}\r\n"),
            plain_issue,
        )
        .await;
        assert_eq!(response, StatusCode::OK.as_u16());
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            read_state(dir.path()).await.unwrap().unwrap().event_count,
            0
        );

        let accepted = http_request(
            daemon.local_addr(),
            "/webhook",
            &format!("x-github-event: push\r\nx-hub-signature-256: {signature}\r\n"),
            body,
        )
        .await;
        assert_eq!(accepted, StatusCode::OK.as_u16());
        let completion = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let state = read_state(dir.path()).await.unwrap().unwrap();
                let snapshot = read_snapshot(dir.path()).await.unwrap();
                if state.event_count == 1
                    && source.fetches.load(Ordering::SeqCst) >= 2
                    && snapshot
                        .as_ref()
                        .is_some_and(|snapshot| snapshot.updated_at != initial_updated_at)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        if completion.is_err() {
            panic!(
                "event was not processed; state={:?}, fetches={}",
                read_state(dir.path()).await,
                source.fetches.load(Ordering::SeqCst)
            );
        }
        assert_eq!(
            read_state(dir.path()).await.unwrap().unwrap().event_count,
            1
        );
        assert_eq!(source.fetches.load(Ordering::SeqCst), 2);
        assert!(dir.path().join("github.touch").is_file());

        let prior = read_snapshot(dir.path()).await.unwrap().unwrap();
        source.fail.store(true, Ordering::SeqCst);
        let accepted = http_request(
            daemon.local_addr(),
            "/webhook",
            &format!("x-github-event: push\r\nx-hub-signature-256: {signature}\r\n"),
            body,
        )
        .await;
        assert_eq!(accepted, StatusCode::OK.as_u16());
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if source.fetches.load(Ordering::SeqCst) >= 3
                    && read_state(dir.path())
                        .await
                        .unwrap()
                        .unwrap()
                        .last_error
                        .is_some()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(read_snapshot(dir.path()).await.unwrap().unwrap(), prior);
        cancel.cancel();
        daemon.shutdown().await;
    }

    #[test]
    fn only_ref_changing_events_fetch_origin() {
        for event in ["push", "merge_group"] {
            assert!(changes_refs(event, &json!({})), "{event}");
        }
        assert!(changes_refs(
            "pull_request",
            &json!({"action": "synchronize"})
        ));
        assert!(changes_refs("pull_request", &json!({"action": "closed"})));
        assert!(changes_refs(
            "pull_request",
            &json!({"action": "something_new"})
        ));
        assert!(!changes_refs("pull_request", &json!({"action": "labeled"})));
        for event in [
            "check_run",
            "check_suite",
            "status",
            "pull_request_review",
            "pull_request_review_thread",
            "issue_comment",
        ] {
            assert!(!changes_refs(event, &json!({})), "{event}");
        }
    }

    async fn signed(addr: std::net::SocketAddr, event: &str, body: &[u8]) -> u16 {
        let mut mac = Hmac::<Sha256>::new_from_slice(b"test-secret").unwrap();
        mac.update(body);
        let signature = format!("sha256={}", hex(&mac.finalize().into_bytes()));
        http_request(
            addr,
            "/webhook",
            &format!("x-github-event: {event}\r\nx-hub-signature-256: {signature}\r\n"),
            body,
        )
        .await
    }

    async fn wait_for_fetches(source: &MockSource, count: usize) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while source.fetches.load(Ordering::SeqCst) < count {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("GitHub refresh ran");
    }

    /// A check event refreshes GitHub without fetching origin; a push does
    /// both. The first refresh always fetches origin.
    #[tokio::test]
    async fn check_events_refresh_github_without_fetching_origin() {
        let dir = TempDir::new().unwrap();
        let source = Arc::new(MockSource::default());
        let cancel = CancellationToken::new();
        let daemon = start(test_config(&dir), source.clone(), cancel.clone())
            .await
            .unwrap();
        wait_for_fetches(&source, 1).await;
        assert_eq!(source.origin_fetches.load(Ordering::SeqCst), 1);

        let check = br#"{"check_run":{"check_suite":{"head_branch":"feature-x"}}}"#;
        assert_eq!(signed(daemon.local_addr(), "check_run", check).await, 200);
        wait_for_fetches(&source, 2).await;
        assert_eq!(source.origin_fetches.load(Ordering::SeqCst), 1);

        let push = br#"{"ref":"refs/heads/feature-x"}"#;
        assert_eq!(signed(daemon.local_addr(), "push", push).await, 200);
        wait_for_fetches(&source, 3).await;
        assert_eq!(source.origin_fetches.load(Ordering::SeqCst), 2);
        cancel.cancel();
        daemon.shutdown().await;
    }

    #[test]
    fn only_unfinished_ci_deliveries_count_as_progress() {
        for (event, payload, progress) in [
            ("check_run", json!({"action":"created"}), true),
            ("check_run", json!({"action":"rerequested"}), true),
            ("check_run", json!({"action":"completed"}), false),
            ("check_suite", json!({"action":"requested"}), true),
            ("check_suite", json!({"action":"completed"}), false),
            ("status", json!({"state":"pending"}), true),
            ("status", json!({"state":"success"}), false),
            ("status", json!({"state":"failure"}), false),
            // Unknown is not assumed to be progress.
            ("check_run", json!({}), false),
            ("check_run", json!({"action":"something_new"}), false),
            ("status", json!({}), false),
            // Review, merge queue and push deliveries are never slowed.
            ("pull_request", json!({"action":"created"}), false),
            (
                "pull_request_review",
                json!({"action":"submitted","state":"pending"}),
                false,
            ),
            ("merge_group", json!({"action":"checks_requested"}), false),
            ("push", json!({}), false),
        ] {
            assert_eq!(ci_progress(event, &payload), progress, "{event} {payload}");
        }
    }

    /// CI that has only started waits behind its own, longer floor; a result
    /// and a review are fetched on the normal one, even with progress pending.
    #[tokio::test]
    async fn unfinished_ci_waits_behind_a_longer_floor_and_results_stay_prompt() {
        let dir = TempDir::new().unwrap();
        let source = Arc::new(MockSource::default());
        let cancel = CancellationToken::new();
        let mut config = test_config(&dir);
        config.progress_fetch_interval_ms = 60_000;
        let daemon = start(config, source.clone(), cancel.clone()).await.unwrap();
        wait_for_fetches(&source, 1).await;
        let run = |action: &str| {
            json!({"action":action,"check_run":{"check_suite":{"head_branch":"feature-x"}}})
                .to_string()
                .into_bytes()
        };
        let status = |state: &str| {
            json!({"state":state,"branches":[{"name":"feature-x"}]})
                .to_string()
                .into_bytes()
        };
        for (event, body) in [
            ("check_run", run("created")),
            ("status", status("pending")),
            ("check_run", run("created")),
        ] {
            assert_eq!(signed(daemon.local_addr(), event, &body).await, 200);
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(source.fetches.load(Ordering::SeqCst), 1);
        assert_eq!(
            read_state(dir.path()).await.unwrap().unwrap().event_count,
            3
        );

        let completed = run("completed");
        assert_eq!(
            signed(daemon.local_addr(), "check_run", &completed).await,
            200
        );
        wait_for_fetches(&source, 2).await;
        let review = br#"{"action":"submitted","pull_request":{"head":{"ref":"feature-x"},"base":{"ref":"main"}}}"#;
        assert_eq!(
            signed(daemon.local_addr(), "pull_request_review", review).await,
            200
        );
        wait_for_fetches(&source, 3).await;
        cancel.cancel();
        daemon.shutdown().await;
    }

    #[test]
    fn the_fetch_floor_stretches_with_the_budget_left() {
        const MINUTE: u64 = 60_000;
        let now = 1_000 * MINUTE;
        let budget = |remaining, cost, reset_in_ms: u64| {
            Some(RateBudget {
                limit: 5_000,
                remaining,
                cost,
                reset_at_ms: now + reset_in_ms,
            })
        };
        let paced = |budget| paced_interval_ms(10_000, budget, now);
        // A full budget at the top of the hour: 5 points per read against
        // 3,750 spendable is under the floor, which stands.
        assert_eq!(paced(budget(5_000, 5, 60 * MINUTE)), 10_000);
        // 500 spendable with half an hour to go: 5 * 30 min * 2 / 500.
        assert_eq!(paced(budget(1_750, 5, 30 * MINUTE)), 36_000);
        // What the old query cost per read would have had to wait.
        assert_eq!(paced(budget(1_750, 42, 30 * MINUTE)), 302_400);
        // Only the reserve is left, or nothing at all: wait for the reset.
        for remaining in [1_250, 1_000, 0] {
            assert_eq!(
                paced(budget(remaining, 5, 20 * MINUTE)),
                20 * MINUTE + RESET_SLACK_MS
            );
        }
        // Never beyond the reset, even for one point of spendable budget.
        assert_eq!(
            paced(budget(1_251, 5, 20 * MINUTE)),
            20 * MINUTE + RESET_SLACK_MS
        );
        // A reply that reported no cost is paced as the cheapest read.
        assert_eq!(paced(budget(1_300, 0, 20 * MINUTE)), 48_000);
        // No budget reported (rate limiting off, an older server), a window
        // that has already reset, or a reset too far off to believe: the
        // floor alone, as before budgets were read.
        assert_eq!(paced(None), 10_000);
        assert_eq!(paced_interval_ms(10_000, budget(0, 5, 0), now), 10_000);
        assert_eq!(
            paced_interval_ms(10_000, budget(0, 5, MINUTE), now + 2 * MINUTE),
            10_000
        );
        assert_eq!(paced(budget(0, 5, 3 * 60 * MINUTE)), 10_000);
    }

    /// With only the reserve left the daemon stops refreshing in the
    /// background until the reset, and publishes the budget it saw.
    #[tokio::test]
    async fn a_low_budget_holds_background_refreshes_until_the_reset() {
        let dir = TempDir::new().unwrap();
        let budget = RateBudget {
            limit: 5_000,
            remaining: 900,
            cost: 5,
            reset_at_ms: now_millis() + 10 * 60_000,
        };
        let source = Arc::new(MockSource {
            budget: Some(budget),
            ..MockSource::default()
        });
        let cancel = CancellationToken::new();
        let daemon = start(test_config(&dir), source.clone(), cancel.clone())
            .await
            .unwrap();
        wait_for_fetches(&source, 1).await;
        let push = br#"{"ref":"refs/heads/feature-x"}"#;
        assert_eq!(signed(daemon.local_addr(), "push", push).await, 200);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(source.fetches.load(Ordering::SeqCst), 1);
        let snapshot = read_snapshot(dir.path()).await.unwrap().unwrap();
        assert_eq!(snapshot.rate_limit, Some(budget));
        cancel.cancel();
        daemon.shutdown().await;
    }

    /// A PR nobody here has a branch for costs neither an origin fetch nor a
    /// GitHub read; one whose head or base is a local branch still does.
    #[tokio::test]
    async fn only_pull_requests_touching_a_known_branch_schedule_a_refresh() {
        let dir = TempDir::new().unwrap();
        let source = Arc::new(MockSource::default());
        let cancel = CancellationToken::new();
        let daemon = start(test_config(&dir), source.clone(), cancel.clone())
            .await
            .unwrap();
        wait_for_fetches(&source, 1).await;
        let pr = |head: &str, base: &str| {
            json!({"action":"synchronize","pull_request":{"head":{"ref":head},"base":{"ref":base}}})
                .to_string()
                .into_bytes()
        };

        let foreign = pr("theirs", "main");
        assert_eq!(
            signed(daemon.local_addr(), "pull_request", &foreign).await,
            200
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(source.fetches.load(Ordering::SeqCst), 1);
        assert_eq!(source.origin_fetches.load(Ordering::SeqCst), 1);

        let local_head = pr("feature-x", "main");
        assert_eq!(
            signed(daemon.local_addr(), "pull_request", &local_head).await,
            200
        );
        wait_for_fetches(&source, 2).await;
        assert_eq!(source.origin_fetches.load(Ordering::SeqCst), 2);

        let local_base = pr("theirs", "feature-x");
        assert_eq!(
            signed(daemon.local_addr(), "pull_request", &local_base).await,
            200
        );
        wait_for_fetches(&source, 3).await;

        let no_refs = br#"{"action":"synchronize","pull_request":{}}"#;
        assert_eq!(
            signed(daemon.local_addr(), "pull_request", no_refs).await,
            200
        );
        wait_for_fetches(&source, 4).await;
        cancel.cancel();
        daemon.shutdown().await;
    }

    /// Restarting the daemon cancels the read in flight. That is not a
    /// GitHub failure, and must not be left behind as the daemon's last error.
    #[tokio::test]
    async fn a_refresh_cancelled_by_shutdown_records_no_error() {
        let dir = TempDir::new().unwrap();
        let source = Arc::new(MockSource::default());
        source.hang_until_cancelled.store(true, Ordering::SeqCst);
        let cancel = CancellationToken::new();
        let daemon = start(test_config(&dir), source.clone(), cancel.clone())
            .await
            .unwrap();
        wait_for_fetches(&source, 1).await;
        cancel.cancel();
        daemon.shutdown().await;
        let state = read_state(dir.path()).await.unwrap().unwrap();
        assert_eq!(state.last_error, None);
    }

    /// No webhook follows the startup refresh, so only the daemon's own
    /// retry can publish a snapshot after it failed.
    #[tokio::test]
    async fn a_failed_refresh_is_retried_without_another_event() {
        let dir = TempDir::new().unwrap();
        let source = Arc::new(MockSource::default());
        source.fail.store(true, Ordering::SeqCst);
        let cancel = CancellationToken::new();
        let daemon = start(test_config(&dir), source.clone(), cancel.clone())
            .await
            .unwrap();
        wait_for_fetches(&source, 2).await;
        source.fail.store(false, Ordering::SeqCst);
        tokio::time::timeout(Duration::from_secs(2), async {
            while read_snapshot(dir.path()).await.unwrap().is_none()
                || read_state(dir.path())
                    .await
                    .unwrap()
                    .unwrap()
                    .last_error
                    .is_some()
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the retry published a snapshot and cleared the error");
        cancel.cancel();
        daemon.shutdown().await;
    }

    #[tokio::test]
    async fn remote_inventory_error_marks_coverage_unknown_but_keeps_last_good_branches() {
        let dir = TempDir::new().unwrap();
        let source = Arc::new(MockSource {
            fail: AtomicBool::new(false),
            fetches: AtomicUsize::new(0),
            remote_calls: AtomicUsize::new(0),
            remote_failure_after: Some(1),
            ..MockSource::default()
        });
        let cancel = CancellationToken::new();
        let mut config = test_config(&dir);
        config.inventory_ttl_ms = 0;
        let daemon = start(config, source.clone(), cancel.clone()).await.unwrap();

        let initial = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(snapshot) = read_snapshot(dir.path()).await.unwrap()
                    && snapshot
                        .branches
                        .iter()
                        .any(|branch| branch == "remote-only")
                {
                    break snapshot;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            initial
                .branches
                .iter()
                .any(|branch| branch == "remote-only")
        );

        let body = br#"{"check_run":{"check_suite":{"head_branch":"new-remote-branch"}}}"#;
        let mut mac = Hmac::<Sha256>::new_from_slice(b"test-secret").unwrap();
        mac.update(body);
        let signature = format!("sha256={}", hex(&mac.finalize().into_bytes()));
        let response = http_request(
            daemon.local_addr(),
            "/webhook",
            &format!("x-github-event: check_run\r\nx-hub-signature-256: {signature}\r\n"),
            body,
        )
        .await;
        assert_eq!(response, StatusCode::OK.as_u16());

        let refreshed = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let state = read_state(dir.path()).await.unwrap().unwrap();
                let snapshot = read_snapshot(dir.path()).await.unwrap();
                if state.event_count == 1
                    && source.fetches.load(Ordering::SeqCst) >= 2
                    && source.remote_calls.load(Ordering::SeqCst) >= 2
                    && snapshot
                        .as_ref()
                        .is_some_and(|snapshot| snapshot.updated_at != initial.updated_at)
                {
                    break snapshot.unwrap();
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();

        assert!(
            refreshed
                .branches
                .iter()
                .any(|branch| branch == "remote-only")
        );
        assert_eq!(
            read_state(dir.path()).await.unwrap().unwrap().event_count,
            1,
            "a check_run for a branch outside stale inventory must schedule a refresh"
        );
        cancel.cancel();
        daemon.shutdown().await;
    }

    #[tokio::test]
    async fn oversized_request_is_rejected_before_body_collection() {
        let dir = TempDir::new().unwrap();
        let source = Arc::new(MockSource {
            fail: AtomicBool::new(false),
            fetches: AtomicUsize::new(0),
            remote_calls: AtomicUsize::new(0),
            remote_failure_after: None,
            ..MockSource::default()
        });
        let cancel = CancellationToken::new();
        let mut config = test_config(&dir);
        config.max_body_bytes = 8;
        let daemon = start(config, source, cancel.clone()).await.unwrap();
        let mut stream = TcpStream::connect(daemon.local_addr()).await.unwrap();
        stream.write_all(b"POST /webhook HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: 100\r\nx-github-event: push\r\n\r\n").await.unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert!(String::from_utf8_lossy(&response).starts_with("HTTP/1.1 413"));
        cancel.cancel();
        daemon.shutdown().await;
    }

    #[tokio::test]
    async fn partial_headers_are_capped_and_shutdown_closes_idle_connections() {
        let dir = TempDir::new().unwrap();
        let source = Arc::new(MockSource {
            fail: AtomicBool::new(false),
            fetches: AtomicUsize::new(0),
            remote_calls: AtomicUsize::new(0),
            remote_failure_after: None,
            ..MockSource::default()
        });
        let cancel = CancellationToken::new();
        let mut config = test_config(&dir);
        config.max_connections = 2;
        let daemon = start(config, source, cancel.clone()).await.unwrap();

        let mut occupied = Vec::new();
        for _ in 0..2 {
            let mut stream = TcpStream::connect(daemon.local_addr()).await.unwrap();
            stream
                .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\n")
                .await
                .unwrap();
            occupied.push(stream);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;

        for _ in 0..8 {
            let mut stream = TcpStream::connect(daemon.local_addr()).await.unwrap();
            stream
                .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\n")
                .await
                .unwrap();
            let mut response = Vec::new();
            let read = tokio::time::timeout(
                Duration::from_millis(500),
                stream.read_to_end(&mut response),
            )
            .await
            .expect("over-cap connection should be dropped promptly");
            if let Err(error) = read {
                assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);
            }
            assert!(
                response.is_empty(),
                "over-cap client must not reach the handler"
            );
        }

        tokio::time::timeout(Duration::from_millis(500), daemon.shutdown())
            .await
            .expect("shutdown must abort and join partial-header connections");
        for stream in &mut occupied {
            let mut response = Vec::new();
            let read = tokio::time::timeout(
                Duration::from_millis(200),
                stream.read_to_end(&mut response),
            )
            .await
            .expect("server shutdown must close the accepted socket");
            if let Err(error) = read {
                assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);
            }
            assert!(response.is_empty());
        }
    }

    #[tokio::test]
    async fn daemon_roundtrip_preserves_unknown_state_and_snapshot_fields() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(
            dir.path().join("state.json"),
            json!({
                "pid": 17, "port": 8765, "writerSha": "old", "startedAt": 1,
                "lastEventAt": null, "lastFetchAt": null, "eventCount": 8,
                "lastError": null, "futureState": {"retain": true}
            })
            .to_string(),
        )
        .await
        .unwrap();
        tokio::fs::write(
            dir.path().join("github.json"),
            json!({
                "updatedAt": 1, "branches": ["old"], "prs": {}, "mergeQueue": {},
                "writerSha": "old", "futureSnapshot": ["retain"]
            })
            .to_string(),
        )
        .await
        .unwrap();
        let source = Arc::new(MockSource {
            fail: AtomicBool::new(false),
            fetches: AtomicUsize::new(0),
            remote_calls: AtomicUsize::new(0),
            remote_failure_after: None,
            ..MockSource::default()
        });
        let cancel = CancellationToken::new();
        let daemon = start(test_config(&dir), source, cancel.clone())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let snapshot = read_snapshot(dir.path()).await.unwrap().unwrap();
                if snapshot.updated_at > 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let state = read_state(dir.path()).await.unwrap().unwrap();
        let snapshot = read_snapshot(dir.path()).await.unwrap().unwrap();
        assert_eq!(
            state.extra.get("futureState"),
            Some(&json!({"retain": true}))
        );
        assert_eq!(
            snapshot.extra.get("futureSnapshot"),
            Some(&json!(["retain"]))
        );
        assert_eq!(snapshot.branches, ["feature-x", "remote-only"]);
        cancel.cancel();
        daemon.shutdown().await;
    }

    #[tokio::test]
    async fn malformed_cache_is_unusable_and_malformed_state_is_reported() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("github.json"), b"not-json")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("state.json"), b"not-json")
            .await
            .unwrap();
        assert!(read_snapshot(dir.path()).await.unwrap().is_none());
        assert!(matches!(
            read_state(dir.path()).await,
            Err(crate::EventDaemonError::Json(_))
        ));
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}
