//! The configured tracker's batch reader (`[issue_tracker] status_command`).
//! It prints `{"issues":[{"id","status","title"?}]}` for exactly the ids it
//! was given; `title` is optional and seeds a new worktree's title.

use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;
use wt_platform::process::CommandSpec;

use crate::context::AppContext;

/// What the tracker reports for one issue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackedIssue {
    pub status: String,
    pub title: Option<String>,
}

/// Read `ids` with `command`, failing as a whole on any malformed answer.
pub async fn read(
    context: &AppContext,
    command: &[String],
    ids: &[String],
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<BTreeMap<String, TrackedIssue>> {
    let argv = arguments(command, ids);
    let (program, args) = argv
        .split_first()
        .context("issue status reader has no executable")?;
    let mut spec = CommandSpec::new(program)
        .args(args)
        .cwd(&context.config.paths.main_clone);
    spec.timeout = timeout;
    let output = context
        .processes
        .run(spec, cancel)
        .await
        .context("issue status reader")?;
    if !output.status.success() {
        bail!(
            "issue status reader exited {}: {}",
            output.status,
            output.stderr_text().trim()
        );
    }
    parse(&output.stdout, ids)
}

/// The tracker's title for one issue, for seeding a new worktree's title.
/// `None` when no reader is configured, it fails, or it reports no title.
pub async fn title(context: &AppContext, id: &str, cancel: &CancellationToken) -> Option<String> {
    let command = context
        .config
        .issue_tracker
        .as_ref()?
        .status_command
        .as_ref()?;
    let ids = [id.to_owned()];
    match read(context, command, &ids, Duration::from_secs(15), cancel).await {
        Ok(mut issues) => issues.remove(id)?.title,
        Err(error) => {
            tracing::warn!(issue = %id, error = %format!("{error:#}"), "issue title unavailable");
            None
        }
    }
}

pub fn arguments(command: &[String], ids: &[String]) -> Vec<String> {
    command
        .iter()
        .flat_map(|arg| {
            if arg == "{ids}" {
                ids.to_vec()
            } else {
                vec![arg.clone()]
            }
        })
        .collect()
}

pub fn parse(bytes: &[u8], ids: &[String]) -> Result<BTreeMap<String, TrackedIssue>> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Response {
        issues: Vec<Issue>,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Issue {
        id: String,
        status: String,
        #[serde(default)]
        title: Option<serde_json::Value>,
    }
    let response: Response =
        serde_json::from_slice(bytes).context("issue status reader: invalid JSON response")?;
    let expected = ids.iter().collect::<BTreeSet<_>>();
    let mut issues = BTreeMap::new();
    for Issue { id, status, title } in response.issues {
        if !expected.contains(&id) {
            bail!("issue status reader: unexpected issue {id:?}");
        }
        if issues.contains_key(&id) {
            bail!("issue status reader: duplicate issue {id}");
        }
        if status.trim().is_empty() || status.chars().any(char::is_control) {
            bail!(
                "issue status reader: invalid status for {id}: expected a nonempty single-line string"
            );
        }
        // A title is a convenience: a blank, multi-line or non-string one is
        // dropped rather than failing every status in the batch.
        let title = title
            .as_ref()
            .and_then(serde_json::Value::as_str)
            .map(|title| title.trim().to_owned())
            .filter(|title| !title.is_empty() && !title.chars().any(char::is_control));
        issues.insert(id, TrackedIssue { status, title });
    }
    let missing = ids
        .iter()
        .filter(|id| !issues.contains_key(*id))
        .cloned()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        bail!(
            "issue status reader: missing issues: {}",
            missing.join(", ")
        );
    }
    Ok(issues)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_partial_duplicate_extra_and_control_statuses() {
        let ids = vec!["ENG-1".into()];
        for invalid in [
            r#"{"issues":[]}"#,
            r#"{"issues":[{"id":"ENG-1","status":"Open"},{"id":"ENG-1","status":"Open"}]}"#,
            r#"{"issues":[{"id":"ENG-2","status":"Open"}]}"#,
            r#"{"issues":[{"id":"ENG-1","status":"\u001b[31m"}]}"#,
            r#"{"issues":[{"id":"ENG-1","status":"Open","extra":true}]}"#,
        ] {
            assert!(parse(invalid.as_bytes(), &ids).is_err(), "{invalid}");
        }
        assert_eq!(
            parse(br#"{"issues":[{"id":"ENG-1","status":"In Review"}]}"#, &ids).unwrap()["ENG-1"],
            TrackedIssue {
                status: "In Review".into(),
                title: None
            }
        );
        assert_eq!(
            arguments(
                &["reader".into(), "{ids}".into(), "literal{id}".into()],
                &ids
            ),
            vec!["reader", "ENG-1", "literal{id}"]
        );
    }

    #[test]
    fn a_title_is_optional_and_a_bad_one_is_dropped_not_fatal() {
        let ids = vec!["ENG-1".into()];
        let title = |body: &str| parse(body.as_bytes(), &ids).unwrap()["ENG-1"].title.clone();
        assert_eq!(
            title(r#"{"issues":[{"id":"ENG-1","status":"Open","title":" Fix calendar "}]}"#),
            Some("Fix calendar".into())
        );
        assert_eq!(
            title(r#"{"issues":[{"id":"ENG-1","status":"Open","title":"  "}]}"#),
            None
        );
        assert_eq!(
            title(r#"{"issues":[{"id":"ENG-1","status":"Open","title":"two\nlines"}]}"#),
            None
        );
        assert_eq!(
            title(r#"{"issues":[{"id":"ENG-1","status":"Open","title":42}]}"#),
            None
        );
    }
}
