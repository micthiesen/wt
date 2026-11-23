use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::{net::UnixStream, time::timeout};
use tokio_tungstenite::{
    WebSocketStream, client_async_with_config,
    tungstenite::{Message, protocol::WebSocketConfig},
};
use tokio_util::sync::CancellationToken;

const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum InspectorError {
    #[error("Claude inspector socket {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("Claude inspector websocket: {0}")]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("Claude inspector call {method} timed out")]
    Timeout { method: String },
    #[error("Claude inspector call {method} was cancelled")]
    Cancelled { method: String },
    #[error("Claude inspector closed before replying to {method}")]
    Closed { method: String },
    #[error("Claude inspector returned malformed JSON for {method}: {source}")]
    Json {
        method: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("Claude inspector call {method} failed: {detail}")]
    Remote { method: String, detail: String },
}

/// Listeners whose bound `this` leads to Claude's React root, tried in order.
/// The terminal-resize listener belongs to the Ink instance on every build
/// seen so far; the stdin reader was Claude's app component until Claude Code
/// 2.1.330 moved it into a plain class with no React fiber.
const APP_ANCHORS: [&str; 2] = [
    "process.stdout.listeners('resize')[0]",
    "process.stdin.listeners('readable')[0]",
];

pub struct InspectorClient {
    socket: WebSocketStream<UnixStream>,
    next_id: u64,
    call_timeout: Duration,
    anchor: usize,
}

pub fn inspector_socket_path(cache_dir: &Path, tmux_name: &str) -> PathBuf {
    cache_dir.join("insp").join(format!("{tmux_name}.sock"))
}

impl InspectorClient {
    pub async fn connect(
        path: &Path,
        cancellation: &CancellationToken,
    ) -> Result<Self, InspectorError> {
        let stream = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(InspectorError::Cancelled { method: "connect".into() }),
            result = timeout(Duration::from_secs(3), UnixStream::connect(path)) => match result {
                Err(_) => return Err(InspectorError::Timeout { method: "connect".into() }),
                Ok(Err(source)) => return Err(InspectorError::Io { path: path.to_owned(), source }),
                Ok(Ok(stream)) => stream,
            }
        };
        let (socket, _response) = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(InspectorError::Cancelled { method: "handshake".into() }),
            result = timeout(Duration::from_secs(3), client_async_with_config(
                "ws://localhost/",
                stream,
                Some(WebSocketConfig::default().max_message_size(Some(MAX_RESPONSE_BYTES)).max_frame_size(Some(MAX_RESPONSE_BYTES))),
            )) => match result {
                Err(_) => return Err(InspectorError::Timeout { method: "handshake".into() }),
                Ok(Err(err)) => return Err(InspectorError::WebSocket(err)),
                Ok(Ok(pair)) => pair,
            }
        };
        Ok(Self {
            socket,
            next_id: 1,
            call_timeout: Duration::from_secs(12),
            anchor: 0,
        })
    }

    pub fn with_call_timeout(mut self, timeout: Duration) -> Self {
        self.call_timeout = timeout;
        self
    }

    pub async fn call(
        &mut self,
        method: &str,
        params: Value,
        cancellation: &CancellationToken,
    ) -> Result<Value, InspectorError> {
        let id = self.next_id;
        self.next_id += 1;
        let request = Message::Text(
            json!({"id": id, "method": method, "params": params})
                .to_string()
                .into(),
        );
        let response = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(InspectorError::Cancelled { method: method.into() }),
            result = timeout(self.call_timeout, async {
                self.socket.send(request).await?;
                loop {
                    match self.socket.next().await {
                        Some(Ok(Message::Text(text))) => {
                            if text.len() > MAX_RESPONSE_BYTES { return Err(InspectorError::Remote { method: method.into(), detail: "response exceeded 8 MiB cap".into() }); }
                            let value: Value = serde_json::from_str(&text).map_err(|source| InspectorError::Json { method: method.into(), source })?;
                            if value.get("id").and_then(Value::as_u64) != Some(id) { continue; }
                            if let Some(error) = value.get("error") { return Err(InspectorError::Remote { method: method.into(), detail: error.to_string() }); }
                            return Ok(value.get("result").cloned().unwrap_or(Value::Null));
                        }
                        Some(Ok(Message::Binary(bytes))) => {
                            if bytes.len() > MAX_RESPONSE_BYTES { return Err(InspectorError::Remote { method: method.into(), detail: "response exceeded 8 MiB cap".into() }); }
                        }
                        Some(Ok(Message::Ping(bytes))) => self.socket.send(Message::Pong(bytes)).await?,
                        Some(Ok(Message::Close(_))) | None => return Err(InspectorError::Closed { method: method.into() }),
                        Some(Ok(_)) => {},
                        Some(Err(error)) => return Err(InspectorError::WebSocket(error)),
                    }
                }
            }) => match result {
                Err(_) => return Err(InspectorError::Timeout { method: method.into() }),
                Ok(result) => result?,
            }
        };
        Ok(response)
    }

    /// Switch to the next anchor after the current one led nowhere. A probe
    /// that then succeeds leaves the working anchor selected for the submit.
    pub fn try_next_anchor(&mut self) {
        self.anchor = (self.anchor + 1) % APP_ANCHORS.len();
    }

    /// Return the Inspector objectId of the object that owns Claude's React
    /// root, using the bound-this relationship of the selected anchor listener.
    pub async fn app_instance_object_id(
        &mut self,
        cancellation: &CancellationToken,
    ) -> Result<String, InspectorError> {
        let result = self
            .call(
                "Runtime.evaluate",
                json!({
                    "expression": APP_ANCHORS[self.anchor],
                    "objectGroup": "wt-inject"
                }),
                cancellation,
            )
            .await?;
        let listener_id = result
            .pointer("/result/objectId")
            .and_then(Value::as_str)
            .ok_or_else(|| InspectorError::Remote {
                method: "Runtime.evaluate".into(),
                detail: "anchor listener not found".into(),
            })?;
        let props = self
            .call(
                "Runtime.getProperties",
                json!({ "objectId": listener_id, "ownProperties": true }),
                cancellation,
            )
            .await?;
        props
            .get("internalProperties")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|property| {
                property
                    .get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|name| {
                        name.to_ascii_lowercase().contains("bound")
                            && name.to_ascii_lowercase().contains("this")
                    })
            })
            .and_then(|property| property.pointer("/value/objectId"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| InspectorError::Remote {
                method: "Runtime.getProperties".into(),
                detail: "anchor listener is not a bound method".into(),
            })
    }

    /// Execute an opaque Claude page routine payload. This crate deliberately
    /// treats upstream JavaScript as protocol data rather than reimplementing
    /// Claude's changing internal React/Ink structure in Rust.
    pub async fn call_page_routine(
        &mut self,
        app_object_id: &str,
        routine_source: &str,
        arguments: Vec<Value>,
        cancellation: &CancellationToken,
    ) -> Result<Value, InspectorError> {
        let args: Vec<_> = arguments
            .into_iter()
            .map(|value| json!({"value": value}))
            .collect();
        self.call(
            "Runtime.callFunctionOn",
            json!({
                "objectId": app_object_id,
                "functionDeclaration": routine_source,
                "arguments": args,
                "returnByValue": true,
                "awaitPromise": false
            }),
            cancellation,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tempfile::tempdir;
    use tokio::net::UnixListener;
    use tokio_tungstenite::accept_async;

    #[tokio::test]
    async fn websocket_transport_roundtrips_over_an_isolated_unix_socket() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("inspector.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            let request = ws.next().await.unwrap().unwrap().into_text().unwrap();
            let request: Value = serde_json::from_str(&request).unwrap();
            assert_eq!(request["method"], "Runtime.enable");
            ws.send(Message::Text(
                json!({"id":request["id"],"result":{"enabled":true}})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        });
        let cancel = CancellationToken::new();
        let mut client = InspectorClient::connect(&path, &cancel).await.unwrap();
        let response = client
            .call("Runtime.enable", json!({}), &cancel)
            .await
            .unwrap();
        assert_eq!(response["enabled"], true);
        server.await.unwrap();
    }
}
