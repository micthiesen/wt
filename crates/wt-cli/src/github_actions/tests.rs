//! Shared GitHub fixtures and executor tests.

use super::*;
use wt_github::AutoMerge;
use wt_tui::ReplyOutcome;

use super::writes::{LedgerState, record_accepted};

pub(crate) fn pr(branch: &str) -> PullRequest {
    PullRequest {
        id: Some("PR_node".into()),
        number: 7,
        url: "https://example.test/pr/7".into(),
        head_ref_name: branch.into(),
        head_ref_oid: Some("deadbeef".into()),
        base_ref_name: "main".into(),
        merge_commit_oid: None,
        title: "test".into(),
        is_draft: true,
        state: "OPEN".into(),
        mergeable: None,
        merge_state_status: None,
        checks: PrChecks::Pending,
        failed_checks: vec![],
        review: wt_github::PrReview::None,
        review_requests: 0,
        requested_reviewers: vec![],
        suggested_reviewers: vec![],
        review_bot: None,
        auto_merge: None,
        comments: vec![],
        unresolved_threads: 0,
        unresolved_threads_total: 0,
        merged_at: None,
        closed_at: None,
    }
}

/// GitHub data with one open draft PR #7 on `branch`.
pub(crate) fn open_pr_data(branch: &str) -> GithubData {
    let mut data = GithubData::default();
    data.covered_branches.insert(branch.into());
    data.prs.insert(branch.into(), pr(branch));
    data
}

/// The same data with classic auto-merge armed on every PR.
pub(crate) fn with_auto_merge(mut data: GithubData) -> GithubData {
    for pr in data.prs.values_mut() {
        pr.auto_merge = Some(AutoMerge {
            enabled_at: "then".into(),
            merge_method: wt_github::AutoMergeMethod::Rebase,
        });
    }
    data
}

pub(crate) fn ledger_with_accepted_enable(branch: &str, number: u64, at: Stamp) -> LedgerState {
    let mut ledger = LedgerState::default();
    record_accepted(
        &mut ledger,
        1,
        GithubWrite::auto_merge(branch, number, true),
        at,
    );
    ledger
}

#[test]
fn row_resolution_rejects_ambiguous_key_or_slug_matches() {
    let board = Board {
        rows: vec![
            wt_tui::BoardRow {
                key: "host/one".into(),
                slug: "one".into(),
                ..Default::default()
            },
            wt_tui::BoardRow {
                key: "one".into(),
                slug: "two".into(),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    assert!(resolve_row(&board, "one").is_none());
}

#[test]
fn merge_target_keeps_the_exact_head_and_requires_identity() {
    let target = merge_target(&pr("feature/one")).unwrap();
    assert_eq!(target.head_ref_oid, "deadbeef");
    assert!(
        merge_target(&PullRequest {
            id: None,
            ..pr("feature/one")
        })
        .is_none()
    );
    assert!(
        merge_target(&PullRequest {
            head_ref_oid: None,
            ..pr("feature/one")
        })
        .is_none()
    );
}

#[test]
fn pins_require_the_same_pr_and_head() {
    let pin = PrPin {
        number: 7,
        head_sha: Some("deadbeef".into()),
    };
    let current = pr("feature");
    assert!(pin_matches(Some(&pin), Some(&current)));
    assert!(pin_matches(None, None));
    assert!(!pin_matches(None, Some(&current)));
    assert!(!pin_matches(Some(&pin), None));
    let moved = PullRequest {
        head_ref_oid: Some("cafe".into()),
        ..current.clone()
    };
    assert!(!pin_matches(Some(&pin), Some(&moved)));
    let replaced = PullRequest {
        number: 8,
        ..current
    };
    assert!(!pin_matches(Some(&pin), Some(&replaced)));
}

#[cfg(unix)]
struct FakeGh {
    fixture: wt_app_core::test_support::CommandFixture,
    calls: std::path::PathBuf,
    actions: GithubActions,
}

#[cfg(unix)]
async fn fake_gh() -> FakeGh {
    use std::{fs, os::unix::fs::PermissionsExt};

    let fixture = wt_app_core::test_support::CommandFixture::new()
        .await
        .unwrap();
    let fake = fixture._root.path().join("fake-gh");
    let calls = fixture._root.path().join("gh-calls.jsonl");
    fs::write(
        &fake,
        format!(
            "#!/usr/bin/env python3\nimport json,sys\nwith open({calls:?},'a') as f: f.write(json.dumps(sys.argv[1:])+'\\n')\n",
            calls = calls.to_string_lossy()
        ),
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let client = GithubClient::new(
        fixture.ctx.processes.clone(),
        fixture.ctx.config.paths.main_clone.clone(),
        GithubOptions::from_config(&fixture.ctx.config, false),
    )
    .with_gh_program(fake);
    let actions = GithubActions {
        client,
        ledger: WriteLedger::new(GithubRefresh::detached()),
        retries: Retries::detached(),
        cancellation: fixture.ctx.cancellation.clone(),
        reviewers_enabled: false,
        default_reviewer: None,
    };
    FakeGh {
        fixture,
        calls,
        actions,
    }
}

fn board() -> Board {
    Board {
        rows: vec![wt_tui::BoardRow {
            key: "one".into(),
            slug: "one".into(),
            branch: "feature/one".into(),
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn seen() -> Option<PrPin> {
    Some(PrPin {
        number: 7,
        head_sha: Some("deadbeef".into()),
    })
}

#[cfg(unix)]
#[tokio::test]
async fn mark_ready_targets_the_selected_open_pr_and_records_the_write() {
    let fake = fake_gh().await;
    let github = Confirmed::for_test(open_pr_data("feature/one"));
    let reply = fake
        .actions
        .execute(
            GithubAction::MarkReady {
                key: "one".into(),
                pin: seen(),
            },
            &board(),
            Some(&github),
        )
        .await
        .unwrap();
    assert!(!reply.failed);
    assert!(reply.message.contains("ready for review"));
    assert!(matches!(reply.outcome, Some(ReplyOutcome::Accepted(_))));
    // The lagging read still shows a draft; the record makes a repeat a no-op.
    assert!(!fake.actions.ledger.effective(&github).prs["feature/one"].is_draft);
    assert_eq!(
        std::fs::read_to_string(&fake.calls).unwrap().trim(),
        r#"["pr", "ready", "7"]"#
    );
    fake.fixture.close().await.unwrap();
}

/// A write names the PR the user reviewed. A different PR, a moved
/// head, or no confirmed data refuses the write before anything is sent.
#[cfg(unix)]
#[tokio::test]
async fn a_pin_mismatch_or_missing_confirmed_data_refuses_the_write() {
    let fake = fake_gh().await;
    let mut moved = open_pr_data("feature/one");
    moved.prs.get_mut("feature/one").unwrap().head_ref_oid = Some("cafe".into());
    let moved = Confirmed::for_test(moved);
    let current = Confirmed::for_test(open_pr_data("feature/one"));
    let requests = [
        (
            GithubAction::SetAutoMerge {
                key: "one".into(),
                enable: true,
                pin: seen(),
            },
            Some(&moved),
        ),
        (
            GithubAction::Ship {
                key: "one".into(),
                pin: Some(PrPin {
                    number: 6,
                    head_sha: Some("deadbeef".into()),
                }),
            },
            Some(&current),
        ),
        (
            GithubAction::MarkReady {
                key: "one".into(),
                pin: seen(),
            },
            None,
        ),
    ];
    for (action, github) in requests {
        let reply = match fake.actions.execute(action, &board(), github).await {
            Ok(reply) => reply,
            Err(error) => wt_app_core::host_errors::error_reply(&error),
        };
        assert!(reply.failed);
        if github.is_some() {
            // A pin mismatch reopens the menu from the board, not a failure.
            assert!(reply.stale);
            assert_eq!(
                reply.message,
                format!("Changed since you looked: {STALE_PIN}")
            );
        } else {
            assert_eq!(reply.message, "GitHub data is still loading");
        }
    }
    assert!(!fake.calls.exists(), "no GitHub request may be sent");
    fake.fixture.close().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn confirmed_snapshot_must_cover_the_selected_branch_before_reads_or_writes() {
    let fake = fake_gh().await;
    let old_inventory = Confirmed::for_test(open_pr_data("feature/old"));
    // Even a retained PR entry cannot substitute for explicit read coverage.
    let mut uncovered_entry = open_pr_data("feature/one");
    uncovered_entry.covered_branches.clear();
    let uncovered_entry = Confirmed::for_test(uncovered_entry);
    for github in [&old_inventory, &uncovered_entry] {
        for action in [
            GithubAction::MarkReady {
                key: "one".into(),
                pin: seen(),
            },
            GithubAction::SetAutoMerge {
                key: "one".into(),
                enable: true,
                pin: seen(),
            },
            GithubAction::Ship {
                key: "one".into(),
                pin: seen(),
            },
            GithubAction::FailedChecks { key: "one".into() },
        ] {
            let reply = fake
                .actions
                .execute(action, &board(), Some(github))
                .await
                .unwrap();
            assert!(reply.failed);
            assert_eq!(
                reply.message,
                "GitHub data is still loading for this branch"
            );
        }
    }
    assert!(
        !fake.calls.exists(),
        "uncovered branches cannot cause GitHub requests"
    );
    fake.fixture.close().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn covered_absence_is_no_pr_but_cannot_match_a_visible_pr_pin() {
    let fake = fake_gh().await;
    let github = Confirmed::for_test(GithubData {
        covered_branches: ["feature/one".into()].into(),
        ..Default::default()
    });
    for action in [
        GithubAction::FailedChecks { key: "one".into() },
        GithubAction::MarkReady {
            key: "one".into(),
            pin: None,
        },
    ] {
        let reply = fake
            .actions
            .execute(action, &board(), Some(&github))
            .await
            .unwrap();
        assert!(!reply.failed);
        assert_eq!(reply.message, "no PR for this row");
        assert_eq!(reply.outcome, Some(ReplyOutcome::NoChange));
    }
    let error = fake
        .actions
        .execute(
            GithubAction::MarkReady {
                key: "one".into(),
                pin: seen(),
            },
            &board(),
            Some(&github),
        )
        .await
        .unwrap_err();
    assert!(wt_app_core::host_errors::error_reply(&error).stale);
    assert_eq!(error.to_string(), STALE_PIN);
    assert!(!fake.calls.exists());
    fake.fixture.close().await.unwrap();
}
