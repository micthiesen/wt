//! The HTTPS read path against an in-process API, with a fake gh that only
//! hands out the token (and stands in where a test says reads fall back).

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use serde_json::{Value, json};
use tempfile::tempdir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tokio_util::sync::CancellationToken;
use wt_platform::process::ProcessRunner;

use super::{
    transport_tests::{fake_gh, options, paged_pr_reply},
    *,
};

const TOKEN: &str = "gho_0123456789abcdefghij";
const NEWER_TOKEN: &str = "gho_abcdefghij0123456789";
const BUDGET: &str = r#"{"cost":1,"remaining":4990,"limit":5000,"resetAt":"2026-10-04T13:00:00Z"}"#;

/// One request the API received.
#[derive(Clone, Debug)]
struct Seen {
    method: String,
    path: String,
    authorization: String,
    user_agent: String,
    accept_encoding: String,
    body: Value,
}

impl Seen {
    fn query(&self) -> &str {
        self.body["query"].as_str().unwrap_or("")
    }
}

type Headers = Vec<(&'static str, String)>;

enum Answer {
    Reply(u16, Headers, String),
    /// Close the connection without a response.
    HangUp,
}

fn ok(body: impl Into<String>) -> Answer {
    Answer::Reply(200, Vec::new(), body.into())
}

struct Api {
    base: String,
    host: String,
    seen: Arc<Mutex<Vec<Seen>>>,
    /// The most requests that were being answered at one time.
    peak: Arc<AtomicUsize>,
}

impl Api {
    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

/// Serve `answer` on a loopback port, holding each request for `hold`
/// so overlapping requests can be counted.
async fn serve(hold: Duration, answer: impl Fn(&Seen) -> Answer + Send + Sync + 'static) -> Api {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let host = listener.local_addr().unwrap().to_string();
    // An earlier fixture may have left this port marked unreachable.
    transport::forget(&host);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let peak = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let answer = Arc::new(answer);
    tokio::spawn({
        let (seen, peak) = (seen.clone(), peak.clone());
        async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (seen, peak, active, answer) =
                    (seen.clone(), peak.clone(), active.clone(), answer.clone());
                tokio::spawn(async move {
                    let mut stream = stream;
                    while let Some(request) = read_request(&mut stream).await {
                        seen.lock().unwrap().push(request.clone());
                        peak.fetch_max(active.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                        tokio::time::sleep(hold).await;
                        active.fetch_sub(1, Ordering::SeqCst);
                        let Answer::Reply(status, headers, body) = answer(&request) else {
                            return;
                        };
                        let mut body = body.into_bytes();
                        if headers.contains(&("content-encoding", "gzip".to_owned())) {
                            body = gzip(&body);
                        }
                        let mut reply = format!(
                            "HTTP/1.1 {status} Fixture\r\ncontent-type: application/json\r\ncontent-length: {}\r\n",
                            body.len()
                        );
                        for (name, value) in headers {
                            reply.push_str(&format!("{name}: {value}\r\n"));
                        }
                        reply.push_str("\r\n");
                        let mut reply = reply.into_bytes();
                        reply.extend(body);
                        if stream.write_all(&reply).await.is_err() {
                            return;
                        }
                    }
                });
            }
        }
    });
    Api {
        base: format!("http://{host}"),
        host,
        seen,
        peak,
    }
}

fn gzip(body: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(body).unwrap();
    encoder.finish().unwrap()
}

async fn read_request(stream: &mut TcpStream) -> Option<Seen> {
    let mut bytes = Vec::new();
    let head_end = loop {
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break end;
        }
        let mut chunk = [0u8; 8192];
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return None,
            Ok(read) => bytes.extend_from_slice(&chunk[..read]),
        }
    };
    let head = String::from_utf8_lossy(&bytes[..head_end]).into_owned();
    let mut lines = head.lines();
    let mut request_line = lines.next()?.split(' ');
    let (method, path) = (request_line.next()?, request_line.next()?);
    let header = |name: &str| {
        head.lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(key, _)| key.trim().eq_ignore_ascii_case(name))
            .map(|(_, value)| value.trim().to_owned())
            .unwrap_or_default()
    };
    let length: usize = header("content-length").parse().unwrap_or(0);
    let mut body = bytes[head_end + 4..].to_vec();
    while body.len() < length {
        let mut chunk = [0u8; 8192];
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return None,
            Ok(read) => body.extend_from_slice(&chunk[..read]),
        }
    }
    Some(Seen {
        method: method.to_owned(),
        path: path.to_owned(),
        authorization: header("authorization"),
        user_agent: header("user-agent"),
        accept_encoding: header("accept-encoding"),
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
    })
}

/// The batched query's answer when no asked branch has a pull request.
fn no_prs(request: &Seen) -> String {
    let aliases: serde_json::Map<String, Value> = request.body["variables"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(name, _)| name.strip_prefix('b')?.parse::<usize>().ok())
        .map(|index| (format!("wt_{index}"), json!({"nodes": []})))
        .collect();
    json!({"data": {"repository": aliases, "rateLimit": serde_json::from_str::<Value>(BUDGET).unwrap()}})
        .to_string()
}

fn asks_for(request: &Seen, branch: &str) -> bool {
    request.body["variables"]
        .as_object()
        .is_some_and(|variables| variables.values().any(|value| value == branch))
}

/// A gh that logs each call, prints what `token` prints for
/// `gh auth token`, and runs `otherwise` for anything else.
fn token_gh(dir: &Path, token: &str, otherwise: &str) -> PathBuf {
    fake_gh(
        dir,
        "gh",
        &format!(
            "cd '{}'\necho \"$*\" >> calls\ncase \"$1 $2\" in\n'auth token') {token};;\n*) {otherwise};;\nesac",
            dir.display()
        ),
    )
}

fn calls(dir: &Path) -> Vec<String> {
    fs::read_to_string(dir.join("calls"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn client(dir: &Path, gh: PathBuf, api: &Api) -> GithubClient {
    GithubClient::new(ProcessRunner::default(), dir.to_owned(), options())
        .with_gh_program(gh)
        .with_api_base(api.base.clone())
        .with_repository(RepoSlug::parse("acme/app").unwrap())
}

fn branches(count: usize) -> Vec<String> {
    (0..count).map(|n| format!("b{n}")).collect()
}

/// Every chunk of a read is in flight at once, each carries gh's token and
/// wt's name, and gh is asked for the token once for any number of reads.
#[tokio::test]
async fn reads_use_https_with_ghs_token_and_run_every_chunk_at_once() {
    let dir = tempdir().unwrap();
    // The API compresses its replies for a client that asks, as GitHub does.
    let api = serve(Duration::from_millis(150), |request| {
        if request.accept_encoding == "gzip" {
            Answer::Reply(
                200,
                vec![("content-encoding", "gzip".to_owned())],
                no_prs(request),
            )
        } else {
            ok(no_prs(request))
        }
    })
    .await;
    let gh = token_gh(dir.path(), &format!("echo {TOKEN}"), "exit 9");
    let client = client(dir.path(), gh, &api);
    let cancel = CancellationToken::new();
    let asked = [branches(18), vec!["false".to_owned(), "2026".to_owned()]].concat();
    let data = client.fetch_worktrees(&asked, &cancel).await.unwrap();
    assert_eq!(data.covered_branches.len(), 20);
    assert!(data.prs.is_empty());
    assert_eq!(data.rate_limit.unwrap().cost, 3);

    let seen = api.seen();
    assert_eq!(seen.len(), 3);
    assert_eq!(api.peak.load(Ordering::SeqCst), 3);
    for request in &seen {
        assert_eq!(
            (request.method.as_str(), request.path.as_str()),
            ("POST", "/graphql")
        );
        assert_eq!(request.authorization, format!("Bearer {TOKEN}"));
        assert_eq!(request.accept_encoding, "gzip");
        assert!(
            request.user_agent.starts_with("wt/"),
            "{}",
            request.user_agent
        );
    }
    // Variables stay strings, whatever they look like.
    assert!(seen.iter().any(|request| asks_for(request, "false")));
    assert!(
        seen.iter()
            .any(|request| request.body["variables"]["b3"] == json!("2026"))
    );
    // Only the first chunk asks for the merge queue.
    assert_eq!(
        seen.iter()
            .filter(|request| !request.body["variables"]["mergeQueueBranch"].is_null())
            .count(),
        1
    );

    client.fetch_worktrees(&asked, &cancel).await.unwrap();
    assert_eq!(api.seen().len(), 6);
    assert_eq!(
        calls(dir.path()),
        [format!("auth token --hostname {}", api.host)]
    );
}

/// A token the API refuses is read from gh again once. A newer one is sent;
/// the same one is not, and the refusal is not retried as a transient.
#[tokio::test]
async fn a_refused_token_is_read_again_once_and_never_resent() {
    let accept_newer = |request: &Seen| {
        if request.authorization == format!("Bearer {NEWER_TOKEN}") {
            ok(no_prs(request))
        } else {
            Answer::Reply(401, Vec::new(), r#"{"message":"Bad credentials"}"#.into())
        }
    };
    let cancel = CancellationToken::new();

    // gh hands out the old token first and the refreshed one afterwards.
    let dir = tempdir().unwrap();
    let api = serve(Duration::ZERO, accept_newer).await;
    let rotating =
        format!("if mkdir asked 2>/dev/null; then echo {TOKEN}; else echo {NEWER_TOKEN}; fi");
    let gh = token_gh(dir.path(), &rotating, "exit 9");
    let data = client(dir.path(), gh, &api)
        .fetch_worktrees(&branches(1), &cancel)
        .await
        .unwrap();
    assert_eq!(data.covered_branches.len(), 1);
    let sent: Vec<String> = api
        .seen()
        .into_iter()
        .map(|request| request.authorization)
        .collect();
    assert_eq!(
        sent,
        [format!("Bearer {TOKEN}"), format!("Bearer {NEWER_TOKEN}")]
    );
    assert_eq!(calls(dir.path()).len(), 2);

    // gh has nothing newer: one request, one definite failure.
    let dir = tempdir().unwrap();
    let api = serve(Duration::ZERO, accept_newer).await;
    let gh = token_gh(dir.path(), &format!("echo {TOKEN}"), "exit 9");
    let refused = client(dir.path(), gh, &api);
    let error = refused
        .fetch_worktrees(&branches(1), &cancel)
        .await
        .unwrap_err();
    assert!(
        matches!(&error, GithubError::Command(message) if message.contains("HTTP 401")),
        "{error:?}"
    );
    assert_eq!(api.seen().len(), 1);
    // The refusal is remembered: a read of several chunks is refused
    // chunk by chunk without gh being asked again for each.
    let error = refused
        .fetch_worktrees(&branches(20), &cancel)
        .await
        .unwrap_err();
    assert!(matches!(error, GithubError::Command(_)), "{error:?}");
    assert_eq!(calls(dir.path()).len(), 2);
}

/// A redirect is an answer, not a place to send the token: nothing follows it.
#[tokio::test]
async fn a_redirect_is_never_followed() {
    let dir = tempdir().unwrap();
    let cancel = CancellationToken::new();
    let elsewhere = serve(Duration::ZERO, |request| ok(no_prs(request))).await;
    let target = elsewhere.base.clone();
    let api = serve(Duration::ZERO, move |request| {
        let location = format!("{target}{}", request.path);
        Answer::Reply(307, vec![("location", location)], "{}".into())
    })
    .await;
    let gh = token_gh(dir.path(), &format!("echo {TOKEN}"), "exit 9");
    let client = client(dir.path(), gh, &api);
    let error = client
        .fetch_worktrees(&branches(1), &cancel)
        .await
        .unwrap_err();
    assert!(
        matches!(&error, GithubError::Command(message) if message.contains("HTTP 307")),
        "{error:?}"
    );
    assert!(client.viewer_login(&cancel).await.is_err());
    assert_eq!(api.seen().len(), 2);
    assert!(elsewhere.seen().is_empty());
}

/// A read sized for HTTPS whose API turns out unreachable runs gh for
/// every chunk, and still no more subprocesses at once than a gh read.
#[tokio::test]
async fn a_read_that_falls_back_to_gh_keeps_ghs_concurrency() {
    let dir = tempdir().unwrap();
    let closed = {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().to_string()
    };
    transport::forget(&closed);
    let aliases: serde_json::Map<String, Value> = (0..CHUNK_SIZE)
        .map(|index| (format!("wt_{index}"), json!({"nodes": []})))
        .collect();
    fs::write(
        dir.path().join("reply.json"),
        json!({"data": {"repository": aliases}}).to_string(),
    )
    .unwrap();
    let gh = token_gh(
        dir.path(),
        &format!("echo {TOKEN}"),
        "echo start >> running; sleep 0.3; echo end >> running; cat reply.json",
    );
    let client = GithubClient::new(ProcessRunner::default(), dir.path().to_owned(), options())
        .with_gh_program(gh)
        .with_api_base(format!("http://{closed}"))
        .with_repository(RepoSlug::parse("acme/app").unwrap());
    let data = client
        .fetch_worktrees(&branches(7 * CHUNK_SIZE), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(data.covered_branches.len(), 7 * CHUNK_SIZE);
    let (mut running, mut peak) = (0usize, 0usize);
    for line in fs::read_to_string(dir.path().join("running"))
        .unwrap()
        .lines()
    {
        if line == "start" {
            running += 1;
            peak = peak.max(running);
        } else {
            running -= 1;
        }
    }
    assert!((2..=MAX_CHUNK_CONCURRENCY).contains(&peak), "{peak}");
}

/// A rate-limited reply fails the read with the reset its headers name and
/// is never sent again, however GitHub words it.
#[tokio::test]
async fn rate_limited_replies_fail_the_read_once_with_their_reset() {
    let cancel = CancellationToken::new();
    let primary = || {
        vec![
            ("x-ratelimit-remaining", "0".to_owned()),
            ("x-ratelimit-reset", "1791118800".to_owned()),
        ]
    };
    let replies: [(u16, Headers, &str); 3] = [
        (
            403,
            primary(),
            r#"{"message":"API rate limit exceeded for user ID 1."}"#,
        ),
        (
            200,
            primary(),
            r#"{"data":null,"errors":[{"type":"RATE_LIMITED","message":"API rate limit exceeded for user ID 1."}]}"#,
        ),
        (
            429,
            vec![("retry-after", "60".to_owned())],
            r#"{"message":"Too many requests"}"#,
        ),
    ];
    for (status, headers, body) in replies {
        let dir = tempdir().unwrap();
        let reply_headers = headers.clone();
        let api = serve(Duration::ZERO, move |_| {
            Answer::Reply(status, reply_headers.clone(), body.to_owned())
        })
        .await;
        let gh = token_gh(dir.path(), &format!("echo {TOKEN}"), "exit 9");
        let before = epoch_ms();
        let error = client(dir.path(), gh, &api)
            .fetch_worktrees(&branches(1), &cancel)
            .await
            .unwrap_err();
        assert!(
            matches!(error, GithubError::RateLimit { .. }),
            "{status}: {error:?}"
        );
        let reset = error.rate_limit_reset_ms().unwrap();
        if status == 429 {
            assert!((before + 60_000..=epoch_ms() + 60_000).contains(&reset));
        } else {
            assert_eq!(reset, 1_791_118_800_000);
        }
        assert_eq!(api.seen().len(), 1, "{status}");
    }
}

/// A chunk whose request fails in transport is retried alone. When it keeps
/// failing the whole read fails: the other chunks' answers are not a read.
#[tokio::test]
async fn a_transport_failure_mid_chunk_is_retried_there_and_fails_the_whole_read_if_it_persists() {
    let cancel = CancellationToken::new();
    let gh_for = |dir: &Path| token_gh(dir, &format!("echo {TOKEN}"), "exit 9");
    let second_chunk = |request: &Seen| asks_for(request, "b9");

    // The connection drops under the second chunk's first request.
    let dir = tempdir().unwrap();
    let dropped = Arc::new(AtomicUsize::new(0));
    let api = serve(Duration::ZERO, {
        let dropped = dropped.clone();
        move |request| {
            if second_chunk(request) && dropped.fetch_add(1, Ordering::SeqCst) == 0 {
                Answer::HangUp
            } else {
                ok(no_prs(request))
            }
        }
    })
    .await;
    let data = client(dir.path(), gh_for(dir.path()), &api)
        .fetch_worktrees(&branches(20), &cancel)
        .await
        .unwrap();
    assert_eq!(data.covered_branches.len(), 20);
    let seen = api.seen();
    assert!(seen.iter().filter(|request| second_chunk(request)).count() >= 2);
    assert_eq!(
        seen.iter().filter(|request| !second_chunk(request)).count(),
        2
    );

    // The second chunk never gets an answer.
    let dir = tempdir().unwrap();
    let api = serve(Duration::ZERO, move |request| {
        if second_chunk(request) {
            Answer::Reply(502, Vec::new(), "<html>Bad Gateway</html>".into())
        } else {
            ok(no_prs(request))
        }
    })
    .await;
    let error = client(dir.path(), gh_for(dir.path()), &api)
        .fetch_worktrees(&branches(20), &cancel)
        .await
        .unwrap_err();
    assert!(
        matches!(&error, GithubError::Transient { message } if message.contains("HTTP 502")),
        "{error:?}"
    );
    let seen = api.seen();
    assert_eq!(
        seen.iter().filter(|request| second_chunk(request)).count(),
        MAX_ATTEMPTS
    );
    assert_eq!(
        seen.iter().filter(|request| !second_chunk(request)).count(),
        2
    );
    // Neither failure sent anything through gh.
    assert_eq!(calls(dir.path()).len(), 1);
}

/// With no token from gh, or an API that cannot be connected to before it
/// ever answered, the read runs gh as it always did.
#[tokio::test]
async fn without_a_token_or_a_reachable_api_reads_run_gh() {
    let cancel = CancellationToken::new();
    let through_gh = r#"echo '{"data":{"repository":{"wt_0":{"nodes":[]}}}}'"#;
    for token in [
        "exit 1",
        "echo '{\"message\":\"not a token\"}'",
        "echo short",
    ] {
        let dir = tempdir().unwrap();
        let api = serve(Duration::ZERO, |request| ok(no_prs(request))).await;
        let gh = token_gh(dir.path(), token, through_gh);
        let data = client(dir.path(), gh, &api)
            .fetch_worktrees(&branches(1), &cancel)
            .await
            .unwrap();
        assert_eq!(data.covered_branches.len(), 1);
        assert!(api.seen().is_empty(), "{token}");
        assert!(
            calls(dir.path())
                .iter()
                .any(|call| call.starts_with("api graphql"))
        );
    }

    // Nothing listens at the API.
    let dir = tempdir().unwrap();
    let closed = {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().to_string()
    };
    let gh = token_gh(dir.path(), &format!("echo {TOKEN}"), through_gh);
    let client = GithubClient::new(ProcessRunner::default(), dir.path().to_owned(), options())
        .with_gh_program(gh)
        .with_api_base(format!("http://{closed}"))
        .with_repository(RepoSlug::parse("acme/app").unwrap());
    let data = client.fetch_worktrees(&branches(1), &cancel).await.unwrap();
    assert_eq!(data.covered_branches.len(), 1);
    assert!(!transport::usable(&closed));
    // The next read does not try the API again.
    client.fetch_worktrees(&branches(1), &cancel).await.unwrap();
    assert_eq!(
        calls(dir.path())
            .iter()
            .filter(|call| call.starts_with("api graphql"))
            .count(),
        2
    );
}

/// The thread authors and the thread pages past the first are read at the
/// same time, and the picker and failed-run reads use the API too.
#[tokio::test]
async fn follow_up_reads_overlap_and_plain_reads_use_the_api() {
    let dir = tempdir().unwrap();
    let cancel = CancellationToken::new();
    let prs = paged_pr_reply(
        json!([
            {"id":"PRRT_a","isResolved":false},
            {"id":"PRRT_done","isResolved":true}
        ]),
        "Y3Vyc29yOjUw",
    );
    let api = serve(Duration::from_millis(150), move |request| {
        let query = request.query();
        match (request.method.as_str(), request.path.as_str()) {
            ("GET", "/user") => ok(r#"{"login":"reviewer","id":1}"#),
            ("GET", path) if path.starts_with("/repos/acme/app/contributors") => {
                ok(r#"[{"login":"author","contributions":7},{"login":"ci[bot]","contributions":9}]"#)
            }
            ("GET", path) if path.starts_with("/repos/acme/app/commits") => ok("[]"),
            ("GET", path) if path.starts_with("/repos/acme/app/actions/runs") => {
                ok(r#"{"total_count":1,"workflow_runs":[{"id":77}]}"#)
            }
            ("POST", _) if query.contains("PullRequestReviewThread") => {
                let authors: serde_json::Map<String, Value> = (0..query
                    .matches("PullRequestReviewThread")
                    .count())
                    .map(|index| {
                        (
                            format!("t{index}"),
                            json!({"comments":{"nodes":[{"author":{"login":"person","__typename":"User"}}]}}),
                        )
                    })
                    .collect();
                ok(json!({"data": authors}).to_string())
            }
            ("POST", _) if query.contains("after:") => ok(
                r#"{"data":{"p0":{"reviewThreads":{"pageInfo":{"hasNextPage":false,"endCursor":"Y3Vyc29yOjUx"},"nodes":[{"id":"PRRT_b","isResolved":false}]}}}}"#,
            ),
            ("POST", _) => ok(prs.clone()),
            _ => Answer::Reply(404, Vec::new(), r#"{"message":"Not Found"}"#.into()),
        }
    })
    .await;
    let gh = token_gh(
        dir.path(),
        &format!("echo {TOKEN}"),
        "case \"$1 $2\" in 'run view') printf 'build\\tstep\\tboom\\n';; *) exit 9;; esac",
    );
    let client = client(dir.path(), gh, &api);

    let data = client
        .fetch_worktrees(&["feature/a".into()], &cancel)
        .await
        .unwrap();
    let pr = &data.prs["feature/a"];
    assert_eq!(
        (
            pr.unresolved_threads_total,
            pr.unresolved_threads,
            pr.unresolved_threads_more
        ),
        (2, 2, false)
    );
    let seen = api.seen();
    // The batched query, then the first page's author and the next page
    // together, then the author of the thread that page brought.
    assert_eq!(seen.len(), 4);
    assert_eq!(api.peak.load(Ordering::SeqCst), 2);
    assert!(seen[3].query().contains("PRRT_b"));

    assert_eq!(client.viewer_login(&cancel).await.unwrap(), "reviewer");
    let contributors = client.fetch_repo_contributors(&cancel).await.unwrap();
    assert_eq!(
        contributors
            .iter()
            .map(|c| c.login.as_str())
            .collect::<Vec<_>>(),
        ["author"]
    );
    let (run, lines) = client
        .fetch_failed_run_log("team/fix#1", &cancel)
        .await
        .unwrap();
    assert_eq!((run, lines), (77, vec!["build\tstep\tboom".to_owned()]));
    assert!(api.seen().iter().any(|request| {
        request.path == "/repos/acme/app/actions/runs?branch=team/fix%231&status=failure&per_page=1"
    }));
    // gh gave the token and read the log archive, and nothing else.
    assert_eq!(
        calls(dir.path()),
        [
            format!("auth token --hostname {}", api.host),
            "run view 77 --log-failed".to_owned()
        ]
    );
}

#[test]
fn the_api_is_placed_as_gh_places_it_for_each_kind_of_host() {
    let at = |host: &str| Endpoint::for_host(host).map(|endpoint| format!("{endpoint:?}"));
    let github = at("github.com").unwrap();
    assert!(
        github.contains("https://api.github.com/graphql")
            && github.contains("\"https://api.github.com/\"")
    );
    let tenant = at("Acme.GHE.com").unwrap();
    assert!(
        tenant.contains("https://api.acme.ghe.com/graphql"),
        "{tenant}"
    );
    let server = at("git.corp.example").unwrap();
    assert!(
        server.contains("https://git.corp.example/api/graphql")
            && server.contains("https://git.corp.example/api/v3/"),
        "{server}"
    );
    // Not a host: nothing is sent anywhere.
    assert_eq!(at("evil.example/path?x="), None);
    assert_eq!(at(""), None);
    assert_eq!(
        transport::url_host("https://git.corp.example:8443/acme/app").as_deref(),
        Some("git.corp.example:8443")
    );
    assert_eq!(transport::url_host("acme/app"), None);
}
