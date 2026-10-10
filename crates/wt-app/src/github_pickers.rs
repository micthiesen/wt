//! GitHub reviewer picker preparation and submission.

use anyhow::Result;
use wt_github::{Contributor, GithubClient, GithubData, GithubOptions, PullRequest};
use wt_runtime::SourceHandle;
use wt_tui::{Board, ReviewerOption, UiModal, UiReply};

use crate::{context::AppContext, github_actions::GithubActions};

pub struct GithubPickers {
    client: GithubClient,
    cancellation: tokio_util::sync::CancellationToken,
    reviewers_enabled: bool,
}

pub struct ReviewerSubmission<'a> {
    pub key: &'a str,
    pub pr_number: u64,
    pub original: &'a [String],
    pub selected: &'a [String],
    pub board: &'a Board,
    pub github: &'a GithubData,
    pub actions: &'a GithubActions,
}

impl GithubPickers {
    pub fn start(context: &AppContext, _github: SourceHandle<GithubData>) -> Self {
        Self {
            client: GithubClient::new(
                context.processes.clone(),
                context.config.paths.main_clone.clone(),
                GithubOptions::from_config(&context.config, false),
            ),
            cancellation: context.cancellation.clone(),
            reviewers_enabled: context.config.github.reviewers,
        }
    }

    pub async fn invalidate_cache(&self) {
        self.client.invalidate_picker_cache().await;
    }

    pub async fn prepare_reviewers(
        &self,
        key: &str,
        board: &Board,
        github: &GithubData,
    ) -> Result<UiReply> {
        if !self.reviewers_enabled {
            return Ok(reply("reviewers are disabled for this repository"));
        }
        let Some(row) = resolve_row(board, key) else {
            return Ok(failed(format!(
                "worktree {key:?} is no longer on the board"
            )));
        };
        let branch = row.branch.clone();
        let Some(pr) = github.prs.get(&branch) else {
            return Ok(reply("no PR for this row"));
        };
        if pr.state != "OPEN" {
            return Ok(reply(format!("PR #{} is not open", pr.number)));
        }
        if pr.is_draft {
            return Ok(reply(format!(
                "PR #{} is a draft (mark it ready first)",
                pr.number
            )));
        }

        let (contributors, viewer) = tokio::join!(
            self.client.fetch_repo_contributors(&self.cancellation),
            self.client.viewer_login(&self.cancellation),
        );
        let contributors = match contributors {
            Ok(contributors) => contributors,
            Err(error) => return Ok(failed(format!("reviewers unavailable: {error}"))),
        };
        let viewer = match viewer {
            Ok(viewer) => viewer,
            Err(error) => return Ok(failed(format!("reviewer identity unavailable: {error}"))),
        };
        let candidates = reviewer_candidates(pr, &contributors, &viewer);
        if candidates.is_empty() {
            return Ok(reply("no reviewer candidates"));
        }
        Ok(UiReply {
            message: format!("edit reviewers for PR #{}", pr.number),
            modal: Some(UiModal::Reviewers {
                key: key.to_owned(),
                pr_number: pr.number,
                original: pr.requested_reviewers.clone(),
                candidates,
            }),
            ..UiReply::default()
        })
    }

    pub async fn submit_reviewers(&self, request: ReviewerSubmission<'_>) -> Result<UiReply> {
        let ReviewerSubmission {
            key,
            pr_number,
            original,
            selected,
            board,
            github,
            actions,
        } = request;
        if !self.reviewers_enabled {
            return Ok(failed("reviewers are disabled for this repository"));
        }
        let Some(row) = resolve_row(board, key) else {
            return Ok(failed(
                "worktree is no longer on the board; reviewer edit aborted",
            ));
        };
        let branch = row.branch.clone();
        let Some(pr) = github.prs.get(&branch) else {
            return Ok(failed(
                "PR data is no longer available; reviewer edit aborted",
            ));
        };
        if pr.number != pr_number || pr.state != "OPEN" || pr.is_draft {
            return Ok(failed(
                "PR changed since the reviewer picker opened; refresh and retry",
            ));
        }
        if !same_reviewers(original, &pr.requested_reviewers) {
            return Ok(failed(
                "the requested reviewer list changed; refresh and reopen the picker",
            ));
        }
        if original.len() > 100 || selected.len() > 100 {
            return Ok(failed("reviewer selection exceeds the 100-entry limit"));
        }
        if original
            .iter()
            .chain(selected)
            .any(|login| !valid_login(login))
        {
            return Ok(failed("reviewer selection contains an invalid login"));
        }
        if has_duplicates(selected) {
            return Ok(failed("reviewer selection contains duplicate logins"));
        }

        let original_set = original
            .iter()
            .map(String::as_str)
            .collect::<std::collections::HashSet<_>>();
        let selected_set = selected
            .iter()
            .map(String::as_str)
            .collect::<std::collections::HashSet<_>>();
        let add = selected
            .iter()
            .filter(|login| !original_set.contains(login.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        let remove = original
            .iter()
            .filter(|login| !selected_set.contains(login.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        if add.is_empty() && remove.is_empty() {
            return Ok(reply("no reviewer changes"));
        }

        let result = self
            .client
            .edit_reviewers(pr_number, &add, &remove, &self.cancellation)
            .await;
        if !result.ok {
            return Ok(failed(
                result
                    .error
                    .unwrap_or_else(|| "reviewer edit failed".into()),
            ));
        }
        let count = pr
            .review_requests
            .saturating_add(add.len() as u32)
            .saturating_sub(remove.len() as u32);
        actions.optimistic_reviewers(branch, selected.to_vec(), count);
        let mut changes = Vec::new();
        if !add.is_empty() {
            changes.push(format!("added {}", add.join(", ")));
        }
        if !remove.is_empty() {
            changes.push(format!("removed {}", remove.join(", ")));
        }
        Ok(reply(format!(
            "edited reviewers for PR #{pr_number}: {}",
            changes.join("; ")
        )))
    }
}

fn reviewer_candidates(
    pr: &PullRequest,
    contributors: &[Contributor],
    viewer: &str,
) -> Vec<ReviewerOption> {
    let requested = pr
        .requested_reviewers
        .iter()
        .cloned()
        .collect::<std::collections::HashSet<_>>();
    let mut candidates = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for suggestion in &pr.suggested_reviewers {
        if suggestion.login == viewer || !seen.insert(suggestion.login.clone()) {
            continue;
        }
        let mut tags = Vec::new();
        if requested.contains(&suggestion.login) {
            tags.push("requested");
        }
        tags.push("suggested");
        if suggestion.is_author {
            tags.push("author");
        }
        if suggestion.is_commenter {
            tags.push("commenter");
        }
        candidates.push(ReviewerOption {
            login: suggestion.login.clone(),
            label: format!("{} ({})", suggestion.login, tags.join(", ")),
            selected: requested.contains(&suggestion.login),
        });
    }
    for login in &pr.requested_reviewers {
        if login == viewer || !seen.insert(login.clone()) {
            continue;
        }
        candidates.push(ReviewerOption {
            login: login.clone(),
            label: format!("{login} (requested)"),
            selected: true,
        });
    }
    for contributor in contributors {
        if contributor.login == viewer || !seen.insert(contributor.login.clone()) {
            continue;
        }
        candidates.push(ReviewerOption {
            login: contributor.login.clone(),
            label: format!(
                "{} ({} commits)",
                contributor.login, contributor.contributions
            ),
            selected: requested.contains(&contributor.login),
        });
    }
    candidates
}

fn resolve_row<'a>(board: &'a Board, key: &str) -> Option<&'a wt_tui::BoardRow> {
    let mut matches = board
        .rows
        .iter()
        .filter(|row| row.key == key || row.slug == key);
    let first = matches.next()?;
    matches.next().is_none().then_some(first)
}

fn same_reviewers(left: &[String], right: &[String]) -> bool {
    left.len() == right.len()
        && left.iter().collect::<std::collections::HashSet<_>>()
            == right.iter().collect::<std::collections::HashSet<_>>()
}

fn has_duplicates(values: &[String]) -> bool {
    values
        .iter()
        .collect::<std::collections::HashSet<_>>()
        .len()
        != values.len()
}

fn valid_login(login: &str) -> bool {
    !login.is_empty()
        && login.len() <= 128
        && login.split('/').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
        })
        && login.matches('/').count() <= 1
}

fn reply(message: impl Into<String>) -> UiReply {
    UiReply {
        message: message.into(),
        ..UiReply::default()
    }
}

fn failed(message: impl Into<String>) -> UiReply {
    UiReply {
        message: message.into(),
        failed: true,
        ..UiReply::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_github::{PrChecks, PrReview, SuggestedReviewer};

    fn pr() -> PullRequest {
        PullRequest {
            id: Some("node".into()),
            number: 42,
            url: "https://github.com/a/b/pull/42".into(),
            head_ref_name: "feature/42".into(),
            head_ref_oid: Some("abc".into()),
            base_ref_name: "main".into(),
            merge_commit_oid: None,
            title: "x".into(),
            is_draft: false,
            state: "OPEN".into(),
            mergeable: None,
            merge_state_status: None,
            checks: PrChecks::None,
            failed_checks: vec![],
            review: PrReview::None,
            review_requests: 1,
            requested_reviewers: vec!["maintainer".into()],
            suggested_reviewers: vec![
                SuggestedReviewer {
                    login: "suggested".into(),
                    is_author: false,
                    is_commenter: true,
                },
                SuggestedReviewer {
                    login: "maintainer".into(),
                    is_author: true,
                    is_commenter: false,
                },
            ],
            review_bot: None,
            auto_merge: None,
            comments: vec![],
            unresolved_threads: 0,
            unresolved_threads_total: 0,
            merged_at: None,
            closed_at: None,
        }
    }

    #[test]
    fn candidates_follow_suggestion_requested_contributor_tiers_and_skip_self() {
        let candidates = reviewer_candidates(
            &pr(),
            &[
                Contributor {
                    login: "contributor".into(),
                    contributions: 12,
                },
                Contributor {
                    login: "suggested".into(),
                    contributions: 99,
                },
                Contributor {
                    login: "viewer".into(),
                    contributions: 2,
                },
            ],
            "viewer",
        );
        assert_eq!(
            candidates
                .iter()
                .map(|item| item.login.as_str())
                .collect::<Vec<_>>(),
            ["suggested", "maintainer", "contributor"]
        );
        assert!(candidates[0].label.contains("suggested, commenter"));
        assert!(candidates[1].selected);
        assert!(candidates.iter().all(|item| item.login != "viewer"));
    }

    #[test]
    fn reviewer_selection_validation_allows_team_slugs_but_rejects_shell_like_input() {
        assert!(valid_login("org/review-team"));
        assert!(!valid_login("reviewer --delete"));
        assert!(!valid_login("a/b/c"));
        assert!(same_reviewers(
            &["a".into(), "b".into()],
            &["b".into(), "a".into()]
        ));
    }
}
