//! Persistent controller connection to one prepared native worker.
//!
//! Exactly one SSH subprocess and one protocol actor belong to each configured
//! endpoint. Snapshots continue flowing while a command awaits its reply. A
//! command whose write was attempted is never retried after a lost reply.

use std::{
    collections::VecDeque,
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
use wt_remote::{RemoteClient, WorkerInfo};
use wt_runtime::{
    SourceHandle, SourcePublisher, SourceSnapshot, SourceState, TaskScope, source_channel,
};
use wt_tui::{UiAction, UiReply};

use crate::{
    context::AppContext,
    host_protocol::{self as protocol, ClientFrame, HostSnapshot, ServerFrame},
};

const COMMAND_QUEUE: usize = 8;
const READER_QUEUE: usize = 16;
const STDERR_TAIL: usize = 8 * 1024;
const COMMAND_REPLY_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const COMMAND_DRAIN_GRACE: Duration = Duration::from_secs(5);
const RECONNECT_MIN: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(30);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RemoteHostError {
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
}

#[derive(Clone)]
pub struct RemoteHost {
    pub endpoint: RemoteConfig,
    pub snapshot: SourceHandle<HostSnapshot>,
    requests: mpsc::Sender<Request>,
    ready: watch::Receiver<Option<u64>>,
    client: watch::Receiver<Option<RemoteClient>>,
    views: watch::Sender<protocol::HostViews>,
}

impl RemoteHost {
    pub fn start(scope: &TaskScope, context: &AppContext, endpoint: RemoteConfig) -> Self {
        let (snapshot, publisher) = source_channel();
        crate::remote_cache::start_writer(scope, context, &endpoint, snapshot.clone());
        let (requests, receiver) = mpsc::channel(COMMAND_QUEUE);
        let (ready, readiness) = watch::channel(None);
        let (client, client_state) = watch::channel(None);
        let (views, view_state) = watch::channel(protocol::HostViews::default());
        let cancellation = scope.token();
        let app_cancel = context.cancellation.clone();
        let context = context.clone();
        let host = endpoint.clone();
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
                },
                cancellation,
                app_cancel,
            )
            .await;
        });
        Self {
            endpoint,
            snapshot,
            requests,
            ready: readiness,
            client: client_state,
            views,
        }
    }

    pub fn refresh(&self) -> bool {
        let Some(connection) = *self.ready.borrow() else {
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
        let Some(connection) = *self.ready.borrow() else {
            return Err(RemoteHostError::NotStarted {
                host: self.endpoint.key(),
                detail: "the authenticated worker stream is not ready".into(),
            });
        };
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
            Err(RemoteHostError::Ambiguous {
                host: self.endpoint.key(),
                detail: "the connection actor stopped after accepting the request; delivery state is unknown".into(),
            })
        })
    }

    /// The selected runtime path is immutable for this connection. Callers
    /// prepare interactive session handoff from the same pinned client.
    pub async fn session_client(&self) -> Result<RemoteClient> {
        if self.ready.borrow().is_none() {
            anyhow::bail!("remote host {} is not connected", self.endpoint.key());
        }
        self.client
            .borrow()
            .clone()
            .context("remote runtime is not prepared")
    }
}

struct HostChannels {
    requests: mpsc::Receiver<Request>,
    publisher: SourcePublisher<HostSnapshot>,
    readiness: watch::Sender<Option<u64>>,
    prepared_client: watch::Sender<Option<RemoteClient>>,
    views: watch::Receiver<protocol::HostViews>,
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
        mut publisher,
        readiness,
        prepared_client,
        mut views,
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
    let mut last_good = crate::remote_cache::load(&context, &endpoint).await;
    if last_good.is_some() {
        publish_failed(
            &mut publisher,
            &mut last_good,
            "Cached snapshot; connecting to host".into(),
        );
    }
    let mut backoff = RECONNECT_MIN;
    let mut command_id = 0u64;
    let mut connection_id = 0u64;
    loop {
        if scope_cancel.is_cancelled() || app_cancel.is_cancelled() {
            break;
        }
        if client.is_none() {
            match crate::remote::prepare_remote_client(&context, &endpoint).await {
                Ok(prepared) => {
                    prepared_client.send_replace(Some(prepared.0.clone()));
                    client = Some(prepared);
                    backoff = RECONNECT_MIN;
                }
                Err(error) => {
                    publish_failed(
                        &mut publisher,
                        &mut last_good,
                        format!("worker preparation: {error:#}"),
                    );
                    mark_not_ready(&readiness);
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
                        publisher: &mut publisher,
                        last_good: &mut last_good,
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
                        publish_failed(&mut publisher, &mut last_good, detail);
                        drain_offline(&mut requests, &endpoint.key());
                        break;
                    }
                    ConnectionEnd::Disconnected(detail) => {
                        if connected_at.elapsed() >= RECONNECT_MAX {
                            backoff = RECONNECT_MIN;
                        }
                        publish_failed(&mut publisher, &mut last_good, detail);
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
                publish_failed(&mut publisher, &mut last_good, detail.clone());
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
                Some(Request::Refresh { .. }) => true,
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
    publisher: &'a mut SourcePublisher<HostSnapshot>,
    last_good: &'a mut Option<HostSnapshot>,
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
        last_good,
        command_id,
        active_connection,
        host,
    } = state;
    let mut latest = None::<HostSnapshot>;
    loop {
        tokio::select! {
            biased;
            _ = scope_cancel.cancelled() => return ConnectionEnd::Cancelled,
            _ = app_cancel.cancelled() => return ConnectionEnd::Cancelled,
            changed = views.changed() => {
                if changed.is_err() { return ConnectionEnd::Cancelled; }
                let current = views.borrow_and_update().clone();
                if let Err(error) = send_frame(&mut connection.stdin, &ClientFrame::Views(current), scope_cancel, app_cancel).await {
                    return ConnectionEnd::Disconnected(format!("send view subscriptions: {error}"));
                }
            },
            incoming = connection.incoming.recv() => match incoming {
                Some(ReaderMessage::Frame(frame)) => {
                    if let Err(error) = handle_frame(frame, &mut latest, publisher, last_good, host) { return ConnectionEnd::Fatal(error); }
                }
                Some(ReaderMessage::Error(error)) => return ConnectionEnd::Disconnected(format!("host stream read failed: {error}; {}", stderr_text(connection))),
                Some(ReaderMessage::Eof) | None => return ConnectionEnd::Disconnected(format!("SSH host stream closed; {}", stderr_text(connection))),
            },
            request = requests.recv() => match request {
                Some(Request::Refresh { connection })
                    if !belongs_to_connection(connection, active_connection) => {}
                Some(Request::Refresh { .. }) => {
                    if let Err(error) = send_frame(&mut connection.stdin, &ClientFrame::Refresh, scope_cancel, app_cancel).await {
                        return ConnectionEnd::Disconnected(format!("send refresh request: {error}; {}", stderr_text(connection)));
                    }
                }
                Some(Request::Command { connection, reply, .. })
                    if !belongs_to_connection(connection, active_connection) =>
                {
                    let _ = reply.send(Err(RemoteHostError::NotStarted {
                        host: host.to_owned(),
                        detail: "the request was queued for an earlier SSH connection; no command was sent".into(),
                    }));
                }
                Some(Request::Command { action, reply, .. }) => {
                    if crate::host_routing::controller_owned(&action) || matches!(action, UiAction::OnHost { .. }) {
                        let _ = reply.send(Err(RemoteHostError::NotStarted { host: host.to_owned(), detail: "action is controller-owned and was not sent".into() }));
                        continue;
                    }
                    *command_id = command_id.saturating_add(1);
                    let id = *command_id;
                    let frame = ClientFrame::Command { id, action };
                    let encoded = match protocol::encode(&frame) {
                        Ok(bytes) => bytes,
                        Err(error) => {
                            let _ = reply.send(Err(RemoteHostError::NotStarted { host: host.to_owned(), detail: format!("could not encode command: {error:#}") }));
                            continue;
                        }
                    };
                    let write_result = tokio::select! {
                        biased;
                        _ = scope_cancel.cancelled() => Err("connection cancelled after command write was attempted".to_owned()),
                        _ = app_cancel.cancelled() => Err("application stopped after command write was attempted".to_owned()),
                        result = async {
                            connection.stdin.write_all(&encoded).await?;
                            connection.stdin.flush().await
                        } => result.map_err(|error| error.to_string()),
                    };
                    if let Err(error) = write_result {
                        let drained = drain_command_reply(&mut connection.incoming, id, &mut latest, publisher, last_good, host).await;
                        let result = drained.map_err(|drain_error| RemoteHostError::Ambiguous {
                            host: host.to_owned(),
                            detail: format!("{error}; reply drain failed: {drain_error}"),
                        });
                        let _ = reply.send(result);
                        return if scope_cancel.is_cancelled() || app_cancel.is_cancelled() { ConnectionEnd::Cancelled } else { ConnectionEnd::Disconnected(format!("command write failed: {}; {}", error, stderr_text(connection))) };
                    }
                    let deadline = tokio::time::Instant::now() + COMMAND_REPLY_TIMEOUT;
                    loop {
                        tokio::select! {
                            biased;
                            _ = scope_cancel.cancelled() => {
                                let drained = drain_command_reply(&mut connection.incoming, id, &mut latest, publisher, last_good, host).await;
                                let result = drained.map_err(|detail| RemoteHostError::Ambiguous { host: host.to_owned(), detail: format!("connection cancelled while waiting for command reply: {detail}") });
                                let _ = reply.send(result);
                                return ConnectionEnd::Cancelled;
                            }
                            _ = app_cancel.cancelled() => {
                                let drained = drain_command_reply(&mut connection.incoming, id, &mut latest, publisher, last_good, host).await;
                                let result = drained.map_err(|detail| RemoteHostError::Ambiguous { host: host.to_owned(), detail: format!("application stopped while waiting for command reply: {detail}") });
                                let _ = reply.send(result);
                                return ConnectionEnd::Cancelled;
                            }
                            _ = tokio::time::sleep_until(deadline) => {
                                let _ = reply.send(Err(RemoteHostError::Ambiguous { host: host.to_owned(), detail: "command reply timed out after 15 minutes".into() }));
                                return ConnectionEnd::Disconnected("command reply timed out after 15 minutes".into());
                            }
                            incoming = connection.incoming.recv() => match incoming {
                                Some(ReaderMessage::Frame(ServerFrame::Reply { id: reply_id, reply: response })) if reply_id == id => {
                                    let _ = reply.send(Ok(response));
                                    break;
                                }
                                Some(ReaderMessage::Frame(ServerFrame::Snapshot(snapshot))) => {
                                    if let Err(error) = publish_snapshot(snapshot, &mut latest, publisher, last_good, host) {
                                        let _ = reply.send(Err(RemoteHostError::Ambiguous { host: host.to_owned(), detail: format!("invalid worker snapshot while command reply was pending: {error}") }));
                                        return ConnectionEnd::Fatal(error);
                                    }
                                }
                                Some(ReaderMessage::Frame(ServerFrame::Reply { id: received, .. })) => {
                                    let detail = format!("reply id {received} did not match outstanding command {id}");
                                    let _ = reply.send(Err(RemoteHostError::Ambiguous { host: host.to_owned(), detail: detail.clone() }));
                                    return ConnectionEnd::Fatal(detail);
                                }
                                Some(ReaderMessage::Frame(ServerFrame::Hello { .. })) => {
                                    let detail = "worker repeated hello after handshake".to_owned();
                                    let _ = reply.send(Err(RemoteHostError::Ambiguous { host: host.to_owned(), detail: detail.clone() }));
                                    return ConnectionEnd::Fatal(detail);
                                }
                                Some(ReaderMessage::Error(error)) => {
                                    let detail = format!("reply stream failed after command write: {error}; {}", stderr_text(connection));
                                    let _ = reply.send(Err(RemoteHostError::Ambiguous { host: host.to_owned(), detail: detail.clone() }));
                                    return ConnectionEnd::Disconnected(detail);
                                }
                                Some(ReaderMessage::Eof) | None => {
                                    let detail = format!("worker disconnected after command write; {}", stderr_text(connection));
                                    let _ = reply.send(Err(RemoteHostError::Ambiguous { host: host.to_owned(), detail: detail.clone() }));
                                    return ConnectionEnd::Disconnected(detail);
                                }
                            }
                        }
                    }
                }
                None => return ConnectionEnd::Cancelled,
            }
        }
    }
}

async fn drain_command_reply(
    incoming: &mut mpsc::Receiver<ReaderMessage>,
    command_id: u64,
    latest: &mut Option<HostSnapshot>,
    publisher: &mut SourcePublisher<HostSnapshot>,
    last_good: &mut Option<HostSnapshot>,
    host: &str,
) -> Result<UiReply, String> {
    let drain = async {
        loop {
            match incoming.recv().await {
                Some(ReaderMessage::Frame(ServerFrame::Reply { id, reply }))
                    if id == command_id =>
                {
                    return Ok(reply);
                }
                Some(ReaderMessage::Frame(ServerFrame::Snapshot(snapshot))) => {
                    publish_snapshot(snapshot, latest, publisher, last_good, host)?;
                }
                Some(ReaderMessage::Frame(ServerFrame::Reply { id, .. })) => {
                    return Err(format!(
                        "reply id {id} did not match outstanding command {command_id}"
                    ));
                }
                Some(ReaderMessage::Frame(ServerFrame::Hello { .. })) => {
                    return Err("worker repeated hello while draining command reply".into());
                }
                Some(ReaderMessage::Error(error)) => {
                    return Err(format!("reply stream failed while draining: {error}"));
                }
                Some(ReaderMessage::Eof) | None => {
                    return Err("worker disconnected while draining command reply".into());
                }
            }
        }
    };
    match tokio::time::timeout(COMMAND_DRAIN_GRACE, drain).await {
        Ok(result) => result,
        Err(_) => Err(format!(
            "no command reply arrived within {} seconds",
            COMMAND_DRAIN_GRACE.as_secs()
        )),
    }
}

fn handle_frame(
    frame: ServerFrame,
    latest: &mut Option<HostSnapshot>,
    publisher: &mut SourcePublisher<HostSnapshot>,
    last_good: &mut Option<HostSnapshot>,
    host: &str,
) -> Result<(), String> {
    match frame {
        ServerFrame::Snapshot(snapshot) => {
            publish_snapshot(snapshot, latest, publisher, last_good, host)
        }
        ServerFrame::Reply { id, .. } => Err(format!("unsolicited worker reply id {id}")),
        ServerFrame::Hello { .. } => Err("worker repeated hello after handshake".into()),
    }
}

fn publish_snapshot(
    snapshot: HostSnapshot,
    latest: &mut Option<HostSnapshot>,
    publisher: &mut SourcePublisher<HostSnapshot>,
    last_good: &mut Option<HostSnapshot>,
    host: &str,
) -> Result<(), String> {
    snapshot
        .validate()
        .map_err(|error| format!("invalid snapshot from {host}: {error:#}"))?;
    if latest.as_ref() == Some(&snapshot) {
        return Ok(());
    }
    *latest = Some(snapshot.clone());
    *last_good = Some(snapshot.clone());
    let state = match &snapshot.state {
        protocol::HostState::Empty => SourceState::Empty,
        protocol::HostState::Refreshing => SourceState::Refreshing,
        protocol::HostState::Ready => SourceState::Ready,
        protocol::HostState::Failed(error) => SourceState::Failed(error.clone().into()),
    };
    publisher.publish(SourceSnapshot {
        data: Some(Arc::new(snapshot)),
        state,
        updated_at: Some(tokio::time::Instant::now()),
        revision: 0,
    });
    Ok(())
}

fn publish_failed(
    publisher: &mut SourcePublisher<HostSnapshot>,
    last_good: &mut Option<HostSnapshot>,
    detail: String,
) {
    let mut snapshot = last_good.clone().unwrap_or(HostSnapshot {
        board: None,
        state: protocol::HostState::Empty,
        layout: Default::default(),
    });
    snapshot.state = protocol::HostState::Failed(detail.clone());
    publisher.publish(SourceSnapshot {
        data: Some(Arc::new(snapshot)),
        state: SourceState::Failed(detail.into()),
        updated_at: None,
        revision: 0,
    });
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
        result = async { writer.write_all(&bytes).await?; writer.flush().await } => result.context("write host protocol frame"),
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

fn mark_ready(readiness: &watch::Sender<Option<u64>>, connection: u64) {
    readiness.send_replace(Some(connection));
}
fn mark_not_ready(readiness: &watch::Sender<Option<u64>>) {
    readiness.send_replace(None);
}

fn belongs_to_connection(request_connection: u64, active_connection: u64) -> bool {
    request_connection == active_connection
}

#[cfg(test)]
mod tests {
    use super::{ReaderMessage, belongs_to_connection, drain_command_reply};
    use crate::host_protocol::{HostSnapshot, ServerFrame};
    use wt_runtime::source_channel;
    use wt_tui::UiReply;

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
    async fn cancellation_drain_returns_a_reply_that_arrives_during_grace() {
        let (incoming_tx, mut incoming) = tokio::sync::mpsc::channel(1);
        let response = UiReply {
            message: "completed".into(),
            ..Default::default()
        };
        assert!(
            incoming_tx
                .send(ReaderMessage::Frame(ServerFrame::Reply {
                    id: 42,
                    reply: response,
                }))
                .await
                .is_ok()
        );

        let (_source, mut publisher) = source_channel::<HostSnapshot>();
        let mut latest = None;
        let mut last_good = None;
        let reply = drain_command_reply(
            &mut incoming,
            42,
            &mut latest,
            &mut publisher,
            &mut last_good,
            "test-host",
        )
        .await
        .expect("a reply received during shutdown drain is authoritative");
        assert_eq!(reply.message, "completed");
    }
}
