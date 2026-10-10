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
use tracing::{error, warn};

const MAX_HTTP_HEADERS: usize = 32;
const MAX_HTTP_HEADER_BYTES: usize = 16 * 1024;
const HTTP_HEADER_TIMEOUT: Duration = Duration::from_secs(5);
const HTTP_CONNECTION_TIMEOUT: Duration = Duration::from_secs(30);

use crate::{
    EventDaemonConfig, EventDaemonError, EventDaemonState, EventSource, EventsDaemon,
    GithubSnapshot,
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

pub(super) fn extract_event_branches(
    event: &str,
    payload: &Value,
    base_branch: &str,
) -> Option<Vec<String>> {
    let unscoped = || None;
    match event {
        "pull_request"
        | "pull_request_review"
        | "pull_request_review_thread"
        | "status"
        | "merge_group" => unscoped(),
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
    let mut pending_deadline: Option<Instant> = Some(started);
    let mut cached_inventory_at: Option<Instant> = None;
    let mut local_branches = BTreeSet::new();
    let mut remote_branches = BTreeSet::new();
    let mut local_known = false;
    let mut remote_known = false;
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
                let relevant = match candidates {
                    Some(names) if names.is_empty() => false,
                    None => true,
                    Some(names) => {
                        let all: BTreeSet<_> = local_branches.union(&remote_branches).cloned().collect();
                        let complete = local_known && remote_known;
                        !complete || names.iter().any(|branch| all.contains(branch))
                    }
                };
                if relevant {
                    {
                        let mut state = durable.state.lock().await;
                        state.last_event_at = Some(event.received_at);
                        state.event_count = state.event_count.saturating_add(1);
                        let _ = durable.write_state_locked(&state).await;
                    }
                    let now_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
                    let last_ms = last_fetch_start.map(|instant| instant.duration_since(started).as_millis().min(u64::MAX as u128) as u64);
                    let pending_ms = pending_deadline.map(|instant| instant.duration_since(started).as_millis().min(u64::MAX as u128) as u64);
                    if let Some(deadline) = next_fetch_at(now_ms, last_ms, pending_ms, config.debounce_ms, config.min_fetch_interval_ms) {
                        pending_deadline = Some(started + Duration::from_millis(deadline));
                    }
                }
            }
            _ = coalesced_rx.recv() => {
                let now_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
                let last_ms = last_fetch_start.map(|instant| instant.duration_since(started).as_millis().min(u64::MAX as u128) as u64);
                let pending_ms = pending_deadline.map(|instant| instant.duration_since(started).as_millis().min(u64::MAX as u128) as u64);
                if let Some(deadline) = next_fetch_at(now_ms, last_ms, pending_ms, config.debounce_ms, config.min_fetch_interval_ms) {
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
                    Err(e) => { local_known = false; set_error(&durable, format!("local worktree inventory failed: {e}")).await; continue; }
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
                if let Err(e) = source.fetch_origin(&cancel).await { warn!(error = %e, "git fetch origin failed before GitHub refresh"); }
                match source.fetch_github(fetch_branches.iter().cloned().collect(), &cancel).await {
                    Ok(data) => {
                        let snapshot = GithubSnapshot {
                            updated_at: now_millis(), branches: fetch_branches.into_iter().collect(), prs: data.prs,
                            merge_queue: data.merge_queue, writer_sha: config.writer_sha.clone(),
                            extra: read_json::<GithubSnapshot>(config.events_dir.join("github.json")).await.ok().flatten().map(|snapshot| snapshot.extra).unwrap_or_default(),
                        };
                        match durable.write_snapshot(&snapshot).await {
                            Ok(()) => {
                                let mut state = durable.state.lock().await;
                                state.last_fetch_at = Some(snapshot.updated_at);
                                state.last_error = None;
                                let _ = durable.write_state_locked(&state).await;
                                if let Err(e) = atomic_write(&config.events_dir.join("github.touch"), snapshot.updated_at.to_string().as_bytes()).await { warn!(error = %e, "cannot write GitHub snapshot marker"); }
                            }
                            Err(e) => set_error(&durable, format!("cannot persist GitHub snapshot: {e}")).await,
                        }
                    }
                    Err(e) => set_error(&durable, format!("GitHub refresh failed: {e}")).await,
                }
            }
        }
    }
}

async fn set_error(durable: &DurableState, message: String) {
    error!(error = %message, "GitHub event refresh failed");
    let mut state = durable.state.lock().await;
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
    use crate::{EventFuture, EventGithubData, read_snapshot, read_state};
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tempfile::TempDir;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpStream,
    };

    struct MockSource {
        fail: AtomicBool,
        fetches: AtomicUsize,
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
            Box::pin(async { Ok(()) })
        }
        fn fetch_github<'a>(
            &'a self,
            branches: Vec<String>,
            _: &'a CancellationToken,
        ) -> EventFuture<'a, Result<EventGithubData, String>> {
            self.fetches.fetch_add(1, Ordering::SeqCst);
            let fail = self.fail.load(Ordering::SeqCst);
            Box::pin(async move {
                if fail {
                    return Err("fixture failure".into());
                }
                Ok(EventGithubData {
                    prs: json!({"branches": branches}),
                    merge_queue: json!({}),
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

    #[tokio::test]
    async fn remote_inventory_error_marks_coverage_unknown_but_keeps_last_good_branches() {
        let dir = TempDir::new().unwrap();
        let source = Arc::new(MockSource {
            fail: AtomicBool::new(false),
            fetches: AtomicUsize::new(0),
            remote_calls: AtomicUsize::new(0),
            remote_failure_after: Some(1),
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
