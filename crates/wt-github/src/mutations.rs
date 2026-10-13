use serde_json::Value;

/// Which merge feature is currently armed on the PR itself.
pub fn merge_arm_kind(node: &Value) -> &'static str {
    let queue = node
        .pointer("/mergeQueueEntry/id")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty());
    let classic = node
        .pointer("/autoMergeRequest/enabledAt")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty());
    match (queue, classic) {
        (true, true) => "both",
        (true, false) => "queue",
        (false, true) => "classic",
        _ => "none",
    }
}

#[derive(Debug)]
struct AsyncResponse {
    code: u16,
    status: String,
    details: Value,
}

fn response(text: &str) -> Result<AsyncResponse, String> {
    let re = text
        .find("\r\n\r\n")
        .or_else(|| text.find("\n\n"))
        .ok_or_else(|| "async merge response omitted HTTP body".to_owned())?;
    let headers = &text[..re];
    let status_line = headers
        .lines()
        .next()
        .ok_or_else(|| "async merge response omitted status".to_owned())?;
    let code = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| "async merge response has invalid HTTP status".to_owned())?;
    let body: Value =
        serde_json::from_str(&text[re + if text[re..].starts_with("\r\n") { 4 } else { 2 }..])
            .map_err(|_| "async merge response body is invalid JSON".to_owned())?;
    let status = body
        .get("status")
        .and_then(Value::as_str)
        .ok_or_else(|| "async merge response omitted status field".to_owned())?
        .to_owned();
    let details = body
        .get("details")
        .filter(|v| v.is_object())
        .cloned()
        .ok_or_else(|| "async merge response omitted details".to_owned())?;
    if details.get("message").and_then(Value::as_str).is_none() {
        return Err("async merge response omitted details.message".into());
    }
    Ok(AsyncResponse {
        code,
        status,
        details,
    })
}

/// The asynchronous operation has its own state, independent of HTTP success.
#[derive(Debug, PartialEq, Eq)]
pub enum AsyncMergeOutcome {
    Pending { uuid: String },
    Succeeded,
    Failed(String),
}

#[derive(Clone, Copy)]
pub enum AsyncMergeEndpoint {
    Submission,
    Poll,
}

/// Submission uses 202/409 for pending and 400 for failure. Polling returns
/// HTTP 200 for every operation state, including pending and failed. Invalid
/// responses are unknown outcomes, never proof that resending is safe.
pub fn parse_async_merge_response(
    text: &str,
    expected_head_sha: &str,
    endpoint: AsyncMergeEndpoint,
) -> Result<AsyncMergeOutcome, String> {
    let value = response(text)?;
    let valid_code = match (endpoint, value.status.as_str()) {
        (AsyncMergeEndpoint::Poll, _) => value.code == 200,
        (AsyncMergeEndpoint::Submission, "pending") => matches!(value.code, 202 | 409),
        (AsyncMergeEndpoint::Submission, "failed") => value.code == 400,
        (AsyncMergeEndpoint::Submission, _) => value.code == 200,
    };
    if !valid_code {
        return Err(format!(
            "GitHub returned incompatible async merge HTTP status {}",
            value.code
        ));
    }
    let message = value.details["message"].as_str().unwrap();
    match value.status.as_str() {
        "failed" => return Ok(AsyncMergeOutcome::Failed(message.into())),
        "enqueued" => return Ok(AsyncMergeOutcome::Succeeded),
        "merged"
            if value
                .details
                .get("sha")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty()) =>
        {
            return Ok(AsyncMergeOutcome::Succeeded);
        }
        "pending" => {}
        _ => {
            return Err(format!(
                "GitHub returned incompatible async merge response: {message}"
            ));
        }
    }
    let uuid = value
        .details
        .get("uuid")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !valid_uuid(uuid)
        || value
            .details
            .get("expected_head_sha")
            .and_then(Value::as_str)
            != Some(expected_head_sha)
        || value.details.get("merge_action").and_then(Value::as_str) != Some("merge_queue")
        || !matches!(
            value.details.get("merge_method").and_then(Value::as_str),
            Some("default" | "merge" | "squash" | "rebase")
        )
        || value
            .details
            .get("bypass_rules")
            .is_some_and(|v| v.as_bool() != Some(false))
    {
        return Err("GitHub returned malformed or incompatible async merge result".into());
    }
    Ok(AsyncMergeOutcome::Pending { uuid: uuid.into() })
}

fn valid_uuid(s: &str) -> bool {
    let chunks: Vec<_> = s.split('-').collect();
    chunks.len() == 5
        && [8, 4, 4, 4, 12]
            .iter()
            .zip(chunks)
            .all(|(n, c)| c.len() == *n && c.bytes().all(|b| b.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_kind_uses_the_pr_state() {
        assert_eq!(
            merge_arm_kind(
                &serde_json::json!({"mergeQueueEntry":{"id":"Q"},"autoMergeRequest":null})
            ),
            "queue"
        );
        assert_eq!(
            merge_arm_kind(
                &serde_json::json!({"mergeQueueEntry":null,"autoMergeRequest":{"enabledAt":"now"}})
            ),
            "classic"
        );
        assert_eq!(merge_arm_kind(&serde_json::json!({})), "none");
    }

    const UUID: &str = "01234567-89ab-cdef-0123-456789abcdef";

    fn pending(code: u16) -> String {
        format!(
            "HTTP/2 {code}\r\n\r\n{}",
            serde_json::json!({"status":"pending","details":{"message":"pending","uuid":UUID,"expected_head_sha":"deadbeef","merge_action":"merge_queue","merge_method":"default","bypass_rules":false}})
        )
    }

    #[test]
    fn async_merge_distinguishes_submission_from_poll_statuses() {
        use AsyncMergeEndpoint::*;
        let expected = || Ok(AsyncMergeOutcome::Pending { uuid: UUID.into() });
        for code in [202, 409] {
            assert_eq!(
                parse_async_merge_response(&pending(code), "deadbeef", Submission),
                expected()
            );
            assert!(parse_async_merge_response(&pending(code), "deadbeef", Poll).is_err());
        }
        assert_eq!(
            parse_async_merge_response(&pending(200), "deadbeef", Poll),
            expected()
        );
        assert!(parse_async_merge_response(&pending(200), "deadbeef", Submission).is_err());
        assert!(parse_async_merge_response(&pending(200), "other", Poll).is_err());
        for endpoint in [Submission, Poll] {
            let enqueued =
                "HTTP/1.1 200 OK\n\n{\"status\":\"enqueued\",\"details\":{\"message\":\"added\"}}";
            assert_eq!(
                parse_async_merge_response(enqueued, "deadbeef", endpoint),
                Ok(AsyncMergeOutcome::Succeeded)
            );
        }
    }

    #[test]
    fn terminal_failure_is_distinct_from_an_unreadable_outcome() {
        for (code, endpoint) in [
            (400, AsyncMergeEndpoint::Submission),
            (200, AsyncMergeEndpoint::Poll),
        ] {
            let failed = format!(
                "HTTP/2 {code}\n\n{{\"status\":\"failed\",\"details\":{{\"message\":\"head changed\"}}}}"
            );
            assert_eq!(
                parse_async_merge_response(&failed, "deadbeef", endpoint),
                Ok(AsyncMergeOutcome::Failed("head changed".into()))
            );
            assert!(parse_async_merge_response("broken", "deadbeef", endpoint).is_err());
        }
    }
}
