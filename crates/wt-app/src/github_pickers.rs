//! GitHub reviewer picker preparation and submission.

use anyhow::Result;
use wt_github::{Contributor, CoveredPr, GithubClient, GithubData, GithubOptions, PullRequest};
use wt_runtime::{Confirmed, Stamp};
use wt_tui::{Board, ReviewerOption, UiModal, UiReply};

use crate::context::AppContext;

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
    /// The submission is checked against confirmed data, never the display.
    pub github: Option<&'a Confirmed<GithubData>>,
}

impl GithubPickers {
    pub fn start(context: &AppContext) -> Self {
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
        github: Option<&Confirmed<GithubData>>,
    ) -> Result<UiReply> {
        if !self.reviewers_enabled {
            return Ok(UiReply::message(
                "reviewers are disabled for this repository",
            ));
        }
        let Some(github) = github else {
            return Ok(UiReply::failure("GitHub data is still loading"));
        };
        let Some(row) = resolve_row(board, key) else {
            return Ok(UiReply::failure(format!(
                "worktree {key:?} is no longer on the board"
            )));
        };
        let branch = row.branch.clone();
        let pr = match github.covered_pr(&branch) {
            CoveredPr::Uncovered => {
                return Ok(UiReply::failure(
                    "GitHub data is still loading for this branch",
                ));
            }
            CoveredPr::Absent => {
                return Ok(UiReply {
                    notice: true,
                    ..UiReply::message("no PR for this row")
                });
            }
            CoveredPr::Present(pr) => pr,
        };
        if pr.state != "OPEN" {
            return Ok(UiReply::message(format!("PR #{} is not open", pr.number)));
        }
        if pr.is_draft {
            return Ok(UiReply::message(format!(
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
            Err(error) => return Ok(UiReply::failure(format!("reviewers unavailable: {error}"))),
        };
        let viewer = match viewer {
            Ok(viewer) => viewer,
            Err(error) => {
                return Ok(UiReply::failure(format!(
                    "reviewer identity unavailable: {error}"
                )));
            }
        };
        let candidates = reviewer_candidates(pr, &contributors, &viewer);
        if candidates.is_empty() {
            return Ok(UiReply::message("no reviewer candidates"));
        }
        Ok(UiReply {
            modal: Some(UiModal::Reviewers {
                key: key.to_owned(),
                pr_number: pr.number,
                original: pr.requested_reviewers.clone(),
                candidates,
            }),
            ..UiReply::message(format!("edit reviewers for PR #{}", pr.number))
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
        } = request;
        if !self.reviewers_enabled {
            return Ok(UiReply::failure(
                "reviewers are disabled for this repository",
            ));
        }
        let Some(github) = github else {
            return Ok(UiReply::failure(
                "GitHub data is still loading; reviewer edit aborted",
            ));
        };
        let Some(row) = resolve_row(board, key) else {
            return Ok(UiReply::failure(
                "worktree is no longer on the board; reviewer edit aborted",
            ));
        };
        let branch = row.branch.clone();
        let pr = match github.covered_pr(&branch) {
            CoveredPr::Uncovered => {
                return Ok(UiReply::failure(
                    "GitHub data is still loading for this branch; reviewer edit aborted",
                ));
            }
            CoveredPr::Absent => {
                return Ok(UiReply::failure(
                    "PR data is no longer available; reviewer edit aborted",
                ));
            }
            CoveredPr::Present(pr) => pr,
        };
        if pr.number != pr_number || pr.state != "OPEN" || pr.is_draft {
            return Ok(UiReply::failure(
                "PR changed since the reviewer picker opened; refresh and retry",
            ));
        }
        if !same_reviewers(original, &pr.requested_reviewers) {
            return Ok(UiReply::failure(
                "the requested reviewer list changed; refresh and reopen the picker",
            ));
        }
        if original.len() > 100 || selected.len() > 100 {
            return Ok(UiReply::failure(
                "reviewer selection exceeds the 100-entry limit",
            ));
        }
        if original
            .iter()
            .chain(selected)
            .any(|login| !valid_login(login))
        {
            return Ok(UiReply::failure(
                "reviewer selection contains an invalid login",
            ));
        }
        if has_duplicates(selected) {
            return Ok(UiReply::failure(
                "reviewer selection contains duplicate logins",
            ));
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
            return Ok(UiReply::no_change("no reviewer changes"));
        }

        let result = self
            .client
            .edit_reviewers(pr_number, &add, &remove, &self.cancellation)
            .await;
        if !result.ok {
            return Ok(UiReply::failure(
                result
                    .error
                    .unwrap_or_else(|| "reviewer edit failed".into()),
            ));
        }
        let at = Stamp::now();
        let mut changes = Vec::new();
        if !add.is_empty() {
            changes.push(format!("added {}", add.join(", ")));
        }
        if !remove.is_empty() {
            changes.push(format!("removed {}", remove.join(", ")));
        }
        // The host requests a direct GitHub read for accepted GitHub writes.
        Ok(UiReply::accepted(
            format!(
                "edited reviewers for PR #{pr_number}: {}",
                changes.join("; ")
            ),
            at,
        ))
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

#[cfg(test)]
mod tests {
    use super::*;
    use wt_github::{PrChecks, PrReview, SuggestedReviewer};
    use wt_tui::ReplyOutcome;

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

    #[tokio::test]
    async fn submissions_are_checked_against_confirmed_data_and_report_their_outcome() {
        let fixture = crate::commands::test_support::CommandFixture::new()
            .await
            .unwrap();
        let pickers = GithubPickers {
            reviewers_enabled: true,
            ..GithubPickers::start(&fixture.ctx)
        };
        let board = Board {
            rows: vec![wt_tui::BoardRow {
                key: "one".into(),
                slug: "one".into(),
                branch: "feature/42".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut data = GithubData::default();
        data.covered_branches.insert("feature/42".into());
        data.prs.insert("feature/42".into(), pr());
        let confirmed = Confirmed::for_test(data);
        let original = vec!["maintainer".to_owned()];
        let submit = |github, pr_number| {
            pickers.submit_reviewers(ReviewerSubmission {
                key: "one",
                pr_number,
                original: &original,
                selected: &original,
                board: &board,
                github,
            })
        };
        let loading = submit(None, 42).await.unwrap();
        assert!(loading.failed && loading.message.contains("still loading"));
        let replaced = submit(Some(&confirmed), 41).await.unwrap();
        assert!(replaced.failed && replaced.message.contains("PR changed"));
        let unchanged = submit(Some(&confirmed), 42).await.unwrap();
        assert!(!unchanged.failed);
        assert_eq!(unchanged.outcome, Some(ReplyOutcome::NoChange));
        fixture.close().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reviewer_reads_and_writes_require_branch_coverage_before_github_io() {
        use std::{fs, os::unix::fs::PermissionsExt};
        let fixture = crate::commands::test_support::CommandFixture::new()
            .await
            .unwrap();
        let fake = fixture._root.path().join("reject-gh");
        fs::write(&fake, "#!/bin/sh\nprintf called > \"$0.calls\"\nexit 99\n").unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
        let mut pickers = GithubPickers::start(&fixture.ctx);
        pickers.reviewers_enabled = true;
        pickers.client = pickers.client.with_gh_program(fake.clone());
        let board = Board {
            rows: vec![wt_tui::BoardRow {
                key: "one".into(),
                slug: "one".into(),
                branch: "feature/42".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let original = ["maintainer".to_owned()];
        let selected = ["new-reviewer".to_owned()];
        for retain_pr in [false, true] {
            let mut data = GithubData::default();
            data.covered_branches.insert("feature/old".into());
            if retain_pr {
                data.prs.insert("feature/42".into(), pr());
            }
            let confirmed = Confirmed::for_test(data);
            let prepare = pickers
                .prepare_reviewers("one", &board, Some(&confirmed))
                .await
                .unwrap();
            assert!(prepare.failed && prepare.message.contains("still loading"));
            let submit = pickers
                .submit_reviewers(ReviewerSubmission {
                    key: "one",
                    pr_number: 42,
                    original: &original,
                    selected: &selected,
                    board: &board,
                    github: Some(&confirmed),
                })
                .await
                .unwrap();
            assert!(submit.failed && submit.message.contains("still loading"));
        }
        let absent = Confirmed::for_test(GithubData {
            covered_branches: ["feature/42".into()].into(),
            ..Default::default()
        });
        let prepare = pickers
            .prepare_reviewers("one", &board, Some(&absent))
            .await
            .unwrap();
        assert!(!prepare.failed && prepare.message == "no PR for this row");
        let submit = pickers
            .submit_reviewers(ReviewerSubmission {
                key: "one",
                pr_number: 42,
                original: &original,
                selected: &selected,
                board: &board,
                github: Some(&absent),
            })
            .await
            .unwrap();
        assert!(submit.failed && submit.message.contains("no longer available"));
        assert!(
            !fake.with_extension("calls").exists(),
            "no GitHub reads or mutations may run"
        );
        fixture.close().await.unwrap();
    }
}
