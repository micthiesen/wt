use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde_json::Value;
use tokio_util::sync::CancellationToken;
use wt_tmux::PaneTarget;

use super::{
    ClaudeInjectFailureKind, ClaudeInjectOutcome, ClaudeInjector, ClaudeSessionManager,
    ClaudeSessionManagerError, ClaudeSessionTarget, RegistryStatus, identity::session_jsonl_path,
    lifecycle::SessionLock, transcript::parse_timestamp_ms,
};

const HUMAN_WAIT_POLL: Duration = Duration::from_millis(250);
const WARM_SETTLE: Duration = Duration::from_millis(300);
const READY_POLL: Duration = Duration::from_millis(350);
const READY_MAX: Duration = Duration::from_secs(12);
const PASTE_VERIFY: Duration = Duration::from_secs(5);
const PASTE_RETRIES: usize = 3;
const SUBMIT_GAP: Duration = Duration::from_millis(250);
const CONFIRM_POLL: Duration = Duration::from_millis(250);
const CONFIRM_MAX: Duration = Duration::from_secs(8);
const CONFIRM_TAIL_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClaudeMessageTransport {
    Inspector,
    Terminal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClaudeMessageOutcome {
    Sent {
        transport: ClaudeMessageTransport,
        cold_started: bool,
        delivered: Option<bool>,
        resent: bool,
        fallback: Option<ClaudeInjectFailureKind>,
    },
    Failed {
        reason: String,
        maybe_submitted: bool,
    },
}

#[derive(Clone)]
pub struct ClaudeMessenger {
    sessions: ClaudeSessionManager,
    injector: ClaudeInjector,
}

impl ClaudeMessenger {
    pub fn new(sessions: ClaudeSessionManager, injector: ClaudeInjector) -> Self {
        Self { sessions, injector }
    }

    /// Serialize, cold-start if needed, prefer Inspector submission, and use
    /// guarded literal terminal paste only before the Inspector submit may
    /// have reached Claude.
    pub async fn send(
        &self,
        target: &ClaudeSessionTarget,
        text: &str,
        cancel: &CancellationToken,
    ) -> Result<ClaudeMessageOutcome, ClaudeSessionManagerError> {
        let sender = std::env::var("WT_AGENT").ok();
        self.send_with_sender(target, text, sender.as_deref(), cancel)
            .await
    }

    pub async fn send_with_sender(
        &self,
        target: &ClaudeSessionTarget,
        text: &str,
        sender: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<ClaudeMessageOutcome, ClaudeSessionManagerError> {
        if text.trim().is_empty() {
            return Ok(ClaudeMessageOutcome::Failed {
                reason: "message is empty".into(),
                maybe_submitted: false,
            });
        }
        let _send_lock = SessionLock::acquire(
            &self.sessions.paths().lock_dir,
            &format!("__claude_send__{}", self.sessions.tmux_name(target)),
            Duration::from_secs(24 * 60 * 60),
            cancel,
        )
        .await?;
        let text = stamp_sender(text, sender);
        let (session, cold_started) = self.sessions.ensure(target, cancel).await?;
        let tmux_name = self.sessions.tmux_name(target);

        let mut snapshot = Some(session.clone());
        let mut require_ready = session.status == RegistryStatus::Waiting;
        let inspector_enabled =
            !std::env::var("WT_INSPECT").is_ok_and(|v| v.eq_ignore_ascii_case("off"));
        if inspector_enabled {
            loop {
                snapshot = self
                    .wait_until_unblocked(
                        target,
                        tmux_name.as_str(),
                        snapshot,
                        require_ready,
                        cancel,
                    )
                    .await?;
                let since_ms = now_ms();
                let outcome = self
                    .injector
                    .deliver(
                        &tmux_name,
                        &text,
                        if cold_started {
                            Duration::from_secs(20)
                        } else {
                            Duration::from_secs(4)
                        },
                        cancel,
                        || match self.sessions.find(target) {
                            Ok(Some(session)) if session.status == RegistryStatus::Waiting => {
                                Some(format!(
                                    "{} is waiting on a human{}",
                                    tmux_name,
                                    session
                                        .waiting_for
                                        .map(|x| format!(" ({x})"))
                                        .unwrap_or_default()
                                ))
                            }
                            Err(error) => Some(format!(
                                "cannot establish safe Claude session readiness: {error}"
                            )),
                            _ => None,
                        },
                    )
                    .await;
                if let ClaudeInjectOutcome::Failed {
                    kind: ClaudeInjectFailureKind::Blocked,
                    ..
                } = &outcome
                {
                    snapshot = self.sessions.find(target)?;
                    require_ready = true;
                    continue;
                }
                let idle_at_submit = snapshot
                    .as_ref()
                    .is_some_and(|s| s.status == RegistryStatus::Idle);
                match outcome {
                    ClaudeInjectOutcome::Submitted { .. } => {
                        let delivered = if is_harness_command(&text) {
                            None
                        } else {
                            self.confirm(target, &text, since_ms, idle_at_submit, cancel)
                                .await
                        };
                        return Ok(ClaudeMessageOutcome::Sent {
                            transport: ClaudeMessageTransport::Inspector,
                            cold_started,
                            delivered,
                            resent: false,
                            fallback: None,
                        });
                    }
                    ClaudeInjectOutcome::Failed {
                        kind: ClaudeInjectFailureKind::SubmittedUnknown,
                        reason,
                    } => {
                        if self.confirm(target, &text, since_ms, true, cancel).await == Some(true) {
                            return Ok(ClaudeMessageOutcome::Sent {
                                transport: ClaudeMessageTransport::Inspector,
                                cold_started,
                                delivered: Some(true),
                                resent: false,
                                fallback: None,
                            });
                        }
                        return Ok(ClaudeMessageOutcome::Failed {
                            reason: format!(
                                "{reason}; message was not confirmed in the transcript. Resend only if it is still missing because a duplicate is possible."
                            ),
                            maybe_submitted: true,
                        });
                    }
                    ClaudeInjectOutcome::Failed { kind, reason } => {
                        let delivered = self
                            .terminal_send(target, &tmux_name, &text, cold_started, cancel)
                            .await?;
                        return Ok(match delivered {
                            TerminalOutcome::Sent { delivered, resent } => {
                                ClaudeMessageOutcome::Sent {
                                    transport: ClaudeMessageTransport::Terminal,
                                    cold_started,
                                    delivered,
                                    resent,
                                    fallback: Some(kind),
                                }
                            }
                            TerminalOutcome::Failed {
                                reason: terminal_reason,
                                maybe_submitted,
                            } => ClaudeMessageOutcome::Failed {
                                reason: format!(
                                    "Inspector unavailable ({reason}); terminal fallback failed: {terminal_reason}"
                                ),
                                maybe_submitted,
                            },
                        });
                    }
                }
            }
        }
        let _snapshot = self
            .wait_until_unblocked(target, &tmux_name, snapshot, require_ready, cancel)
            .await?;
        match self
            .terminal_send(target, &tmux_name, &text, cold_started, cancel)
            .await?
        {
            TerminalOutcome::Sent { delivered, resent } => Ok(ClaudeMessageOutcome::Sent {
                transport: ClaudeMessageTransport::Terminal,
                cold_started,
                delivered,
                resent,
                fallback: Some(ClaudeInjectFailureKind::Failed),
            }),
            TerminalOutcome::Failed {
                reason,
                maybe_submitted,
            } => Ok(ClaudeMessageOutcome::Failed {
                reason,
                maybe_submitted,
            }),
        }
    }

    async fn wait_until_unblocked(
        &self,
        target: &ClaudeSessionTarget,
        name: &str,
        mut snapshot: Option<super::ClaudeSessionInfo>,
        require_ready: bool,
        cancel: &CancellationToken,
    ) -> Result<Option<super::ClaudeSessionInfo>, ClaudeSessionManagerError> {
        while snapshot
            .as_ref()
            .is_some_and(|s| s.status == RegistryStatus::Waiting)
            || (require_ready && snapshot.is_none())
        {
            tokio::select! { biased; _ = cancel.cancelled() => return Err(ClaudeSessionManagerError::Operation { operation: "send", detail: format!("cancelled while {name} waits for a human") }), _ = tokio::time::sleep(HUMAN_WAIT_POLL) => {} }
            snapshot = self.sessions.find(target)?;
        }
        Ok(snapshot)
    }

    async fn terminal_send(
        &self,
        target: &ClaudeSessionTarget,
        name: &str,
        text: &str,
        cold_started: bool,
        cancel: &CancellationToken,
    ) -> Result<TerminalOutcome, ClaudeSessionManagerError> {
        let tmux = self.sessions.tmux();
        let pane = PaneTarget::active_session_pane(name);
        if !tmux.session_exists(name, cancel).await? {
            return Ok(TerminalOutcome::Failed {
                reason: format!(
                    "{name} stopped before terminal delivery; no replacement session was started"
                ),
                maybe_submitted: false,
            });
        }
        if let Some(session) = self.sessions.find(target)?
            && session.status == RegistryStatus::Waiting
        {
            return Ok(TerminalOutcome::Failed {
                reason: format!(
                    "{name} is waiting on a human{}",
                    session
                        .waiting_for
                        .map(|x| format!(" ({x})"))
                        .unwrap_or_default()
                ),
                maybe_submitted: false,
            });
        }
        if cold_started {
            let _ = wait_for_pane_ready(tmux, &pane, cancel).await;
            if cancel.is_cancelled() {
                return Ok(TerminalOutcome::Failed {
                    reason: "cancelled while waiting for the pane to settle".into(),
                    maybe_submitted: false,
                });
            }
        } else {
            tokio::select! { biased; _ = cancel.cancelled() => return Ok(TerminalOutcome::Failed { reason: "cancelled before terminal paste".into(), maybe_submitted: false }), _ = tokio::time::sleep(WARM_SETTLE) => {} }
        }
        if self
            .sessions
            .find(target)?
            .is_some_and(|s| s.status == RegistryStatus::Waiting)
        {
            return Ok(TerminalOutcome::Failed {
                reason: format!("{name} began waiting for a human before terminal paste"),
                maybe_submitted: false,
            });
        }
        let idle_at_submit = self
            .sessions
            .find(target)?
            .is_some_and(|s| s.status == RegistryStatus::Idle);
        let since_ms = now_ms();
        let baseline = tmux
            .capture_pane(&pane, None, cancel)
            .await
            .unwrap_or_default()
            .trim()
            .to_owned();
        if let Err(error) = tmux.send_literal(&pane, text, cancel).await {
            return Ok(TerminalOutcome::Failed {
                reason: error.to_string(),
                maybe_submitted: true,
            });
        }
        tokio::select! { biased; _ = cancel.cancelled() => return Ok(TerminalOutcome::Failed { reason: "cancelled after terminal paste".into(), maybe_submitted: true }), _ = tokio::time::sleep(Duration::from_millis(500)) => {} }
        let deadline = tokio::time::Instant::now() + PASTE_VERIFY;
        let mut changed = tmux
            .capture_pane(&pane, None, cancel)
            .await
            .unwrap_or_default()
            .trim()
            != baseline;
        for _ in 0..PASTE_RETRIES {
            if changed || tokio::time::Instant::now() >= deadline {
                break;
            }
            let _ = wait_for_pane_ready(tmux, &pane, cancel).await;
            if cancel.is_cancelled() {
                return Ok(TerminalOutcome::Failed {
                    reason: "cancelled while verifying terminal paste".into(),
                    maybe_submitted: true,
                });
            }
            if tmux
                .capture_pane(&pane, None, cancel)
                .await
                .unwrap_or_default()
                .trim()
                != baseline
            {
                break;
            }
            if let Err(error) = tmux.send_literal(&pane, text, cancel).await {
                return Ok(TerminalOutcome::Failed {
                    reason: error.to_string(),
                    maybe_submitted: true,
                });
            }
            tokio::select! { biased; _ = cancel.cancelled() => return Ok(TerminalOutcome::Failed { reason: "cancelled after terminal paste".into(), maybe_submitted: true }), _ = tokio::time::sleep(Duration::from_millis(500)) => {} }
            changed = tmux
                .capture_pane(&pane, None, cancel)
                .await
                .unwrap_or_default()
                .trim()
                != baseline;
        }
        // A paste may have been accepted despite a delayed pane refresh; the
        // bounded retry checks above only run before submitting, never after.
        tokio::select! { biased; _ = cancel.cancelled() => return Ok(TerminalOutcome::Failed { reason: "cancelled before submit keys".into(), maybe_submitted: true }), _ = tokio::time::sleep(SUBMIT_GAP) => {} }
        for i in 0..2 {
            if i > 0 {
                tokio::select! { biased; _ = cancel.cancelled() => return Ok(TerminalOutcome::Failed { reason: "cancelled between submit keys".into(), maybe_submitted: true }), _ = tokio::time::sleep(SUBMIT_GAP) => {} }
            }
            if let Err(error) = tmux.send_keys(&pane, &["Enter"], cancel).await {
                return Ok(TerminalOutcome::Failed {
                    reason: error.to_string(),
                    maybe_submitted: true,
                });
            }
        }
        let delivered = if is_harness_command(text) {
            None
        } else {
            self.confirm(target, text, since_ms, idle_at_submit, cancel)
                .await
        };
        Ok(TerminalOutcome::Sent {
            delivered,
            resent: false,
        })
    }

    async fn confirm(
        &self,
        target: &ClaudeSessionTarget,
        text: &str,
        since_ms: i64,
        idle: bool,
        cancel: &CancellationToken,
    ) -> Option<bool> {
        let deadline = tokio::time::Instant::now() + CONFIRM_MAX;
        loop {
            if transcript_has_prompt(self.sessions.paths(), target, text, since_ms) {
                return Some(true);
            }
            if tokio::time::Instant::now() >= deadline {
                return if idle { Some(false) } else { None };
            }
            tokio::select! { biased; _ = cancel.cancelled() => return None, _ = tokio::time::sleep(CONFIRM_POLL) => {} }
        }
    }
}

enum TerminalOutcome {
    Sent {
        delivered: Option<bool>,
        resent: bool,
    },
    Failed {
        reason: String,
        maybe_submitted: bool,
    },
}

async fn wait_for_pane_ready(
    tmux: &wt_tmux::TmuxClient,
    pane: &PaneTarget,
    cancel: &CancellationToken,
) -> bool {
    let deadline = tokio::time::Instant::now() + READY_MAX;
    tokio::select! { biased; _ = cancel.cancelled() => return false, _ = tokio::time::sleep(READY_POLL) => {} }
    let mut previous = None;
    while tokio::time::Instant::now() < deadline {
        let current = tmux
            .capture_pane(pane, None, cancel)
            .await
            .unwrap_or_default()
            .trim()
            .to_owned();
        if !current.is_empty() && previous.as_deref() == Some(current.as_str()) {
            return true;
        }
        previous = Some(current);
        tokio::select! { biased; _ = cancel.cancelled() => return false, _ = tokio::time::sleep(READY_POLL) => {} }
    }
    false
}

fn transcript_has_prompt(
    paths: &super::ClaudePaths,
    target: &ClaudeSessionTarget,
    text: &str,
    since_ms: i64,
) -> bool {
    let id = crate::claude_session_id(&target.cwd, target.managed_name.as_deref());
    let path = session_jsonl_path(&paths.home, &target.cwd, &id);
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let Ok(size) = file.metadata().map(|m| m.len()) else {
        return false;
    };
    let start = size.saturating_sub(CONFIRM_TAIL_BYTES);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return false;
    }
    let mut bytes = Vec::with_capacity((size - start) as usize);
    if file.read_to_end(&mut bytes).is_err() {
        return false;
    }
    let body = String::from_utf8_lossy(&bytes);
    let needle = normalize_for_match(text);
    if needle.is_empty() {
        return false;
    }
    body.lines()
        .skip(usize::from(start > 0))
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .any(|entry| {
            if entry.get("type").and_then(Value::as_str) != Some("user") {
                return false;
            }
            if entry
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(parse_timestamp_ms)
                .is_some_and(|time| time < since_ms)
            {
                return false;
            }
            user_body(&entry).is_some_and(|found| normalize_for_match(&found).contains(&needle))
        })
}

fn user_body(entry: &Value) -> Option<String> {
    match entry.pointer("/message/content")? {
        Value::String(text) => Some(text.clone()),
        Value::Array(blocks) => Some(
            blocks
                .iter()
                .filter_map(|b| {
                    if b.get("type").and_then(Value::as_str) == Some("text") {
                        b.get("text").and_then(Value::as_str)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => None,
    }
}
fn normalize_for_match(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
        .chars()
        .take(160)
        .collect()
}
fn is_harness_command(text: &str) -> bool {
    let token = text.split_whitespace().next().unwrap_or("");
    let Some(rest) = token.strip_prefix('/').or_else(|| token.strip_prefix('$')) else {
        return false;
    };
    !rest.is_empty()
        && rest
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}
fn stamp_sender(text: &str, sender: Option<&str>) -> String {
    let Some(tag) = sender.map(str::trim).filter(|x| !x.is_empty()) else {
        return text.to_owned();
    };
    if text.trim_start().starts_with('[') || is_harness_command(text) {
        text.to_owned()
    } else {
        format!("[{tag}] {text}")
    }
}
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}
