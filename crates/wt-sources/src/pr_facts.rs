//! What the board says about a pull request that the pull request alone
//! does not: who can review it, and why GitHub took it out of the merge
//! queue. The first needs the repository's contributors, the second the
//! arming wt stored.

use std::sync::Arc;

use wt_app_core::feeds::FailureClock;
use wt_core::Domain;
use wt_github::GithubData;
use wt_runtime::{SourceHandle, SourceSnapshot, SourceState, TaskScope, derived_channel};
use wt_tui::{Board, ReviewerPool};

use crate::local_source::Metadata;

/// `github` and `reviewers` are display data (callers pass views). The
/// reviewer pool is read about once a day by its own source, so a refresh
/// of this stage is passed on to it only while there is no pool at all:
/// that is when the picker cannot open and asks for one.
pub fn overlay(
    scope: &TaskScope,
    board: SourceHandle<Board>,
    github: SourceHandle<GithubData>,
    metadata: SourceHandle<Metadata>,
    reviewers: SourceHandle<ReviewerPool>,
) -> SourceHandle<Board> {
    let (source, mut publisher) = derived_channel(scope, "pr-facts");
    let cancellation = scope.token();
    scope.spawn(async move {
        let mut failures = FailureClock::default();
        let mut board_updates = board.subscribe();
        let mut github_updates = github.subscribe();
        let mut metadata_updates = metadata.subscribe();
        let mut reviewer_updates = reviewers.subscribe();
        board_updates.mark_changed();
        loop {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => break,
                request = publisher.requested() => {
                    if request.is_none() { break; }
                    board.refresh();
                    if reviewer_updates.borrow().data().is_none() {
                        reviewers.refresh();
                    }
                    continue;
                },
                changed = board_updates.changed() => if changed.is_err() { break; },
                changed = github_updates.changed() => if changed.is_err() { break; },
                changed = metadata_updates.changed() => if changed.is_err() { break; },
                changed = reviewer_updates.changed() => if changed.is_err() { break; },
            }
            let prepared = compose(
                &mut failures,
                board_updates.borrow_and_update().clone(),
                &github_updates.borrow_and_update(),
                &metadata_updates.borrow_and_update(),
                &reviewer_updates.borrow_and_update(),
            );
            publisher.publish_if_changed(prepared);
        }
    });
    source
}

fn compose(
    failures: &mut FailureClock,
    source: SourceSnapshot<Board>,
    github: &SourceSnapshot<GithubData>,
    metadata: &SourceSnapshot<Metadata>,
    reviewers: &SourceSnapshot<ReviewerPool>,
) -> SourceSnapshot<Board> {
    let Some(prepared) = source.data() else {
        // This compose reports nothing, so every tracked failure has cleared.
        failures.sweep();
        return source;
    };
    let mut board = prepared.as_ref().clone();
    // The saved pool is as good a list of names as a fresh one; what a
    // commit does with a name is checked by its host.
    board.reviewers = reviewers.data().map(|pool| pool.as_ref().clone());
    // With no pool the picker cannot open, and only this says why. A failed
    // daily read of a pool that exists costs nothing and is tried again.
    if let (None, SourceState::Failed(error)) = (&board.reviewers, reviewers.state()) {
        let text = format!("Reviewer list: {}", wt_core::sanitize_terminal_text(error));
        failures.attention(&mut board, "Reviewer list", &text);
    }
    failures.sweep();
    // "Not queued now" is only true of a read made by this process: a saved
    // one may predate a later arming.
    let read_live = github
        .domains()
        .get(&Domain::Github)
        .is_some_and(|meta| meta.is_live());
    if let (true, Some(data), Some(stored)) = (read_live, github.data(), metadata.data()) {
        for row in &mut board.rows {
            let removal = crate::work_presentation::pick_pr(row, data).and_then(|pr| {
                let arm = serde_json::from_value::<wt_core::MergeArm>(
                    stored.0["slugs"][&row.key]["mergeArm"].clone(),
                )
                .ok();
                let queued = data
                    .merge_queue
                    .get(&row.branch)
                    .map(|_| true)
                    .or(data.merge_queue_complete.then_some(false));
                // The one rule `wt wait` and `wt fleet` say it by.
                let why = crate::wait_facts::queue_removal(pr, arm.as_ref(), queued)?;
                Some((pr.number, why))
            });
            if let (Some((number, why)), Some(shown)) = (removal, row.pr.as_mut())
                && shown.number == Some(number)
            {
                shown.queue_removal = Some(why);
            }
        }
    }
    // The pool carries no provenance the board uses, and a saved one must
    // not make the board's GitHub data read as restored.
    source
        .with_data(Some(Arc::new(board)))
        .with_input(github)
        .with_input(metadata)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_tui::{BoardRow, PrPresentation};

    fn pr(removed_at_ms: Option<u64>, auto_merge: bool) -> wt_github::PullRequest {
        let mut value = serde_json::json!({
            "number": 7, "url": "https://github.com/o/r/pull/7", "headRefName": "feature/one",
            "baseRefName": "main", "title": "A PR", "isDraft": false, "state": "OPEN",
            "checks": "pass", "failedChecks": [], "review": "none", "reviewRequests": 0,
            "requestedReviewers": [], "suggestedReviewers": [], "comments": [],
            "unresolvedThreads": 0, "unresolvedThreadsTotal": 0
        });
        if let Some(at_ms) = removed_at_ms {
            value["queueRemoval"] = serde_json::json!({"reason": "merge_conflict", "atMs": at_ms});
        }
        if auto_merge {
            value["autoMerge"] =
                serde_json::json!({"enabledAt": "2026-11-27T00:00:00Z", "mergeMethod": "SQUASH"});
        }
        serde_json::from_value(value).unwrap()
    }

    fn board() -> SourceSnapshot<Board> {
        SourceSnapshot::test_live(
            Some(Board {
                rows: vec![BoardRow {
                    key: "one".into(),
                    slug: "one".into(),
                    branch: "feature/one".into(),
                    path: "/nonexistent/one".into(),
                    pr: Some(PrPresentation {
                        number: Some(7),
                        state: Some("OPEN".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            SourceState::Ready,
            Domain::Git,
        )
    }

    fn github(pr: wt_github::PullRequest, complete: bool) -> GithubData {
        GithubData {
            prs: [("feature/one".into(), pr)].into(),
            covered_branches: ["feature/one".into()].into(),
            merge_queue_complete: complete,
            ..Default::default()
        }
    }

    fn armed(at_ms: i64) -> SourceSnapshot<Metadata> {
        SourceSnapshot::test_live(
            Some((
                serde_json::json!({"slugs": {"one": {"mergeArm": {
                    "pr": 7, "armedAtMs": at_ms, "mechanism": "queue"
                }}}}),
                Default::default(),
            )),
            SourceState::Ready,
            Domain::Metadata,
        )
    }

    fn shown(
        github: SourceSnapshot<GithubData>,
        metadata: &SourceSnapshot<Metadata>,
    ) -> Option<String> {
        let composed = stage(board(), &github, metadata, &SourceSnapshot::default());
        composed.data().unwrap().rows[0]
            .pr
            .as_ref()
            .unwrap()
            .queue_removal
            .clone()
    }

    /// One pass of the stage with no failure remembered from an earlier one.
    fn stage(
        source: SourceSnapshot<Board>,
        github: &SourceSnapshot<GithubData>,
        metadata: &SourceSnapshot<Metadata>,
        reviewers: &SourceSnapshot<ReviewerPool>,
    ) -> SourceSnapshot<Board> {
        compose(
            &mut FailureClock::default(),
            source,
            github,
            metadata,
            reviewers,
        )
    }

    fn live(data: GithubData) -> SourceSnapshot<GithubData> {
        SourceSnapshot::test_live(Some(data), SourceState::Ready, Domain::Github)
    }

    #[test]
    fn a_removal_after_the_arming_is_shown_under_the_shared_rule() {
        let evicted = github(pr(Some(2_000), false), true);
        // The words every other surface uses, from the same function.
        assert_eq!(
            shown(live(evicted.clone()), &armed(1_000)).as_deref(),
            Some("removed from the merge queue: merge_conflict")
        );
        assert_eq!(
            shown(live(evicted.clone()), &armed(1_000)),
            crate::wait_facts::queue_removal(
                &evicted.prs["feature/one"],
                serde_json::from_value(serde_json::json!({
                    "pr": 7, "armedAtMs": 1_000, "mechanism": "queue"
                }))
                .ok()
                .as_ref(),
                Some(false),
            )
        );
        // An older removal says nothing about this arming, and neither does
        // one a later arming has made history.
        assert_eq!(shown(live(evicted.clone()), &armed(3_000)), None);
        assert_eq!(
            shown(live(github(pr(Some(2_000), true), true)), &armed(1_000)),
            None
        );
        // A read of part of the queue cannot say the PR is out of it.
        assert_eq!(
            shown(live(github(pr(Some(2_000), false), false)), &armed(1_000)),
            None
        );
        // No arming by wt: nothing to explain.
        let unarmed = SourceSnapshot::test_live(
            Some((serde_json::json!({"slugs": {}}), Default::default())),
            SourceState::Ready,
            Domain::Metadata,
        );
        assert_eq!(shown(live(evicted), &unarmed), None);
    }

    #[test]
    fn a_saved_github_read_never_says_a_pull_request_left_the_queue() {
        let evicted = github(pr(Some(2_000), false), true);
        let restored = SourceSnapshot::test_restored(evicted, Domain::Github, 1);
        assert_eq!(shown(restored, &armed(1_000)), None);
        assert_eq!(
            shown(SourceSnapshot::default(), &armed(1_000)),
            None,
            "no read at all"
        );
    }

    #[test]
    fn the_pool_reaches_the_board_without_lending_it_provenance() {
        let pool = ReviewerPool {
            viewer: "me".into(),
            fetched_at_ms: 9,
            ..Default::default()
        };
        // Restored from disk, days old: still the list the picker opens with.
        let saved = SourceSnapshot::test_restored(pool.clone(), Domain::Github, 1);
        let composed = stage(board(), &live(GithubData::default()), &armed(1_000), &saved);
        assert_eq!(composed.data().unwrap().reviewers, Some(pool));
        assert!(
            composed.domains()[&Domain::Github].is_live(),
            "a saved pool made the board's GitHub data read as restored"
        );
        // No pool yet, and no board yet: both pass through.
        let composed = stage(
            board(),
            &SourceSnapshot::default(),
            &SourceSnapshot::default(),
            &SourceSnapshot::default(),
        );
        assert_eq!(composed.data().unwrap().reviewers, None);
        assert!(
            stage(
                SourceSnapshot::default(),
                &SourceSnapshot::default(),
                &SourceSnapshot::default(),
                &SourceSnapshot::default(),
            )
            .data()
            .is_none()
        );
    }

    /// The picker says only that the list is not there. Why a read that
    /// never succeeded failed is said on the attention feed, and not at all
    /// while an earlier pool still serves the picker.
    #[test]
    fn a_pool_that_could_not_be_read_at_all_is_said_on_the_attention_feed() {
        let failed = |pool: Option<ReviewerPool>| {
            let snapshot = match pool {
                Some(pool) => SourceSnapshot::test_restored(pool, Domain::Github, 1),
                None => SourceSnapshot::default(),
            };
            snapshot.with_state(SourceState::Failed("gh: not signed in\u{1b}[31m".into()))
        };
        let nothing = SourceSnapshot::default();
        let composed = stage(board(), &nothing, &SourceSnapshot::default(), &failed(None));
        let [line] = composed.data().unwrap().attention.as_slice() else {
            panic!("{:?}", composed.data().unwrap().attention)
        };
        assert_eq!(line.source, "Reviewer list");
        assert_eq!(line.text, "Reviewer list: gh: not signed in");
        let saved = failed(Some(ReviewerPool::default()));
        let composed = stage(board(), &nothing, &SourceSnapshot::default(), &saved);
        assert!(composed.data().unwrap().attention.is_empty());
    }
}
