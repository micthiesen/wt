use std::{
    collections::{BTreeMap, BTreeSet},
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
    BudgetWindows, EventDaemonConfig, EventDaemonError, EventDaemonState, EventSource,
    EventsDaemon, GithubSnapshot, RateBudget,
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

/// Whether a delivery reports only the checks of the branches it names.
/// Nothing else a read returns moves with it, so a read of those branches
/// alone serves it. Pull request, review, push and merge queue deliveries
/// can move other pull requests (a retarget, a merge, a queue position) and
/// follow this host's own writes, which only a read of every branch settles.
pub(super) fn scoped_event(event: &str) -> bool {
    matches!(event, "check_run" | "check_suite" | "status")
}

/// The most branches a scoped read asks about. The reader batches eight
/// branches to a request, so this is one request's worth.
const SCOPED_BRANCH_LIMIT: usize = 8;

/// What one read asks GitHub about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ReadScope {
    /// Every branch.
    Full,
    /// Only these, merged into the last read of every branch.
    Branches(BTreeSet<String>),
}

/// What the deliveries waiting on the next read ask for, and what there is
/// to merge a scoped read into.
pub(super) struct ScopeInput<'a> {
    /// A delivery that a scoped read cannot serve is waiting.
    pub full_asked: bool,
    /// Branches named by check and status deliveries.
    pub named: &'a BTreeSet<String>,
    /// The last snapshot this process wrote.
    pub base: Option<&'a GithubSnapshot>,
    /// The branches a read of every branch would ask about now.
    pub branches: &'a BTreeSet<String>,
    /// Time since the last read of every branch started.
    pub sweep_age_ms: Option<u64>,
    pub max_sweep_age_ms: u64,
}

/// A scoped read is taken only when it loses nothing: every waiting
/// delivery names its branches, the last read of every branch covers
/// exactly today's branches and is recent, and the named branches fit one
/// request. Anything else reads every branch.
pub(super) fn read_scope(input: ScopeInput<'_>) -> ReadScope {
    let ScopeInput {
        full_asked,
        named,
        base,
        branches,
        sweep_age_ms,
        max_sweep_age_ms,
    } = input;
    let covered = base.is_some_and(|base| {
        base.prs.is_object()
            && base.branches.len() == branches.len()
            && base.branches.iter().all(|branch| branches.contains(branch))
    });
    if full_asked
        || !covered
        || named.is_empty()
        || named.len() > SCOPED_BRANCH_LIMIT
        || !named.is_subset(branches)
        || sweep_age_ms.is_none_or(|age| age >= max_sweep_age_ms)
    {
        ReadScope::Full
    } else {
        ReadScope::Branches(named.clone())
    }
}

/// How old the last read of every branch is. The snapshot's own wall-clock
/// record decides when it has one: the process clock stops while the machine
/// sleeps, and a base from before a long sleep must not pass for recent.
pub(super) fn sweep_age_ms(
    base: Option<&GithubSnapshot>,
    wall_now_ms: u64,
    since_start_ms: Option<u64>,
) -> Option<u64> {
    // A wall clock behind the stamp was set back and measures nothing.
    base.and_then(|base| base.swept_at_ms)
        .and_then(|swept| wall_now_ms.checked_sub(swept))
        .or(since_start_ms)
}

/// How long a read of every branch must still wait for the pace its budget
/// asks for, counted from the start of the last one, on the wall clock when
/// the snapshot records that start.
pub(super) fn full_wait_ms(
    base: Option<&GithubSnapshot>,
    wall_now_ms: u64,
    since_start_ms: Option<u64>,
    full_paced_ms: u64,
) -> u64 {
    base.and_then(|base| base.fetch_started_at_ms)
        .and_then(|started| wall_now_ms.checked_sub(started))
        .or(since_start_ms)
        .map_or(0, |since| full_paced_ms.saturating_sub(since))
}

/// A deadline no earlier than the postponed read of every branch it would
/// only be postponed to again.
fn held_deadline(deadline: Option<Instant>, hold: Option<Instant>) -> Option<Instant> {
    match (deadline, hold) {
        (Some(deadline), Some(hold)) => Some(deadline.max(hold)),
        (deadline, _) => deadline,
    }
}

/// `base` with the pull requests of `named` replaced by what a read of
/// exactly those branches returned: a branch the read found no pull request
/// for loses its entry, and no other branch is touched. The merge queue is
/// repository-wide and every read returns it, so the newer one stands. The
/// snapshot keeps the read start of its oldest data; `branch_reads` records
/// which branches are newer. `None` when either side is not a branch map.
pub(super) fn merge_scoped(
    base: &GithubSnapshot,
    named: &BTreeSet<String>,
    prs: &Value,
    merge_queue: Value,
    started_ms: u64,
) -> Option<GithubSnapshot> {
    let read = prs.as_object()?;
    let mut merged = base.clone();
    let shown = merged.prs.as_object_mut()?;
    for branch in named {
        match read.get(branch) {
            Some(pr) => shown.insert(branch.clone(), pr.clone()),
            None => shown.remove(branch),
        };
        merged.branch_reads.insert(branch.clone(), started_ms);
    }
    merged.merge_queue = merge_queue;
    Some(merged)
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

/// The longest a budget stretches the floor. A reader that stopped until
/// the reset showed nothing new for up to an hour; four reads an hour out
/// of the reserve keep the board moving.
pub const MAX_PACED_WAIT_MS: u64 = 15 * 60 * 1000;

/// How long until `budget`'s window resets, while it is open and its reset
/// is near enough to believe.
fn open_window_ms(budget: RateBudget, now_ms: u64) -> Option<u64> {
    budget
        .reset_at_ms
        .checked_sub(now_ms)
        .filter(|wait| *wait > 0 && *wait <= MAX_RESET_WAIT_MS)
}

/// Whether `budget`, in a window still open at `now_ms`, is down to the
/// reserve background reads leave alone.
pub fn budget_low(budget: RateBudget, now_ms: u64) -> bool {
    open_window_ms(budget, now_ms).is_some() && budget.remaining <= budget.limit / RESERVE_DIVISOR
}

/// The floor between starts of background GitHub reads, given the budget
/// the last one left: `floor_ms`, stretched so that reads costing what the
/// last one did would use the unreserved budget evenly until it resets, up
/// to the reset or [`MAX_PACED_WAIT_MS`], whichever is sooner; that is also
/// the interval once only the reserve is left. A budget that is unknown,
/// already reset, or resetting implausibly far away leaves the floor as it
/// is. User-initiated reads do not ask this.
pub fn paced_interval_ms(floor_ms: u64, budget: Option<RateBudget>, now_ms: u64) -> u64 {
    let Some(budget) = budget else {
        return floor_ms;
    };
    let Some(to_reset) = open_window_ms(budget, now_ms) else {
        return floor_ms;
    };
    let longest = to_reset
        .saturating_add(RESET_SLACK_MS)
        .min(MAX_PACED_WAIT_MS);
    let spendable = budget
        .remaining
        .saturating_sub(budget.limit / RESERVE_DIVISOR);
    if spendable == 0 {
        return longest.max(floor_ms);
    }
    let paced = budget
        .cost
        .max(1)
        .saturating_mul(to_reset)
        .saturating_mul(PACE_SAFETY)
        / spendable;
    paced.min(longest).max(floor_ms)
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
    // What the waiting deliveries ask of the next read. The first read, a
    // retry and anything a scoped read cannot serve read every branch.
    let mut full_asked = true;
    let mut named_branches: BTreeSet<String> = BTreeSet::new();
    // The last snapshot this process wrote, which a scoped read merges into.
    let mut base: Option<GithubSnapshot> = None;
    let mut last_full_start: Option<Instant> = None;
    // The floor between reads of every branch that the budget asks for, and
    // what the last such read cost.
    let mut full_paced_ms = config.min_fetch_interval_ms;
    let mut full_cost = 0u64;
    // A read of every branch that its pace postponed is due then. Deliveries
    // arriving meanwhile wait for it instead of waking the scheduler early.
    let mut full_hold: Option<Instant> = None;
    let mut windows = BudgetWindows::default();
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
                let (relevant, known) = match candidates {
                    Some(names) if names.is_empty() => (false, None),
                    None => (true, None),
                    Some(_) if !(local_known && remote_known) => (true, None),
                    Some(names) => {
                        let known: Vec<String> = names.into_iter().filter(|branch| {
                            local_branches.contains(branch) || remote_branches.contains(branch)
                        }).collect();
                        (!known.is_empty(), Some(known))
                    }
                };
                let matched = known.as_ref().and_then(|known| known.first().cloned());
                if relevant {
                    match known {
                        Some(known) if scoped_event(&event.name) => named_branches.extend(known),
                        _ => full_asked = true,
                    }
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
                    pending_deadline = held_deadline(pending_deadline, full_asked.then_some(full_hold).flatten());
                }
            }
            _ = coalesced_rx.recv() => {
                full_asked = true;
                origin_trigger.get_or_insert_with(|| ("coalesced overflow".into(), None));
                let now_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
                let last_ms = last_fetch_start.map(|instant| instant.duration_since(started).as_millis().min(u64::MAX as u128) as u64);
                let pending_ms = pending_deadline.map(|instant| instant.duration_since(started).as_millis().min(u64::MAX as u128) as u64);
                if let Some(deadline) = next_fetch_at(now_ms, last_ms, pending_ms, config.debounce_ms, fetch_interval_ms) {
                    pending_deadline = Some(started + Duration::from_millis(deadline));
                }
                pending_deadline = held_deadline(pending_deadline, full_hold);
            }
            _ = sleep => {
                pending_deadline = None;
                let now = Instant::now();
                let prior_fetch_start = last_fetch_start;
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
                let wall_now_ms = now_millis();
                let since_full_ms = last_full_start.map(|at| now.duration_since(at).as_millis().min(u64::MAX as u128) as u64);
                let scope = read_scope(ScopeInput {
                    full_asked,
                    named: &named_branches,
                    base: base.as_ref(),
                    branches: &fetch_branches,
                    sweep_age_ms: sweep_age_ms(base.as_ref(), wall_now_ms, since_full_ms),
                    max_sweep_age_ms: config.full_sweep_interval_ms,
                });
                // Scoped reads cost a fraction of a read of every branch, so
                // their pace does not hold one of those to the budget.
                let wait_ms = full_wait_ms(base.as_ref(), wall_now_ms, since_full_ms, full_paced_ms);
                if scope == ReadScope::Full && wait_ms > 0 {
                    let due = now + Duration::from_millis(wait_ms);
                    full_asked = true;
                    full_hold = Some(due);
                    last_fetch_start = prior_fetch_start;
                    pending_deadline = Some(due);
                    continue;
                }
                full_hold = None;
                full_asked = false;
                named_branches.clear();
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
                let asked: Vec<String> = match &scope {
                    ReadScope::Full => fetch_branches.iter().cloned().collect(),
                    ReadScope::Branches(named) => named.iter().cloned().collect(),
                };
                let fetched = source.fetch_github(asked, &cancel).await;
                let mut paced_ms = config.min_fetch_interval_ms;
                let mut reset_at_ms = None;
                let failure = match fetched {
                    Ok(data) => {
                        let finished_ms = now_millis();
                        let budget = data.budget.and_then(|read| windows.observe(read, finished_ms));
                        // A busy hour stretches the floor instead of running the
                        // budget out; the floor still counts from this fetch's start.
                        paced_ms = paced_interval_ms(config.min_fetch_interval_ms, budget, finished_ms);
                        let rate_windows = Some(windows.open(finished_ms)).filter(|open| open.len() > 1).unwrap_or_default();
                        let snapshot = match &scope {
                            ReadScope::Full => {
                                full_cost = budget.map_or(0, |budget| budget.cost);
                                full_paced_ms = paced_ms;
                                Some(GithubSnapshot {
                                    updated_at: finished_ms, fetch_started_at_ms: Some(fetch_started_at_ms),
                                    swept_at_ms: Some(finished_ms), branch_reads: BTreeMap::new(),
                                    branches: fetch_branches.into_iter().collect(), prs: data.prs,
                                    merge_queue: data.merge_queue, rate_limit: budget, rate_windows,
                                    next_fetch_at_ms: Some(fetch_started_at_ms.saturating_add(paced_ms)),
                                    writer_sha: config.writer_sha.clone(),
                                    repository: config.repository.clone(),
                                    extra: read_json::<GithubSnapshot>(config.events_dir.join("github.json")).await.ok().flatten().map(|snapshot| snapshot.extra).unwrap_or_default(),
                                })
                            }
                            ReadScope::Branches(named) => base.as_ref()
                                .and_then(|base| merge_scoped(base, named, &data.prs, data.merge_queue, fetch_started_at_ms))
                                .map(|mut merged| {
                                    // The next read of every branch is paced by what one costs.
                                    full_paced_ms = paced_interval_ms(
                                        config.min_fetch_interval_ms,
                                        budget.map(|budget| RateBudget { cost: full_cost.max(budget.cost), ..budget }),
                                        finished_ms,
                                    );
                                    merged.updated_at = finished_ms;
                                    // Readers pace their own reads of every
                                    // branch by this cost, not a scoped read's.
                                    merged.rate_limit = budget.map(|budget| RateBudget { cost: full_cost.max(budget.cost), ..budget });
                                    merged.rate_windows = rate_windows;
                                    merged.next_fetch_at_ms = merged.fetch_started_at_ms.map(|started| started.saturating_add(full_paced_ms));
                                    merged
                                }),
                        };
                        let written = match snapshot {
                            Some(snapshot) => durable.write_snapshot(&snapshot).await.map(|()| snapshot).map_err(|e| format!("cannot persist GitHub snapshot: {e}")),
                            None => Err("a scoped GitHub read had nothing to merge into".to_owned()),
                        };
                        match written {
                            Ok(snapshot) => {
                                if scope == ReadScope::Full {
                                    last_full_start = Some(now);
                                }
                                let mut state = durable.state.lock().await;
                                state.last_fetch_at = Some(snapshot.updated_at);
                                state.last_error = None;
                                let _ = durable.write_state_locked(&state).await;
                                if let Err(e) = atomic_write(&config.events_dir.join("github.touch"), snapshot.updated_at.to_string().as_bytes()).await { warn!(error = %e, "cannot write GitHub snapshot marker"); }
                                base = Some(snapshot);
                                None
                            }
                            Err(message) => Some(message),
                        }
                    }
                    Err(e) => {
                        reset_at_ms = e.rate_limit_reset_ms;
                        Some(format!("GitHub refresh failed: {}", e.message))
                    }
                };
                match failure {
                    None => (failures, fetch_interval_ms) = (0, paced_ms),
                    // A read cut short by shutdown says nothing about GitHub.
                    Some(message) if cancel.is_cancelled() => {
                        debug!(error = %message, "GitHub refresh interrupted by shutdown");
                        break;
                    }
                    Some(message) => {
                        // Nothing was merged or written, and what a read of
                        // a few branches would have shown is unknown: the
                        // retry reads every branch.
                        full_asked = true;
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
        /// Budgets successive reads report, before `budget` takes over.
        budgets: std::sync::Mutex<std::collections::VecDeque<RateBudget>>,
        /// Report pull requests keyed by branch, each naming the read that
        /// returned it, for every branch asked about except these.
        by_branch: AtomicBool,
        without_pr: std::sync::Mutex<BTreeSet<String>>,
        /// The branches each read asked about.
        asked: std::sync::Mutex<Vec<Vec<String>>>,
        fetches: AtomicUsize,
        local_calls: AtomicUsize,
        origin_fetches: AtomicUsize,
        remote_calls: AtomicUsize,
        remote_failure_after: Option<usize>,
    }

    impl EventSource for MockSource {
        fn local_branches<'a>(
            &'a self,
            _: &'a CancellationToken,
        ) -> EventFuture<'a, Result<Vec<String>, String>> {
            self.local_calls.fetch_add(1, Ordering::SeqCst);
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
            let read = self.fetches.fetch_add(1, Ordering::SeqCst) + 1;
            self.asked.lock().unwrap().push(branches.clone());
            let budget = self.budgets.lock().unwrap().pop_front().or(self.budget);
            let by_branch = self.by_branch.load(Ordering::SeqCst);
            let without_pr = self.without_pr.lock().unwrap().clone();
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
                if by_branch {
                    let prs: serde_json::Map<String, Value> = branches
                        .iter()
                        .filter(|branch| !without_pr.contains(*branch))
                        .map(|branch| (branch.clone(), json!({"read": read})))
                        .collect();
                    return Ok(EventGithubData {
                        prs: Value::Object(prs),
                        merge_queue: json!({"read": read}),
                        budget,
                    });
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

    async fn wait_for_snapshot(
        dir: &TempDir,
        ready: impl Fn(&GithubSnapshot) -> bool,
    ) -> GithubSnapshot {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(snapshot) = read_snapshot(dir.path()).await.unwrap()
                    && ready(&snapshot)
                {
                    return snapshot;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the snapshot was written")
    }

    fn names(branches: &[&str]) -> BTreeSet<String> {
        branches.iter().map(|branch| (*branch).to_owned()).collect()
    }

    #[test]
    fn a_scoped_read_is_taken_only_when_it_loses_nothing() {
        let base = GithubSnapshot {
            branches: vec!["a".into(), "b".into()],
            prs: json!({"a": {}}),
            ..GithubSnapshot::default()
        };
        let all = names(&["a", "b"]);
        let named = names(&["a"]);
        let scope = |full_asked,
                     named: &BTreeSet<String>,
                     base: Option<&GithubSnapshot>,
                     branches: &BTreeSet<String>,
                     sweep_age_ms| {
            read_scope(ScopeInput {
                full_asked,
                named,
                base,
                branches,
                sweep_age_ms,
                max_sweep_age_ms: 45_000,
            })
        };
        assert_eq!(
            scope(false, &named, Some(&base), &all, Some(44_999)),
            ReadScope::Branches(named.clone())
        );
        // A delivery a scoped read cannot serve, or none naming a branch.
        assert_eq!(
            scope(true, &named, Some(&base), &all, Some(0)),
            ReadScope::Full
        );
        assert_eq!(
            scope(false, &names(&[]), Some(&base), &all, Some(0)),
            ReadScope::Full
        );
        // Nothing to merge into, or a read of every branch that is too old.
        assert_eq!(scope(false, &named, None, &all, Some(0)), ReadScope::Full);
        assert_eq!(
            scope(false, &named, Some(&base), &all, None),
            ReadScope::Full
        );
        assert_eq!(
            scope(false, &named, Some(&base), &all, Some(45_000)),
            ReadScope::Full
        );
        // The branches changed since the last read of every branch, in
        // either direction, or the named branch is not one of them.
        assert_eq!(
            scope(
                false,
                &named,
                Some(&base),
                &names(&["a", "b", "c"]),
                Some(0)
            ),
            ReadScope::Full
        );
        assert_eq!(
            scope(false, &named, Some(&base), &names(&["a"]), Some(0)),
            ReadScope::Full
        );
        assert_eq!(
            scope(false, &names(&["z"]), Some(&base), &all, Some(0)),
            ReadScope::Full
        );
        // More branches than one request reads.
        let many: BTreeSet<String> = (0..=SCOPED_BRANCH_LIMIT).map(|n| format!("b{n}")).collect();
        let wide = GithubSnapshot {
            branches: many.iter().cloned().collect(),
            prs: json!({}),
            ..GithubSnapshot::default()
        };
        assert_eq!(
            scope(false, &many, Some(&wide), &many, Some(0)),
            ReadScope::Full
        );
        // A snapshot whose pull requests are not a branch map cannot be merged into.
        let odd = GithubSnapshot {
            prs: json!([]),
            ..base.clone()
        };
        assert_eq!(
            scope(false, &named, Some(&odd), &all, Some(0)),
            ReadScope::Full
        );
    }

    /// The process clock stops while the machine sleeps. After a long sleep
    /// the last read of every branch is as old as the wall clock says, so a
    /// check delivery does not merge into an hours-old base, and the pace of
    /// the next read of every branch has long passed.
    #[test]
    fn the_age_of_the_last_sweep_is_taken_from_the_wall_clock() {
        const HOUR: u64 = 3_600_000;
        let base = GithubSnapshot {
            fetch_started_at_ms: Some(10_000),
            swept_at_ms: Some(18_000),
            ..GithubSnapshot::default()
        };
        // Five seconds on the process clock, three hours on the wall.
        let now = 18_000 + 3 * HOUR;
        assert_eq!(sweep_age_ms(Some(&base), now, Some(5_000)), Some(3 * HOUR));
        assert_eq!(full_wait_ms(Some(&base), now, Some(5_000), 600_000), 0);
        // Awake, both clocks agree and the pace still holds.
        assert_eq!(sweep_age_ms(Some(&base), 20_000, Some(10_000)), Some(2_000));
        assert_eq!(
            full_wait_ms(Some(&base), 20_000, Some(10_000), 600_000),
            590_000
        );
        // A wall clock set back an hour measures nothing, so the process
        // clock does: the postponed read waits out what is left of one
        // pace and is then due, instead of being postponed by a whole pace
        // each time it comes up, and the sweep still ages.
        let late = GithubSnapshot {
            fetch_started_at_ms: Some(2 * HOUR),
            swept_at_ms: Some(2 * HOUR + 8_000),
            ..GithubSnapshot::default()
        };
        let set_back = HOUR;
        let mut since_start = 10_000;
        let mut postponed = 0;
        loop {
            let wait = full_wait_ms(
                Some(&late),
                set_back + since_start,
                Some(since_start),
                600_000,
            );
            if wait == 0 {
                break;
            }
            postponed += 1;
            since_start += wait;
        }
        assert_eq!((postponed, since_start), (1, 600_000));
        assert_eq!(
            sweep_age_ms(Some(&late), set_back + since_start, Some(since_start)),
            Some(600_000)
        );
        assert_eq!(sweep_age_ms(Some(&late), set_back, None), None);
        // No wall-clock record: the process clock, or nothing to wait for.
        let bare = GithubSnapshot::default();
        assert_eq!(sweep_age_ms(Some(&bare), now, Some(5_000)), Some(5_000));
        assert_eq!(
            full_wait_ms(Some(&bare), now, Some(5_000), 600_000),
            595_000
        );
        assert_eq!(sweep_age_ms(None, now, None), None);
        assert_eq!(full_wait_ms(None, now, None, 600_000), 0);
    }

    /// A read of every branch that its pace postponed is taken once, when it
    /// is due: deliveries arriving meanwhile neither read GitHub nor wake
    /// the scheduler to list the worktrees and postpone it again.
    #[tokio::test]
    async fn deliveries_during_a_postponed_sweep_cause_one_read_when_it_is_due() {
        let dir = TempDir::new().unwrap();
        let now = now_millis();
        // The first read of every branch finds the budget full and paces
        // by 0.15 seconds. The scoped read finds 200 points left to spend
        // with 40 seconds to go: 0.4 seconds between one-point reads, and
        // 2.8 between seven-point reads of every branch.
        let budget = |cost, remaining| RateBudget {
            limit: 5_000,
            remaining,
            cost,
            reset_at_ms: now + 40_000,
        };
        let source = Arc::new(MockSource::default());
        source.budgets.lock().unwrap().extend([
            budget(7, 5_000),
            budget(1, 1_450),
            budget(7, 1_450),
        ]);
        let cancel = CancellationToken::new();
        let mut config = test_config(&dir);
        config.inventory_ttl_ms = 60_000;
        let daemon = start(config, source.clone(), cancel.clone()).await.unwrap();
        let swept = wait_for_snapshot(&dir, |_| true).await;
        let check =
            br#"{"action":"completed","check_run":{"check_suite":{"head_branch":"feature-x"}}}"#;
        assert_eq!(signed(daemon.local_addr(), "check_run", check).await, 200);
        wait_for_snapshot(&dir, |snapshot| snapshot.updated_at > swept.updated_at).await;
        assert_eq!(source.local_calls.load(Ordering::SeqCst), 2);

        let review = br#"{"action":"submitted","pull_request":{"head":{"ref":"feature-x"},"base":{"ref":"main"}}}"#;
        // The first review wakes the scheduler on the scoped read's floor,
        // 0.4 seconds on, where the read of every branch is not yet due.
        assert_eq!(
            signed(daemon.local_addr(), "pull_request_review", review).await,
            200
        );
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert_eq!(source.local_calls.load(Ordering::SeqCst), 3);
        for _ in 0..5 {
            assert_eq!(
                signed(daemon.local_addr(), "pull_request_review", review).await,
                200
            );
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        // The rest waited for it.
        assert_eq!(source.fetches.load(Ordering::SeqCst), 2);
        assert_eq!(source.local_calls.load(Ordering::SeqCst), 3);
        let full = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(snapshot) = read_snapshot(dir.path()).await.unwrap()
                    && snapshot.fetch_started_at_ms > swept.fetch_started_at_ms
                {
                    return snapshot;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the postponed read ran");
        let paced = full.fetch_started_at_ms.unwrap() - swept.fetch_started_at_ms.unwrap();
        assert!(paced >= 2_000, "{paced}");
        assert_eq!(source.fetches.load(Ordering::SeqCst), 3);
        assert_eq!(source.local_calls.load(Ordering::SeqCst), 4);
        assert_eq!(source.asked.lock().unwrap()[2].len(), 2);
        cancel.cancel();
        daemon.shutdown().await;
    }

    #[test]
    fn only_check_and_status_deliveries_are_served_by_a_scoped_read() {
        for event in ["check_run", "check_suite", "status"] {
            assert!(scoped_event(event), "{event}");
        }
        for event in [
            "pull_request",
            "pull_request_review",
            "pull_request_review_thread",
            "issue_comment",
            "merge_group",
            "push",
            "something_new",
        ] {
            assert!(!scoped_event(event), "{event}");
        }
    }

    #[test]
    fn a_scoped_merge_touches_only_the_named_branches_and_keeps_the_oldest_stamp() {
        let base = GithubSnapshot {
            updated_at: 1_100,
            fetch_started_at_ms: Some(1_000),
            swept_at_ms: Some(1_100),
            branches: vec!["a".into(), "b".into(), "c".into()],
            prs: json!({"a": {"n": 1}, "b": {"n": 1}, "c": {"n": 1}}),
            merge_queue: json!({"a": 1}),
            ..GithubSnapshot::default()
        };
        // The read asked about a and b and found a pull request only for a.
        // An entry for a branch it was not asked about is not taken.
        let merged = merge_scoped(
            &base,
            &names(&["a", "b"]),
            &json!({"a": {"n": 2}, "c": {"n": 2}}),
            json!({"a": 2}),
            5_000,
        )
        .unwrap();
        assert_eq!(merged.prs, json!({"a": {"n": 2}, "c": {"n": 1}}));
        assert_eq!(merged.merge_queue, json!({"a": 2}));
        // The snapshot is still only as recent as its oldest part.
        assert_eq!(merged.fetch_started_at_ms, Some(1_000));
        assert_eq!(merged.swept_at_ms, Some(1_100));
        assert_eq!(
            merged.branch_reads,
            BTreeMap::from([("a".to_owned(), 5_000), ("b".to_owned(), 5_000)])
        );
        assert_eq!(merged.branches, base.branches);
        // A later scoped read of another branch keeps the first one's stamp.
        let again = merge_scoped(&merged, &names(&["c"]), &json!({}), json!({}), 6_000).unwrap();
        assert_eq!(again.prs, json!({"a": {"n": 2}}));
        assert_eq!(again.branch_reads.get("a"), Some(&5_000));
        assert_eq!(again.branch_reads.get("c"), Some(&6_000));
        // Without a branch map on both sides nothing is merged.
        assert!(merge_scoped(&base, &names(&["a"]), &json!([]), json!({}), 1).is_none());
        let odd = GithubSnapshot {
            prs: json!(null),
            ..base.clone()
        };
        assert!(merge_scoped(&odd, &names(&["a"]), &json!({}), json!({}), 1).is_none());
    }

    #[test]
    fn the_window_with_the_least_left_is_the_budget() {
        let at = |remaining, cost, reset_at_ms| RateBudget {
            limit: 5_000,
            remaining,
            cost,
            reset_at_ms,
        };
        let mut windows = BudgetWindows::default();
        // One window: the reading as it is.
        assert_eq!(
            windows.observe(at(4_900, 7, 9_000), 1_000),
            Some(at(4_900, 7, 9_000))
        );
        // A second, nearly spent window is the one that runs out, wherever
        // its reset falls; the answer carries the cost of the latest read.
        assert_eq!(
            windows.observe(at(300, 1, 5_000), 1_000),
            Some(at(300, 1, 5_000))
        );
        // A later read that saw only the roomy window does not hide it.
        assert_eq!(
            windows.observe(at(4_890, 7, 9_000), 2_000),
            Some(at(300, 7, 5_000))
        );
        assert_eq!(
            windows.open(2_000),
            vec![at(300, 1, 5_000), at(4_890, 7, 9_000)]
        );
        // A window's figure only falls: a higher reading of it is older.
        assert_eq!(
            windows.observe(at(310, 1, 5_000), 2_500),
            Some(at(300, 1, 5_000))
        );
        // Once it resets it says nothing, and a reading of a closed window
        // is not kept.
        assert_eq!(
            windows.observe(at(4_880, 7, 9_000), 5_000),
            Some(at(4_880, 7, 9_000))
        );
        assert_eq!(
            windows.observe(at(0, 1, 4_000), 5_001),
            Some(at(4_880, 1, 9_000))
        );
        assert_eq!(windows.lowest(9_000), None);
    }

    /// A finished check on one branch reads that branch alone and merges it
    /// into the last read of every branch, which keeps its own stamps. A
    /// review reads every branch again.
    #[tokio::test]
    async fn a_check_delivery_reads_only_its_branch_and_a_review_reads_them_all() {
        let dir = TempDir::new().unwrap();
        let source = Arc::new(MockSource::default());
        source.by_branch.store(true, Ordering::SeqCst);
        let cancel = CancellationToken::new();
        let daemon = start(test_config(&dir), source.clone(), cancel.clone())
            .await
            .unwrap();
        let swept = wait_for_snapshot(&dir, |_| true).await;
        assert_eq!(
            swept.prs,
            json!({"feature-x": {"read": 1}, "remote-only": {"read": 1}})
        );
        assert_eq!(swept.swept_at_ms, Some(swept.updated_at));
        assert!(swept.branch_reads.is_empty());

        // The branch's pull request is gone by the time its check finishes.
        source.without_pr.lock().unwrap().insert("feature-x".into());
        let check =
            br#"{"action":"completed","check_run":{"check_suite":{"head_branch":"feature-x"}}}"#;
        assert_eq!(signed(daemon.local_addr(), "check_run", check).await, 200);
        let scoped =
            wait_for_snapshot(&dir, |snapshot| snapshot.updated_at > swept.updated_at).await;
        assert_eq!(
            source.asked.lock().unwrap()[1],
            vec!["feature-x".to_owned()]
        );
        assert_eq!(scoped.prs, json!({"remote-only": {"read": 1}}));
        assert_eq!(scoped.merge_queue, json!({"read": 2}));
        assert_eq!(scoped.branches, swept.branches);
        assert_eq!(scoped.fetch_started_at_ms, swept.fetch_started_at_ms);
        assert_eq!(scoped.swept_at_ms, swept.swept_at_ms);
        let read_at = scoped.branch_reads["feature-x"];
        assert!(
            read_at >= swept.updated_at && read_at <= scoped.updated_at,
            "{read_at}"
        );
        assert_eq!(scoped.branch_reads.len(), 1);

        source.without_pr.lock().unwrap().clear();
        let review = br#"{"action":"submitted","pull_request":{"head":{"ref":"feature-x"},"base":{"ref":"main"}}}"#;
        assert_eq!(
            signed(daemon.local_addr(), "pull_request_review", review).await,
            200
        );
        let full =
            wait_for_snapshot(&dir, |snapshot| snapshot.updated_at > scoped.updated_at).await;
        assert_eq!(source.asked.lock().unwrap()[2].len(), 2);
        assert_eq!(
            full.prs,
            json!({"feature-x": {"read": 3}, "remote-only": {"read": 3}})
        );
        assert!(full.fetch_started_at_ms > swept.fetch_started_at_ms);
        assert_eq!(full.swept_at_ms, Some(full.updated_at));
        assert!(full.branch_reads.is_empty());
        cancel.cancel();
        daemon.shutdown().await;
    }

    /// Once the last read of every branch is as old as the sweep interval,
    /// a check delivery reads every branch, so readers never hold a
    /// snapshot whose other branches have aged out.
    #[tokio::test]
    async fn a_check_delivery_reads_every_branch_once_the_last_sweep_is_old() {
        let dir = TempDir::new().unwrap();
        let source = Arc::new(MockSource::default());
        let cancel = CancellationToken::new();
        let mut config = test_config(&dir);
        config.full_sweep_interval_ms = 1;
        let daemon = start(config, source.clone(), cancel.clone()).await.unwrap();
        wait_for_fetches(&source, 1).await;
        let check =
            br#"{"action":"completed","check_run":{"check_suite":{"head_branch":"feature-x"}}}"#;
        assert_eq!(signed(daemon.local_addr(), "check_run", check).await, 200);
        wait_for_fetches(&source, 2).await;
        assert_eq!(source.asked.lock().unwrap()[1].len(), 2);
        cancel.cancel();
        daemon.shutdown().await;
    }

    /// A scoped read that fails merges nothing: the last good snapshot
    /// stands, and the retry reads every branch.
    #[tokio::test]
    async fn a_failed_scoped_read_changes_nothing_and_the_retry_reads_every_branch() {
        let dir = TempDir::new().unwrap();
        let source = Arc::new(MockSource::default());
        source.by_branch.store(true, Ordering::SeqCst);
        let cancel = CancellationToken::new();
        let daemon = start(test_config(&dir), source.clone(), cancel.clone())
            .await
            .unwrap();
        let swept = wait_for_snapshot(&dir, |_| true).await;
        source.fail.store(true, Ordering::SeqCst);
        let check =
            br#"{"action":"completed","check_run":{"check_suite":{"head_branch":"feature-x"}}}"#;
        assert_eq!(signed(daemon.local_addr(), "check_run", check).await, 200);
        wait_for_fetches(&source, 2).await;
        assert_eq!(
            source.asked.lock().unwrap()[1],
            vec!["feature-x".to_owned()]
        );
        assert_eq!(read_snapshot(dir.path()).await.unwrap().unwrap(), swept);
        source.fail.store(false, Ordering::SeqCst);
        let retried =
            wait_for_snapshot(&dir, |snapshot| snapshot.updated_at > swept.updated_at).await;
        assert!(
            source.asked.lock().unwrap()[2..]
                .iter()
                .all(|asked| asked.len() == 2)
        );
        assert!(retried.branch_reads.is_empty());
        assert_eq!(retried.swept_at_ms, Some(retried.updated_at));
        cancel.cancel();
        daemon.shutdown().await;
    }

    /// The daemon paces and publishes by the window with the least left,
    /// including when a later read reports only the roomier one.
    #[tokio::test]
    async fn the_snapshot_reports_the_window_that_runs_out_first() {
        let dir = TempDir::new().unwrap();
        let now = now_millis();
        // Short windows keep the paced floor near the test's. The first
        // read is of every branch and costs what one does.
        let spent = RateBudget {
            limit: 5_000,
            remaining: 4_000,
            cost: 7,
            reset_at_ms: now + 60_000,
        };
        let roomy = RateBudget {
            limit: 5_000,
            remaining: 4_900,
            cost: 1,
            reset_at_ms: now + 120_000,
        };
        let source = Arc::new(MockSource::default());
        source.budgets.lock().unwrap().extend([spent, roomy]);
        let cancel = CancellationToken::new();
        let daemon = start(test_config(&dir), source.clone(), cancel.clone())
            .await
            .unwrap();
        let first = wait_for_snapshot(&dir, |_| true).await;
        assert_eq!(first.rate_limit, Some(spent));
        assert!(first.rate_windows.is_empty());
        let check =
            br#"{"action":"completed","check_run":{"check_suite":{"head_branch":"feature-x"}}}"#;
        assert_eq!(signed(daemon.local_addr(), "check_run", check).await, 200);
        let second =
            wait_for_snapshot(&dir, |snapshot| snapshot.updated_at > first.updated_at).await;
        // The scoped read cost one point, and the snapshot still says what
        // a read of every branch costs: readers pace theirs by it.
        assert_eq!(second.rate_limit, Some(spent));
        assert_eq!(second.rate_windows, vec![spent, roomy]);
        cancel.cancel();
        daemon.shutdown().await;
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
        // Only the reserve is left, or nothing at all: wait for the reset
        // when it is near, and never longer than the cap.
        for remaining in [1_250, 1_000, 0] {
            assert_eq!(
                paced(budget(remaining, 5, 10 * MINUTE)),
                10 * MINUTE + RESET_SLACK_MS
            );
            assert_eq!(paced(budget(remaining, 5, 50 * MINUTE)), MAX_PACED_WAIT_MS);
            assert!(budget_low(budget(remaining, 5, 50 * MINUTE).unwrap(), now));
        }
        // Never beyond the reset or the cap, even for one point of
        // spendable budget.
        assert_eq!(
            paced(budget(1_251, 5, 10 * MINUTE)),
            10 * MINUTE + RESET_SLACK_MS
        );
        assert_eq!(paced(budget(1_251, 5, 50 * MINUTE)), MAX_PACED_WAIT_MS);
        // Above the reserve, or in a window that has closed, is not low.
        assert!(!budget_low(budget(1_251, 5, 50 * MINUTE).unwrap(), now));
        assert!(!budget_low(budget(0, 5, MINUTE).unwrap(), now + 2 * MINUTE));
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

    /// With only the reserve left the daemon holds background refreshes
    /// for the paced interval, and publishes the budget it saw and when it
    /// may read next.
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
        let planned = snapshot.next_fetch_at_ms.unwrap() - snapshot.fetch_started_at_ms.unwrap();
        assert!(
            (9 * 60_000..=10 * 60_000 + RESET_SLACK_MS).contains(&planned),
            "{planned}"
        );
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
