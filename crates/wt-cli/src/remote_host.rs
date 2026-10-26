//! Persistent controller connection to one prepared native worker.
//!
//! Exactly one SSH subprocess and one protocol actor belong to each configured
//! endpoint. Snapshots continue flowing while a command awaits its reply. A
//! command whose write was attempted is never retried after a lost reply.

use std::{
    collections::{BTreeMap, VecDeque},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result};
use thiserror::Error;
use tokio::{
    io::{AsyncWriteExt, BufReader, BufWriter},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command},
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use wt_config::RemoteConfig;
use wt_core::Domain;
use wt_remote::{RemoteClient, WorkerInfo};
use wt_runtime::{
    DiskCache, LeafHandle, LeafPublisher, Persistence, RefreshPolicy, SourceSpec, SourceState,
    Stamp, TaskScope, leaf_channel,
};
use wt_tui::{UiAction, UiReply};

use wt_app_core::{
    context::AppContext,
    host_protocol::{self as protocol, ClientFrame, HostSnapshot, ServerFrame},
};

const COMMAND_QUEUE: usize = 8;
const READER_QUEUE: usize = 16;
const STDERR_TAIL: usize = 8 * 1024;
const COMMAND_REPLY_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const REMOVAL_REPLY_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const COMMAND_DRAIN_GRACE: Duration = Duration::from_secs(5);
const RECONNECT_MIN: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(30);
/// A managed worker reconnecting sooner than this after its last provisioning
/// reuses it; a flapping link must not re-upload the whole managed tree.
const REPROVISION_AFTER: Duration = Duration::from_secs(10 * 60);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RemoteHostError {
    #[error("remote host {host} read-only request failed: {detail}")]
    ReadFailed { host: String, detail: String },
    #[error("remote host {host} is not connected; no command was sent: {detail}")]
    NotStarted { host: String, detail: String },
    #[error(
        "remote host {host} lost the command reply after sending it; the result is ambiguous: {detail}"
    )]
    Ambiguous { host: String, detail: String },
    #[error("remote host {host} command queue is full")]
    QueueFull { host: String },
    #[error("remote host {host} connection stopped")]
    Stopped { host: String },
}

enum Request {
    Command {
        connection: u64,
        action: UiAction,
        reply: oneshot::Sender<Result<UiReply, RemoteHostError>>,
    },
    Refresh {
        connection: u64,
    },
    /// Retry preparation now, e.g. after a sign-in on a blocked managed host.
    Wake,
}

#[derive(Clone, Debug, Default)]
enum ConnectionReadiness {
    #[default]
    Preparing,
    Ready(u64),
    UnavailableBuild(Arc<wt_ops::remote::MissingRemoteBuild>),
}

impl ConnectionReadiness {
    fn connection(&self) -> Option<u64> {
        match self {
            Self::Ready(connection) => Some(*connection),
            Self::Preparing | Self::UnavailableBuild(_) => None,
        }
    }
}

#[derive(Clone)]
pub struct RemoteHost {
    pub endpoint: RemoteConfig,
    /// The worker's latest snapshot. Persisted, so a restart shows the last
    /// one as `Restored` while connecting; it never authorizes a command.
    pub snapshot: LeafHandle<HostSnapshot>,
    /// The latest managed-worker checks; always `None` for unmanaged hosts.
    pub managed: watch::Receiver<Option<Arc<wt_ops::managed::ManagedStatus>>>,
    managed_status: Arc<watch::Sender<Option<Arc<wt_ops::managed::ManagedStatus>>>>,
    requests: mpsc::Sender<Request>,
    ready: watch::Receiver<ConnectionReadiness>,
    client: watch::Receiver<Option<RemoteClient>>,
    views: watch::Sender<protocol::HostViews>,
}

impl RemoteHost {
    pub fn start(scope: &TaskScope, context: &AppContext, endpoint: RemoteConfig) -> Self {
        let (snapshot, publisher) = leaf_channel(
            scope,
            SourceSpec {
                name: source_name(&endpoint),
                domain: Domain::Host,
                persistence: Persistence::Disk(DiskCache::new(
                    &wt_app_core::query_cache::remote_store(&context.config, &endpoint),
                )),
                // Fetches are driven by the worker's stream, not by a timer.
                policy: RefreshPolicy::default(),
            },
        );
        let (requests, receiver) = mpsc::channel(COMMAND_QUEUE);
        #[expect(
            clippy::disallowed_methods,
            reason = "connection readiness control signal, not board data"
        )]
        let (ready, readiness) = watch::channel(ConnectionReadiness::Preparing);
        #[expect(
            clippy::disallowed_methods,
            reason = "prepared SSH client handle, not board data"
        )]
        let (client, client_state) = watch::channel(None);
        #[expect(
            clippy::disallowed_methods,
            reason = "view subscriptions sent to the worker, not board data"
        )]
        let (views, view_state) = watch::channel(protocol::HostViews::default());
        #[expect(
            clippy::disallowed_methods,
            reason = "managed-worker provisioning report, composed into the fleet board by remote_board"
        )]
        let (managed_status, managed) = watch::channel(None);
        let managed_status = Arc::new(managed_status);
        let cancellation = scope.token();
        let app_cancel = context.cancellation.clone();
        let context = context.clone();
        let host = endpoint.clone();
        let actor_managed = managed_status.clone();
        scope.spawn(async move {
            run_host_actor(
                context,
                host,
                HostChannels {
                    requests: receiver,
                    publisher,
                    readiness: ready,
                    prepared_client: client,
                    views: view_state,
                    managed: actor_managed,
                },
                cancellation,
                app_cancel,
            )
            .await;
        });
        Self {
            endpoint,
            snapshot,
            managed,
            managed_status,
            requests,
            ready: readiness,
            client: client_state,
            views,
        }
    }

    /// The staged runtime client, available as soon as the runtime is staged
    /// even when a managed worker is blocked on a sign-in.
    pub fn staged_client(&self) -> Result<RemoteClient> {
        self.client.borrow().clone().with_context(|| {
            format!(
                "remote host {} has not been reached yet",
                self.endpoint.label
            )
        })
    }

    pub fn set_managed(&self, report: wt_remote::managed::ManagedReport) {
        self.managed_status
            .send_replace(Some(Arc::new(wt_ops::managed::ManagedStatus::now(report))));
    }

    /// Ask a disconnected or blocked host to retry preparation now.
    pub fn wake(&self) {
        let _ = self.requests.try_send(Request::Wake);
    }

    /// Wait until the worker stream is ready, up to `limit`.
    pub async fn wait_ready(&self, limit: Duration) -> bool {
        let mut ready = self.ready.clone();
        tokio::time::timeout(limit, ready.wait_for(|state| state.connection().is_some()))
            .await
            .is_ok_and(|result| result.is_ok())
    }

    pub fn is_ready(&self) -> bool {
        self.ready.borrow().connection().is_some()
    }

    /// Preparation can require authentication before a stream can exist.
    /// Wake the request as soon as either outcome is known.
    pub async fn wait_for_action(
        &self,
        action: &UiAction,
        primary: wt_core::HarnessId,
        limit: Duration,
    ) -> bool {
        let mut ready = self.ready.clone();
        let mut managed = self.managed.clone();
        tokio::time::timeout(limit, async {
            loop {
                if matches!(*ready.borrow(), ConnectionReadiness::UnavailableBuild(_)) {
                    return false;
                }
                if ready.borrow().connection().is_some()
                    || managed.borrow().as_ref().is_some_and(|status| {
                        wt_ops::managed::required_sign_in(&status.report, action, primary).is_some()
                    })
                {
                    return true;
                }
                tokio::select! {
                    result = ready.changed() => if result.is_err() { return false; },
                    result = managed.changed() => if result.is_err() { return false; },
                }
            }
        })
        .await
        .unwrap_or(false)
    }

    pub fn missing_remote_build(&self) -> Option<Arc<wt_ops::remote::MissingRemoteBuild>> {
        match &*self.ready.borrow() {
            ConnectionReadiness::UnavailableBuild(error) => Some(error.clone()),
            ConnectionReadiness::Preparing | ConnectionReadiness::Ready(_) => None,
        }
    }

    pub fn connection_detail(&self) -> String {
        match self.snapshot.snapshot().state() {
            SourceState::Failed(detail) => detail.to_string(),
            _ => "the worker is still being prepared or connected".into(),
        }
    }

    pub fn refresh(&self) -> bool {
        let Some(connection) = self.ready.borrow().connection() else {
            return false;
        };
        matches!(
            self.requests.try_send(Request::Refresh { connection }),
            Ok(())
        )
    }

    pub fn set_history_active(&self, active: bool) {
        self.views.send_modify(|views| views.history = active);
    }

    pub fn set_perf(&self, active: bool, continuous: bool, refresh: bool) {
        self.views.send_modify(|views| {
            views.perf = active;
            views.perf_continuous = continuous;
            if refresh {
                views.perf_revision = views.perf_revision.wrapping_add(1);
            }
        });
    }

    pub async fn execute(&self, action: UiAction) -> Result<UiReply, RemoteHostError> {
        let Some(connection) = self.ready.borrow().connection() else {
            return Err(RemoteHostError::NotStarted {
                host: self.endpoint.key(),
                detail: self.connection_detail(),
            });
        };
        let preparation = action.is_preparation();
        let (reply, result) = oneshot::channel();
        match self.requests.try_send(Request::Command {
            connection,
            action,
            reply,
        }) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                return Err(RemoteHostError::QueueFull {
                    host: self.endpoint.key(),
                });
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                return Err(RemoteHostError::Stopped {
                    host: self.endpoint.key(),
                });
            }
        }
        result.await.unwrap_or_else(|_| {
            let host = self.endpoint.key();
            let detail = "connection actor stopped before replying".into();
            Err(if preparation {
                RemoteHostError::ReadFailed { host, detail }
            } else {
                RemoteHostError::Ambiguous { host, detail }
            })
        })
    }

    /// The selected runtime path is immutable for this connection. Callers
    /// prepare interactive session handoff from the same pinned client.
    pub async fn session_client(&self) -> Result<RemoteClient> {
        if self.ready.borrow().connection().is_none() {
            anyhow::bail!("remote host {} is not connected", self.endpoint.key());
        }
        self.client
            .borrow()
            .clone()
            .context("remote runtime is not prepared")
    }
}

/// Cache name of one endpoint's snapshot leaf: `host-` and the first 16 hex
/// digits of the SHA-256 of the endpoint key, so another endpoint or config
/// selection never reads it. Each distinct name is allocated once per process.
pub(crate) fn source_name(endpoint: &RemoteConfig) -> &'static str {
    use sha2::{Digest, Sha256};
    use std::collections::BTreeMap;
    use std::sync::{Mutex, OnceLock};
    static NAMES: OnceLock<Mutex<BTreeMap<String, &'static str>>> = OnceLock::new();
    let digest = Sha256::digest(endpoint.key().as_bytes());
    let name = format!("host-{}", &format!("{digest:x}")[..16]);
    let mut names = NAMES
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    names
        .entry(name)
        .or_insert_with_key(|name| Box::leak(name.clone().into_boxed_str()))
}

struct HostChannels {
    requests: mpsc::Receiver<Request>,
    publisher: LeafPublisher<HostSnapshot>,
    readiness: watch::Sender<ConnectionReadiness>,
    prepared_client: watch::Sender<Option<RemoteClient>>,
    views: watch::Receiver<protocol::HostViews>,
    managed: Arc<watch::Sender<Option<Arc<wt_ops::managed::ManagedStatus>>>>,
}

/// Stage the runtime, provision a managed worker, then handshake. The staged
/// client is published before provisioning so a sign-in can reach a worker
/// that is blocked waiting for one.
async fn prepare_host(
    context: &AppContext,
    endpoint: &RemoteConfig,
    prepared_client: &watch::Sender<Option<RemoteClient>>,
    managed: &watch::Sender<Option<Arc<wt_ops::managed::ManagedStatus>>>,
) -> Result<(RemoteClient, WorkerInfo)> {
    // A report from an earlier connection no longer describes a worker this
    // controller cannot reach, so it must not keep prompting for sign-ins.
    let unreachable = |error: anyhow::Error| {
        managed.send_replace(None);
        error
    };
    let staged = wt_ops::remote::stage_remote_runtime(context, endpoint)
        .await
        .map_err(unreachable)?;
    if endpoint.managed {
        prepared_client.send_replace(Some(staged.clone()));
        let report = wt_ops::managed::provision(context, &staged)
            .await
            .map_err(unreachable)?;
        let blocking = report.blocking_failure().map(|check| check.detail.clone());
        managed.send_replace(Some(Arc::new(wt_ops::managed::ManagedStatus::now(report))));
        if let Some(detail) = blocking {
            anyhow::bail!("{detail}");
        }
    }
    let worker = staged.require_worker(&context.cancellation).await?;
    Ok((staged, worker))
}

async fn run_host_actor(
    context: AppContext,
    endpoint: RemoteConfig,
    channels: HostChannels,
    scope_cancel: CancellationToken,
    app_cancel: CancellationToken,
) {
    let HostChannels {
        mut requests,
        publisher,
        readiness,
        prepared_client,
        mut views,
        managed,
    } = channels;
    let operation_cancel = CancellationToken::new();
    let cancel_signal = operation_cancel.clone();
    let scope_signal = scope_cancel.clone();
    let app_signal = app_cancel.clone();
    let cancellation_mirror = tokio::spawn(async move {
        tokio::select! {
            _ = cancel_signal.cancelled() => {},
            _ = scope_signal.cancelled() => {},
            _ = app_signal.cancelled() => {},
        }
        cancel_signal.cancel();
    });
    let mut context = context;
    context.cancellation = operation_cancel.clone();
    let mut client: Option<(RemoteClient, WorkerInfo)> = None;
    let mut backoff = RECONNECT_MIN;
    let mut command_id = 0u64;
    let mut connection_id = 0u64;
    let mut provisioned_at: Option<tokio::time::Instant> = None;
    loop {
        if scope_cancel.is_cancelled() || app_cancel.is_cancelled() {
            break;
        }
        if client.is_none() {
            match prepare_host(&context, &endpoint, &prepared_client, &managed).await {
                Ok(prepared) => {
                    prepared_client.send_replace(Some(prepared.0.clone()));
                    provisioned_at = Some(tokio::time::Instant::now());
                    client = Some(prepared);
                    backoff = RECONNECT_MIN;
                }
                Err(error) => {
                    publish_failed(&publisher, format!("worker preparation: {error:#}"));
                    if let Some(missing) =
                        error.downcast_ref::<wt_ops::remote::MissingRemoteBuild>()
                    {
                        readiness.send_replace(ConnectionReadiness::UnavailableBuild(Arc::new(
                            missing.clone(),
                        )));
                    } else {
                        mark_not_ready(&readiness);
                    }
                    if !wait_offline(
                        &mut requests,
                        backoff,
                        &scope_cancel,
                        &app_cancel,
                        &endpoint.key(),
                    )
                    .await
                    {
                        break;
                    }
                    backoff = (backoff * 2).min(RECONNECT_MAX);
                    continue;
                }
            }
        }
        let (remote, worker) = client.as_ref().expect("client is initialized");
        match open_connection(remote.host_stream(), worker, &scope_cancel, &app_cancel).await {
            Ok(mut connection) => {
                // Re-establish view subscriptions on every new worker stream.
                views.mark_changed();
                connection_id = connection_id.saturating_add(1);
                let active_connection = connection_id;
                mark_ready(&readiness, active_connection);
                let connected_at = tokio::time::Instant::now();
                let result = serve_connection(
                    &mut connection,
                    ConnectionState {
                        requests: &mut requests,
                        views: &mut views,
                        publisher: &publisher,
                        command_id: &mut command_id,
                        active_connection,
                        host: &endpoint.key(),
                    },
                    &scope_cancel,
                    &app_cancel,
                )
                .await;
                mark_not_ready(&readiness);
                terminate_connection(&mut connection).await;
                match result {
                    ConnectionEnd::Cancelled => break,
                    ConnectionEnd::Fatal(detail) => {
                        publish_failed(&publisher, detail);
                        drain_offline(&mut requests, &endpoint.key());
                        break;
                    }
                    ConnectionEnd::Disconnected(detail) => {
                        if connected_at.elapsed() >= RECONNECT_MAX {
                            backoff = RECONNECT_MIN;
                        }
                        // A managed worker is provisioned again on reconnect,
                        // so credentials rotated meanwhile reach it.
                        if endpoint.managed
                            && provisioned_at.is_none_or(|at| at.elapsed() >= REPROVISION_AFTER)
                        {
                            client = None;
                        }
                        publish_failed(&publisher, detail);
                        drain_offline(&mut requests, &endpoint.key());
                        if !wait_offline(
                            &mut requests,
                            backoff,
                            &scope_cancel,
                            &app_cancel,
                            &endpoint.key(),
                        )
                        .await
                        {
                            break;
                        }
                        backoff = (backoff * 2).min(RECONNECT_MAX);
                    }
                }
            }
            Err(ConnectionFailure { detail, fatal }) => {
                mark_not_ready(&readiness);
                publish_failed(&publisher, detail.clone());
                if fatal {
                    drain_offline(&mut requests, &endpoint.key());
                    break;
                }
                drain_offline(&mut requests, &endpoint.key());
                if !wait_offline(
                    &mut requests,
                    backoff,
                    &scope_cancel,
                    &app_cancel,
                    &endpoint.key(),
                )
                .await
                {
                    break;
                }
                backoff = (backoff * 2).min(RECONNECT_MAX);
            }
        }
    }
    mark_not_ready(&readiness);
    drain_offline(&mut requests, &endpoint.key());
    operation_cancel.cancel();
    let _ = cancellation_mirror.await;
}

async fn wait_offline(
    requests: &mut mpsc::Receiver<Request>,
    duration: Duration,
    scope_cancel: &CancellationToken,
    app_cancel: &CancellationToken,
    host: &str,
) -> bool {
    tokio::select! {
        biased;
        _ = scope_cancel.cancelled() => false,
        _ = app_cancel.cancelled() => false,
        _ = tokio::time::sleep(duration) => true,
        request = requests.recv() => {
            match request {
                Some(Request::Command { reply, .. }) => {
                    let _ = reply.send(Err(RemoteHostError::NotStarted { host: host.to_owned(), detail: "the SSH stream is reconnecting; no command was sent".into() }));
                    true
                }
                Some(Request::Refresh { .. } | Request::Wake) => true,
                None => false,
            }
        }
    }
}

fn drain_offline(requests: &mut mpsc::Receiver<Request>, host: &str) {
    while let Ok(request) = requests.try_recv() {
        if let Request::Command { reply, .. } = request {
            let _ = reply.send(Err(RemoteHostError::NotStarted {
                host: host.to_owned(),
                detail: "the SSH stream is disconnected; no command was sent".into(),
            }));
        }
    }
}

#[allow(
    clippy::large_enum_variant,
    reason = "The 16-entry channel is bounded; inline frames avoid an allocation for every snapshot."
)]
enum ReaderMessage {
    Frame(ServerFrame),
    Error(String),
    Eof,
}

struct Connection {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    incoming: mpsc::Receiver<ReaderMessage>,
    reader: JoinHandle<()>,
    stderr: Arc<Mutex<VecDeque<u8>>>,
    stderr_reader: JoinHandle<()>,
}

struct ConnectionFailure {
    detail: String,
    fatal: bool,
}

async fn open_connection(
    prepared: wt_remote::PreparedRemoteSession,
    worker: &WorkerInfo,
    scope_cancel: &CancellationToken,
    app_cancel: &CancellationToken,
) -> Result<Connection, ConnectionFailure> {
    let mut command = Command::new(prepared.program);
    command
        .args(prepared.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if scope_cancel.is_cancelled() || app_cancel.is_cancelled() {
        return Err(ConnectionFailure {
            detail: "cancelled before SSH launch".into(),
            fatal: true,
        });
    }
    let mut child = command.spawn().map_err(|error| ConnectionFailure {
        detail: format!("start SSH stream: {error}"),
        fatal: false,
    })?;
    let stdin = child.stdin.take().expect("SSH stdin configured");
    let stdout = child.stdout.take().expect("SSH stdout configured");
    let stderr = child.stderr.take().expect("SSH stderr configured");
    let (incoming_tx, incoming) = mpsc::channel(READER_QUEUE);
    let reader = tokio::spawn(read_frames(stdout, incoming_tx));
    let stderr_tail = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL)));
    let stderr_reader = tokio::spawn(drain_stderr(stderr, stderr_tail.clone()));
    let mut connection = Connection {
        child,
        stdin: BufWriter::new(stdin),
        incoming,
        reader,
        stderr: stderr_tail,
        stderr_reader,
    };
    if let Err(error) = send_frame(
        &mut connection.stdin,
        &ClientFrame::Hello {
            protocol: protocol::HOST_PROTOCOL,
        },
        scope_cancel,
        app_cancel,
    )
    .await
    {
        let detail = format!("send host handshake: {error}");
        terminate_connection(&mut connection).await;
        return Err(ConnectionFailure {
            detail,
            fatal: false,
        });
    }
    let hello = tokio::select! {
        biased;
        _ = scope_cancel.cancelled() => { terminate_connection(&mut connection).await; return Err(ConnectionFailure { detail: "host handshake cancelled".into(), fatal: true }); },
        _ = app_cancel.cancelled() => { terminate_connection(&mut connection).await; return Err(ConnectionFailure { detail: "host handshake cancelled".into(), fatal: true }); },
        result = tokio::time::timeout(HANDSHAKE_TIMEOUT, connection.incoming.recv()) => match result {
            Ok(Some(ReaderMessage::Frame(frame))) => frame,
            Ok(Some(ReaderMessage::Error(error))) => { let detail = format!("read host handshake: {error}; {}", stderr_text(&connection)); terminate_connection(&mut connection).await; return Err(ConnectionFailure { detail, fatal: false }); },
            Ok(Some(ReaderMessage::Eof)) | Ok(None) => { let detail = format!("worker closed during handshake; {}", stderr_text(&connection)); terminate_connection(&mut connection).await; return Err(ConnectionFailure { detail, fatal: false }); },
            Err(_) => { let detail = format!("host handshake timed out; {}", stderr_text(&connection)); terminate_connection(&mut connection).await; return Err(ConnectionFailure { detail, fatal: false }); },
        }
    };
    match hello {
        ServerFrame::Hello {
            protocol: actual,
            build: _,
        } if actual != protocol::HOST_PROTOCOL => {
            terminate_connection(&mut connection).await;
            Err(ConnectionFailure {
                detail: format!(
                    "host protocol mismatch: expected {}, got {actual}",
                    protocol::HOST_PROTOCOL
                ),
                fatal: true,
            })
        }
        ServerFrame::Hello { build, .. } if build != worker.build => {
            terminate_connection(&mut connection).await;
            Err(ConnectionFailure {
                detail: format!(
                    "host build changed after verified worker lookup (expected {}, got {build})",
                    worker.build
                ),
                fatal: true,
            })
        }
        ServerFrame::Hello { .. } => Ok(connection),
        _ => {
            terminate_connection(&mut connection).await;
            Err(ConnectionFailure {
                detail: "worker sent a non-hello frame before handshake completion".into(),
                fatal: true,
            })
        }
    }
}

async fn read_frames(stdout: ChildStdout, sender: mpsc::Sender<ReaderMessage>) {
    let mut reader = BufReader::new(stdout);
    loop {
        match protocol::read::<ServerFrame, _>(&mut reader).await {
            Ok(Some(frame)) => {
                if sender.send(ReaderMessage::Frame(frame)).await.is_err() {
                    return;
                }
            }
            Ok(None) => {
                let _ = sender.send(ReaderMessage::Eof).await;
                return;
            }
            Err(error) => {
                let _ = sender
                    .send(ReaderMessage::Error(format!("{error:#}")))
                    .await;
                return;
            }
        }
    }
}

async fn drain_stderr(mut stderr: ChildStderr, tail: Arc<Mutex<VecDeque<u8>>>) {
    let mut chunk = [0u8; 4096];
    loop {
        match tokio::io::AsyncReadExt::read(&mut stderr, &mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(count) => {
                if let Ok(mut tail) = tail.lock() {
                    for byte in &chunk[..count] {
                        if tail.len() == STDERR_TAIL {
                            tail.pop_front();
                        }
                        tail.push_back(*byte);
                    }
                }
            }
        }
    }
}

struct ConnectionState<'a> {
    requests: &'a mut mpsc::Receiver<Request>,
    views: &'a mut watch::Receiver<protocol::HostViews>,
    publisher: &'a LeafPublisher<HostSnapshot>,
    command_id: &'a mut u64,
    active_connection: u64,
    host: &'a str,
}

async fn serve_connection(
    connection: &mut Connection,
    state: ConnectionState<'_>,
    scope_cancel: &CancellationToken,
    app_cancel: &CancellationToken,
) -> ConnectionEnd {
    let ConnectionState {
        requests,
        views,
        publisher,
        command_id,
        active_connection,
        host,
    } = state;
    let mut latest = None::<HostSnapshot>;
    let mut outstanding = BTreeMap::<u64, Outstanding>::new();
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let end = 'connection: loop {
        tokio::select! {
            biased;
            _ = scope_cancel.cancelled() => break ConnectionEnd::Cancelled,
            _ = app_cancel.cancelled() => break ConnectionEnd::Cancelled,
            _ = tick.tick(), if !outstanding.is_empty() => {
                if outstanding.values().any(|pending| tokio::time::Instant::now() >= pending.deadline) {
                    break ConnectionEnd::Disconnected("worker command reply deadline exceeded; accepted mutations may still be running".into());
                }
                for (id, pending) in &mut outstanding {
                    if pending.preparation && pending.reply.is_closed() && !pending.cancel_sent {
                        pending.cancel_sent = true;
                        if let Err(error) = send_frame(&mut connection.stdin, &ClientFrame::CancelPreparation { id: *id }, scope_cancel, app_cancel).await {
                            break 'connection ConnectionEnd::Disconnected(format!("cancel read-only request: {error}"));
                        }
                    }
                }
            },
            changed = views.changed() => {
                if changed.is_err() { break ConnectionEnd::Cancelled; }
                let current = views.borrow_and_update().clone();
                if let Err(error) = send_frame(&mut connection.stdin, &ClientFrame::Views(current), scope_cancel, app_cancel).await {
                    break ConnectionEnd::Disconnected(format!("send view subscriptions: {error}"));
                }
            },
            incoming = connection.incoming.recv() => match incoming {
                Some(ReaderMessage::Frame(ServerFrame::Reply { id, reply })) => {
                    let Some(pending) = outstanding.remove(&id) else { break ConnectionEnd::Fatal(format!("unsolicited worker reply id {id}")); };
                    let _ = pending.reply.send(Ok(reply));
                },
                Some(ReaderMessage::Frame(frame)) => {
                    if let Err(error) = handle_frame(frame, &mut latest, publisher, host) { break ConnectionEnd::Fatal(error); }
                }
                Some(ReaderMessage::Error(error)) => break ConnectionEnd::Disconnected(format!("host stream read failed: {error}; {}", stderr_text(connection))),
                Some(ReaderMessage::Eof) | None => break ConnectionEnd::Disconnected(format!("SSH host stream closed; {}", stderr_text(connection))),
            },
            request = requests.recv(), if outstanding.len() < COMMAND_QUEUE => match request {
                Some(Request::Wake) => {}
                Some(Request::Refresh { connection }) if !belongs_to_connection(connection, active_connection) => {}
                Some(Request::Refresh { .. }) => {
                    if let Err(error) = send_frame(&mut connection.stdin, &ClientFrame::Refresh, scope_cancel, app_cancel).await {
                        break ConnectionEnd::Disconnected(format!("send refresh request: {error}; {}", stderr_text(connection)));
                    }
                }
                Some(Request::Command { connection, reply, .. }) if !belongs_to_connection(connection, active_connection) => {
                    let _ = reply.send(Err(RemoteHostError::NotStarted { host: host.into(), detail: "request belonged to an earlier connection; no command was sent".into() }));
                }
                Some(Request::Command { action, reply, .. }) => {
                    if wt_app_core::host_routing::controller_owned(&action) || matches!(action, UiAction::OnHost { .. }) {
                        let _ = reply.send(Err(RemoteHostError::NotStarted { host: host.into(), detail: "action is controller-owned and was not sent".into() }));
                        continue;
                    }
                    let preparation = action.is_preparation();
                    if preparation && reply.is_closed() { continue; }
                    *command_id = command_id.saturating_add(1);
                    let id = *command_id;
                    let reply_timeout = if matches!(action, UiAction::Remove { .. } | UiAction::Cleanup { .. } | UiAction::CleanupTargets { .. }) { REMOVAL_REPLY_TIMEOUT } else { COMMAND_REPLY_TIMEOUT };
                    let frame = ClientFrame::Command { id, action };
                    let encoded = match protocol::encode(&frame) {
                        Ok(bytes) => bytes,
                        Err(error) => { let _ = reply.send(Err(RemoteHostError::NotStarted { host: host.into(), detail: format!("could not encode command: {error:#}") })); continue; },
                    };
                    // Once a write is attempted, loss of its reply never
                    // permits mutation replay. Track it before touching stdin.
                    outstanding.insert(id, Outstanding { reply, preparation, cancel_sent: false, deadline: tokio::time::Instant::now() + reply_timeout });
                    let written = tokio::select! {
                        _ = scope_cancel.cancelled() => Err("connection cancelled during write".to_owned()),
                        _ = app_cancel.cancelled() => Err("application stopped during write".to_owned()),
                        result = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
                            connection.stdin.write_all(&encoded).await?;
                            connection.stdin.flush().await
                        }) => match result { Ok(result) => result.map_err(|error| error.to_string()), Err(_) => Err("command write timed out".into()) },
                    };
                    if let Err(error) = written { break ConnectionEnd::Disconnected(error); }
                }
                None => break ConnectionEnd::Cancelled,
            }
        }
    };
    // Keep the reply pipe open while notifying the worker to close admission.
    // Its durable-removal wait may now detach after proving a running job;
    // other accepted mutations still finish before the worker exits.
    if matches!(end, ConnectionEnd::Cancelled) {
        if let Ok(bytes) = protocol::encode(&ClientFrame::Shutdown) {
            let _ = tokio::time::timeout(Duration::from_secs(1), async {
                connection.stdin.write_all(&bytes).await?;
                connection.stdin.flush().await
            })
            .await;
        }
    } else {
        let _ = tokio::time::timeout(Duration::from_secs(1), connection.stdin.shutdown()).await;
    }
    // A single bounded drain reconciles every in-flight ID, irrespective of
    // completion order. Accepted worker mutations are never cancelled here.
    drain_pending_replies(
        &mut connection.incoming,
        &mut outstanding,
        &mut latest,
        publisher,
        host,
    )
    .await;
    for (_, pending) in outstanding {
        let detail = "worker connection ended without a confirmed reply".to_owned();
        let error = if pending.preparation {
            RemoteHostError::ReadFailed {
                host: host.into(),
                detail,
            }
        } else {
            RemoteHostError::Ambiguous {
                host: host.into(),
                detail,
            }
        };
        let _ = pending.reply.send(Err(error));
    }
    end
}

struct Outstanding {
    reply: oneshot::Sender<Result<UiReply, RemoteHostError>>,
    preparation: bool,
    cancel_sent: bool,
    deadline: tokio::time::Instant,
}

async fn drain_pending_replies(
    incoming: &mut mpsc::Receiver<ReaderMessage>,
    outstanding: &mut BTreeMap<u64, Outstanding>,
    latest: &mut Option<HostSnapshot>,
    publisher: &LeafPublisher<HostSnapshot>,
    host: &str,
) {
    if outstanding.is_empty() {
        return;
    }
    let _ = tokio::time::timeout(COMMAND_DRAIN_GRACE, async {
        while !outstanding.is_empty() {
            match incoming.recv().await {
                Some(ReaderMessage::Frame(ServerFrame::Reply { id, reply })) => {
                    let pending = outstanding.remove(&id).ok_or(())?;
                    let _ = pending.reply.send(Ok(reply));
                }
                Some(ReaderMessage::Frame(ServerFrame::Snapshot(snapshot))) => {
                    publish_snapshot(snapshot, latest, publisher, host).map_err(|_| ())?;
                }
                _ => return Err(()),
            }
        }
        Ok::<(), ()>(())
    })
    .await;
}

fn handle_frame(
    frame: ServerFrame,
    latest: &mut Option<HostSnapshot>,
    publisher: &LeafPublisher<HostSnapshot>,
    host: &str,
) -> Result<(), String> {
    match frame {
        ServerFrame::Snapshot(snapshot) => publish_snapshot(snapshot, latest, publisher, host),
        ServerFrame::Reply { id, .. } => Err(format!("unsolicited worker reply id {id}")),
        ServerFrame::Hello { .. } => Err("worker repeated hello after handshake".into()),
    }
}

fn publish_snapshot(
    snapshot: HostSnapshot,
    latest: &mut Option<HostSnapshot>,
    publisher: &LeafPublisher<HostSnapshot>,
    host: &str,
) -> Result<(), String> {
    snapshot
        .validate()
        .map_err(|error| format!("invalid snapshot from {host}: {error:#}"))?;
    if latest
        .as_ref()
        .is_some_and(|latest| latest.same_frame(&snapshot))
    {
        return Ok(());
    }
    *latest = Some(snapshot.clone());
    // Startup progress has no inventory to replace the last known rows. Set
    // only state so a pending disk restore can still populate the source.
    if snapshot.board.is_none() {
        publisher.set_state(match &snapshot.state {
            protocol::HostState::Failed(error) => SourceState::Failed(error.clone().into()),
            _ => SourceState::Refreshing,
        });
        return Ok(());
    }
    let state = match &snapshot.state {
        protocol::HostState::Empty => SourceState::Empty,
        protocol::HostState::Refreshing => SourceState::Refreshing,
        // The connection lane reports the connection. A failure inside the
        // host's board (e.g. its GitHub read) arrives in the host's own
        // per-domain states, so each pane names the source that failed.
        protocol::HostState::Ready | protocol::HostState::Failed(_) => SourceState::Ready,
    };
    // Received over this connection now: live, stamped on our clock. The
    // host's own domains keep their stamps on the host's clock.
    publisher.publish_live_with_state(snapshot, Stamp::now(), state);
    Ok(())
}

/// The connection is down: keep the last data (live or restored) and its
/// provenance, and report why it is not current.
fn publish_failed(publisher: &LeafPublisher<HostSnapshot>, detail: String) {
    publisher.set_state(SourceState::Failed(detail.into()));
}

enum ConnectionEnd {
    Cancelled,
    Disconnected(String),
    Fatal(String),
}

async fn send_frame(
    writer: &mut BufWriter<ChildStdin>,
    frame: &ClientFrame,
    scope_cancel: &CancellationToken,
    app_cancel: &CancellationToken,
) -> Result<()> {
    let bytes = protocol::encode(frame)?;
    tokio::select! {
        biased;
        _ = scope_cancel.cancelled() => anyhow::bail!("host stream cancelled"),
        _ = app_cancel.cancelled() => anyhow::bail!("application stopped"),
        result = tokio::time::timeout(HANDSHAKE_TIMEOUT, async { writer.write_all(&bytes).await?; writer.flush().await }) => result.context("host protocol write timed out")?.context("write host protocol frame"),
    }
}

async fn terminate_connection(connection: &mut Connection) {
    let _ = connection.child.start_kill();
    if tokio::time::timeout(Duration::from_secs(2), connection.child.wait())
        .await
        .is_err()
    {
        let _ = connection.child.start_kill();
        let _ = connection.child.wait().await;
    }
    connection.reader.abort();
    let _ = (&mut connection.reader).await;
    connection.stderr_reader.abort();
    let _ = (&mut connection.stderr_reader).await;
}

fn stderr_text(connection: &Connection) -> String {
    let Ok(tail) = connection.stderr.lock() else {
        return "SSH stderr unavailable".into();
    };
    if tail.is_empty() {
        return "no SSH stderr".into();
    }
    String::from_utf8_lossy(&tail.iter().copied().collect::<Vec<_>>())
        .trim()
        .to_owned()
}

fn mark_ready(readiness: &watch::Sender<ConnectionReadiness>, connection: u64) {
    readiness.send_replace(ConnectionReadiness::Ready(connection));
}
fn mark_not_ready(readiness: &watch::Sender<ConnectionReadiness>) {
    readiness.send_replace(ConnectionReadiness::Preparing);
}

fn belongs_to_connection(request_connection: u64, active_connection: u64) -> bool {
    request_connection == active_connection
}

#[cfg(test)]
mod tests {
    use super::belongs_to_connection;
    use wt_app_core::host_protocol::HostSnapshot;
    use wt_core::Domain;
    use wt_runtime::{Persistence, RefreshPolicy, SourceSpec, TaskScope, leaf_channel};

    #[tokio::test(start_paused = true)]
    async fn host_snapshots_meet_the_persisted_source_contract() {
        use wt_runtime::{SourceSnapshot, SourceState};
        let endpoint = wt_config::RemoteConfig {
            host: "builder".into(),
            label: "Builder".into(),
            ..Default::default()
        };
        let name = super::source_name(&endpoint);
        assert!(name.starts_with("host-") && name.len() == "host-".len() + 16);
        let mut domains = SourceSnapshot::test_live(Some(()), SourceState::Ready, Domain::Git)
            .domains()
            .clone();
        wt_runtime::merge_domains(
            &mut domains,
            SourceSnapshot::test_restored((), Domain::Github, 1_700_000_000_000).domains(),
        );
        let sample = HostSnapshot {
            board: Some(wt_tui::Board {
                name: "worker".into(),
                rows: vec![wt_tui::BoardRow {
                    key: "one".into(),
                    slug: "one".into(),
                    title: "Fix the flaky test".into(),
                    branch: "feature/one".into(),
                    path: "/srv/wt/one".into(),
                    details: vec!["PR #42 open".into()],
                    ..Default::default()
                }],
                ..Default::default()
            }),
            state: wt_app_core::host_protocol::HostState::Ready,
            domains,
            layout: std::collections::BTreeMap::from([(
                "one".to_owned(),
                wt_app_core::host_protocol::RowLayout {
                    base_branch: Some("main".into()),
                    work: None,
                },
            )]),
        };
        sample.validate().unwrap();
        wt_runtime::conformance::persisted_source(name, Domain::Host, sample).await;
    }

    #[allow(
        clippy::disallowed_methods,
        reason = "isolated connection control channels for tests"
    )]
    fn waiting_host(
        scope: &TaskScope,
    ) -> (
        super::RemoteHost,
        tokio::sync::watch::Sender<super::ConnectionReadiness>,
    ) {
        let (snapshot, _) = leaf_channel(
            scope,
            SourceSpec {
                name: "waiting-host-test",
                domain: Domain::Host,
                persistence: Persistence::Never("test fixture"),
                policy: RefreshPolicy::default(),
            },
        );
        let (readiness, ready) = tokio::sync::watch::channel(super::ConnectionReadiness::Preparing);
        let (managed_status, managed) = tokio::sync::watch::channel(None);
        let (requests, _) = tokio::sync::mpsc::channel(1);
        let (_, client) = tokio::sync::watch::channel(None);
        let (views, _) = tokio::sync::watch::channel(Default::default());
        (
            super::RemoteHost {
                endpoint: wt_config::RemoteConfig {
                    host: "worker".into(),
                    managed: true,
                    ..Default::default()
                },
                snapshot,
                managed,
                managed_status: std::sync::Arc::new(managed_status),
                requests,
                ready,
                client,
                views,
            },
            readiness,
        )
    }

    #[tokio::test(start_paused = true)]
    async fn missing_build_ends_session_wait_without_the_connection_timeout() {
        let scope = TaskScope::new();
        let (host, readiness) = waiting_host(&scope);
        let missing = wt_ops::remote::MissingRemoteBuild {
            summary: "matching Linux build is not available".into(),
            details: "GitHub returned HTTP status 404".into(),
        };
        let started = tokio::time::Instant::now();
        let waiter = tokio::spawn(async move {
            tokio::task::yield_now().await;
            readiness.send_replace(super::ConnectionReadiness::UnavailableBuild(
                std::sync::Arc::new(missing),
            ));
        });
        assert!(
            !host
                .wait_for_action(
                    &wt_tui::UiAction::Session {
                        key: Some("one".into()),
                        target: wt_tui::SessionTarget::Harness
                    },
                    wt_core::HarnessId::Codex,
                    std::time::Duration::from_secs(90),
                )
                .await
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert!(host.missing_remote_build().is_some());
        assert!(!host.is_ready());
        waiter.await.unwrap();
        scope
            .shutdown(std::time::Duration::from_secs(2))
            .await
            .unwrap();
    }

    fn github_sign_in_report() -> wt_remote::managed::ManagedReport {
        use wt_remote::managed::{CheckState, ManagedCheck, ManagedReport, SignIn};
        ManagedReport {
            checks: vec![ManagedCheck::new(
                "repository",
                CheckState::SignInNeeded,
                "Sign in to clone",
                Some(SignIn::Github),
            )],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn connection_multiplexes_commands_and_cancels_only_dropped_preparations() {
        use super::*;
        // A private echo process exposes the exact client frames. Worker
        // replies are injected independently, including reverse completion.
        let mut child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut sent = BufReader::new(child.stdout.take().unwrap());
        let (incoming_tx, incoming) = mpsc::channel(8);
        let mut connection = Connection {
            stdin: BufWriter::new(child.stdin.take().unwrap()),
            child,
            incoming,
            reader: tokio::spawn(async {}),
            stderr_reader: tokio::spawn(async {}),
            stderr: Arc::new(Mutex::new(VecDeque::new())),
        };
        let (requests, mut received) = mpsc::channel(8);
        #[expect(
            clippy::disallowed_methods,
            reason = "test view-subscription control signal, not board data"
        )]
        let (_views_tx, mut views) = watch::channel(protocol::HostViews::default());
        let scope = TaskScope::new();
        let (_, publisher) = leaf_channel::<HostSnapshot>(
            &scope,
            SourceSpec {
                name: "multiplex-fixture",
                domain: Domain::Host,
                persistence: Persistence::Never("test fixture"),
                policy: RefreshPolicy::default(),
            },
        );
        let task = tokio::spawn(async move {
            let mut command_id = 0;
            let end = serve_connection(
                &mut connection,
                ConnectionState {
                    requests: &mut received,
                    views: &mut views,
                    publisher: &publisher,
                    command_id: &mut command_id,
                    active_connection: 1,
                    host: "fixture",
                },
                &CancellationToken::new(),
                &CancellationToken::new(),
            )
            .await;
            terminate_connection(&mut connection).await;
            end
        });
        let (first, first_result) = oneshot::channel();
        let (second, second_result) = oneshot::channel();
        requests
            .send(Request::Command {
                connection: 1,
                action: UiAction::SetTitle {
                    key: "one".into(),
                    title: "first".into(),
                },
                reply: first,
            })
            .await
            .unwrap();
        requests
            .send(Request::Command {
                connection: 1,
                action: UiAction::PrepareHardRefresh,
                reply: second,
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            for expected in [1, 2] {
                assert!(matches!(protocol::read::<ClientFrame, _>(&mut sent).await.unwrap(), Some(ClientFrame::Command { id, .. }) if id == expected));
            }
        }).await.expect("second command must be sent before first completes");
        incoming_tx
            .send(ReaderMessage::Frame(ServerFrame::Reply {
                id: 2,
                reply: UiReply::message("second"),
            }))
            .await
            .unwrap();
        assert_eq!(second_result.await.unwrap().unwrap().message, "second");
        incoming_tx
            .send(ReaderMessage::Frame(ServerFrame::Reply {
                id: 1,
                reply: UiReply::message("first"),
            }))
            .await
            .unwrap();
        assert_eq!(first_result.await.unwrap().unwrap().message, "first");
        let (third, third_result) = oneshot::channel();
        requests
            .send(Request::Command {
                connection: 1,
                action: UiAction::PrepareHardRefresh,
                reply: third,
            })
            .await
            .unwrap();
        assert!(matches!(
            protocol::read::<ClientFrame, _>(&mut sent).await.unwrap(),
            Some(ClientFrame::Command { id: 3, .. })
        ));
        drop(third_result);
        let cancellation = tokio::time::timeout(
            Duration::from_secs(2),
            protocol::read::<ClientFrame, _>(&mut sent),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(matches!(
            cancellation,
            Some(ClientFrame::CancelPreparation { id: 3 })
        ));
        incoming_tx
            .send(ReaderMessage::Frame(ServerFrame::Reply {
                id: 3,
                reply: UiReply::failure("cancelled"),
            }))
            .await
            .unwrap();
        drop(requests);
        tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn action_wait_returns_for_known_blocking_sign_in() {
        let scope = TaskScope::new();
        let (host, _readiness) = waiting_host(&scope);
        host.set_managed(github_sign_in_report());
        assert!(
            host.wait_for_action(
                &wt_tui::UiAction::Create {
                    placement: None,
                    input: "test".into()
                },
                wt_core::HarnessId::Codex,
                std::time::Duration::from_millis(100)
            )
            .await
        );
        assert!(!host.is_ready());
    }

    #[tokio::test]
    async fn action_wait_returns_when_blocking_sign_in_arrives() {
        let scope = TaskScope::new();
        let (host, _readiness) = waiting_host(&scope);
        let action = wt_tui::UiAction::Create {
            placement: None,
            input: "test".into(),
        };
        let mut waiting = Box::pin(host.wait_for_action(
            &action,
            wt_core::HarnessId::Codex,
            std::time::Duration::from_secs(1),
        ));
        tokio::select! {
            biased;
            _ = &mut waiting => panic!("must wait until preparation has an outcome"),
            _ = tokio::task::yield_now() => {},
        }
        host.set_managed(github_sign_in_report());
        assert!(waiting.await);
        assert!(!host.is_ready());
    }

    #[tokio::test]
    async fn action_wait_returns_when_stream_becomes_ready() {
        let scope = TaskScope::new();
        let (host, readiness) = waiting_host(&scope);
        let action = wt_tui::UiAction::Create {
            placement: None,
            input: "test".into(),
        };
        let mut waiting = Box::pin(host.wait_for_action(
            &action,
            wt_core::HarnessId::Codex,
            std::time::Duration::from_secs(1),
        ));
        tokio::select! {
            biased;
            _ = &mut waiting => panic!("must wait until stream is ready"),
            _ = tokio::task::yield_now() => {},
        }
        super::mark_ready(&readiness, 1);
        assert!(waiting.await);
    }

    #[tokio::test]
    async fn action_wait_times_out_without_readiness_or_relevant_sign_in() {
        let scope = TaskScope::new();
        let (host, _readiness) = waiting_host(&scope);
        host.set_managed(github_sign_in_report());
        let action = wt_tui::UiAction::Session {
            key: None,
            target: wt_tui::SessionTarget::Shell,
        };
        assert!(
            !host
                .wait_for_action(
                    &action,
                    wt_core::HarnessId::Codex,
                    std::time::Duration::from_millis(10)
                )
                .await
        );
    }

    #[test]
    fn queued_requests_cannot_cross_an_ssh_reconnect() {
        let disconnected_connection = 12;
        let reconnected_connection = 13;

        assert!(belongs_to_connection(
            disconnected_connection,
            disconnected_connection
        ));
        assert!(!belongs_to_connection(
            disconnected_connection,
            reconnected_connection
        ));
    }

    #[tokio::test]
    async fn cancellation_drain_reconciles_multiple_out_of_order_replies() {
        use super::*;
        let (incoming_tx, mut incoming) = mpsc::channel(2);
        let (first, first_result) = oneshot::channel();
        let (second, second_result) = oneshot::channel();
        let mut pending = [(1, first), (2, second)]
            .into_iter()
            .map(|(id, reply)| {
                (
                    id,
                    Outstanding {
                        reply,
                        preparation: false,
                        cancel_sent: false,
                        deadline: tokio::time::Instant::now(),
                    },
                )
            })
            .collect();
        for id in [2, 1] {
            incoming_tx
                .send(ReaderMessage::Frame(ServerFrame::Reply {
                    id,
                    reply: UiReply::message(id.to_string()),
                }))
                .await
                .unwrap();
        }
        let scope = TaskScope::new();
        let (_, publisher) = leaf_channel::<HostSnapshot>(
            &scope,
            SourceSpec {
                name: "drain-test",
                domain: Domain::Host,
                persistence: Persistence::Never("fixture"),
                policy: RefreshPolicy::default(),
            },
        );
        drain_pending_replies(
            &mut incoming,
            &mut pending,
            &mut None,
            &publisher,
            "fixture",
        )
        .await;
        assert!(pending.is_empty());
        assert_eq!(first_result.await.unwrap().unwrap().message, "1");
        assert_eq!(second_result.await.unwrap().unwrap().message, "2");
    }

    #[tokio::test]
    async fn remote_rows_restore_before_connection_and_survive_empty_startup_frames() {
        use super::*;
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let endpoint = RemoteConfig {
            host: "builder".into(),
            ..Default::default()
        };
        let name = source_name(&endpoint);
        let store = wt_app_core::query_cache::remote_store(&fixture.ctx.config, &endpoint);
        let spec = || SourceSpec {
            name,
            domain: Domain::Host,
            persistence: Persistence::Disk(DiskCache::new(&store)),
            policy: RefreshPolicy::default(),
        };
        let remembered = HostSnapshot {
            board: Some(wt_tui::Board {
                rows: vec![wt_tui::BoardRow {
                    key: "one".into(),
                    slug: "one".into(),
                    path: "/worktrees/one".into(),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            state: protocol::HostState::Ready,
            domains: Default::default(),
            layout: Default::default(),
        };
        let scope = TaskScope::new();
        let (_, publisher) = leaf_channel::<HostSnapshot>(&scope, spec());
        publish_snapshot(remembered.clone(), &mut None, &publisher, "builder").unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !store.directory().join(format!("{name}.json")).exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        scope.shutdown(Duration::from_secs(2)).await.unwrap();

        let restarted = TaskScope::new();
        let (source, publisher) = leaf_channel::<HostSnapshot>(&restarted, spec());
        let empty = HostSnapshot {
            board: None,
            state: protocol::HostState::Refreshing,
            domains: Default::default(),
            layout: Default::default(),
        };
        // Progress may arrive before the asynchronous disk restore.
        publish_snapshot(empty.clone(), &mut None, &publisher, "builder").unwrap();
        let mut updates = source.subscribe();
        tokio::time::timeout(
            Duration::from_secs(5),
            updates.wait_for(|value| value.data().is_some()),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(source.snapshot().is_restored());
        assert!(source.confirmed().is_none());
        assert_eq!(
            source
                .snapshot()
                .data()
                .unwrap()
                .board
                .as_ref()
                .unwrap()
                .rows[0]
                .slug,
            "one"
        );
        // Progress after restore must keep the same cached rows and provenance.
        publish_snapshot(empty, &mut None, &publisher, "builder").unwrap();
        assert!(source.snapshot().is_restored());
        assert!(source.confirmed().is_none());
        assert!(source.snapshot().data().unwrap().board.is_some());
        // A real empty inventory is authoritative and removes obsolete rows.
        let mut live = remembered;
        live.board.as_mut().unwrap().rows.clear();
        publish_snapshot(live, &mut None, &publisher, "builder").unwrap();
        assert!(source.confirmed().is_some());
        assert!(
            source
                .snapshot()
                .data()
                .unwrap()
                .board
                .as_ref()
                .unwrap()
                .rows
                .is_empty()
        );
        restarted.shutdown(Duration::from_secs(2)).await.unwrap();
        fixture.close().await.unwrap();
    }

    /// A failure inside the worker's board (its GitHub read, say) is not a
    /// connection failure: the lane stays ready and the worker's own domain
    /// states say which source failed.
    #[tokio::test]
    async fn a_worker_side_failure_leaves_the_connection_lane_ready() {
        let scope = TaskScope::new();
        let (source, publisher) = leaf_channel::<HostSnapshot>(
            &scope,
            SourceSpec {
                name: "host-test",
                domain: Domain::Host,
                persistence: Persistence::Never("test fixture"),
                policy: RefreshPolicy::default(),
            },
        );
        let snapshot = HostSnapshot {
            board: Some(wt_tui::Board::default()),
            state: wt_app_core::host_protocol::HostState::Failed("GitHub: offline".into()),
            layout: Default::default(),
            domains: Default::default(),
        };
        let mut latest = None;
        super::publish_snapshot(snapshot, &mut latest, &publisher, "test-host").unwrap();
        assert_eq!(*source.snapshot().state(), wt_runtime::SourceState::Ready);
        assert!(source.confirmed().is_some());
    }
}
