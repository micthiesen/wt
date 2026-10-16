use super::effect::NEW_SECTION;
use super::*;
use crate::{PrPresentation, ReviewRequestRow, UiAction};
use wt_runtime::conformance;

fn board() -> Board {
    Board {
        rows: vec![BoardRow {
            key: "a".into(),
            title: "Old".into(),
            pr: Some(PrPresentation {
                number: Some(7),
                head_sha: Some("abc".into()),
                state: Some("OPEN".into()),
                draft: true,
                ..Default::default()
            }),
            ..Default::default()
        }],
        sections: vec![
            BoardSection {
                key: INBOX_SECTION.into(),
                title: "Inbox".into(),
                rows: vec![0],
                ..Default::default()
            },
            BoardSection {
                key: "Batch".into(),
                title: "Batch".into(),
                ..Default::default()
            },
        ],
        review_requests: vec![ReviewRequestRow {
            url: "https://github.com/o/r/pull/9".into(),
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn row(kind: impl Into<MutationKind>) -> UiMutation {
    UiMutation {
        target: Target::row(None, "a"),
        kind: kind.into(),
    }
}

fn whole(kind: impl Into<MutationKind>) -> UiMutation {
    UiMutation {
        target: Target::board(None),
        kind: kind.into(),
    }
}

fn with(edit: impl FnOnce(&mut Board)) -> Board {
    let mut board = board();
    edit(&mut board);
    board
}

fn pr(board: &mut Board) -> &mut PrPresentation {
    board.rows[0].pr.as_mut().unwrap()
}

/// Every mutation kind meets the shared optimistic contract against data a
/// real fetch would return.
#[tokio::test(start_paused = true)]
async fn every_mutation_kind_meets_the_contract() {
    let cases: Vec<(UiMutation, Board, Board)> = vec![
        (
            row(GithubChange::AutoMerge { enable: true }),
            board(),
            with(|b| pr(b).auto_merge_armed = true),
        ),
        (
            row(GithubChange::AutoMerge { enable: false }),
            with(|b| pr(b).auto_merge_armed = true),
            board(),
        ),
        (
            row(GithubChange::MarkReady),
            board(),
            with(|b| pr(b).draft = false),
        ),
        (
            row(GithubChange::Ship),
            board(),
            with(|b| {
                pr(b).draft = false;
                pr(b).merge_queue = Some(Default::default());
            }),
        ),
        (
            row(GithubChange::Reviewers {
                reviewers: vec!["sam".into()],
            }),
            board(),
            with(|b| pr(b).reviewers = vec!["sam".into()]),
        ),
        (
            row(RowChange::Status {
                state: wt_core::WorkState::Review,
            }),
            board(),
            with(|b| {
                b.rows[0].work = Some(crate::WorkPresentation {
                    record: Some(wt_core::WorkStatusRecord::new(
                        wt_core::WorkState::Review,
                        "2026-10-11T00:00:00Z",
                    )),
                    ..Default::default()
                })
            }),
        ),
        (
            row(LayoutChange::Archive { archived: true }),
            board(),
            with(|b| {
                b.rows[0].archived = true;
                b.sections[0].rows.clear();
                b.sections.push(BoardSection {
                    key: ARCHIVED_SECTION.into(),
                    title: "Archived".into(),
                    rows: vec![0],
                    ..Default::default()
                });
            }),
        ),
        (
            row(LayoutChange::MoveSection {
                section: Some("batch".into()),
            }),
            board(),
            with(|b| {
                b.sections[0].rows.clear();
                b.sections[1].rows.push(0);
            }),
        ),
        (
            whole(LayoutChange::FoldSection {
                section: "Batch".into(),
                folded: true,
            }),
            board(),
            with(|b| b.sections[1].folded = true),
        ),
        (
            whole(LayoutChange::RenameSection {
                old: "Batch".into(),
                new: "Later".into(),
            }),
            board(),
            with(|b| {
                b.sections[1].key = "Later".into();
                b.sections[1].title = "Later".into();
            }),
        ),
        (
            row(RowChange::Title {
                title: "New".into(),
            }),
            board(),
            with(|b| b.rows[0].title = "New".into()),
        ),
        (
            row(RowChange::Base {
                base: Some("feature".into()),
            }),
            board(),
            with(|b| b.rows[0].base_branch = Some("feature".into())),
        ),
        (
            row(RowChange::Issue {
                issue_id: Some("ENG-1".into()),
            }),
            board(),
            with(|b| b.rows[0].issue_id = Some("ENG-1".into())),
        ),
        (
            row(RowChange::Automations { paused: true }),
            board(),
            with(|b| b.rows[0].automations_paused = true),
        ),
        (
            whole(RowChange::Automations { paused: true }),
            board(),
            with(|b| b.automations_paused = true),
        ),
        (
            whole(LayoutChange::AttentionSeen { at_ms: 5 }),
            board(),
            with(|b| b.attention_seen_ms = 9),
        ),
        (
            whole(GithubChange::DismissReview {
                url: "https://github.com/o/r/pull/9".into(),
            }),
            board(),
            with(|b| b.review_requests.clear()),
        ),
        (
            whole(LifecycleChange::Create {
                request_id: 1,
                input: "new".into(),
                resolved: Some("new".into()),
            }),
            board(),
            with(|b| {
                b.rows.push(BoardRow {
                    key: "new".into(),
                    ..Default::default()
                })
            }),
        ),
        (
            whole(LifecycleChange::Remove {
                keys: vec!["a".into()],
            }),
            board(),
            with(|b| {
                b.rows.clear();
                b.sections[0].rows.clear();
            }),
        ),
        (
            whole(RowChange::Primary {
                harness: Some("Codex".into()),
            }),
            board(),
            with(|b| b.display.primary_harness = "codex".into()),
        ),
    ];
    for (mutation, before, after) in cases {
        if matches!(
            mutation.kind,
            MutationKind::Lifecycle(LifecycleChange::Remove { .. })
        ) {
            conformance::mutation_with_projection(mutation, before, after, |mutation, board| {
                let MutationKind::Lifecycle(LifecycleChange::Remove { keys }) = &mutation.kind
                else {
                    unreachable!();
                };
                keys.iter().all(|key| {
                    board
                        .rows
                        .iter()
                        .find(|row| &row.key == key)
                        .is_none_or(|row| {
                            matches!(row.badge.as_str(), "removing" | "checking removal")
                                && row.busy.as_ref().is_some_and(|busy| busy.op == "remove")
                        })
                })
            })
            .await;
        } else if matches!(
            mutation.kind,
            MutationKind::Lifecycle(LifecycleChange::Create { .. })
        ) {
            conformance::mutation_progress_only(mutation, before, after).await;
        } else {
            conformance::mutation(mutation, before, after).await;
        }
    }
}

#[test]
fn every_action_declares_an_effect_and_toggles_use_the_shown_state() {
    let shown = with(|b| {
        pr(b).auto_merge_armed = true;
        b.rows[0].archived = true;
    });
    let effect = UiAction::ToggleArchive {
        key: "a".into(),
        seen: None,
    }
    .effect(&shown);
    assert!(matches!(
        effect,
        Effect::Mutation(UiMutation {
            kind: MutationKind::Layout(LayoutChange::Archive { archived: false }),
            ..
        })
    ));
    // A witnessed toggle keeps the direction the user saw, even when the
    // board it is resolved against has since changed.
    let effect = UiAction::ToggleArchive {
        key: "a".into(),
        seen: Some(crate::Seen(false)),
    }
    .effect(&shown);
    assert!(matches!(
        effect,
        Effect::Mutation(UiMutation {
            kind: MutationKind::Layout(LayoutChange::Archive { archived: true }),
            ..
        })
    ));
    let paused = with(|b| {
        b.rows[0].automations_paused = true;
        b.automations_paused = true;
    });
    for (action, expected) in [
        (
            UiAction::ToggleAutomations {
                key: Some("a".into()),
                seen: Some(crate::Seen(false)),
            },
            true,
        ),
        (
            UiAction::ToggleAutomations {
                key: None,
                seen: Some(crate::Seen(false)),
            },
            true,
        ),
        (
            UiAction::ToggleAutomations {
                key: Some("a".into()),
                seen: None,
            },
            false,
        ),
    ] {
        let Effect::Mutation(mutation) = action.effect(&paused) else {
            panic!("automation toggles are mutations");
        };
        assert!(
            matches!(
                mutation.kind,
                MutationKind::Row(RowChange::Automations { paused }) if paused == expected
            ),
            "{action:?}"
        );
    }
    let remote = UiAction::OnHost {
        host: Some("box".into()),
        action: Box::new(UiAction::GithubSetAutoMerge {
            key: "missing".into(),
            enable: false,
            pin: None,
        }),
    }
    .effect(&shown);
    let Effect::Mutation(mutation) = remote else {
        panic!("a GitHub write is a mutation");
    };
    assert_eq!(mutation.target.host.as_deref(), Some("box"));
    assert_eq!(mutation.domains(), [Domain::Github]);
    assert_eq!(UiAction::PrepareCleanup.effect(&shown), Effect::ReadOnly);
    assert_eq!(
        UiAction::MoveSection {
            key: "a".into(),
            section: Some(NEW_SECTION.into()),
            seen: None,
        }
        .effect(&shown),
        Effect::ReadOnly
    );
}

#[test]
fn interactive_entry_never_waits_for_a_board_mutation_stamp() {
    use crate::{SessionMode, SessionSelection, SessionTarget};
    let mut actions = [
        SessionTarget::Shell,
        SessionTarget::Diff,
        SessionTarget::Harness,
        SessionTarget::Main,
        SessionTarget::Manager,
        SessionTarget::WtSource,
        SessionTarget::Dotfiles,
    ]
    .into_iter()
    .map(|target| UiAction::Session {
        key: Some("a".into()),
        target,
    })
    .collect::<Vec<_>>();
    actions.push(UiAction::SelectSession {
        selection: SessionSelection {
            key: Some("a".into()),
            target: SessionTarget::Harness,
            harness: wt_core::HarnessId::Codex,
            session_id: None,
            live_slot: None,
            managed_name: None,
            mode: SessionMode::New,
            live: false,
            seen_live: None,
        },
    });
    actions.push(UiAction::EnterHarness {
        key: "a".into(),
        harness: wt_core::HarnessId::Codex,
    });
    for then in [
        None,
        Some(Box::new(UiAction::Session {
            key: Some("a".into()),
            target: SessionTarget::Harness,
        })),
    ] {
        actions.push(UiAction::ManagedSignIn {
            provider: "codex".into(),
            then,
        });
    }
    for action in actions {
        assert_eq!(action.effect(&board()), Effect::ReadOnly);
        let remote = UiAction::OnHost {
            host: Some("MaxBook".into()),
            action: Box::new(action),
        };
        assert_eq!(remote.effect(&board()), Effect::ReadOnly);
    }
}

#[test]
fn every_declared_domain_can_settle() {
    use crate::{ActionSurface, RemovalRevision, SessionMode, SessionSelection, SessionTarget};
    let revision = RemovalRevision {
        key: "a".into(),
        path: String::new(),
        branch: String::new(),
        head: String::new(),
        digest: String::new(),
        hazards: Vec::new(),
        published_base: None,
    };
    let selection = SessionSelection {
        key: Some("a".into()),
        target: SessionTarget::Harness,
        harness: wt_core::HarnessId::Claude,
        session_id: None,
        live_slot: None,
        managed_name: None,
        mode: SessionMode::New,
        live: false,
        seen_live: None,
    };
    let actions = vec![
        UiAction::HardRefresh,
        UiAction::SetAttentionSeen { at_ms: 1 },
        UiAction::ToggleAutomations {
            key: Some("a".into()),
            seen: None,
        },
        UiAction::ToggleAutomations {
            key: None,
            seen: None,
        },
        UiAction::CancelAutomations,
        UiAction::RestoreRemoved {
            key: "a".into(),
            removed_at: String::new(),
            branch: String::new(),
        },
        UiAction::ToggleRemovedAutomations { key: "a".into() },
        UiAction::PerfInvestigate { report: Vec::new() },
        UiAction::Restack { key: "a".into() },
        UiAction::ReviewCheckout {
            url: String::new(),
            updated_at: String::new(),
            branch: String::new(),
        },
        UiAction::DismissReviewRequest {
            url: "u".into(),
            updated_at: String::new(),
        },
        UiAction::SubmitReviewers {
            key: "a".into(),
            pr_number: 7,
            original: Vec::new(),
            selected: Vec::new(),
        },
        UiAction::GithubMarkReady {
            key: "a".into(),
            pin: None,
        },
        UiAction::GithubSetAutoMerge {
            key: "a".into(),
            enable: true,
            pin: None,
        },
        UiAction::GithubShip {
            key: "a".into(),
            pin: None,
        },
        UiAction::GithubFailedChecks { key: "a".into() },
        UiAction::KillAction {
            action_key: String::new(),
            run_id: String::new(),
        },
        UiAction::RunAction {
            surface: ActionSurface::Row { key: "a".into() },
            id: String::new(),
            arg: None,
            extras: String::new(),
        },
        UiAction::FoldSection {
            key: "Batch".into(),
            folded: true,
        },
        UiAction::MoveSection {
            key: "a".into(),
            section: Some("Batch".into()),
            seen: None,
        },
        UiAction::Reorder {
            key: Some("a".into()),
            section: String::new(),
            down: true,
        },
        UiAction::RenameSection {
            old: "Batch".into(),
            new: "Later".into(),
        },
        UiAction::CyclePrimary,
        UiAction::SetTitle {
            key: "a".into(),
            title: "t".into(),
        },
        UiAction::Create { input: "n".into() },
        UiAction::Remove {
            key: "a".into(),
            force: false,
            revision: Some(revision.clone()),
            seen: None,
        },
        UiAction::Cleanup {
            revisions: vec![revision],
        },
        UiAction::ToggleArchive {
            key: "a".into(),
            seen: None,
        },
        UiAction::SetStatus {
            key: "a".into(),
            state: Some("review".into()),
            note: None,
            verify_after_merge: None,
            seen: None,
        },
        UiAction::SetStatus {
            key: "a".into(),
            state: None,
            note: None,
            verify_after_merge: None,
            seen: None,
        },
        UiAction::SetBase {
            key: "a".into(),
            base: None,
            seen: None,
        },
        UiAction::SetIssueOverride {
            key: "a".into(),
            issue_id: None,
        },
        UiAction::KillSession {
            selection: selection.clone(),
        },
        UiAction::StopSession { selection },
        UiAction::StopTerminal {
            key: "a".into(),
            target: SessionTarget::Shell,
            session_id: String::new(),
            created_at: 0,
        },
    ];
    for action in actions {
        let Effect::Mutation(mutation) = action.effect(&board()) else {
            panic!("{action:?} is a write");
        };
        for domain in mutation.domains() {
            assert!(
                SETTLING_DOMAINS.contains(domain),
                "{action:?} waits on {domain:?}, which no source publishes"
            );
        }
    }
}

#[test]
fn creation_projection_replaces_placeholder_with_the_exact_inventory_row() {
    let mut creating = whole(LifecycleChange::Create {
        request_id: 1,
        input: "new".into(),
        resolved: None,
    });
    creating.assign_creation(1);
    let mut projected = board();
    creating.apply(&mut projected);
    assert!(projected.rows.last().unwrap().pending_creation);
    creating.target.key = Some("new".into());
    let mut source = board();
    source.rows.push(BoardRow {
        key: "new".into(),
        title: "Real title".into(),
        path: "/actual/new".into(),
        ..Default::default()
    });
    creating.apply(&mut source);
    assert_eq!(source.rows.iter().filter(|row| row.key == "new").count(), 1);
    let actual = source.rows.last().unwrap();
    assert_eq!(actual.title, "Real title");
    assert_eq!(actual.path, "/actual/new");
    assert!(actual.pending_creation);
}

#[test]
fn removal_stays_in_place_until_the_worker_accepts_it() {
    let removing = whole(LifecycleChange::Remove {
        keys: vec!["a".into()],
    });
    let mut latest = with(|b| b.rows[0].title = "New title".into());
    removing.apply(&mut latest);
    assert!(!latest.rows[0].archived);
    assert_eq!(latest.rows[0].title, "New title");
    assert_eq!(latest.rows[0].badge, "checking removal");
    assert!(latest.sections[0].rows.contains(&0));
}

#[test]
fn removal_waits_for_inventory_after_worker_acceptance() {
    let removal = LifecycleChange::Remove {
        keys: vec!["a".into()],
    };
    let target = Target::row(None, "a");
    let mut latest = board();
    assert!(!removal.observed(&target, &latest));
    latest.rows[0].archived = true;
    latest.rows[0].removal = Some(crate::RemovalView::default());
    assert!(!removal.observed(&target, &latest));
    removal.apply(&target, &mut latest);
    assert_eq!(latest.rows[0].badge, "removing");
    latest.rows.clear();
    assert!(removal.observed(&target, &latest));
}

#[test]
fn pending_removal_does_not_hide_a_durable_failure() {
    let removal = LifecycleChange::Remove {
        keys: vec!["a".into()],
    };
    let target = Target::row(None, "a");
    let mut latest = board();
    latest.rows[0].removal = Some(crate::RemovalView {
        error: Some("denied".into()),
    });
    latest.rows[0].badge = "Removal failed".into();
    removal.apply(&target, &mut latest);
    assert_eq!(latest.rows[0].badge, "Removal failed");
    assert!(latest.rows[0].busy.is_none());
}
