//! GitHub reads, derived pull-request state, and explicitly requested writes.
//!
//! The crate has no UI or application dependency. Every process call receives
//! its cwd, configuration, and cancellation token through [`GithubClient`].

mod checks;
mod client;
mod mutations;
mod types;

pub use checks::{
    CHUNK_SIZE, checks_still_pending, is_transient_failure, missing_workflow_scope,
    not_yet_enqueueable, rollup_checks,
};
pub use client::{GithubClient, GithubOptions};
pub use mutations::{merge_arm_kind, parse_async_merge_response};
pub use types::*;

/// Route an ordinary GitHub pull request URL to Linear Reviews when selected.
pub fn pull_request_open_url(url: &str, target: wt_config::PullRequestTarget) -> String {
    if target != wt_config::PullRequestTarget::Linear {
        return url.into();
    }
    let Some(rest) = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
    else {
        return url.into();
    };
    let path = rest.split(['?', '#']).next().unwrap_or("");
    let parts: Vec<_> = path.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() < 4 || parts[2] != "pull" || parts[0].contains('@') {
        return url.into();
    }
    format!(
        "linear://review/{}/{}/pull/{}",
        parts[0], parts[1], parts[3]
    )
}

pub fn review_repository_is_ignored(repository: Option<&str>, ignored: &[String]) -> bool {
    let Some(repository) = repository else {
        return false;
    };
    ignored
        .iter()
        .any(|name| name.trim().eq_ignore_ascii_case(repository.trim()))
}

/// Count unchecked Markdown list items outside fenced code blocks.
pub fn count_unticked_review_boxes(body: &str) -> u32 {
    let (mut count, mut fence) = (0, false);
    for line in body.lines() {
        let line = line.trim_start();
        if line.starts_with("```") || line.starts_with("~~~") {
            fence = !fence;
            continue;
        }
        if !fence && line.starts_with("- [ ]") {
            count += 1;
        }
    }
    count
}

/// Choose a branch's PR only if it belongs to the current worktree directory
/// era. If filesystem birth time is unavailable, preserve the PR fail-safe.
pub fn pick_pr_for_worktree<'a>(
    branch: Option<&str>,
    path: &std::path::Path,
    prs: &'a std::collections::BTreeMap<String, PullRequest>,
) -> Option<&'a PullRequest> {
    let pr = prs.get(branch?)?;
    if pr.state == "OPEN" {
        return Some(pr);
    }
    let Some(terminal) = pr.merged_at.as_deref().or(pr.closed_at.as_deref()) else {
        return Some(pr);
    };
    let Ok(terminal) =
        time::OffsetDateTime::parse(terminal, &time::format_description::well_known::Rfc3339)
    else {
        return Some(pr);
    };
    let created = std::fs::metadata(path).ok().and_then(|m| m.created().ok());
    let Some(created) = created else {
        return Some(pr);
    };
    let created = time::OffsetDateTime::from(created);
    (terminal.unix_timestamp_nanos() >= created.unix_timestamp_nanos()).then_some(pr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_config::PullRequestTarget;

    #[test]
    fn linear_review_link_rewrites_only_github_pull_requests() {
        assert_eq!(
            pull_request_open_url(
                "https://github.com/acme/app/pull/42",
                PullRequestTarget::Linear
            ),
            "linear://review/acme/app/pull/42"
        );
        assert_eq!(
            pull_request_open_url(
                "https://example.com/acme/app/pull/42",
                PullRequestTarget::Linear
            ),
            "https://example.com/acme/app/pull/42"
        );
        assert_eq!(
            pull_request_open_url(
                "https://github.com/acme/app/issues/42",
                PullRequestTarget::Linear
            ),
            "https://github.com/acme/app/issues/42"
        );
    }

    #[test]
    fn serialized_data_keeps_the_protocol_field_names_and_enum_values() {
        let value = serde_json::to_value(PrChecks::Pending).unwrap();
        assert_eq!(value, "pending");
        let action = serde_json::to_value(GhActionResult::error("failure")).unwrap();
        assert_eq!(action["ok"], false);
        assert_eq!(action["error"], "failure");
    }

    #[test]
    fn terminal_prs_from_an_older_worktree_era_are_hidden_but_missing_paths_keep_them() {
        let pr:PullRequest=serde_json::from_value(serde_json::json!({
            "number":1,"url":"https://github.com/o/r/pull/1","headRefName":"old","baseRefName":"main","title":"old","isDraft":false,"state":"MERGED","checks":"pass","failedChecks":[],"review":"none","reviewRequests":0,"requestedReviewers":[],"suggestedReviewers":[],"autoMerge":null,"comments":[],"unresolvedThreads":0,"unresolvedThreadsTotal":0,"mergedAt":"2000-01-01T00:00:00Z","closedAt":null
        })).unwrap();
        let prs = std::collections::BTreeMap::from([("old".into(), pr)]);
        let dir = tempfile::tempdir().unwrap();
        assert!(pick_pr_for_worktree(Some("old"), dir.path(), &prs).is_none());
        assert_eq!(
            pick_pr_for_worktree(Some("old"), &dir.path().join("gone"), &prs)
                .unwrap()
                .number,
            1
        );
    }
}
