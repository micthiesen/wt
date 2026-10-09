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

/// Validate a queue request/poll response. `Ok(true)` is terminal success,
/// `Ok(false)` is a valid pending result, and `Err` is definitive or malformed.
pub fn parse_async_merge_response(text: &str, expected_head_sha: &str) -> Result<bool, String> {
    let value = response(text)?;
    let message = value
        .details
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("async merge failed");
    if value.status == "failed" {
        return if value.code == 400 {
            Err(message.into())
        } else {
            Err(format!(
                "GitHub returned incompatible failed async merge HTTP status {}",
                value.code
            ))
        };
    }
    let terminal_success = value.status == "enqueued"
        || (value.status == "merged"
            && value
                .details
                .get("sha")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty()));
    if terminal_success {
        return if value.code == 200 {
            Ok(true)
        } else {
            Err(format!(
                "GitHub returned incompatible async merge HTTP status {}",
                value.code
            ))
        };
    }
    if value.status != "pending" {
        return Err(format!(
            "GitHub returned incompatible async merge response: {message}"
        ));
    }
    if !valid_uuid(
        value
            .details
            .get("uuid")
            .and_then(Value::as_str)
            .unwrap_or(""),
    ) || value
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
    if value.code != 202 && value.code != 409 {
        return Err(format!(
            "GitHub returned incompatible async merge HTTP status {}",
            value.code
        ));
    }
    Ok(false)
}

pub(crate) fn async_uuid(text: &str) -> Option<String> {
    let value = response(text).ok()?;
    let uuid = value.details.get("uuid")?.as_str()?;
    valid_uuid(uuid).then(|| uuid.into())
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

    #[test]
    fn async_merge_requires_matching_fixed_sha_and_terminal_state() {
        let pending = "HTTP/2 202 Accepted\r\ncontent-type: application/json\r\n\r\n{\"status\":\"pending\",\"details\":{\"message\":\"pending\",\"uuid\":\"01234567-89ab-cdef-0123-456789abcdef\",\"expected_head_sha\":\"deadbeef\",\"merge_action\":\"merge_queue\",\"merge_method\":\"default\",\"bypass_rules\":false}}";
        assert_eq!(parse_async_merge_response(pending, "deadbeef"), Ok(false));
        assert!(parse_async_merge_response(pending, "other").is_err());
        let enqueued =
            "HTTP/1.1 200 OK\n\n{\"status\":\"enqueued\",\"details\":{\"message\":\"added\"}}";
        assert_eq!(parse_async_merge_response(enqueued, "deadbeef"), Ok(true));
        let pending_on_200 = pending.replace("202 Accepted", "200 OK");
        assert!(parse_async_merge_response(&pending_on_200, "deadbeef").is_err());
        let enqueued_on_202 = enqueued.replace("200 OK", "202 Accepted");
        assert!(parse_async_merge_response(&enqueued_on_202, "deadbeef").is_err());
    }
}
