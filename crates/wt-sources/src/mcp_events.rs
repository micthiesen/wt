//! MCP Events (SEP-3415, extension `io.modelcontextprotocol/events`): one poll
//! loop per `mcp.event` automation rule. Each `events/poll` result is written
//! to wt state in one mutation, the new cursor beside the events it returned,
//! so a crash replays undispatched events instead of losing them. The
//! automation engine fires from those pending events; dedupe is the ledger's
//! fire key (rule and `eventId`), never the cursor.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use wt_app_core::context::AppContext;
use wt_config::{AutomationDef, AutomationTrigger, McpServerConfig};
use wt_platform::process::{CommandSpec, ProcessError};

const PROTOCOL_VERSION: &str = "2026-07-28";
const EXTENSION: &str = "io.modelcontextprotocol/events";
const USER_AGENT: &str = "OpenAI File Downloader, XaiImageApiFetch/1.0";
/// The SEP's suggested floor: a server cannot drive a tighter loop.
const MIN_POLL: Duration = Duration::from_secs(1);
const DEFAULT_POLL: Duration = Duration::from_secs(60);
const MAX_POLL: Duration = Duration::from_secs(15 * 60);
const FIRST_RETRY: Duration = Duration::from_secs(30);
const TOKEN_TIMEOUT: Duration = Duration::from_secs(15);
const TOKEN_OUTPUT_LIMIT: usize = 16 * 1024;
const MAX_EVENTS: u64 = 50;
/// Polls one pass may make while the server reports `hasMore`.
const MAX_DRAIN: usize = 20;
const MAX_REPLY_BYTES: usize = 1024 * 1024;
/// Longest stored or prompted string in an event payload.
const MAX_FIELD_CHARS: usize = 2000;

/// Start a poller for each `mcp.event` rule. Each stops with `token`.
pub fn start(
    scope: &wt_runtime::TaskScope,
    ctx: &AppContext,
    token: tokio_util::sync::CancellationToken,
) {
    for rule in ctx
        .config
        .automations
        .iter()
        .filter(|rule| rule.on == AutomationTrigger::McpEvent)
    {
        let Some(server) = rule
            .server
            .as_ref()
            .and_then(|name| ctx.config.mcp_servers.get(name))
        else {
            continue;
        };
        let poller = Poller {
            ctx: ctx.clone(),
            rule: rule.clone(),
            client: match McpClient::new(server.clone()) {
                Ok(client) => client,
                Err(error) => {
                    attention_error(rule, &format!("{error:#}"));
                    continue;
                }
            },
        };
        let token = token.clone();
        scope.spawn(async move { poller.run(token).await });
    }
}

struct Poller {
    ctx: AppContext,
    rule: AutomationDef,
    client: McpClient,
}

impl Poller {
    async fn run(mut self, token: tokio_util::sync::CancellationToken) {
        let mut retry = FIRST_RETRY;
        let mut last_error: Option<String> = None;
        let mut discovered = false;
        loop {
            let wait = match self.pass(&mut discovered, &token).await {
                Ok(wait) => {
                    if last_error.take().is_some() {
                        tracing::info!(target: "wt_attention", event_channel = "attention", event_source = %self.rule.id, event_text = "MCP events are arriving again", "mcp events recovered");
                    }
                    retry = FIRST_RETRY;
                    wait
                }
                Err(_) if token.is_cancelled() => return,
                Err(error) => {
                    let message = format!("{error:#}");
                    if last_error.as_deref() != Some(message.as_str()) {
                        attention_error(&self.rule, &message);
                        last_error = Some(message);
                    }
                    let wait = retry;
                    retry = (retry * 2).min(MAX_POLL);
                    wait
                }
            };
            tokio::select! {
                _ = token.cancelled() => return,
                _ = tokio::time::sleep(wait) => {}
            }
        }
    }

    /// Poll until the server has nothing more, then return how long to wait.
    async fn pass(
        &mut self,
        discovered: &mut bool,
        token: &tokio_util::sync::CancellationToken,
    ) -> Result<Duration> {
        if !*discovered {
            self.client.require_events(&self.ctx, token).await?;
            *discovered = true;
        }
        for _ in 0..MAX_DRAIN {
            let subscription = wt_automations::mcp_subscription(&self.rule);
            let cursor = self.stored_cursor(&subscription).await?;
            let first_sight = cursor.is_none();
            let result = self
                .client
                .poll(&self.ctx, &self.rule, cursor.clone(), token)
                .await?;
            let poll = PollResult::parse(&result)?;
            if poll.malformed > 0 {
                tracing::warn!(target: "wt_attention", event_channel = "attention", event_source = %self.rule.id, event_text = %format!("skipped {} MCP event(s) without an eventId or name", poll.malformed), "mcp events malformed");
            }
            let next = match (&poll.cursor, first_sight) {
                (Some(next), _) => next.clone(),
                // Without a cursor every poll is "from now" and nothing ever
                // arrives; say so rather than discard events quietly.
                (None, true) => bail!(
                    "{} cannot replay this event type (it returned no cursor), so poll delivery cannot work",
                    self.client.server.url
                ),
                // A stray null must not reset a good cursor.
                (None, false) => cursor.clone().unwrap_or_default(),
            };
            let received = wt_app_core::feeds::epoch_ms() as i64;
            // `cursor: null` from the start means "from now": no backfill.
            let events = if first_sight {
                Vec::new()
            } else {
                poll.events
                    .iter()
                    .map(|event| {
                        let mut event = bounded(event.clone());
                        event["receivedAtMs"] = json!(received);
                        event
                    })
                    .collect()
            };
            if poll.truncated && !first_sight {
                tracing::warn!(target: "wt_attention", event_channel = "attention", event_source = %self.rule.id, event_text = "MCP events may have been missed: the server could not replay from wt's cursor", "mcp events truncated");
            }
            let rule = self.rule.id.clone();
            let recorded = next.clone();
            let discarded = self
                .ctx
                .database
                .call(move |store| {
                    Ok(store.record_mcp_poll(
                        &rule,
                        &subscription,
                        Some(&recorded),
                        &events,
                        received,
                    )?)
                })
                .await
                .context("record MCP events")?;
            if discarded > 0 {
                tracing::warn!(target: "wt_attention", event_channel = "attention", event_source = %self.rule.id, event_text = %format!("discarded {discarded} undelivered MCP event(s): too many waiting, or waiting over 7 days"), "mcp events discarded");
            }
            let stuck = cursor.as_deref() == Some(next.as_str()) && poll.events.is_empty();
            if !poll.has_more || first_sight || stuck {
                return Ok(poll.next_poll.clamp(MIN_POLL, MAX_POLL));
            }
        }
        // A server that keeps saying hasMore waits like any other poll.
        Ok(MIN_POLL)
    }

    /// The cursor stored for this exact subscription; none means start now.
    async fn stored_cursor(&self, subscription: &str) -> Result<Option<String>> {
        let rule = self.rule.id.clone();
        let subscription = subscription.to_owned();
        self.ctx
            .database
            .call(move |store| {
                let state = store.read_wt_state()?;
                let entry = &state["mcpEvents"][&rule];
                Ok(
                    (entry["subscription"].as_str() == Some(subscription.as_str()))
                        .then(|| entry["cursor"].as_str().map(str::to_owned))
                        .flatten(),
                )
            })
            .await
            .context("read MCP event cursor")
    }
}

fn attention_error(rule: &AutomationDef, message: &str) {
    tracing::warn!(target: "wt_attention", event_channel = "attention", event_source = %rule.id, event_text = %format!("MCP events unavailable: {message}"), "mcp events unavailable");
}

#[derive(Debug, PartialEq)]
struct PollResult {
    events: Vec<Value>,
    malformed: usize,
    cursor: Option<String>,
    truncated: bool,
    has_more: bool,
    next_poll: Duration,
}

impl PollResult {
    fn parse(result: &Value) -> Result<Self> {
        let events = match result.get("events") {
            Some(Value::Array(events)) => events.clone(),
            None | Some(Value::Null) => Vec::new(),
            Some(_) => bail!("events/poll returned events that are not a list"),
        };
        let total = events.len();
        let events = events
            .into_iter()
            .filter(|event| {
                event.get("eventId").and_then(Value::as_str).is_some()
                    && event.get("name").and_then(Value::as_str).is_some()
            })
            .collect::<Vec<_>>();
        Ok(Self {
            malformed: total - events.len(),
            events,
            // Absent means null (SEP-3415, Cursor Lifecycle).
            cursor: result
                .get("cursor")
                .and_then(Value::as_str)
                .map(str::to_owned),
            truncated: result.get("truncated").and_then(Value::as_bool) == Some(true),
            has_more: result.get("hasMore").and_then(Value::as_bool) == Some(true),
            next_poll: result
                .get("nextPollMs")
                .and_then(Value::as_u64)
                .map_or(DEFAULT_POLL, Duration::from_millis),
        })
    }
}

/// A minimal Streamable HTTP client for the stateless 2026-07-28 protocol:
/// every request carries its version and capabilities, no session.
struct McpClient {
    server: McpServerConfig,
    http: reqwest::Client,
    token: Option<String>,
    next_id: u64,
}

impl McpClient {
    fn new(server: McpServerConfig) -> Result<Self> {
        if rustls::crypto::CryptoProvider::get_default().is_none() {
            let _ = rustls::crypto::ring::default_provider().install_default();
        }
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("build the MCP HTTP client")?;
        Ok(Self {
            server,
            http,
            token: None,
            next_id: 0,
        })
    }

    /// A server that has not advertised the extension must not be sent
    /// `events/*` requests.
    async fn require_events(
        &mut self,
        ctx: &AppContext,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<()> {
        let result = self
            .request(ctx, "server/discover", None, json!({}), cancel)
            .await?;
        let capabilities = &result["capabilities"];
        if capabilities["extensions"].get(EXTENSION).is_none()
            && capabilities.get("events").is_none()
        {
            bail!("{} does not offer MCP events", self.server.url);
        }
        Ok(())
    }

    async fn poll(
        &mut self,
        ctx: &AppContext,
        rule: &AutomationDef,
        cursor: Option<String>,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Value> {
        let name = rule.event.clone().unwrap_or_default();
        let params = json!({
            "name": name,
            "arguments": rule.arguments,
            "cursor": cursor,
            "maxEvents": MAX_EVENTS,
        });
        self.request(ctx, "events/poll", Some(&name), params, cancel)
            .await
    }

    async fn request(
        &mut self,
        ctx: &AppContext,
        method: &str,
        name: Option<&str>,
        params: Value,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Value> {
        for attempt in 0..2 {
            let token = match &self.token {
                Some(token) => Some(token.clone()),
                None => match &self.server.token_command {
                    Some(command) => {
                        let token = fetch_token(ctx, command, cancel).await?;
                        self.token = Some(token.clone());
                        Some(token)
                    }
                    None => None,
                },
            };
            self.next_id += 1;
            let body = request_body(self.next_id, method, params.clone());
            let mut request = self
                .http
                .post(&self.server.url)
                .header("Content-Type", "application/json")
                .header("Accept", "application/json, text/event-stream")
                .header("MCP-Protocol-Version", PROTOCOL_VERSION)
                .header("Mcp-Method", method)
                .json(&body);
            if let Some(name) = name {
                request = request.header("Mcp-Name", name_header(name));
            }
            if let Some(token) = &token {
                request = request.bearer_auth(token);
            }
            let response = tokio::select! {
                _ = cancel.cancelled() => bail!("cancelled"),
                response = request.send() => response.with_context(|| format!("{method} to {}", self.server.url))?,
            };
            let status = response.status();
            // A rotated token: fetch it once more, then report.
            if status == reqwest::StatusCode::UNAUTHORIZED
                && attempt == 0
                && self.server.token_command.is_some()
            {
                self.token = None;
                continue;
            }
            let content_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            let mut text = Vec::new();
            let mut response = response;
            while let Some(chunk) = response
                .chunk()
                .await
                .with_context(|| format!("read the {method} reply"))?
            {
                text.extend_from_slice(&chunk);
                if text.len() > MAX_REPLY_BYTES {
                    bail!("the {method} reply is over {MAX_REPLY_BYTES} bytes");
                }
            }
            let text = String::from_utf8_lossy(&text).into_owned();
            if status == reqwest::StatusCode::UNAUTHORIZED
                || status == reqwest::StatusCode::FORBIDDEN
            {
                bail!(
                    "{} refused the credential (HTTP {})",
                    self.server.url,
                    status.as_u16()
                );
            }
            let message = parse_reply(&content_type, &text, self.next_id)
                .with_context(|| format!("{method}: HTTP {}", status.as_u16()))?;
            if let Some(error) = message.get("error") {
                let code = error["code"].as_i64().unwrap_or_default();
                let text = error["message"].as_str().unwrap_or("error");
                bail!(
                    "{method} failed: {} ({code})",
                    wt_core::sanitize_terminal_text(text)
                );
            }
            return message
                .get("result")
                .cloned()
                .ok_or_else(|| anyhow!("{method} returned no result"));
        }
        bail!("{} refused the credential twice", self.server.url)
    }
}

/// Truncate every string in an event so one payload cannot bloat wt state or
/// the prompt it is delivered in.
fn bounded(value: Value) -> Value {
    match value {
        Value::String(text) if text.chars().count() > MAX_FIELD_CHARS => {
            Value::String(text.chars().take(MAX_FIELD_CHARS).chain(['…']).collect())
        }
        Value::Array(items) => Value::Array(items.into_iter().map(bounded).collect()),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| (key, bounded(value)))
                .collect(),
        ),
        other => other,
    }
}

fn request_body(id: u64, method: &str, mut params: Value) -> Value {
    params["_meta"] = json!({
        "io.modelcontextprotocol/protocolVersion": PROTOCOL_VERSION,
        "io.modelcontextprotocol/clientCapabilities": {"extensions": {EXTENSION: {}}},
    });
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

/// `Mcp-Name` carries non-ASCII names in the transport's Base64 sentinel.
fn name_header(name: &str) -> String {
    if name.bytes().all(|byte| (0x20..0x7f).contains(&byte)) {
        name.to_owned()
    } else {
        format!("=?base64?{}?=", base64(name.as_bytes()))
    }
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, byte)| n | u32::from(*byte) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[(n >> (18 - 6 * i) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// A JSON reply, or the response message of a single-message SSE stream.
fn parse_reply(content_type: &str, text: &str, id: u64) -> Result<Value> {
    if content_type.starts_with("text/event-stream") {
        for data in text
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim)
        {
            if let Ok(message) = serde_json::from_str::<Value>(data)
                && message.get("id").and_then(Value::as_u64) == Some(id)
            {
                return Ok(message);
            }
        }
        bail!("the event stream carried no reply");
    }
    serde_json::from_str(text).map_err(|_| anyhow!("the reply is not JSON"))
}

/// Run the configured token helper. Its stdout is the secret and its stderr
/// may echo it, so neither ever appears in an error or a log.
async fn fetch_token(
    ctx: &AppContext,
    command: &[String],
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<String> {
    let (program, args) = command
        .split_first()
        .context("token_command names no program")?;
    let mut spec = CommandSpec::new(program).args(args);
    spec.timeout = TOKEN_TIMEOUT;
    spec.output_limit = TOKEN_OUTPUT_LIMIT;
    let output = ctx
        .processes
        .run(spec, cancel)
        .await
        .map_err(|error| match error {
            ProcessError::Timeout { .. } => anyhow!("token_command {program} timed out"),
            _ => anyhow!("token_command {program} could not be run"),
        })?;
    if !output.status.success() {
        bail!("token_command {program} failed");
    }
    let token = String::from_utf8(output.stdout)
        .ok()
        .map(|token| token.trim().to_owned())
        .filter(|token| !token.is_empty() && token.bytes().all(|byte| byte.is_ascii_graphic()))
        .with_context(|| format!("token_command {program} printed no usable token"))?;
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A stateless fake MCP server: each poll is answered from `replies` by
    /// the cursor it carries, and every request it saw is kept.
    async fn fake_server(
        replies: Arc<Mutex<std::collections::BTreeMap<String, Value>>>,
        seen: Arc<Mutex<Vec<Value>>>,
    ) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "http://127.0.0.1:{}/mcp",
            listener.local_addr().unwrap().port()
        );
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let replies = replies.clone();
                let seen = seen.clone();
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    let body = loop {
                        let mut chunk = [0; 4096];
                        let read = socket.read(&mut chunk).await.unwrap();
                        buffer.extend_from_slice(&chunk[..read]);
                        let text = String::from_utf8_lossy(&buffer).into_owned();
                        if let Some((head, body)) = text.split_once("\r\n\r\n") {
                            let length = head
                                .lines()
                                .find_map(|line| {
                                    line.to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                        .map(|v| v.trim().parse::<usize>().unwrap())
                                })
                                .unwrap_or(0);
                            if body.len() >= length {
                                break body.to_owned();
                            }
                        }
                    };
                    let request: Value = serde_json::from_str(&body).unwrap();
                    seen.lock().unwrap().push(request.clone());
                    let result = match request["method"].as_str() {
                        Some("server/discover") => {
                            json!({"capabilities": {"extensions": {EXTENSION: {}}}})
                        }
                        _ => {
                            let cursor = request["params"]["cursor"]
                                .as_str()
                                .unwrap_or("null")
                                .to_owned();
                            replies
                                .lock()
                                .unwrap()
                                .get(&cursor)
                                .cloned()
                                .unwrap_or_else(|| json!({"events": [], "cursor": cursor}))
                        }
                    };
                    let reply = json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
                        .to_string();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                        reply.len()
                    );
                    socket.write_all(response.as_bytes()).await.unwrap();
                });
            }
        });
        url
    }

    fn event(id: &str) -> Value {
        json!({"eventId": id, "name": "comment.created", "timestamp": "2026-11-01T00:00:00Z", "data": {"artifactId": "guide1", "text": "hi"}})
    }

    async fn poller(url: &str) -> (wt_app_core::test_support::CommandFixture, Poller) {
        let mut fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let mut config = (*fixture.ctx.config).clone();
        let server = McpServerConfig {
            url: url.to_owned(),
            token_command: None,
        };
        config.mcp_servers.insert("boo".into(), server.clone());
        let rule = AutomationDef {
            id: "comments".into(),
            on: AutomationTrigger::McpEvent,
            run: "builtin:notify".into(),
            server: Some("boo".into()),
            event: Some("comment.created".into()),
            ..AutomationDef::default()
        };
        config.automations = vec![rule.clone()];
        fixture.ctx.config = std::sync::Arc::new(config);
        let poller = Poller {
            ctx: fixture.ctx.clone(),
            rule,
            client: McpClient::new(server).unwrap(),
        };
        (fixture, poller)
    }

    async fn pending(
        fixture: &wt_app_core::test_support::CommandFixture,
    ) -> (Option<String>, Vec<String>) {
        let state = fixture
            .ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        let events = wt_automations::pending_mcp_events(
            &fixture.ctx.config.automations,
            &fixture.ctx.config.mcp_servers,
            &state,
        );
        (
            state["mcpEvents"]["comments"]["cursor"]
                .as_str()
                .map(str::to_owned),
            events.into_iter().map(|event| event.event_id).collect(),
        )
    }

    #[tokio::test]
    async fn first_sight_backfills_nothing_and_a_restart_resumes_without_duplicates() {
        let replies = Arc::new(Mutex::new(std::collections::BTreeMap::from([
            // Events the server already had before wt subscribed.
            (
                "null".to_owned(),
                json!({"events": [event("old")], "cursor": "c1"}),
            ),
            (
                "c1".to_owned(),
                json!({"events": [event("e1")], "cursor": "c2", "nextPollMs": 60000}),
            ),
        ])));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let url = fake_server(replies.clone(), seen.clone()).await;
        let (fixture, mut first) = poller(&url).await;
        let token = tokio_util::sync::CancellationToken::new();
        let mut discovered = false;

        first.pass(&mut discovered, &token).await.unwrap();
        assert_eq!(
            pending(&fixture).await,
            (Some("c1".into()), vec![]),
            "no backfill on first sight"
        );

        assert_eq!(
            first.pass(&mut discovered, &token).await.unwrap(),
            DEFAULT_POLL
        );
        assert_eq!(
            pending(&fixture).await,
            (Some("c2".into()), vec!["e1".into()])
        );

        // A restarted poller replays from the persisted cursor; the server
        // resends e1 with a new one, and it stays one pending event.
        replies.lock().unwrap().insert(
            "c2".into(),
            json!({"events": [event("e1"), event("e2")], "cursor": "c3"}),
        );
        let mut restarted = Poller {
            ctx: fixture.ctx.clone(),
            rule: first.rule.clone(),
            client: McpClient::new(first.client.server.clone()).unwrap(),
        };
        let mut discovered = false;
        restarted.pass(&mut discovered, &token).await.unwrap();
        assert_eq!(
            pending(&fixture).await,
            (Some("c3".into()), vec!["e1".into(), "e2".into()])
        );

        // A truncated reply moves the cursor and reports; it is no event.
        replies.lock().unwrap().insert(
            "c3".into(),
            json!({"events": [], "cursor": "c9", "truncated": true}),
        );
        restarted.pass(&mut discovered, &token).await.unwrap();
        assert_eq!(
            pending(&fixture).await,
            (Some("c9".into()), vec!["e1".into(), "e2".into()])
        );

        // Delivery drops an event; a replay of it can only re-add it, and
        // the ledger's fire key keeps that from firing twice.
        fixture
            .ctx
            .database
            .call(|store| Ok(store.drop_mcp_event("comments", "e1")?))
            .await
            .unwrap();
        assert_eq!(pending(&fixture).await.1, vec!["e2".to_owned()]);

        let requests = seen.lock().unwrap();
        let poll = requests
            .iter()
            .find(|request| request["method"] == "events/poll")
            .unwrap();
        assert_eq!(poll["params"]["name"], "comment.created");
        assert!(poll["params"]["cursor"].is_null());
    }

    #[tokio::test]
    async fn has_more_drains_before_waiting() {
        let replies = Arc::new(Mutex::new(std::collections::BTreeMap::from([
            (
                "c1".to_owned(),
                json!({"events": [event("a")], "cursor": "c2", "hasMore": true}),
            ),
            (
                "c2".to_owned(),
                json!({"events": [event("b")], "cursor": "c3", "nextPollMs": 2000}),
            ),
        ])));
        let url = fake_server(replies, Arc::new(Mutex::new(Vec::new()))).await;
        let (fixture, mut poller) = poller(&url).await;
        let subscription = wt_automations::mcp_subscription(&poller.rule);
        fixture
            .ctx
            .database
            .call(move |store| {
                Ok(store.record_mcp_poll("comments", &subscription, Some("c1"), &[], 0)?)
            })
            .await
            .unwrap();
        let token = tokio_util::sync::CancellationToken::new();
        let mut discovered = false;
        assert_eq!(
            poller.pass(&mut discovered, &token).await.unwrap(),
            Duration::from_secs(2)
        );
        assert_eq!(
            pending(&fixture).await,
            (Some("c3".into()), vec!["a".into(), "b".into()])
        );
    }

    #[test]
    fn poll_results_treat_absent_fields_as_the_spec_defaults() {
        let parsed = PollResult::parse(&json!({"events": [], "cursor": "c1"})).unwrap();
        assert_eq!(parsed.cursor.as_deref(), Some("c1"));
        assert!(!parsed.truncated && !parsed.has_more);
        assert_eq!(parsed.next_poll, DEFAULT_POLL);
        let parsed = PollResult::parse(&json!({"truncated": true, "nextPollMs": 5})).unwrap();
        assert_eq!(parsed.cursor, None);
        assert!(parsed.truncated);
        assert_eq!(parsed.next_poll.clamp(MIN_POLL, MAX_POLL), MIN_POLL);
        let parsed =
            PollResult::parse(&json!({"events": [{"name": "x"}, {"eventId": "e", "name": "x"}]}))
                .unwrap();
        assert_eq!((parsed.events.len(), parsed.malformed), (1, 1));
        assert_eq!(
            bounded(json!({"t": "x".repeat(5000)}))["t"]
                .as_str()
                .unwrap()
                .chars()
                .count(),
            MAX_FIELD_CHARS + 1
        );
    }

    #[test]
    fn requests_carry_the_protocol_metadata_and_header_safe_names() {
        let body = request_body(7, "events/poll", json!({"name": "comment.created"}));
        assert_eq!(
            body["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"],
            PROTOCOL_VERSION
        );
        assert!(
            body["params"]["_meta"]["io.modelcontextprotocol/clientCapabilities"]["extensions"]
                [EXTENSION]
                .is_object()
        );
        assert_eq!(name_header("comment.created"), "comment.created");
        assert_eq!(name_header("café"), "=?base64?Y2Fmw6k=?=");
        let sse = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{}}\n\n";
        assert!(parse_reply("text/event-stream", sse, 7).unwrap()["result"].is_object());
    }
}
