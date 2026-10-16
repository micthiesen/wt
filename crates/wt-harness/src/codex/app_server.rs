use std::sync::atomic::{AtomicU64, Ordering};
use std::{path::Path, time::Duration};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::{
    net::UnixStream,
    time::{timeout, timeout_at},
};
use tokio_tungstenite::{WebSocketStream, client_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CodexThreadStatus {
    NotLoaded,
    Idle,
    SystemError,
    Active(Vec<String>),
    Unknown(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexQueueSubmission {
    pub id: String,
    pub client_user_message_id: String,
    pub input: Value,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexQueueDelivery {
    pub submission: CodexQueueSubmission,
    pub state: QueueState,
    pub reconciled: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueState {
    Started,
    Queued,
    QueuedOrStarted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexAppServerInfo {
    pub user_agent: String,
    pub codex_home: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodexAppServerFailureKind {
    Absent,
    Unavailable,
    Unsupported,
    Protocol,
    Rejected,
    /// A thread write may have reached the server without ownership proof.
    Ambiguous,
    /// Cancelled before any thread write was sent. A cancelled queue/add is
    /// reported as `Ambiguous` instead.
    Cancelled,
}

#[derive(Debug, Error)]
#[error("Codex app-server {operation} ({kind:?}): {detail}")]
pub struct CodexAppServerError {
    pub operation: &'static str,
    pub kind: CodexAppServerFailureKind,
    pub detail: String,
}

type Socket = WebSocketStream<UnixStream>;
static MESSAGE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

pub(super) struct CodexAppServerClient {
    socket: Socket,
    next_id: u64,
    pub info: CodexAppServerInfo,
}

impl CodexAppServerClient {
    pub fn info(&self) -> &CodexAppServerInfo {
        &self.info
    }

    pub async fn connect(
        path: &Path,
        cancel: &CancellationToken,
    ) -> Result<Self, CodexAppServerError> {
        if cancel.is_cancelled() {
            return Err(cancelled("connect"));
        }
        let stream = tokio::select! {
            _ = cancel.cancelled() => return Err(cancelled("connect")),
            result = timeout(Duration::from_secs(3), UnixStream::connect(path)) => match result {
                Err(_) => return Err(error("connect", CodexAppServerFailureKind::Unavailable, "timed out connecting to Codex's optional control socket")),
                Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound || e.kind() == std::io::ErrorKind::ConnectionRefused => return Err(error("connect", CodexAppServerFailureKind::Absent, "Codex app-server control socket is unavailable")),
                Ok(Err(e)) => return Err(error("connect", CodexAppServerFailureKind::Unavailable, e.to_string())),
                Ok(Ok(stream)) => stream,
            }
        };
        let (socket, _) = tokio::select! {
            _ = cancel.cancelled() => return Err(cancelled("connect")),
            result = timeout(Duration::from_secs(3), client_async("ws://localhost/", stream)) => match result {
                Err(_) => return Err(error("connect", CodexAppServerFailureKind::Unavailable, "WebSocket handshake timed out")),
                Ok(Err(e)) => return Err(error("connect", CodexAppServerFailureKind::Unavailable, e.to_string())),
                Ok(Ok(pair)) => pair,
            }
        };
        let mut client = Self {
            socket,
            next_id: 1,
            info: CodexAppServerInfo {
                user_agent: String::new(),
                codex_home: String::new(),
            },
        };
        // The handshake writes nothing to any thread. A lost initialize reply
        // makes the daemon unavailable, never a possibly-delivered message.
        let initialized = client
            .request(
                "initialize",
                json!({
                    "clientInfo": {"name":"wt", "title":"wt", "version":"0.1.0"},
                    "capabilities": {"experimentalApi":true, "requestAttestation":false}
                }),
                "initialize",
                cancel,
            )
            .await
            .map_err(|mut error| {
                if error.kind == CodexAppServerFailureKind::Ambiguous {
                    error.kind = CodexAppServerFailureKind::Unavailable;
                }
                error
            })?;
        let Some(user_agent) = initialized.get("userAgent").and_then(Value::as_str) else {
            return Err(error(
                "initialize",
                CodexAppServerFailureKind::Protocol,
                "invalid initialize response: missing userAgent",
            ));
        };
        let Some(codex_home) = initialized.get("codexHome").and_then(Value::as_str) else {
            return Err(error(
                "initialize",
                CodexAppServerFailureKind::Protocol,
                "invalid initialize response: missing codexHome",
            ));
        };
        client.info = CodexAppServerInfo {
            user_agent: user_agent.to_owned(),
            codex_home: codex_home.to_owned(),
        };
        client.notify("initialized", None, cancel).await?;
        Ok(client)
    }

    async fn request(
        &mut self,
        method: &str,
        params: Value,
        operation: &'static str,
        cancel: &CancellationToken,
    ) -> Result<Value, CodexAppServerError> {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let request = Message::Text(
            json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params})
                .to_string()
                .into(),
        );
        // One budget covers the whole exchange. Unrelated notifications keep
        // arriving on a busy daemon and must not extend the wait for our reply.
        let deadline = tokio::time::Instant::now() + REQUEST_TIMEOUT;
        tokio::select! {
            _ = cancel.cancelled() => return Err(cancelled(operation)),
            result = timeout_at(deadline, self.socket.send(request)) => match result {
                Err(_) => return Err(error(operation, CodexAppServerFailureKind::Ambiguous, "request send timed out")),
                Ok(Err(e)) => return Err(error(operation, CodexAppServerFailureKind::Ambiguous, e.to_string())),
                Ok(Ok(())) => (),
            }
        }
        loop {
            let next = tokio::select! {
                _ = cancel.cancelled() => return Err(cancelled(operation)),
                result = timeout_at(deadline, self.socket.next()) => match result {
                    Err(_) => return Err(error(operation, CodexAppServerFailureKind::Ambiguous, "request response timed out")),
                    Ok(None) => return Err(error(operation, CodexAppServerFailureKind::Ambiguous, "Codex app-server closed the socket after the request")),
                    Ok(Some(Err(e))) => return Err(error(operation, CodexAppServerFailureKind::Ambiguous, e.to_string())),
                    Ok(Some(Ok(message))) => message,
                }
            };
            let text = match next {
                Message::Text(text) => text,
                Message::Ping(payload) => {
                    tokio::select! {
                        _ = cancel.cancelled() => return Err(cancelled(operation)),
                        result = timeout_at(deadline, self.socket.send(Message::Pong(payload))) => match result {
                            Err(_) => return Err(error(operation, CodexAppServerFailureKind::Ambiguous, "request response timed out")),
                            Ok(Err(e)) => return Err(error(operation, CodexAppServerFailureKind::Ambiguous, e.to_string())),
                            Ok(Ok(())) => (),
                        }
                    }
                    continue;
                }
                Message::Close(_) => {
                    return Err(error(
                        operation,
                        CodexAppServerFailureKind::Ambiguous,
                        "Codex app-server closed the socket",
                    ));
                }
                _ => continue,
            };
            let Ok(value) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if value.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(rpc_error) = value.get("error") {
                let code = rpc_error
                    .get("code")
                    .and_then(Value::as_i64)
                    .unwrap_or(-32603);
                let message = rpc_error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown protocol error");
                let kind = if method_unsupported(code, message) {
                    CodexAppServerFailureKind::Unsupported
                } else {
                    CodexAppServerFailureKind::Rejected
                };
                return Err(error(operation, kind, message));
            }
            return value.get("result").cloned().ok_or_else(|| {
                error(
                    operation,
                    CodexAppServerFailureKind::Protocol,
                    "JSON-RPC response did not include result",
                )
            });
        }
    }

    async fn notify(
        &mut self,
        method: &str,
        params: Option<Value>,
        cancel: &CancellationToken,
    ) -> Result<(), CodexAppServerError> {
        let mut value = json!({"jsonrpc":"2.0", "method":method});
        if let Some(params) = params {
            value["params"] = params;
        }
        tokio::select! {
            _ = cancel.cancelled() => Err(cancelled("initialize")),
            result = self.socket.send(Message::Text(value.to_string().into())) =>
                result.map_err(|e| error("initialize", CodexAppServerFailureKind::Unavailable, e.to_string())),
        }
    }

    pub async fn queue_add(
        &mut self,
        thread: &str,
        text: &str,
        message_id: &str,
        cancel: &CancellationToken,
    ) -> Result<CodexQueueSubmission, CodexAppServerError> {
        let result = self
            .request(
                "thread/queue/add",
                json!({
                    "threadId":thread,
                    "input":[{"type":"text", "text":text, "text_elements":[]}],
                    "clientUserMessageId":message_id
                }),
                "queue-add",
                cancel,
            )
            .await?;
        let submission = parse_submission(result.get("queuedSubmission")).ok_or_else(|| {
            error(
                "queue-add",
                CodexAppServerFailureKind::Protocol,
                "invalid queue/add receipt",
            )
        })?;
        Ok(submission)
    }

    pub async fn queue_list(
        &mut self,
        thread: &str,
        cursor: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<(Vec<CodexQueueSubmission>, Option<String>), CodexAppServerError> {
        let result = self
            .request(
                "thread/queue/list",
                json!({"threadId":thread,"cursor":cursor,"limit":100}),
                "queue-list",
                cancel,
            )
            .await?;
        let data = result
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                error(
                    "queue-list",
                    CodexAppServerFailureKind::Protocol,
                    "invalid queue/list page",
                )
            })?;
        let mut out = Vec::new();
        for entry in data {
            out.push(parse_submission(Some(entry)).ok_or_else(|| {
                error(
                    "queue-list",
                    CodexAppServerFailureKind::Protocol,
                    "invalid queued submission",
                )
            })?);
        }
        let next = result
            .get("nextCursor")
            .and_then(Value::as_str)
            .map(str::to_owned);
        Ok((out, next))
    }

    pub async fn thread_status(
        &mut self,
        thread: &str,
        cancel: &CancellationToken,
    ) -> Result<super::CodexThreadStatus, CodexAppServerError> {
        let result = self
            .request(
                "thread/read",
                json!({"threadId":thread,"includeTurns":false}),
                "thread-read",
                cancel,
            )
            .await?;
        let status = result.pointer("/thread/status").ok_or_else(|| {
            error(
                "thread-read",
                CodexAppServerFailureKind::Protocol,
                "thread/read response lacked status",
            )
        })?;
        let ty = status.get("type").and_then(Value::as_str).ok_or_else(|| {
            error(
                "thread-read",
                CodexAppServerFailureKind::Protocol,
                "thread/read status lacked type",
            )
        })?;
        Ok(match ty {
            "notLoaded" => super::CodexThreadStatus::NotLoaded,
            "idle" => super::CodexThreadStatus::Idle,
            "systemError" => super::CodexThreadStatus::SystemError,
            "active" => super::CodexThreadStatus::Active(
                status
                    .get("activeFlags")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default(),
            ),
            other => super::CodexThreadStatus::Unknown(other.to_owned()),
        })
    }

    pub async fn queue_count(
        &mut self,
        thread: &str,
        cancel: &CancellationToken,
    ) -> Result<u32, CodexAppServerError> {
        let mut cursor = None;
        let mut count = 0u32;
        for page in 0..10 {
            let (rows, next) = self.queue_list(thread, cursor.as_deref(), cancel).await?;
            count = count.saturating_add(rows.len() as u32);
            cursor = next;
            if cursor.is_none() {
                return Ok(count);
            }
            if page == 9 {
                return Err(error(
                    "queue-list",
                    CodexAppServerFailureKind::Protocol,
                    "queue pagination exceeded 1,000 submissions",
                ));
            }
        }
        Ok(count)
    }

    pub async fn find_recent_user(
        &mut self,
        thread: &str,
        message_id: &str,
        cancel: &CancellationToken,
    ) -> Result<Option<CodexQueueSubmission>, CodexAppServerError> {
        let page = self.request("thread/items/list", json!({"threadId":thread,"turnId":null,"cursor":null,"limit":100,"sortDirection":"desc"}), "thread-items", cancel).await?;
        let rows = page.get("data").and_then(Value::as_array).ok_or_else(|| {
            error(
                "thread-items",
                CodexAppServerFailureKind::Protocol,
                "invalid thread/items/list page",
            )
        })?;
        for row in rows {
            let item = &row["item"];
            if item["type"] == "userMessage" && item["clientId"].as_str() == Some(message_id) {
                return Ok(Some(CodexQueueSubmission {
                    id: item["id"].as_str().unwrap_or_default().to_owned(),
                    client_user_message_id: message_id.to_owned(),
                    input: item["content"].clone(),
                }));
            }
        }
        Ok(None)
    }

    pub async fn queue_start(
        &mut self,
        thread: &str,
        submission: &str,
        cancel: &CancellationToken,
    ) -> Result<super::app_server::QueueState, CodexAppServerError> {
        match self
            .request(
                "thread/queue/start",
                json!({"threadId":thread,"queuedSubmissionId":submission}),
                "queue-start",
                cancel,
            )
            .await
        {
            Ok(_) => Ok(super::app_server::QueueState::Started),
            Err(e) if e.kind == CodexAppServerFailureKind::Ambiguous => {
                Ok(super::app_server::QueueState::QueuedOrStarted)
            }
            Err(e)
                if e.kind == CodexAppServerFailureKind::Rejected
                    && (e.detail == "thread already has an active or pending turn"
                        || e.detail == "resume the thread before starting a queued message") =>
            {
                Ok(super::app_server::QueueState::Queued)
            }
            Err(e)
                if e.kind == CodexAppServerFailureKind::Rejected
                    && e.detail.starts_with("queued submission not found:") =>
            {
                Ok(super::app_server::QueueState::QueuedOrStarted)
            }
            Err(e) => Err(e),
        }
    }
}

pub async fn queue_message(
    socket: &Path,
    thread: &str,
    text: &str,
    cancel: &CancellationToken,
) -> Result<CodexQueueDelivery, CodexAppServerError> {
    let sequence = MESSAGE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let message_id = Uuid::new_v5(
        &Uuid::NAMESPACE_OID,
        format!(
            "wt:{}:{}:{}:{}",
            std::process::id(),
            crate::persist::now_ms(),
            sequence,
            thread
        )
        .as_bytes(),
    )
    .to_string();
    let mut client = CodexAppServerClient::connect(socket, cancel).await?;
    match client.queue_add(thread, text, &message_id, cancel).await {
        Ok(submission) => {
            let state = client
                .queue_start(thread, &submission.id, cancel)
                .await
                .unwrap_or(super::app_server::QueueState::QueuedOrStarted);
            Ok(CodexQueueDelivery {
                submission,
                state,
                reconciled: false,
            })
        }
        // The add was written. Cancellation leaves no budget to reconcile, so
        // its outcome is unknown rather than "not sent".
        Err(add_error) if add_error.kind == CodexAppServerFailureKind::Cancelled => Err(error(
            "queue-add",
            CodexAppServerFailureKind::Ambiguous,
            "cancelled after queue/add was sent; wt will not re-submit",
        )),
        // A lost reply, or a success reply wt cannot parse, may still mean the
        // server owns the message. Prove ownership before reporting anything.
        Err(add_error)
            if matches!(
                add_error.kind,
                CodexAppServerFailureKind::Ambiguous | CodexAppServerFailureKind::Protocol
            ) =>
        {
            drop(client);
            let mut reconcile = CodexAppServerClient::connect(socket, cancel)
                .await
                .map_err(|e| {
                    error(
                        "queue-add",
                        CodexAppServerFailureKind::Ambiguous,
                        format!("lost add reply ({add_error}) and could not reconnect for ownership check: {e}"),
                    )
                })?;
            let mut cursor: Option<String> = None;
            for _ in 0..10 {
                let (rows, next_cursor) = reconcile
                    .queue_list(thread, cursor.as_deref(), cancel)
                    .await
                    .map_err(|e| {
                        error(
                            "queue-add",
                            CodexAppServerFailureKind::Ambiguous,
                            format!("lost add reply; queue/list reconciliation failed: {e}"),
                        )
                    })?;
                if let Some(submission) = rows
                    .into_iter()
                    .find(|row| row.client_user_message_id == message_id)
                {
                    let state = reconcile
                        .queue_start(thread, &submission.id, cancel)
                        .await
                        .unwrap_or(super::app_server::QueueState::QueuedOrStarted);
                    return Ok(CodexQueueDelivery {
                        submission,
                        state,
                        reconciled: true,
                    });
                }
                let Some(next) = next_cursor else {
                    break;
                };
                cursor = Some(next);
            }
            if let Some(submission) = reconcile
                .find_recent_user(thread, &message_id, cancel)
                .await
                .map_err(|e| {
                    error(
                        "queue-add",
                        CodexAppServerFailureKind::Ambiguous,
                        format!("lost add reply; item reconciliation failed: {e}"),
                    )
                })?
            {
                return Ok(CodexQueueDelivery {
                    submission,
                    state: super::app_server::QueueState::QueuedOrStarted,
                    reconciled: true,
                });
            }
            Err(error(
                "queue-add",
                CodexAppServerFailureKind::Ambiguous,
                "lost queue/add reply; neither queue/list nor recent thread items proved ownership; wt will not re-submit",
            ))
        }
        Err(e) => Err(e),
    }
}

fn parse_submission(value: Option<&Value>) -> Option<CodexQueueSubmission> {
    let value = value?;
    Some(CodexQueueSubmission {
        id: value.get("id")?.as_str()?.to_owned(),
        client_user_message_id: value.get("clientUserMessageId")?.as_str()?.to_owned(),
        input: value.get("input").cloned().unwrap_or(Value::Null),
    })
}

/// The daemon refused to dispatch the method at all, so nothing reached a
/// thread. Codex 0.163 reports an unknown method as JSON-RPC -32600 with
/// "Invalid request: unknown variant `<method>`" rather than -32601.
fn method_unsupported(code: i64, message: &str) -> bool {
    code == -32601
        || message.contains("requires experimentalApi capability")
        || (code == -32600 && message.contains("unknown variant"))
}

fn cancelled(operation: &'static str) -> CodexAppServerError {
    error(operation, CodexAppServerFailureKind::Cancelled, "cancelled")
}
fn error(
    operation: &'static str,
    kind: CodexAppServerFailureKind,
    detail: impl Into<String>,
) -> CodexAppServerError {
    CodexAppServerError {
        operation,
        kind,
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::{path::PathBuf, sync::Arc};
    use tokio::{net::UnixListener, sync::oneshot};
    use tokio_tungstenite::accept_async;

    async fn receive(socket: &mut WebSocketStream<UnixStream>) -> Value {
        loop {
            if let Some(Ok(Message::Text(text))) = socket.next().await {
                return serde_json::from_str(&text).unwrap();
            }
        }
    }

    async fn reply(socket: &mut WebSocketStream<UnixStream>, request: &Value, result: Value) {
        socket
            .send(Message::Text(
                json!({"jsonrpc":"2.0","id":request["id"],"result":result})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
    }

    async fn initialize(socket: &mut WebSocketStream<UnixStream>) {
        let request = receive(socket).await;
        assert_eq!(request["method"], "initialize");
        reply(
            socket,
            &request,
            json!({"userAgent":"test","codexHome":"/tmp/codex"}),
        )
        .await;
        let notification = receive(socket).await;
        assert_eq!(notification["method"], "initialized");
    }

    async fn run_lost_add_server(
        path: PathBuf,
        prove: bool,
        malformed_receipt: bool,
        add_count: Arc<AtomicUsize>,
        ready: oneshot::Sender<()>,
        done: oneshot::Sender<()>,
    ) {
        let listener = UnixListener::bind(path).unwrap();
        let _ = ready.send(());
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(stream).await.unwrap();
        initialize(&mut socket).await;
        let add = receive(&mut socket).await;
        assert_eq!(add["method"], "thread/queue/add");
        add_count.fetch_add(1, Ordering::SeqCst);
        let message_id = add["params"]["clientUserMessageId"]
            .as_str()
            .unwrap()
            .to_owned();
        let input = add["params"]["input"].clone();
        if malformed_receipt {
            // The server committed the add but its receipt is unreadable.
            reply(&mut socket, &add, json!({"queuedSubmission":{"id":7}})).await;
        }
        drop(socket); // The server committed the add but the reply was lost.

        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(stream).await.unwrap();
        initialize(&mut socket).await;
        let list = receive(&mut socket).await;
        assert_eq!(list["method"], "thread/queue/list");
        let data = if prove {
            json!([{"id":"sub-1","clientUserMessageId":message_id,"input":input}])
        } else {
            json!([])
        };
        reply(&mut socket, &list, json!({"data":data,"nextCursor":null})).await;
        if prove {
            let start = receive(&mut socket).await;
            assert_eq!(start["method"], "thread/queue/start");
            reply(&mut socket, &start, json!({})).await;
        } else {
            let items = receive(&mut socket).await;
            assert_eq!(items["method"], "thread/items/list");
            reply(&mut socket, &items, json!({"data":[]})).await;
        }
        let _ = done.send(());
    }

    #[tokio::test]
    async fn lost_queue_add_reply_is_reconciled_without_duplicate_add() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app-server.sock");
        let count = Arc::new(AtomicUsize::new(0));
        let (ready_tx, ready_rx) = oneshot::channel();
        let (tx, rx) = oneshot::channel();
        let server = tokio::spawn(run_lost_add_server(
            path.clone(),
            true,
            false,
            count.clone(),
            ready_tx,
            tx,
        ));
        ready_rx.await.unwrap();
        let cancellation = CancellationToken::new();
        let result = queue_message(&path, "thread-1", "hello", &cancellation)
            .await
            .unwrap();
        rx.await.unwrap();
        server.await.unwrap();
        assert!(result.reconciled);
        assert_eq!(result.submission.id, "sub-1");
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn malformed_queue_add_receipt_is_reconciled_not_reported_as_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app-server.sock");
        let count = Arc::new(AtomicUsize::new(0));
        let (ready_tx, ready_rx) = oneshot::channel();
        let (tx, rx) = oneshot::channel();
        let server = tokio::spawn(run_lost_add_server(
            path.clone(),
            true,
            true,
            count.clone(),
            ready_tx,
            tx,
        ));
        ready_rx.await.unwrap();
        let result = queue_message(&path, "thread-1", "hello", &CancellationToken::new())
            .await
            .unwrap();
        rx.await.unwrap();
        server.await.unwrap();
        assert!(result.reconciled);
        assert_eq!(result.submission.id, "sub-1");
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn notifications_do_not_extend_the_request_budget_and_handshake_loss_is_not_ambiguous() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app-server.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            let request = receive(&mut socket).await;
            assert_eq!(request["method"], "initialize");
            // A busy daemon streams unrelated notifications but never replies.
            loop {
                let notice = json!({"jsonrpc":"2.0","method":"thread/started","params":{}});
                if socket
                    .send(Message::Text(notice.to_string().into()))
                    .await
                    .is_err()
                {
                    return;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
        let started = tokio::time::Instant::now();
        let error = queue_message(&path, "thread-1", "hello", &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(30));
        // Nothing was written to a thread, so fallback remains permitted.
        assert_eq!(error.kind, CodexAppServerFailureKind::Unavailable);
        server.abort();
    }

    #[tokio::test]
    async fn unproven_lost_queue_add_reply_is_ambiguous_and_never_retried() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app-server.sock");
        let count = Arc::new(AtomicUsize::new(0));
        let (ready_tx, ready_rx) = oneshot::channel();
        let (tx, rx) = oneshot::channel();
        let server = tokio::spawn(run_lost_add_server(
            path.clone(),
            false,
            false,
            count.clone(),
            ready_tx,
            tx,
        ));
        ready_rx.await.unwrap();
        let cancellation = CancellationToken::new();
        let result = queue_message(&path, "thread-1", "hello", &cancellation)
            .await
            .unwrap_err();
        rx.await.unwrap();
        server.await.unwrap();
        assert_eq!(result.kind, CodexAppServerFailureKind::Ambiguous);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert!(result.detail.contains("will not re-submit"));
    }

    #[tokio::test]
    async fn daemon_without_queue_methods_is_unsupported_not_rejected() {
        // Codex 0.163 rejects an unknown method as -32600 "unknown variant"
        // before dispatch; older daemons use -32601. Neither wrote anything,
        // so both permit the queue CLI or exact-thread terminal fallback.
        for (code, message) in [
            (
                -32600,
                "Invalid request: unknown variant `thread/queue/add`, expected one of `initialize`",
            ),
            (-32601, "method not found"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("app-server.sock");
            let listener = UnixListener::bind(&path).unwrap();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut socket = accept_async(stream).await.unwrap();
                initialize(&mut socket).await;
                let add = receive(&mut socket).await;
                assert_eq!(add["method"], "thread/queue/add");
                let reply =
                    json!({"jsonrpc":"2.0","id":add["id"],"error":{"code":code,"message":message}});
                socket
                    .send(Message::Text(reply.to_string().into()))
                    .await
                    .unwrap();
            });
            let error = queue_message(&path, "thread-1", "hello", &CancellationToken::new())
                .await
                .unwrap_err();
            server.await.unwrap();
            assert_eq!(error.kind, CodexAppServerFailureKind::Unsupported, "{code}");
        }
        assert!(!method_unsupported(
            -32600,
            "Invalid request: missing field `threadId`"
        ));
        assert!(!method_unsupported(-32603, "unknown variant"));
    }
}
