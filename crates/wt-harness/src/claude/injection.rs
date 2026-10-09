use std::{fs, os::unix::fs::FileTypeExt, path::Path, time::Duration};

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::{InspectorClient, InspectorError, inspector_socket_path};

const PAGE_ROUTINE: &str = include_str!("page_routine.js");
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(12);
const READY_POLL: Duration = Duration::from_millis(250);
const LOCATE_RETRIES: usize = 6;
const LOCATE_RETRY_GAP: Duration = Duration::from_millis(150);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClaudeInjectFailureKind {
    Absent,
    Stale,
    NotReady,
    Blocked,
    SubmittedUnknown,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClaudeInjectOutcome {
    Submitted {
        draft_preserved: bool,
    },
    Failed {
        kind: ClaudeInjectFailureKind,
        reason: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClaudeSelftestOutcome {
    Ready {
        found_input: bool,
        found_caret: bool,
    },
    Failed {
        kind: ClaudeInjectFailureKind,
        reason: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PageResult {
    Failed(String),
    Submitted {
        draft_len: usize,
    },
    Probe {
        found_input: bool,
        found_caret: bool,
    },
}

#[derive(Clone)]
pub struct ClaudeInjector {
    cache_dir: std::path::PathBuf,
    attempt_timeout: Duration,
    ready_poll: Duration,
    locate_retries: usize,
}

impl ClaudeInjector {
    pub fn new(cache_dir: impl Into<std::path::PathBuf>) -> Self {
        Self {
            cache_dir: cache_dir.into(),
            attempt_timeout: ATTEMPT_TIMEOUT,
            ready_poll: READY_POLL,
            locate_retries: LOCATE_RETRIES,
        }
    }

    pub fn with_timings(
        mut self,
        attempt_timeout: Duration,
        ready_poll: Duration,
        locate_retries: usize,
    ) -> Self {
        self.attempt_timeout = attempt_timeout;
        self.ready_poll = ready_poll;
        self.locate_retries = locate_retries;
        self
    }

    pub async fn deliver<F>(
        &self,
        tmux_name: &str,
        text: &str,
        ready_budget: Duration,
        cancellation: &CancellationToken,
        mut blocked: F,
    ) -> ClaudeInjectOutcome
    where
        F: FnMut() -> Option<String>,
    {
        let path = inspector_socket_path(&self.cache_dir, tmux_name);
        if !is_socket(&path) {
            return failed(
                ClaudeInjectFailureKind::Absent,
                format!("no inspector socket at {}", path.display()),
            );
        }
        let mut client = match InspectorClient::connect(&path, cancellation).await {
            Ok(client) => client.with_call_timeout(self.attempt_timeout),
            Err(error) => {
                return failed(
                    ClaudeInjectFailureKind::Stale,
                    format!("inspector socket is stale ({error})"),
                );
            }
        };
        if let Err(error) = client.call("Runtime.enable", json!({}), cancellation).await {
            return failed(ClaudeInjectFailureKind::Failed, error.to_string());
        }
        let deadline = tokio::time::Instant::now() + ready_budget;
        loop {
            if let Some(reason) = blocked() {
                return failed(ClaudeInjectFailureKind::Blocked, reason);
            }
            match self.run_routine(&mut client, "", true, cancellation).await {
                Ok(PageResult::Probe { .. }) => break,
                Ok(PageResult::Failed(reason)) if is_locate_failure(&reason) => {
                    if tokio::time::Instant::now() >= deadline {
                        return failed(ClaudeInjectFailureKind::NotReady, reason);
                    }
                }
                Ok(PageResult::Failed(reason)) => {
                    return failed(ClaudeInjectFailureKind::Failed, reason);
                }
                Ok(PageResult::Submitted { .. }) => {
                    return failed(
                        ClaudeInjectFailureKind::Failed,
                        "Inspector probe unexpectedly submitted",
                    );
                }
                Err(error) => return failed(ClaudeInjectFailureKind::Failed, error.to_string()),
            }
            if tokio::time::Instant::now() >= deadline {
                return failed(
                    ClaudeInjectFailureKind::NotReady,
                    "prompt did not become ready before the deadline",
                );
            }
            tokio::select! { biased; _ = cancellation.cancelled() => return failed(ClaudeInjectFailureKind::Failed, "Claude Inspector probe cancelled"), _ = tokio::time::sleep(self.ready_poll) => {} }
        }

        // Once this call starts, any transport loss or target-side exception
        // is ambiguous. Retrying through the terminal can duplicate the send.
        if let Some(reason) = blocked() {
            return failed(ClaudeInjectFailureKind::Blocked, reason);
        }
        match self
            .run_routine(&mut client, text, false, cancellation)
            .await
        {
            Ok(PageResult::Submitted { draft_len }) => ClaudeInjectOutcome::Submitted {
                draft_preserved: draft_len > 0,
            },
            Ok(PageResult::Failed(reason)) if is_locate_failure(&reason) => {
                failed(ClaudeInjectFailureKind::NotReady, reason)
            }
            Ok(PageResult::Failed(reason)) => failed(
                ClaudeInjectFailureKind::SubmittedUnknown,
                format!("submit call returned an error after dispatch: {reason}"),
            ),
            Ok(PageResult::Probe { .. }) => failed(
                ClaudeInjectFailureKind::SubmittedUnknown,
                "submit call returned a probe result",
            ),
            Err(error) => failed(
                ClaudeInjectFailureKind::SubmittedUnknown,
                format!("submit may have been accepted but Inspector did not confirm: {error}"),
            ),
        }
    }

    pub async fn selftest(
        &self,
        tmux_name: &str,
        cancellation: &CancellationToken,
    ) -> ClaudeSelftestOutcome {
        let path = inspector_socket_path(&self.cache_dir, tmux_name);
        if !is_socket(&path) {
            return selftest_failed(
                ClaudeInjectFailureKind::Absent,
                format!("no inspector socket at {}", path.display()),
            );
        }
        let mut client = match InspectorClient::connect(&path, cancellation).await {
            Ok(x) => x.with_call_timeout(self.attempt_timeout),
            Err(e) => return selftest_failed(ClaudeInjectFailureKind::Stale, e.to_string()),
        };
        if let Err(e) = client.call("Runtime.enable", json!({}), cancellation).await {
            return selftest_failed(ClaudeInjectFailureKind::Failed, e.to_string());
        }
        match self.run_routine(&mut client, "", true, cancellation).await {
            Ok(PageResult::Probe {
                found_input: true,
                found_caret,
            }) => ClaudeSelftestOutcome::Ready {
                found_input: true,
                found_caret,
            },
            Ok(PageResult::Probe {
                found_input: false, ..
            }) => selftest_failed(
                ClaudeInjectFailureKind::NotReady,
                "prompt input not found (draft would be clobbered)",
            ),
            Ok(PageResult::Failed(reason)) => selftest_failed(
                if is_locate_failure(&reason) {
                    ClaudeInjectFailureKind::NotReady
                } else {
                    ClaudeInjectFailureKind::Failed
                },
                reason,
            ),
            Ok(PageResult::Submitted { .. }) => selftest_failed(
                ClaudeInjectFailureKind::Failed,
                "probe unexpectedly submitted",
            ),
            Err(e) => selftest_failed(ClaudeInjectFailureKind::Failed, e.to_string()),
        }
    }

    async fn run_routine(
        &self,
        client: &mut InspectorClient,
        text: &str,
        probe: bool,
        cancellation: &CancellationToken,
    ) -> Result<PageResult, InspectorError> {
        let mut last = String::new();
        for attempt in 0..self.locate_retries.max(1) {
            let object_id = match client.app_instance_object_id(cancellation).await {
                Ok(id) => id,
                Err(error) if is_locate_failure(&error.to_string()) => {
                    last = error.to_string();
                    if attempt + 1 < self.locate_retries.max(1) {
                        tokio::select! { biased; _ = cancellation.cancelled() => return Err(InspectorError::Cancelled { method: "locate".into() }), _ = tokio::time::sleep(LOCATE_RETRY_GAP) => {} }
                        continue;
                    }
                    return Ok(PageResult::Failed(last));
                }
                Err(error) => return Err(error),
            };
            let result = client
                .call_page_routine(
                    &object_id,
                    PAGE_ROUTINE,
                    vec![
                        Value::String(if probe {
                            String::new()
                        } else {
                            text.to_owned()
                        }),
                        Value::Bool(probe),
                    ],
                    cancellation,
                )
                .await?;
            if let Some(exception) = inspector_exception(&result) {
                return Ok(PageResult::Failed(exception));
            }
            let value = result
                .pointer("/result/value")
                .cloned()
                .unwrap_or(Value::Null);
            match parse_page_result(value) {
                PageResult::Failed(reason)
                    if is_locate_failure(&reason) && attempt + 1 < self.locate_retries.max(1) =>
                {
                    last = reason;
                    tokio::select! { biased; _ = cancellation.cancelled() => return Err(InspectorError::Cancelled { method: "locate".into() }), _ = tokio::time::sleep(LOCATE_RETRY_GAP) => {} }
                }
                other => return Ok(other),
            }
        }
        Ok(PageResult::Failed(last))
    }
}

fn is_socket(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_socket())
}
fn failed(kind: ClaudeInjectFailureKind, reason: impl Into<String>) -> ClaudeInjectOutcome {
    ClaudeInjectOutcome::Failed {
        kind,
        reason: reason.into(),
    }
}
fn selftest_failed(
    kind: ClaudeInjectFailureKind,
    reason: impl Into<String>,
) -> ClaudeSelftestOutcome {
    ClaudeSelftestOutcome::Failed {
        kind,
        reason: reason.into(),
    }
}
fn is_locate_failure(reason: &str) -> bool {
    let lower = reason.to_ascii_lowercase();
    ["not found", "listener", "bound method", "no react root"]
        .iter()
        .any(|needle| lower.contains(needle))
}

fn inspector_exception(value: &Value) -> Option<String> {
    value
        .pointer("/exceptionDetails/text")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            value
                .pointer("/exceptionDetails/exception/description")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
}

fn parse_page_result(value: Value) -> PageResult {
    let raw = if let Some(text) = value.as_str() {
        match serde_json::from_str::<Value>(text) {
            Ok(x) => x,
            Err(e) => {
                return PageResult::Failed(format!(
                    "page routine returned unparseable output: {e}"
                ));
            }
        }
    } else {
        value
    };
    if raw.get("ok").and_then(Value::as_bool) != Some(true) {
        return PageResult::Failed(
            raw.get("err")
                .and_then(Value::as_str)
                .unwrap_or("page routine failed")
                .to_owned(),
        );
    }
    if raw.get("foundPrompt").and_then(Value::as_bool) == Some(true) {
        return PageResult::Probe {
            found_input: raw.get("foundInput").and_then(Value::as_bool) == Some(true),
            found_caret: raw.get("foundCaret").and_then(Value::as_bool) == Some(true),
        };
    }
    if raw.get("submitted").and_then(Value::as_bool) == Some(true) {
        return PageResult::Submitted {
            draft_len: raw.get("draftLen").and_then(Value::as_u64).unwrap_or(0) as usize,
        };
    }
    PageResult::Failed("page routine returned an unrecognized shape".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use serde_json::json;
    use tempfile::tempdir;
    use tokio::net::UnixListener;
    use tokio_tungstenite::{accept_async, tungstenite::Message};
    #[test]
    fn page_result_validation_and_failure_classification() {
        assert_eq!(
            parse_page_result(json!(r#"{"ok":true,"submitted":true,"draftLen":4}"#)),
            PageResult::Submitted { draft_len: 4 }
        );
        assert!(is_locate_failure("prompt input not found"));
        assert!(!is_locate_failure("onSubmit threw: refused"));
    }

    #[tokio::test]
    async fn lost_reply_after_submit_is_submitted_unknown_and_not_retried() {
        let tmp = tempdir().unwrap();
        let socket = tmp.path().join("insp/demo.sock");
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            let mut routine_calls = 0;
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let request: Value = serde_json::from_str(&text).unwrap();
                let method = request["method"].as_str().unwrap();
                if method == "Runtime.callFunctionOn" {
                    routine_calls += 1;
                    if routine_calls == 2 {
                        // The actual submit reached the target; the reply was
                        // lost, so the client must not perform another call.
                        break;
                    }
                }
                let result = match method {
                    "Runtime.enable" => json!({}),
                    "Runtime.evaluate" => json!({"result":{"objectId":"listener"}}),
                    "Runtime.getProperties" => {
                        json!({"internalProperties":[{"name":"[[BoundThis]]","value":{"objectId":"app"}}]})
                    }
                    "Runtime.callFunctionOn" => {
                        json!({"result":{"value":r#"{"ok":true,"foundPrompt":true,"foundInput":true,"foundCaret":true}"#}})
                    }
                    _ => json!({}),
                };
                ws.send(Message::Text(
                    json!({"id":request["id"],"result":result})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            }
        });
        let injector = ClaudeInjector::new(tmp.path()).with_timings(
            Duration::from_secs(1),
            Duration::from_millis(1),
            1,
        );
        let cancel = CancellationToken::new();
        let outcome = injector
            .deliver(
                "demo",
                "do one thing",
                Duration::from_secs(1),
                &cancel,
                || None,
            )
            .await;
        assert!(matches!(
            outcome,
            ClaudeInjectOutcome::Failed {
                kind: ClaudeInjectFailureKind::SubmittedUnknown,
                ..
            }
        ));
        server.await.unwrap();
    }
}
