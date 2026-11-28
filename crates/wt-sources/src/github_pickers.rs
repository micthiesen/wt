//! The reviewer picker's host side: the pool of people it offers, read from
//! GitHub about once a day and published for the board, and the commit that
//! edits a pull request's reviewers.

use std::time::Duration;

use anyhow::Result;
use wt_core::Domain;
use wt_github::{CoveredPr, GithubClient, GithubData, GithubError, GithubOptions};
use wt_runtime::{
    Confirmed, DiskCache, Fetched, LeafHandle, Persistence, RefreshPolicy, SourceHandle,
    SourceSpec, Stamp, TaskScope, start_skippable_source,
};
use wt_tui::{Board, ContributorView, ReviewerPool, UiReply};

use wt_app_core::context::AppContext;

/// How old the pool may grow before GitHub is read again. Who contributes to
/// a repository changes over weeks, and the read is several requests.
const POOL_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// How long after a read that did not succeed the next one is tried.
const POOL_RETRY: Duration = Duration::from_secs(10 * 60);
/// How long after start the saved pool's age is first looked at: the saved
/// copy is restored off this path and must land before it is judged missing.
const POOL_GRACE: Duration = Duration::from_secs(5);

pub struct GithubPickers {
    client: GithubClient,
    cancellation: tokio_util::sync::CancellationToken,
    reviewers_enabled: bool,
    pool: LeafHandle<ReviewerPool>,
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
    pub fn start(scope: &TaskScope, context: &AppContext) -> Self {
        let client = GithubClient::new(
            context.processes.clone(),
            context.config.paths.main_clone.clone(),
            GithubOptions::from_config(&context.config, false),
        );
        let reviewers_enabled = context.config.github.reviewers;
        let reads = reviewers_enabled && std::env::var("WT_GITHUB").as_deref() != Ok("off");
        let pool = start_skippable_source(
            scope,
            SourceSpec {
                name: "reviewer-pool",
                domain: Domain::Github,
                persistence: Persistence::Disk(DiskCache::new(&context.caches)),
                policy: RefreshPolicy {
                    debounce: Duration::from_millis(100),
                    minimum_interval: Duration::from_secs(10),
                },
            },
            {
                let client = client.clone();
                move |cancellation| {
                    let client = client.clone();
                    async move {
                        if !reads {
                            return Ok(None);
                        }
                        let started = Stamp::now();
                        read_pool(&client, &cancellation)
                            .await
                            .map(|pool| Some(Fetched::new(pool, started)))
                    }
                }
            },
        );
        if reads {
            keep_current(scope, pool.clone());
        }
        Self {
            client,
            cancellation: context.cancellation.clone(),
            reviewers_enabled,
            pool,
        }
    }

    /// The people the reviewer picker offers, for the board.
    pub fn reviewers(&self) -> SourceHandle<ReviewerPool> {
        self.pool.view()
    }

    /// An explicit refresh reads the pool again whatever its age.
    pub async fn invalidate_cache(&self) {
        self.client.invalidate_picker_cache().await;
        self.pool.refresh();
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
            // The PR and reviewer list the picker showed are its witness; a
            // mismatch is a stale refusal, so the board refreshes.
            CoveredPr::Absent => {
                return Err(wt_app_core::host_errors::stale(
                    "PR data is no longer available; reviewer edit aborted",
                ));
            }
            CoveredPr::Present(pr) => pr,
        };
        if pr.number != pr_number || pr.state != "OPEN" || pr.is_draft {
            return Err(wt_app_core::host_errors::stale(
                "PR changed since the reviewer picker opened; reopen the picker",
            ));
        }
        if !same_reviewers(original, &pr.requested_reviewers) {
            return Err(wt_app_core::host_errors::stale(
                "the requested reviewer list changed; reopen the picker",
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

/// Read the pool from GitHub. The client's own copy is dropped first: this
/// source decides when the pool is old, and a read it asks for must be one.
async fn read_pool(
    client: &GithubClient,
    cancellation: &tokio_util::sync::CancellationToken,
) -> Result<ReviewerPool, GithubError> {
    client.invalidate_picker_cache().await;
    let (contributors, viewer) = tokio::join!(
        client.fetch_repo_contributors(cancellation),
        client.viewer_login(cancellation),
    );
    Ok(ReviewerPool {
        viewer: wt_core::sanitize_terminal_text(&viewer?),
        contributors: contributors?
            .into_iter()
            .map(|contributor| ContributorView {
                login: wt_core::sanitize_terminal_text(&contributor.login),
                contributions: contributor.contributions,
            })
            .collect(),
        fetched_at_ms: now_ms(),
    })
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// How long until the pool read at `fetched_at_ms` is old; `None` when it is
/// already, or when there is none. A stamp from the future (a clock that
/// was set back) counts as just read.
fn pool_due_in(fetched_at_ms: Option<u64>, now_ms: u64) -> Option<Duration> {
    let age = Duration::from_millis(now_ms.saturating_sub(fetched_at_ms?));
    POOL_AGE.checked_sub(age).filter(|left| !left.is_zero())
}

/// Read the pool when it is missing or a day old, and at no other time: the
/// board's own refreshes never reach it. The saved pool is judged by when
/// GitHub was read, so a restart inside the day reads nothing.
fn keep_current(scope: &TaskScope, pool: LeafHandle<ReviewerPool>) {
    let cancel = scope.token();
    scope.spawn(async move {
        let mut wait = POOL_GRACE;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(wait) => {}
            }
            let fetched_at = pool.snapshot().data().map(|pool| pool.fetched_at_ms);
            wait = match pool_due_in(fetched_at, now_ms()) {
                // Timers stop while the machine sleeps, so look again
                // within the hour rather than trust one long sleep.
                Some(left) => left.min(Duration::from_secs(60 * 60)),
                None => {
                    pool.refresh();
                    POOL_RETRY
                }
            };
        }
    });
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
    use wt_github::{PrChecks, PrReview, PullRequest, SuggestedReviewer};
    use wt_runtime::TaskScope;
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
            unresolved_threads_more: false,
            merged_at: None,
            closed_at: None,
            queue_removal: None,
        }
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
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let scope = TaskScope::new();
        let pickers = GithubPickers {
            reviewers_enabled: true,
            ..GithubPickers::start(&scope, &fixture.ctx)
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
        let replaced =
            wt_app_core::host_errors::error_reply(&submit(Some(&confirmed), 41).await.unwrap_err());
        assert!(replaced.stale && replaced.message.contains("PR changed"));
        let reviewers_changed = pickers
            .submit_reviewers(ReviewerSubmission {
                key: "one",
                pr_number: 42,
                original: &[],
                selected: &original,
                board: &board,
                github: Some(&confirmed),
            })
            .await
            .unwrap_err();
        assert!(wt_app_core::host_errors::error_reply(&reviewers_changed).stale);
        let unchanged = submit(Some(&confirmed), 42).await.unwrap();
        assert!(!unchanged.failed);
        assert_eq!(unchanged.outcome, Some(ReplyOutcome::NoChange));
        scope
            .shutdown(std::time::Duration::from_secs(5))
            .await
            .unwrap();
        fixture.close().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reviewer_writes_require_branch_coverage_before_github_io() {
        use std::{fs, os::unix::fs::PermissionsExt};
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let fake = fixture._root.path().join("reject-gh");
        fs::write(&fake, "#!/bin/sh\nprintf called > \"$0.calls\"\nexit 99\n").unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
        let scope = TaskScope::new();
        let mut pickers = GithubPickers::start(&scope, &fixture.ctx);
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
            .unwrap_err();
        let submit = wt_app_core::host_errors::error_reply(&submit);
        assert!(submit.stale && submit.message.contains("no longer available"));
        assert!(
            !fake.with_extension("calls").exists(),
            "no GitHub reads or mutations may run"
        );
        scope
            .shutdown(std::time::Duration::from_secs(5))
            .await
            .unwrap();
        fixture.close().await.unwrap();
    }

    #[test]
    fn the_pool_is_read_when_missing_or_a_day_old_and_not_before() {
        let day = POOL_AGE.as_millis() as u64;
        let now = 10 * day;
        assert_eq!(pool_due_in(None, now), None, "no pool: read now");
        assert_eq!(pool_due_in(Some(now - day), now), None);
        assert_eq!(pool_due_in(Some(now - day - 1), now), None);
        assert_eq!(
            pool_due_in(Some(now - day + 1_000), now),
            Some(Duration::from_secs(1))
        );
        assert_eq!(pool_due_in(Some(now), now), Some(POOL_AGE));
        // A stamp ahead of the clock is not an excuse to read on every look.
        assert_eq!(pool_due_in(Some(now + 5_000), now), Some(POOL_AGE));
    }

    /// The pool a restart restores is the one the picker opens with, and it
    /// can never read as a live GitHub answer.
    #[tokio::test]
    async fn the_saved_pool_is_restored_as_display_data() {
        let sample = ReviewerPool {
            viewer: "me".into(),
            contributors: vec![ContributorView {
                login: "ana".into(),
                contributions: 12,
            }],
            fetched_at_ms: 1_700_000_000_000,
        };
        wt_runtime::conformance::persisted_source("reviewer-pool", Domain::Github, sample).await;
    }

    /// A restart inside the day reads nothing: the restored pool is judged
    /// by when GitHub was read. A missing or day-old pool is read once the
    /// restore has had time to land.
    #[tokio::test(start_paused = true)]
    async fn a_fresh_saved_pool_is_not_read_again_and_an_old_or_missing_one_is() {
        for (age, reads) in [
            (Some(Duration::from_secs(60)), false),
            (Some(POOL_AGE), true),
            (None, true),
        ] {
            let scope = TaskScope::new();
            let (pool, mut publisher) = wt_runtime::leaf_channel(
                &scope,
                SourceSpec {
                    name: "reviewer-pool-test",
                    domain: Domain::Github,
                    persistence: Persistence::Never("test fixture"),
                    policy: RefreshPolicy::default(),
                },
            );
            if let Some(age) = age {
                publisher.publish_live(
                    ReviewerPool {
                        fetched_at_ms: now_ms() - age.as_millis() as u64,
                        ..Default::default()
                    },
                    Stamp::now(),
                );
            }
            keep_current(&scope, pool);
            let asked = tokio::time::timeout(POOL_GRACE * 2, publisher.requested())
                .await
                .is_ok();
            assert_eq!(asked, reads, "a pool {age:?} old");
            scope.shutdown(Duration::from_secs(5)).await.unwrap();
        }
    }
}
