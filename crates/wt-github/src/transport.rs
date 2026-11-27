//! The in-process HTTPS path for GitHub reads.
//!
//! A `gh api` subprocess pays process start, credential lookup and a TLS
//! handshake on every request. Reads here share one keep-alive client and
//! gh's own token, which is held in memory only. Writes stay on gh.

use std::{
    collections::HashMap,
    ffi::OsString,
    future::Future,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use serde_json::Value;
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

use crate::{
    checks::{is_rate_limit, text_failure},
    types::GithubError,
};

/// How long gh's answer for a host's token stands before gh is asked again,
/// so an account switch or a logout reaches a long-lived process.
const TOKEN_TTL: Duration = Duration::from_secs(5 * 60);
/// How long reads stay on gh after the API host could not be connected to
/// before any request to it had succeeded.
const UNREACHABLE_FOR: Duration = Duration::from_secs(5 * 60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// One request, body included: what a gh subprocess is given.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(45);

static USER_AGENT: OnceLock<String> = OnceLock::new();

/// Name the running wt in the `User-Agent` of every API request. The
/// binary calls this once at startup; a library crate has no version.
pub fn set_user_agent_version(version: &str) {
    let _ = USER_AGENT.set(format!("wt/{version}"));
}

fn client() -> Option<&'static reqwest::Client> {
    static CLIENT: OnceLock<Option<reqwest::Client>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            if rustls::crypto::CryptoProvider::get_default().is_none() {
                let _ = rustls::crypto::ring::default_provider().install_default();
            }
            reqwest::Client::builder()
                .user_agent(USER_AGENT.get().map_or("wt/dev", String::as_str))
                .timeout(REQUEST_TIMEOUT)
                .connect_timeout(CONNECT_TIMEOUT)
                // No read needs a redirect, and following one would rest
                // the token's safety on where the reply points.
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|error| tracing::warn!(%error, "cannot build the GitHub HTTP client"))
                .ok()
        })
        .as_ref()
}

/// Where API requests for one GitHub host go.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Endpoint {
    /// The host gh knows the token by.
    pub host: String,
    graphql: String,
    /// Ends with a slash; REST paths are appended.
    rest: String,
}

impl Endpoint {
    /// The API of `host`, placed as gh places it: beside github.com and
    /// its tenants, under `/api` on GitHub Enterprise Server. `None` for a
    /// name that is not a plain host.
    pub fn for_host(host: &str) -> Option<Self> {
        let host = host.trim().to_ascii_lowercase();
        let plain = !host.is_empty()
            && host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-:".contains(&b));
        if !plain || host == "github.localhost" {
            return None;
        }
        let (graphql, rest) = if host == "github.com" || host.ends_with(".ghe.com") {
            (
                format!("https://api.{host}/graphql"),
                format!("https://api.{host}/"),
            )
        } else {
            (
                format!("https://{host}/api/graphql"),
                format!("https://{host}/api/v3/"),
            )
        };
        Some(Self {
            host,
            graphql,
            rest,
        })
    }

    /// An API served at `base`, for isolated fixtures.
    #[cfg(test)]
    pub fn at(base: &str) -> Option<Self> {
        let base = base.trim_end_matches('/');
        let url = reqwest::Url::parse(base).ok()?;
        let host = match url.port() {
            Some(port) => format!("{}:{port}", url.host_str()?),
            None => url.host_str()?.to_owned(),
        };
        Some(Self {
            host,
            graphql: format!("{base}/graphql"),
            rest: format!("{base}/"),
        })
    }
}

/// The host of a repository's web URL, with its port when it names one.
pub(crate) fn url_host(url: &str) -> Option<String> {
    let url = reqwest::Url::parse(url).ok()?;
    let host = url.host_str()?;
    Some(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    })
}

/// What asking gh for a token came to.
pub(crate) enum TokenRead {
    Found(String),
    /// gh has none for this host; remembered like a token.
    Absent,
    /// The question was interrupted; nothing is remembered.
    Unknown,
}

/// A token as gh prints it: one word of the characters GitHub issues.
pub(crate) fn plausible_token(text: &str) -> Option<String> {
    let token = text.trim();
    (token.len() >= 20
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.=".contains(&b)))
    .then(|| token.to_owned())
}

pub(crate) type TokenKey = (OsString, String);

struct TokenEntry {
    token: Option<String>,
    read_at: Instant,
    /// The API refused this token and gh had no other: gh is not asked
    /// again until the answer ages out.
    refused: bool,
}

/// One key's remembered answer, and the turn to ask gh for a new one.
#[derive(Default)]
struct TokenSlot {
    entry: Mutex<Option<TokenEntry>>,
    asking: AsyncMutex<()>,
}

impl TokenSlot {
    /// The remembered answer while it stands for a caller that had
    /// `rejected` refused.
    fn remembered(&self, rejected: Option<&str>) -> Option<Option<String>> {
        let entry = self
            .entry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = entry
            .as_ref()
            .filter(|entry| entry.read_at.elapsed() < TOKEN_TTL)?;
        (rejected.is_none() || entry.refused || entry.token.as_deref() != rejected)
            .then(|| entry.token.clone())
    }
}

fn token_slot(key: &TokenKey) -> std::sync::Arc<TokenSlot> {
    static SLOTS: OnceLock<Mutex<HashMap<TokenKey, std::sync::Arc<TokenSlot>>>> = OnceLock::new();
    SLOTS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(key.clone())
        .or_default()
        .clone()
}

/// The token for `key`, asking gh through `read` when none is remembered,
/// the remembered answer is old, or it is the one `rejected` names (the
/// API just refused it). Callers for one key share one question, other keys
/// do not wait for it, and a cancelled caller stops waiting. A token gh
/// repeats after the API refused it is remembered as refused, so a read's
/// other requests do not each ask gh again.
pub(crate) async fn token<F, Fut>(
    key: &TokenKey,
    rejected: Option<&str>,
    cancellation: &CancellationToken,
    read: F,
) -> Option<String>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = TokenRead>,
{
    let slot = token_slot(key);
    if let Some(token) = slot.remembered(rejected) {
        return token;
    }
    let _turn = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return None,
        turn = slot.asking.lock() => turn,
    };
    // Whoever held the turn may have asked for the same reason.
    if let Some(token) = slot.remembered(rejected) {
        return token;
    }
    let token = match read().await {
        TokenRead::Found(token) => Some(token),
        TokenRead::Absent => None,
        TokenRead::Unknown => return None,
    };
    *slot
        .entry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(TokenEntry {
        refused: rejected.is_some() && token.as_deref() == rejected,
        token: token.clone(),
        read_at: Instant::now(),
    });
    token
}

#[derive(Default)]
struct Health {
    /// A request to this host has been answered.
    proven: bool,
    unreachable_until: Option<Instant>,
}

fn health() -> std::sync::MutexGuard<'static, HashMap<String, Health>> {
    static HEALTH: OnceLock<Mutex<HashMap<String, Health>>> = OnceLock::new();
    HEALTH
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Whether requests to `host` are worth trying. A host that never answered
/// and could not be connected to (a proxy or trust store only gh is set up
/// for) is left to gh for a while.
pub(crate) fn usable(host: &str) -> bool {
    health()
        .get(host)
        .and_then(|health| health.unreachable_until)
        .is_none_or(|until| until <= Instant::now())
}

fn answered(host: &str) {
    let mut health = health();
    let health = health.entry(host.to_owned()).or_default();
    health.proven = true;
    health.unreachable_until = None;
}

/// Forget what is known of `host`: a fixture's port may have been another
/// fixture's.
#[cfg(test)]
pub(crate) fn forget(host: &str) {
    health().remove(host);
}

/// Record a failed connection. True when the host has never answered, so
/// the caller should use gh for this request.
fn unreachable(host: &str) -> bool {
    let mut health = health();
    let health = health.entry(host.to_owned()).or_default();
    if health.proven {
        return false;
    }
    health.unreachable_until = Some(Instant::now() + UNREACHABLE_FOR);
    true
}

/// One API request.
pub(crate) enum Call<'a> {
    Graphql {
        query: &'a str,
        variables: &'a [(String, Value)],
    },
    /// A REST path relative to the API root, with its query string.
    Get(&'a str),
}

pub(crate) struct HttpReply {
    pub status: u16,
    /// `name: value` lines, as `gh api --include` prints them.
    pub headers: String,
    pub body: Vec<u8>,
}

pub(crate) enum HttpFailure {
    Cancelled,
    /// No connection could be made and the host has never answered.
    Unreachable(String),
    Failed(String),
}

pub(crate) async fn send(
    endpoint: &Endpoint,
    token: &str,
    call: &Call<'_>,
    cancellation: &CancellationToken,
) -> Result<HttpReply, HttpFailure> {
    let client =
        client().ok_or_else(|| HttpFailure::Unreachable("no GitHub HTTP client".into()))?;
    let mut authorization = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| HttpFailure::Unreachable("gh's token is not a header value".into()))?;
    // Keeps the token out of any debug rendering of the request.
    authorization.set_sensitive(true);
    let request = match call {
        Call::Graphql { query, variables } => {
            let variables: serde_json::Map<String, Value> = variables.iter().cloned().collect();
            client
                .post(&endpoint.graphql)
                .json(&serde_json::json!({"query": query, "variables": variables}))
        }
        Call::Get(path) => client
            .get(format!("{}{path}", endpoint.rest))
            .header(reqwest::header::ACCEPT, "application/vnd.github+json"),
    }
    // A fleet's pull requests are a large JSON reply that compresses well;
    // gh asks for the same.
    .header(reqwest::header::ACCEPT_ENCODING, "gzip")
    .header(reqwest::header::AUTHORIZATION, authorization);
    let exchange = async {
        let response = request.send().await?;
        let status = response.status().as_u16();
        let gzip = response
            .headers()
            .get(reqwest::header::CONTENT_ENCODING)
            .is_some_and(|encoding| encoding.as_bytes().eq_ignore_ascii_case(b"gzip"));
        let headers = response
            .headers()
            .iter()
            .filter_map(|(name, value)| Some(format!("{name}: {}", value.to_str().ok()?)))
            .collect::<Vec<_>>()
            .join("\n");
        let body = response.bytes().await?.to_vec();
        Ok::<_, reqwest::Error>((status, headers, gzip, body))
    };
    let result = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err(HttpFailure::Cancelled),
        result = exchange => result,
    };
    match result {
        Ok((status, headers, gzip, body)) => {
            answered(&endpoint.host);
            let body = if gzip {
                inflate(&body).map_err(|error| {
                    HttpFailure::Failed(format!("GitHub reply could not be decompressed: {error}"))
                })?
            } else {
                body
            };
            Ok(HttpReply {
                status,
                headers,
                body,
            })
        }
        Err(error) => {
            let connect = error.is_connect();
            let message = if error.is_timeout() {
                format!(
                    "GitHub request timeout after {}s",
                    REQUEST_TIMEOUT.as_secs()
                )
            } else {
                // The URL names the repository and nothing secret, but the
                // cause says everything a reader needs.
                let error = error.without_url();
                match std::error::Error::source(&error) {
                    Some(source) => format!("GitHub request failed: {error}: {source}"),
                    None => format!("GitHub request failed: {error}"),
                }
            };
            if connect && unreachable(&endpoint.host) {
                Err(HttpFailure::Unreachable(message))
            } else {
                Err(HttpFailure::Failed(message))
            }
        }
    }
}

/// The most a reply may inflate to: far above any read, and bounded.
const MAX_REPLY_BYTES: u64 = 256 * 1024 * 1024;

fn inflate(body: &[u8]) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut inflated = Vec::new();
    flate2::read::GzDecoder::new(body)
        .take(MAX_REPLY_BYTES + 1)
        .read_to_end(&mut inflated)?;
    if inflated.len() as u64 > MAX_REPLY_BYTES {
        return Err(std::io::Error::other("reply too large"));
    }
    Ok(inflated)
}

/// One reply as the read paths judge it, whichever transport carried it.
pub(crate) struct ApiReply {
    /// gh exited zero, or the API answered 2xx with no GraphQL errors (gh
    /// exits non-zero for those too).
    pub ok: bool,
    pub body: Vec<u8>,
    /// What the transport said beside the body: gh's stderr, or the HTTP
    /// status line of a refused request.
    pub diagnostics: String,
    /// When the limit the reply's headers report exhausted lifts.
    pub reset_at_ms: Option<u64>,
}

impl ApiReply {
    /// The failure a reply that is not `ok` stands for.
    pub fn failure(&self) -> GithubError {
        let (message, transient) =
            text_failure(&self.diagnostics, &String::from_utf8_lossy(&self.body));
        if is_rate_limit(&message) {
            GithubError::RateLimit {
                message,
                reset_at_ms: None,
            }
        } else if transient {
            GithubError::Transient { message }
        } else {
            GithubError::Command(message)
        }
    }
}

impl HttpReply {
    pub fn into_reply(self, graphql: bool, reset_at_ms: Option<u64>) -> ApiReply {
        let success = (200..300).contains(&self.status);
        let diagnostics = if success {
            String::new()
        } else {
            let message = serde_json::from_slice::<Value>(&self.body)
                .ok()
                .and_then(|body| Some(body.get("message")?.as_str()?.trim().to_owned()))
                .filter(|message| !message.is_empty())
                .unwrap_or_else(|| "request refused".into());
            // GitHub refuses a limited request with 429, or with 403 and
            // the headers that say when to come back.
            let limited = self.status == 429 || (self.status == 403 && reset_at_ms.is_some());
            if limited && !is_rate_limit(&message) {
                format!("HTTP {}: rate limit: {message}", self.status)
            } else {
                format!("HTTP {}: {message}", self.status)
            }
        };
        let ok = success && !(graphql && has_errors(&self.body));
        ApiReply {
            ok,
            body: self.body,
            diagnostics,
            reset_at_ms,
        }
    }
}

fn has_errors(body: &[u8]) -> bool {
    #[derive(serde::Deserialize)]
    struct Errors {
        #[serde(default)]
        errors: Option<Vec<serde::de::IgnoredAny>>,
    }
    serde_json::from_slice::<Errors>(body)
        .is_ok_and(|reply| reply.errors.is_some_and(|errors| !errors.is_empty()))
}
