use std::collections::BTreeMap;

use serde_json::Value;
use wt_platform::process::ProcessOutput;

use crate::types::PrChecks;

pub const CHUNK_SIZE: usize = 8;
const CHECK_FAIL_CONCLUSIONS: &[&str] = &[
    "FAILURE",
    "CANCELLED",
    "TIMED_OUT",
    "ACTION_REQUIRED",
    "STARTUP_FAILURE",
];
const TRANSIENT_PATTERNS: &[&str] = &[
    "couldn't respond to your request in time",
    "no server is currently available",
    "unexpected end of json input",
    "connection reset",
    "broken pipe",
    "i/o timeout",
    "unexpected eof",
    "internal server error",
    "something went wrong while executing your query",
];

pub fn chunk_branches(branches: &[String], size: usize) -> Vec<Vec<String>> {
    if size == 0 {
        return Vec::new();
    }
    branches.chunks(size).map(<[String]>::to_vec).collect()
}

/// The longest failure message reported: it reaches the log, the board and
/// the events state file.
const MAX_FAILURE_MESSAGE_CHARS: usize = 500;

/// The `errors` of a GitHub JSON reply as `TYPE: message` lines, or `None`
/// when `stdout` is not such a reply. A failed GraphQL read still carries its
/// partial `data`: PR titles, comments and review text, which is neither
/// evidence of the failure's class nor fit for an error message.
fn response_errors(stdout: &str) -> Option<String> {
    let value: Value = serde_json::from_str(stdout.trim()).ok()?;
    let errors = value.get("errors")?.as_array()?;
    // A REST reply names the failure beside its `errors`.
    let mut lines: Vec<String> = value
        .get("message")
        .and_then(Value::as_str)
        .map(|message| message.trim().to_owned())
        .filter(|message| !message.is_empty())
        .into_iter()
        .collect();
    for error in errors {
        let text = |key: &str| error.get(key).and_then(Value::as_str).map(str::trim);
        let line = match (
            text("type").or_else(|| text("code")),
            text("message").or_else(|| error.as_str().map(str::trim)),
        ) {
            (Some(kind), Some(message)) => format!("{kind}: {message}"),
            (Some(only), None) | (None, Some(only)) => only.to_owned(),
            (None, None) => continue,
        };
        // One alias per branch fails the same way; say it once.
        if !line.is_empty() && !lines.contains(&line) {
            lines.push(line);
        }
    }
    if lines.is_empty() {
        lines.push("GitHub returned errors without a message".into());
    }
    // The reported text is capped, and a rate limit is recognised from it.
    lines.sort_by_key(|line| !is_rate_limit(&line.to_lowercase()));
    Some(lines.join("; "))
}

fn capped(message: &str) -> String {
    match message.char_indices().nth(MAX_FAILURE_MESSAGE_CHARS) {
        Some((end, _)) => format!("{}...", &message[..end]),
        None => message.to_owned(),
    }
}

/// Failure classes known to clear without changing a user's settings.
pub fn is_transient_failure(stderr: &str, stdout: &str, timed_out: bool) -> bool {
    let errors = response_errors(stdout);
    let stdout = errors.as_deref().unwrap_or(stdout);
    let text = format!("{stderr}\n{stdout}").to_lowercase();
    if [
        "rate limit",
        "rate_limited",
        "secondary rate",
        "abuse detection",
        "bad credentials",
        "authentication",
        "could not resolve to a repository",
    ]
    .iter()
    .any(|s| text.contains(s))
    {
        return false;
    }
    timed_out
        || (500..600).any(|n| {
            text.contains(&format!("http {n}"))
                || text.contains(&format!("http/2 {n}"))
                || text.contains(&format!("http/1.1 {n}"))
        })
        || text.contains("stream error:") && text.contains("cancel")
        || ["service_unavailable", "timeout"]
            .iter()
            .any(|s| text.contains(s))
        || TRANSIENT_PATTERNS.iter().any(|s| text.contains(s))
}

pub(crate) fn is_rate_limit(text: &str) -> bool {
    let lower = text.to_lowercase();
    [
        "rate limit",
        "rate_limited",
        "secondary rate",
        "abuse detection",
    ]
    .iter()
    .any(|s| lower.contains(s))
}

fn check_name(check: &Value) -> Option<&str> {
    match check.get("__typename").and_then(Value::as_str) {
        Some("CheckRun") => check.get("name").and_then(Value::as_str),
        Some("StatusContext") => check.get("context").and_then(Value::as_str),
        _ => None,
    }
}

fn workflow_run(check: &Value) -> Option<(u64, u64)> {
    if check.get("__typename")?.as_str()? != "CheckRun" {
        return None;
    }
    let run = check
        .pointer("/checkSuite/workflowRun/databaseId")?
        .as_u64()?;
    let workflow = check
        .pointer("/checkSuite/workflowRun/workflow/databaseId")?
        .as_u64()?;
    Some((run, workflow))
}

fn started_at(check: &Value) -> Option<&str> {
    check
        .get("startedAt")
        .or_else(|| check.get("createdAt"))
        .and_then(Value::as_str)
}

/// Removes entries superseded by a later Actions run, then deduplicates only
/// contexts with comparable timestamps. Missing provenance/order fails closed.
fn latest_per_context(raw: &[Value]) -> Vec<&Value> {
    let mut newest_run = BTreeMap::<u64, u64>::new();
    for check in raw {
        if let Some((run, workflow)) = workflow_run(check) {
            newest_run
                .entry(workflow)
                .and_modify(|n| *n = (*n).max(run))
                .or_insert(run);
        }
    }
    let mut names = BTreeMap::<String, (&str, &Value)>::new();
    let mut undated = Vec::new();
    for check in raw {
        if let Some((run, workflow)) = workflow_run(check)
            && run < newest_run[&workflow]
        {
            continue;
        }
        let (Some(name), Some(at)) = (check_name(check), started_at(check)) else {
            undated.push(check);
            continue;
        };
        match names.get(name) {
            Some((previous, _)) if *previous > at => {}
            _ => {
                names.insert(name.to_owned(), (at, check));
            }
        }
    }
    undated
        .into_iter()
        .chain(names.into_values().map(|(_, check)| check))
        .collect()
}

fn ignored(name: Option<&str>, patterns: &[String], bot_contexts: &[String]) -> bool {
    let Some(name) = name else {
        return false;
    };
    patterns
        .iter()
        .chain(bot_contexts)
        .any(|p| wildcard_eq(p, name))
}

pub(crate) fn wildcard_eq(pattern: &str, value: &str) -> bool {
    // The config contract supports only `*`; matching is Unicode-safe and
    // ASCII case-insensitive, as JavaScript's `/i` was for these names.
    let p = pattern.to_lowercase();
    let v = value.to_lowercase();
    let parts: Vec<_> = p.split('*').collect();
    if parts.len() == 1 {
        return p == v;
    }
    let mut rest = v.as_str();
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        let Some(pos) = rest.find(part) else {
            return false;
        };
        if i == 0 && !p.starts_with('*') && pos != 0 {
            return false;
        }
        rest = &rest[pos + part.len()..];
    }
    p.ends_with('*') || rest.is_empty()
}

pub fn rollup_checks(
    raw: &[Value],
    ignored_patterns: &[String],
    bot_contexts: &[String],
) -> (PrChecks, Vec<String>) {
    if raw.is_empty() {
        return (PrChecks::None, Vec::new());
    }
    let mut pending = false;
    let mut failed = Vec::new();
    let mut counted = 0usize;
    for check in latest_per_context(raw) {
        let name = check_name(check);
        if ignored(name, ignored_patterns, bot_contexts) {
            continue;
        }
        counted += 1;
        let typename = check
            .get("__typename")
            .and_then(Value::as_str)
            .unwrap_or("");
        let is_pending = if typename == "CheckRun" {
            check
                .get("status")
                .and_then(Value::as_str)
                .is_some_and(|s| s != "COMPLETED")
        } else {
            matches!(
                check.get("state").and_then(Value::as_str),
                Some("PENDING" | "EXPECTED")
            )
        };
        let is_failed = if typename == "CheckRun" {
            check
                .get("conclusion")
                .and_then(Value::as_str)
                .is_some_and(|s| CHECK_FAIL_CONCLUSIONS.contains(&s))
        } else {
            matches!(
                check.get("state").and_then(Value::as_str),
                Some("FAILURE" | "ERROR")
            )
        };
        pending |= is_pending;
        if is_failed && let Some(name) = name {
            failed.push(name.to_owned());
        }
    }
    let state = if counted == 0 {
        PrChecks::None
    } else if !failed.is_empty() {
        PrChecks::Fail
    } else if pending {
        PrChecks::Pending
    } else {
        PrChecks::Pass
    };
    (state, failed)
}

pub fn checks_still_pending(error: &str) -> bool {
    let lower = error.to_lowercase();
    if lower.contains("required status check")
        && [
            " is expected",
            " is in progress",
            " is queued",
            " is pending",
            " is waiting",
        ]
        .iter()
        .any(|s| lower.contains(s))
    {
        return true;
    }
    let Some((_, breakdown)) = lower
        .split_once("required status checks have not succeeded:")
        .or_else(|| lower.split_once("required status check have not succeeded:"))
    else {
        return false;
    };
    let breakdown = breakdown.split('.').next().unwrap_or("");
    let states: Vec<_> = breakdown
        .split(",")
        .flat_map(|s| s.split(" and "))
        .map(|s| {
            s.trim()
                .trim_start_matches(|c: char| c.is_ascii_digit() || c.is_whitespace())
        })
        .filter(|s| !s.is_empty())
        .collect();
    !states.is_empty()
        && states.iter().all(|s| {
            matches!(
                *s,
                "expected" | "pending" | "queued" | "waiting" | "in progress" | "in_progress"
            )
        })
}

pub fn not_yet_enqueueable(error: &str) -> bool {
    let msg = error.to_lowercase();
    msg.contains("status checks have not succeeded")
        || msg.contains("required status check")
        || msg.contains("not in a mergeable state")
        || msg.contains("is not mergeable")
}

pub fn missing_workflow_scope(error: &str) -> bool {
    let lower = error.to_lowercase();
    lower.contains("refusing to allow") && lower.contains("to create or update workflow")
        || lower.contains("without `workflow` scope")
        || lower.contains("without workflow scope")
}

pub(crate) fn output_failure(output: &ProcessOutput) -> (String, bool) {
    text_failure(&output.stderr_text(), &output.stdout_text())
}

/// [`output_failure`] for a reply that did not come from a subprocess:
/// `stderr` is what the transport said beside the body.
pub(crate) fn text_failure(stderr: &str, stdout: &str) -> (String, bool) {
    let stderr = stderr.to_owned();
    let stdout = response_errors(stdout).unwrap_or_else(|| stdout.to_owned());
    let transient = is_transient_failure(&stderr, &stdout, false);
    let combined = format!("{stderr}\n{stdout}");
    let special = combined.lines().find(|line| {
        let l = line.to_lowercase();
        is_rate_limit(&l)
            || l.contains("refusing to allow")
            || (l.contains("without") && l.contains("workflow") && l.contains("scope"))
    });
    let body = special.map(str::to_owned).unwrap_or_else(|| {
        if stderr.trim().is_empty() {
            stdout
        } else {
            stderr
        }
    });
    (
        capped(
            body.lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or("gh exited nonzero")
                .trim(),
        ),
        transient,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn chunks_and_check_run_supersession_match_contract() {
        let branches: Vec<_> = (0..17).map(|n| format!("b{n}")).collect();
        assert_eq!(
            chunk_branches(&branches, CHUNK_SIZE)
                .iter()
                .map(Vec::len)
                .collect::<Vec<_>>(),
            [8, 8, 1]
        );
        let raw = vec![
            json!({"__typename":"CheckRun","name":"old-only","status":"COMPLETED","conclusion":"FAILURE","startedAt":"2026-01-01T00:00:00Z","checkSuite":{"workflowRun":{"databaseId":4,"workflow":{"databaseId":2}}}}),
            json!({"__typename":"CheckRun","name":"new-name","status":"COMPLETED","conclusion":"SUCCESS","startedAt":"2026-01-02T00:00:00Z","checkSuite":{"workflowRun":{"databaseId":5,"workflow":{"databaseId":2}}}}),
            json!({"__typename":"StatusContext","context":"deploy","state":"PENDING","createdAt":"2026-01-01T00:00:00Z"}),
        ];
        assert_eq!(
            rollup_checks(&raw, &[], &[]),
            (PrChecks::Pending, Vec::new())
        );
    }

    #[test]
    fn pending_aggregate_requires_all_reasons_known() {
        assert!(checks_still_pending(
            "4 of 4 required status checks have not succeeded: 2 expected and 2 in progress."
        ));
        assert!(!checks_still_pending(
            "4 required status checks have not succeeded: 3 expected and 1 failing."
        ));
        assert!(!checks_still_pending(
            "required status checks have not succeeded: ."
        ));
        assert!(!checks_still_pending("expected head oid does not match"));
    }

    #[test]
    fn rate_limits_and_workflow_scope_are_never_transient() {
        assert!(!is_transient_failure("secondary rate limit", "", false));
        assert!(is_transient_failure("HTTP 504", "", false));
        assert!(is_transient_failure("HTTP/2 502 Bad Gateway", "", false));
        assert!(missing_workflow_scope(
            "refusing to allow an OAuth App to create or update workflow without workflow scope"
        ));
        assert!(!missing_workflow_scope("rate limit exceeded"));
    }

    fn failed(stdout: &str, stderr: &str) -> ProcessOutput {
        use std::os::unix::process::ExitStatusExt;
        ProcessOutput {
            status: std::process::ExitStatus::from_raw(1 << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
            stdout_truncated: false,
            stderr_truncated: false,
            elapsed: std::time::Duration::ZERO,
        }
    }

    /// A GraphQL `INTERNAL` error arrives with the partial `data` of the
    /// aliases that did resolve. A review comment in it mentioning
    /// "authentication" must not make the server error permanent, and none
    /// of it belongs in the message.
    #[test]
    fn a_json_error_reply_is_judged_and_reported_by_its_errors_alone() {
        let internal = json!({
            "type": "INTERNAL",
            "path": ["repository", "b3"],
            "message": "Something went wrong while executing your query. Please include `ABCD:1234` when reporting this issue."
        });
        let body = json!({
            "data": {"repository": {"b0": {"nodes": [{
                "title": "Fix authentication for the rate limit banner",
                "comments": {"nodes": [{"body": "secret review text. ".repeat(4000)}]}
            }]}}},
            "errors": [internal.clone(), internal],
        })
        .to_string();
        assert!(body.len() > 77_000);
        assert!(is_transient_failure("", &body, false));
        let (message, transient) = output_failure(&failed(&body, ""));
        assert!(transient);
        assert_eq!(
            message,
            "INTERNAL: Something went wrong while executing your query. Please include `ABCD:1234` when reporting this issue."
        );
        assert!(!is_rate_limit(&message));

        // The errors themselves still decide: these never retry on their own.
        let rate = json!({
            "data": {"note": "internal server error"},
            "errors": [{"type": "RATE_LIMITED", "message": "API quota exhausted for installation."}],
        })
        .to_string();
        assert!(!is_transient_failure("", &rate, false));
        let (message, transient) = output_failure(&failed(&rate, ""));
        assert!(!transient);
        assert!(is_rate_limit(&message), "{message}");
        let auth = json!({"errors": [{"message": "Bad credentials: authentication required"}]})
            .to_string();
        assert!(!is_transient_failure("HTTP 502", &auth, false));

        // Text that is not an error reply is read as before.
        assert!(is_transient_failure("", "HTTP 502 Bad Gateway", false));
        assert!(!is_transient_failure("", "authentication failed", false));
    }

    #[test]
    fn a_reported_failure_is_capped_on_a_character_boundary() {
        let long = "é".repeat(2_000);
        let (message, _) = output_failure(&failed("", &long));
        assert_eq!(
            message,
            format!("{}...", "é".repeat(MAX_FAILURE_MESSAGE_CHARS))
        );
        let errors = json!({"errors": [{"message": long}]}).to_string();
        let (message, _) = output_failure(&failed(&errors, ""));
        assert_eq!(message.chars().count(), MAX_FAILURE_MESSAGE_CHARS + 3);
        assert_eq!(output_failure(&failed("", "gh: short")).0, "gh: short");
    }
}
