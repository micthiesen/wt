//! Content overlays that load from a host, and the keys that used to wait
//! on one: the overlay is on screen from the keypress and owns the keys, and
//! everything else is answered from the board.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use wt_runtime::{SourceSnapshot, SourceState};

use super::*;
use crate::{
    ActionFacts, CheckState, ConfirmAction, PrFact, PrPresentation, SessionTarget, TextAction,
    UiAction, UiModal, UiReply,
};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::from(code)
}

fn ch(model: &mut Model, letter: char) -> InputResult {
    model.input(key(KeyCode::Char(letter)), 20)
}

fn row(name: &str) -> BoardRow {
    BoardRow {
        key: name.into(),
        slug: name.into(),
        title: name.into(),
        branch: format!("feature/{name}"),
        ..BoardRow::default()
    }
}

fn live(board: Board) -> SourceSnapshot<Board> {
    SourceSnapshot::test_live(Some(board), SourceState::Ready, wt_core::Domain::Git)
}

fn failing() -> PrPresentation {
    PrPresentation {
        number: Some(7),
        head_sha: Some("abc".into()),
        state: Some("OPEN".into()),
        checks: CheckState::Fail,
        ..Default::default()
    }
}

/// Two rows, the first with a failing pull request.
fn model() -> Model {
    let mut model = Model::default();
    model.apply(live(Board {
        rows: vec![
            BoardRow {
                pr: Some(failing()),
                ..row("one")
            },
            row("two"),
        ],
        ..Board::default()
    }));
    model
}

/// Press `f` and send the request as the terminal does.
fn load_failed_checks(model: &mut Model, id: u64) -> u64 {
    let InputResult::Action(action) = ch(model, 'f') else {
        panic!("a failing check asks its host for the log")
    };
    assert_eq!(action, UiAction::GithubFailedChecks { key: "one".into() });
    let generation = model.ui_generation;
    model.begin_action(id, &action);
    generation
}

fn log(lines: &[&str], refresh: Option<UiAction>) -> UiModal {
    UiModal::Log {
        title: "Failed checks: feature/one #9".into(),
        lines: lines.iter().map(|line| (*line).to_owned()).collect(),
        close_key: Some('f'),
        refresh: refresh.map(Box::new),
    }
}

#[test]
fn the_overlay_is_open_from_the_keypress_and_the_reply_fills_it() {
    let mut model = model();
    let generation = load_failed_checks(&mut model, 42);
    let Interaction::Loading(loading) = &model.interaction else {
        panic!("the overlay opens with the key")
    };
    assert_eq!(loading.title, "Failed checks: feature/one");
    assert_eq!(loading.request, Some(42));
    assert!(model.layer_open());
    // The overlay says it is loading; the footer need not.
    assert_eq!(model.sent_feedback(), None);
    model.reply(UiReply {
        operation_id: Some(42),
        ui_generation: Some(generation),
        modal: Some(log(&["a", "b", "c", "d", "e"], None)),
        ..UiReply::message("failed CI logs for feature/one (run 9)")
    });
    let Interaction::Log { title, lines, .. } = &model.interaction else {
        panic!("the log replaces the loading body")
    };
    assert_eq!(title, "Failed checks: feature/one #9");
    assert_eq!(lines.len(), 5);
    assert!(model.pending_request.is_none());
    // The filled overlay scrolls and closes like any log.
    assert_eq!(ch(&mut model, 'j'), InputResult::Draw);
    assert_eq!(ch(&mut model, 'f'), InputResult::Draw);
    assert!(matches!(model.interaction, Interaction::None));
}

#[test]
fn keys_while_it_loads_belong_to_the_overlay_and_are_never_replayed() {
    let mut model = model();
    let generation = load_failed_checks(&mut model, 42);
    // `j` would move the cursor, `d` would ask to remove the row, `h` would
    // open removed history and Enter would enter a session.
    for code in [
        KeyCode::Char('j'),
        KeyCode::Down,
        KeyCode::Char('d'),
        KeyCode::Char('h'),
        KeyCode::Enter,
        KeyCode::PageDown,
    ] {
        assert_eq!(
            model.input(key(code), 20),
            InputResult::Unchanged,
            "{code:?}"
        );
    }
    assert_eq!(model.selected_row().unwrap().key, "one");
    assert!(!model.history.active);
    assert!(matches!(model.interaction, Interaction::Loading(_)));
    // None of them disturbed the request: its reply still fills the overlay.
    assert_eq!(model.ui_generation, generation);
    assert!(model.pending_request.is_some());
    model.reply(UiReply {
        operation_id: Some(42),
        ui_generation: Some(generation),
        modal: Some(log(&["a", "b", "c", "d", "e"], None)),
        ..Default::default()
    });
    // Nothing typed during the load acts on the log either.
    assert!(
        matches!(&model.interaction, Interaction::Log { scroll: 0, .. }),
        "keys typed while loading were replayed"
    );
    assert!(model.follow_up.is_none());
}

#[test]
fn closing_the_loading_overlay_cancels_its_request() {
    for close in [
        key(KeyCode::Esc),
        key(KeyCode::Char('q')),
        key(KeyCode::Char('f')),
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
    ] {
        let mut model = model();
        let InputResult::Action(action) = ch(&mut model, 'f') else {
            panic!()
        };
        let generation = model.ui_generation;
        model.begin_action(42, &action);
        let token = tokio_util::sync::CancellationToken::new();
        model.pending_request.as_mut().unwrap()._cancel_on_drop = Some(token.clone().drop_guard());
        assert_eq!(model.input(close, 20), InputResult::Draw, "{close:?}");
        assert!(matches!(model.interaction, Interaction::None));
        assert!(token.is_cancelled(), "{close:?} left the host reading");
        assert!(model.pending_request.is_none());
        // The board has the keys again at once.
        ch(&mut model, 'j');
        assert_eq!(model.selected_row().unwrap().key, "two");
        // A log that was already on its way opens nothing.
        model.reply(UiReply {
            operation_id: Some(42),
            ui_generation: Some(generation),
            modal: Some(log(&["late"], None)),
            ..Default::default()
        });
        assert!(matches!(model.interaction, Interaction::None));
    }
}

#[test]
fn another_request_replacing_the_overlays_cancels_it_and_closes_it() {
    let mut model = model();
    let InputResult::Action(action) = ch(&mut model, 'f') else {
        panic!()
    };
    model.begin_action(42, &action);
    let token = tokio_util::sync::CancellationToken::new();
    model.pending_request.as_mut().unwrap()._cancel_on_drop = Some(token.clone().drop_guard());
    // A request the model queued itself takes the foreground.
    model.begin_action(43, &UiAction::CyclePrimary);
    assert!(token.is_cancelled());
    assert!(
        matches!(model.interaction, Interaction::None),
        "an overlay nothing will fill stayed open"
    );
    assert_eq!(model.pending_request.as_ref().unwrap().id, 43);
}

#[test]
fn an_answer_without_content_closes_the_overlay_and_says_why() {
    let mut model = model();
    let generation = load_failed_checks(&mut model, 42);
    model.reply(UiReply {
        operation_id: Some(42),
        ui_generation: Some(generation),
        ..UiReply::failure("failed to fetch CI logs: gh exited 1")
    });
    assert!(matches!(model.interaction, Interaction::None));
    assert_eq!(
        model.toast,
        Some((
            "failed to fetch CI logs: gh exited 1".into(),
            ToastTone::Failed
        ))
    );
    // An empty answer still says something.
    let generation = load_failed_checks(&mut model, 43);
    model.reply(UiReply {
        operation_id: Some(43),
        ui_generation: Some(generation),
        ..Default::default()
    });
    assert!(matches!(model.interaction, Interaction::None));
    assert_eq!(
        model.toast,
        Some(("nothing to show".into(), ToastTone::Info))
    );
}

#[test]
fn replies_to_other_requests_leave_the_overlay_loading() {
    let mut model = model();
    let generation = load_failed_checks(&mut model, 42);
    model.reply(UiReply {
        operation_id: Some(41),
        ..UiReply::message("other work finished")
    });
    // A read the terminal sent on its own, answered for an older view.
    model.reply(UiReply {
        operation_id: Some(7),
        ui_generation: Some(generation.wrapping_sub(1)),
        modal: Some(log(&["someone else's"], None)),
        ..Default::default()
    });
    assert!(
        matches!(&model.interaction, Interaction::Loading(loading) if loading.request == Some(42))
    );
    assert!(model.pending_request.is_some());
}

#[test]
fn a_dialog_that_takes_the_overlays_place_ends_its_load() {
    let mut model = model();
    let InputResult::Action(action) = ch(&mut model, 'f') else {
        panic!()
    };
    let generation = model.ui_generation;
    model.begin_action(42, &action);
    let token = tokio_util::sync::CancellationToken::new();
    model.pending_request.as_mut().unwrap()._cancel_on_drop = Some(token.clone().drop_guard());
    model.reply(UiReply {
        operation_id: Some(7),
        ui_generation: Some(generation),
        modal: Some(UiModal::Confirm {
            action: ConfirmAction::HardRefresh,
            title: "Something else".into(),
            lines: Vec::new(),
            cancel_key: None,
        }),
        ..Default::default()
    });
    assert!(matches!(model.interaction, Interaction::Confirm(_)));
    assert!(token.is_cancelled(), "the replaced overlay kept loading");
    assert!(model.pending_request.is_none());
}

#[test]
fn a_request_that_was_never_sent_leaves_no_overlay_behind() {
    let mut model = model();
    model.committed_menu = Some(UiAction::PrepareStatus { key: "one".into() });
    let InputResult::Action(_) = ch(&mut model, 'f') else {
        panic!()
    };
    assert!(matches!(model.interaction, Interaction::Loading(_)));
    // The terminal found its queue full.
    model.request_not_sent();
    assert!(matches!(model.interaction, Interaction::None));
    assert!(model.committed_menu.is_none());
    ch(&mut model, 'j');
    assert_eq!(model.selected_row().unwrap().key, "two");
}

#[test]
fn a_live_overlay_refreshes_with_the_request_that_opened_it() {
    let mut model = Model::default();
    let remote = wt_core::remote_worktree_ledger_key("builder", "one");
    model.apply(live(Board {
        rows: vec![BoardRow {
            key: remote.clone(),
            host: Some("builder".into()),
            ..row("one")
        }],
        ..Board::default()
    }));
    let asked = UiAction::PrepareDevLogs {
        key: remote.clone(),
    };
    // What choosing `! l` produces reaches the model as any key's request.
    let InputResult::Action(action) = model.open_board_menu(asked.clone()) else {
        panic!("dev logs are read from the row's host")
    };
    let Interaction::Loading(loading) = &model.interaction else {
        panic!("the overlay opens at once")
    };
    assert_eq!(loading.title, "one dev logs");
    assert_eq!(loading.close_key, Some('l'));
    let generation = model.ui_generation;
    model.begin_action(42, &action);
    // The host answers with the key it knows the row by.
    model.reply(UiReply {
        operation_id: Some(42),
        ui_generation: Some(generation),
        modal_host: Some("builder".into()),
        modal: Some(UiModal::Log {
            title: "one dev logs".into(),
            lines: vec!["ready".into()],
            close_key: Some('l'),
            refresh: Some(Box::new(UiAction::PrepareDevLogs { key: "one".into() })),
        }),
        ..Default::default()
    });
    assert!(matches!(model.interaction, Interaction::Log { .. }));
    // Refreshing with that key would read this machine's `one`.
    assert_eq!(model.log_refresh, Some(asked.clone()));
    // Each refresh is answered the same way, and none of the answers may
    // turn the next request to this machine either.
    for line in ["ready", "serving"] {
        ch(&mut model, 'j');
        model.reply(UiReply {
            operation_id: Some(50),
            ui_generation: Some(model.ui_generation),
            modal_host: Some("builder".into()),
            modal: Some(UiModal::Log {
                title: "one dev logs".into(),
                lines: vec![line.into()],
                close_key: Some('l'),
                refresh: Some(Box::new(UiAction::PrepareDevLogs { key: "one".into() })),
            }),
            ..Default::default()
        });
        assert!(
            matches!(&model.interaction, Interaction::Log { lines, .. } if lines == &[line.to_owned()])
        );
        assert_eq!(model.log_refresh, Some(asked.clone()));
    }
    // Closing it ends the refresh, and another log starts its own.
    ch(&mut model, 'l');
    assert_eq!(model.log_refresh, None);
}

/// The overlay is bound to the request the terminal sent for it, whatever
/// resolving the action for the host did to it.
#[test]
fn the_overlay_waits_on_the_request_that_was_sent_for_it() {
    let mut model = model();
    let InputResult::Action(_) = ch(&mut model, 'f') else {
        panic!()
    };
    let sent = UiAction::OnHost {
        host: None,
        action: Box::new(UiAction::GithubFailedChecks { key: "one".into() }),
    };
    model.begin_action(42, &sent);
    let Interaction::Loading(loading) = &model.interaction else {
        panic!("sending its request closed the overlay")
    };
    assert_eq!(loading.request, Some(42));
    assert_eq!(loading.action, sent);
}

#[test]
fn checks_the_board_does_not_show_failing_are_answered_without_a_host() {
    let mut passing = failing();
    passing.checks = CheckState::Pass;
    for (pr, fact, said, refreshes) in [
        (Some(passing), PrFact::Unknown, "no failing checks", false),
        (None, PrFact::Absent, "no PR for this branch", false),
        (
            None,
            PrFact::Unknown,
            "GitHub data is still loading for this branch",
            true,
        ),
    ] {
        let mut model = Model::default();
        model.apply(live(Board {
            rows: vec![BoardRow {
                pr,
                action_facts: ActionFacts {
                    pr: fact,
                    ..Default::default()
                },
                ..row("one")
            }],
            display: DisplayPolicy {
                reviewers: true,
                ..Default::default()
            },
            ..Board::default()
        }));
        assert_eq!(ch(&mut model, 'f'), InputResult::Draw, "{said}");
        assert_eq!(model.toast, Some((said.into(), ToastTone::Notice)));
        assert!(matches!(model.interaction, Interaction::None));
        assert!(model.pending_request.is_none());
        // What the board shows is answered as it is. Data that never
        // loaded asks every source again. A pull request the last read did
        // not find may have been opened since: only that branch is read
        // again, not the whole board with its Git sweep.
        assert_eq!(std::mem::take(&mut model.refresh_requested), refreshes);
        let read = UiAction::ReadPullRequest { key: "one".into() };
        assert_eq!(
            std::mem::take(&mut model.background),
            if fact == PrFact::Absent && said.starts_with("no PR") {
                vec![read]
            } else {
                Vec::new()
            },
            "{said}"
        );
    }
}

/// `e`, `E` and `v` on a row whose pull request the board does not show used
/// to hold the key while the host waited up to twelve seconds for GitHub.
#[test]
fn github_keys_never_wait_on_a_host_for_a_row_without_a_shown_pr() {
    for letter in ['e', 'E', 'v'] {
        for (fact, said, refreshes) in [
            (PrFact::Absent, "no PR for this branch", false),
            (
                PrFact::Unknown,
                "GitHub data is still loading for this branch",
                true,
            ),
        ] {
            let mut model = Model::default();
            model.apply(live(Board {
                rows: vec![BoardRow {
                    action_facts: ActionFacts {
                        pr: fact,
                        ..Default::default()
                    },
                    ..row("one")
                }],
                display: DisplayPolicy {
                    reviewers: true,
                    ..Default::default()
                },
                ..Board::default()
            }));
            assert_eq!(ch(&mut model, letter), InputResult::Draw, "{letter}");
            assert_eq!(model.toast, Some((said.into(), ToastTone::Notice)));
            assert_eq!(std::mem::take(&mut model.refresh_requested), refreshes);
            assert_eq!(model.background.len(), usize::from(!refreshes));
            // Nothing is in flight, so the next key is the board's.
            assert!(model.pending_request.is_none());
            assert!(!model.layer_open());
        }
    }
}

fn reviewable() -> SourceSnapshot<Board> {
    live(Board {
        rows: vec![BoardRow {
            pr: Some(PrPresentation {
                reviewers: vec!["maintainer".into()],
                ..failing()
            }),
            ..row("one")
        }],
        display: DisplayPolicy {
            reviewers: true,
            ..Default::default()
        },
        reviewers: Some(crate::ReviewerPool {
            viewer: "me".into(),
            contributors: vec![crate::ContributorView {
                login: "ana".into(),
                contributions: 3,
            }],
            fetched_at_ms: 1,
        }),
        ..Board::default()
    })
}

#[test]
fn the_reviewer_picker_opens_from_the_board_and_a_stale_commit_reopens_it() {
    let mut model = Model::default();
    model.apply(reviewable());
    assert_eq!(ch(&mut model, 'v'), InputResult::Draw);
    let Interaction::Reviewers(picker) = &model.interaction else {
        panic!("the picker opens with the key")
    };
    assert_eq!(
        picker
            .candidates
            .iter()
            .map(|option| (option.login.as_str(), option.selected))
            .collect::<Vec<_>>(),
        [("maintainer", true), ("ana", false)]
    );
    // Choose ana as well and submit: the commit names the PR and the
    // requested list the picker showed.
    ch(&mut model, 'j');
    ch(&mut model, ' ');
    let InputResult::Action(commit) = ch(&mut model, 'v') else {
        panic!("the picker commits")
    };
    assert_eq!(
        commit,
        UiAction::SubmitReviewers {
            key: "one".into(),
            pr_number: 7,
            original: vec!["maintainer".into()],
            selected: vec!["maintainer".into(), "ana".into()],
        }
    );
    let generation = model.ui_generation;
    model.begin_action(9, &commit);
    // Its host found another requested list: nothing was written, and the
    // picker reopens from the board for a fresh choice.
    model.reply(UiReply {
        operation_id: Some(9),
        ui_generation: Some(generation),
        ..UiReply::stale("the requested reviewer list changed; reopen the picker")
    });
    assert!(model.refresh_requested);
    let Interaction::Reviewers(picker) = &model.interaction else {
        panic!("a stale commit reopens the picker")
    };
    assert!(!picker.candidates[1].selected, "the old choice was kept");
    assert_eq!(model.toast.as_ref().unwrap().1, ToastTone::Failed);
}

#[test]
fn an_open_reviewer_picker_keeps_the_choices_made_in_it_across_snapshots() {
    let mut model = Model::default();
    model.apply(reviewable());
    ch(&mut model, 'v');
    ch(&mut model, 'j');
    ch(&mut model, ' ');
    model.apply(reviewable());
    let Interaction::Reviewers(picker) = &model.interaction else {
        panic!("the picker stays open")
    };
    assert!(picker.candidates[1].selected);
    assert_eq!(picker.selected, 1);
}

fn sessions(host: Option<&str>) -> SourceSnapshot<Board> {
    let key = match host {
        Some(host) => wt_core::remote_worktree_ledger_key(host, "one"),
        None => "one".to_owned(),
    };
    let selection = crate::SessionSelection {
        key: Some(key.clone()),
        target: SessionTarget::Harness,
        harness: wt_core::HarnessId::Codex,
        session_id: Some("s1".into()),
        live_slot: Some(crate::LiveSessionIdentity {
            id: "$1".into(),
            created_at: 5,
        }),
        managed_name: None,
        mode: crate::SessionMode::Resume,
        live: true,
        seen_live: None,
    };
    let display = DisplayPolicy {
        harnesses: vec![wt_core::HarnessId::Codex],
        ..Default::default()
    };
    live(Board {
        rows: vec![BoardRow {
            key: key.clone(),
            host: host.map(str::to_owned),
            ..row("one")
        }],
        session_menus: [(
            key.clone(),
            vec![crate::SessionMenuEntry {
                selection,
                display_name: "primary".into(),
                state: Some("working".into()),
                is_live: true,
                last_active_ms: None,
            }],
        )]
        .into(),
        terminals: [(
            key,
            vec![crate::TerminalSlotView {
                target: SessionTarget::Shell,
                name: "one-shell".into(),
                id: "$7".into(),
                created_at: 42,
                retained: false,
            }],
        )]
        .into(),
        host_display: host
            .map(|host| (host.to_owned(), display.clone()))
            .into_iter()
            .collect(),
        display,
        ..Board::default()
    })
}

#[test]
fn session_menus_open_from_the_board_for_local_and_remote_rows() {
    for host in [None, Some("builder")] {
        let mut model = Model::default();
        model.apply(sessions(host));
        let row_key = model.board.rows[0].key.clone();
        // `;` lists the host's sessions and asks it to look again behind
        // the open picker, by the row's own key.
        assert_eq!(ch(&mut model, ';'), InputResult::Draw, "{host:?}");
        assert!(matches!(model.interaction, Interaction::Picker(_)));
        assert_eq!(
            std::mem::take(&mut model.background),
            vec![UiAction::DiscoverSessions {
                key: Some(row_key.clone()),
                target: SessionTarget::Harness,
            }]
        );
        // `d` closes the highlighted live session; it never reaches the
        // board, where it would ask to remove the worktree.
        let InputResult::Action(UiAction::StopSession { selection }) = ch(&mut model, 'd') else {
            panic!("{host:?}: d closes the live session")
        };
        assert_eq!(selection.key.as_deref(), Some(row_key.as_str()));
        assert_eq!(selection.live_slot.as_ref().unwrap().id, "$1");
        // Shift+F10 confirms the exact terminal its host listed.
        model.interaction = Interaction::None;
        let stop = KeyEvent::new(KeyCode::F(10), KeyModifiers::SHIFT);
        assert_eq!(model.input(stop, 20), InputResult::Draw, "{host:?}");
        let Interaction::Confirm(confirm) = &model.interaction else {
            panic!("{host:?}: the stop confirmation opens")
        };
        assert_eq!(
            confirm.action,
            ConfirmAction::StopTerminal {
                key: row_key,
                target: SessionTarget::Shell,
                session_id: "$7".into(),
                created_at: 42,
            }
        );
    }
}

#[test]
fn a_stale_stop_reopens_the_confirmation_from_the_newer_board() {
    let mut model = Model::default();
    model.apply(sessions(None));
    let stop = KeyEvent::new(KeyCode::F(10), KeyModifiers::SHIFT);
    model.input(stop, 20);
    let InputResult::Action(commit) = model.input(key(KeyCode::Enter), 20) else {
        panic!("the confirmation commits")
    };
    let generation = model.ui_generation;
    model.begin_action(5, &commit);
    // The shell was restarted meanwhile, and the board has caught up.
    let mut newer = sessions(None);
    newer.test_data_mut().terminals.get_mut("one").unwrap()[0].id = "$9".into();
    model.apply(newer);
    model.reply(UiReply {
        operation_id: Some(5),
        ui_generation: Some(generation),
        ..UiReply::stale("shell session changed since confirmation; open the stop dialog again")
    });
    let Interaction::Confirm(confirm) = &model.interaction else {
        panic!("a stale stop reopens its confirmation")
    };
    assert!(
        matches!(&confirm.action, ConfirmAction::StopTerminal { session_id, .. } if session_id == "$9"),
        "the reopened confirmation names the old terminal"
    );
}

/// Enter pressed twice in a row on a stop confirmation whose terminal was
/// replaced: the first commit is refused as stale and the confirmation
/// reopens naming the new terminal. The second Enter was typed for the old
/// one and must not stop a terminal the person has not read about.
#[tokio::test(start_paused = true)]
async fn a_key_on_its_way_cannot_commit_a_confirmation_reopened_after_a_refusal() {
    let mut model = Model::default();
    model.apply(sessions(None));
    let stop = KeyEvent::new(KeyCode::F(10), KeyModifiers::SHIFT);
    model.input(stop, 20);
    let InputResult::Action(commit) = model.input(key(KeyCode::Enter), 20) else {
        panic!("the confirmation commits")
    };
    let generation = model.ui_generation;
    model.begin_action(5, &commit);
    let mut newer = sessions(None);
    newer.test_data_mut().terminals.get_mut("one").unwrap()[0].id = "$9".into();
    model.apply(newer.clone());
    let refusal = "Changed since you looked: shell session changed since confirmation";
    model.reply(UiReply {
        operation_id: Some(5),
        ui_generation: Some(generation),
        ..UiReply::stale(refusal)
    });
    // It says what happened where the person is reading, and keeps saying
    // it when the board redraws the confirmation.
    let first_line = |model: &Model| match &model.interaction {
        Interaction::Confirm(confirm) => confirm.lines[0].clone(),
        _ => panic!("a stale stop reopens its confirmation"),
    };
    assert_eq!(first_line(&model), refusal);
    model.apply(newer);
    assert_eq!(first_line(&model), refusal);

    tokio::time::advance(std::time::Duration::from_millis(20)).await;
    for commit_key in [KeyCode::Enter, KeyCode::Char('y')] {
        let result = model.input(key(commit_key), 20);
        assert!(
            !matches!(result, InputResult::Action(_)),
            "{commit_key:?} typed for the old terminal stopped the new one: {result:?}"
        );
        assert!(matches!(model.interaction, Interaction::Confirm(_)));
    }
    assert!(model.pending_request.is_none());
    // Read and confirmed, it stops the terminal it names.
    tokio::time::advance(super::reply::CONFIRM_SETTLE).await;
    let InputResult::Action(UiAction::StopTerminal { session_id, .. }) =
        model.input(key(KeyCode::Enter), 20)
    else {
        panic!("a confirmation that was read commits")
    };
    assert_eq!(session_id, "$9");
}

/// A refusal whose words do not already say so is introduced by them.
#[tokio::test(start_paused = true)]
async fn a_reopened_confirmation_always_says_it_changed() {
    let mut model = Model::default();
    let mut shown = live(Board {
        rows: vec![BoardRow {
            pr: Some(PrPresentation {
                draft: true,
                ..failing()
            }),
            ..row("one")
        }],
        ..Board::default()
    });
    shown.test_data_mut().rows[0].pr.as_mut().unwrap().checks = CheckState::Pass;
    model.apply(shown);
    ch(&mut model, 'E');
    let InputResult::Action(commit) = model.input(key(KeyCode::Enter), 20) else {
        panic!("the ship confirmation commits")
    };
    let generation = model.ui_generation;
    model.begin_action(6, &commit);
    model.reply(UiReply {
        operation_id: Some(6),
        ui_generation: Some(generation),
        ..UiReply::stale("PR head moved")
    });
    let Interaction::Confirm(confirm) = &model.interaction else {
        panic!("a stale ship reopens its confirmation")
    };
    assert_eq!(confirm.lines[0], "Changed since you looked: PR head moved");
    assert!(!matches!(
        model.input(key(KeyCode::Enter), 20),
        InputResult::Action(_)
    ));
}

/// The reply to an overlay that another request replaced was already on
/// its way. It shares a generation with the request that replaced it, and
/// must still open nothing.
#[test]
fn a_replaced_overlays_late_reply_opens_nothing() {
    // Replaced by a request the model sent itself.
    let mut model = model();
    let generation = load_failed_checks(&mut model, 42);
    model.begin_action(43, &UiAction::CyclePrimary);
    model.reply(UiReply {
        operation_id: Some(42),
        ui_generation: Some(generation),
        modal: Some(log(&["late"], None)),
        ..Default::default()
    });
    assert!(
        matches!(model.interaction, Interaction::None),
        "the log nobody waits for opened"
    );
    // The request that replaced it is still answered at this generation.
    assert_eq!(
        model.pending_request.as_ref().unwrap().generation,
        generation
    );
    model.reply(UiReply {
        operation_id: Some(43),
        ui_generation: Some(generation),
        modal: Some(UiModal::Confirm {
            action: ConfirmAction::HardRefresh,
            title: "Its own dialog".into(),
            lines: Vec::new(),
            cancel_key: None,
        }),
        ..Default::default()
    });
    assert!(matches!(model.interaction, Interaction::Confirm(_)));

    // Replaced by a dialog another reply opened.
    let mut model = self::model();
    let generation = load_failed_checks(&mut model, 42);
    model.reply(UiReply {
        operation_id: Some(7),
        ui_generation: Some(generation),
        modal: Some(UiModal::Confirm {
            action: ConfirmAction::HardRefresh,
            title: "Something else".into(),
            lines: Vec::new(),
            cancel_key: None,
        }),
        ..Default::default()
    });
    model.reply(UiReply {
        operation_id: Some(42),
        ui_generation: Some(generation),
        modal: Some(log(&["late"], None)),
        ..Default::default()
    });
    assert!(
        matches!(&model.interaction, Interaction::Confirm(confirm) if confirm.title == "Something else"),
        "the late log replaced the dialog"
    );
    // Only that reply is marked: a later overlay fills as usual.
    model.interaction = Interaction::None;
    let generation = load_failed_checks(&mut model, 44);
    model.reply(UiReply {
        operation_id: Some(44),
        ui_generation: Some(generation),
        modal: Some(log(&["a"], None)),
        ..Default::default()
    });
    assert!(matches!(model.interaction, Interaction::Log { .. }));
}

/// Several overlays replaced in a row each have a reply on its way. None
/// of them may open.
#[test]
fn every_replaced_overlays_late_reply_opens_nothing() {
    let mut model = model();
    let mut replaced = Vec::new();
    for id in [42, 44, 46] {
        let generation = load_failed_checks(&mut model, id);
        // A request the model sent itself takes each overlay's place.
        model.begin_action(id + 1, &UiAction::CyclePrimary);
        assert!(matches!(model.interaction, Interaction::None));
        replaced.push((id, generation));
    }
    // The replies arrive oldest first, and in the other order.
    for order in [[0, 1, 2], [2, 1, 0]] {
        let mut model_after = Model::default();
        std::mem::swap(&mut model_after, &mut model);
        let abandoned = model_after.abandoned_overlays.clone();
        for index in order {
            let (id, generation) = replaced[index];
            model_after.reply(UiReply {
                operation_id: Some(id),
                ui_generation: Some(generation),
                modal: Some(log(&["late"], None)),
                ..Default::default()
            });
            assert!(
                matches!(model_after.interaction, Interaction::None),
                "the reply to replaced overlay {id} opened its log"
            );
        }
        assert!(model_after.abandoned_overlays.is_empty());
        model_after.abandoned_overlays = abandoned;
        model = model_after;
    }
    // The memory is bounded: an overlay replaced long ago is forgotten.
    for id in 100..120 {
        model.abandon_overlay(Some(id));
    }
    assert!(model.abandoned_overlays.len() <= 8);
    assert!(model.abandoned_overlays.contains(&119));
}

/// A removal review that arrived while the overlay was loading waited
/// behind it, and is shown when the overlay closes.
#[test]
fn closing_the_overlay_shows_a_removal_review_that_waited_behind_it() {
    let mut model = model();
    load_failed_checks(&mut model, 42);
    model.reply(UiReply {
        operation_id: Some(9),
        modal: Some(UiModal::Confirm {
            action: ConfirmAction::Remove {
                key: "two".into(),
                force: true,
                revision: Some(crate::RemovalRevision {
                    key: "two".into(),
                    path: "/worktrees/two".into(),
                    branch: "feature/two".into(),
                    head: "head".into(),
                    digest: "digest".into(),
                    hazards: vec!["Will discard 1 changed file".into()],
                    published_base: None,
                }),
                seen: None,
            },
            title: "Review two".into(),
            lines: vec!["two".into()],
            cancel_key: Some('d'),
        }),
        ..Default::default()
    });
    assert!(matches!(model.interaction, Interaction::Loading(_)));
    assert_eq!(model.input(key(KeyCode::Esc), 20), InputResult::Draw);
    assert!(
        matches!(&model.interaction, Interaction::Confirm(prompt) if prompt.title == "Review two"),
        "the review stayed hidden until some other key"
    );
    // It appeared without being asked for, so it settles before it commits.
    assert!(model.confirm_settles_at.is_some());
}

#[test]
fn a_new_worktree_on_a_remote_row_opens_its_prompt_without_asking_the_host() {
    let mut model = Model::default();
    let mut shown = sessions(Some("builder"));
    shown.test_data_mut().hosts = vec![
        HostChoice {
            id: None,
            label: "This machine".into(),
            sign_in_needed: Vec::new(),
        },
        HostChoice {
            id: Some("builder".into()),
            label: "Builder".into(),
            sign_in_needed: Vec::new(),
        },
    ];
    model.apply(shown);
    // Shift+N stacks on the selected row, on that row's host.
    let stack = KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT);
    assert_eq!(model.input(stack, 20), InputResult::Draw);
    let Interaction::Text(prompt) = &model.interaction else {
        panic!("the name prompt opens with the key")
    };
    assert!(matches!(prompt.action, TextAction::Create { .. }));
    assert_eq!(prompt.editor.text(), "--base feature/one");
    assert!(prompt.prompt.contains("Builder"), "{}", prompt.prompt);
    // Typing goes into the prompt, and the name is created on that host.
    for letter in " x".chars() {
        ch(&mut model, letter);
    }
    assert_eq!(
        model.input(key(KeyCode::Enter), 20),
        InputResult::Action(UiAction::OnHost {
            host: Some("builder".into()),
            action: Box::new(UiAction::Create {
                input: "--base feature/one x".into(),
                placement: model.creation_placement(),
            }),
        })
    );
}

#[test]
fn choosing_a_host_opens_its_name_prompt_at_once() {
    let mut model = Model::default();
    let mut shown = sessions(None);
    shown.test_data_mut().hosts = vec![
        HostChoice {
            id: None,
            label: "This machine".into(),
            sign_in_needed: Vec::new(),
        },
        HostChoice {
            id: Some("builder".into()),
            label: "Builder".into(),
            sign_in_needed: Vec::new(),
        },
    ];
    model.apply(shown);
    let create = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL);
    assert_eq!(model.input(create, 20), InputResult::Draw);
    assert!(matches!(model.interaction, Interaction::Picker(_)));
    ch(&mut model, 'j');
    assert_eq!(model.input(key(KeyCode::Enter), 20), InputResult::Draw);
    let Interaction::Text(prompt) = &model.interaction else {
        panic!("the chosen host's prompt opens with the key")
    };
    assert!(prompt.prompt.contains("Builder"), "{}", prompt.prompt);
    assert_eq!(model.interaction_host.as_deref(), Some("builder"));
    assert!(model.pending_request.is_none());
}

/// Key to first frame, at the model: how long the input call that opens
/// each menu takes, on a board of fifty rows. Before these menus were built
/// from the board the same key returned a request, and the first frame of
/// the menu waited for a host's reply.
#[test]
#[ignore = "timing; run with --ignored --nocapture"]
fn key_to_first_frame_of_each_menu() {
    let mut shown = sessions(Some("builder"));
    {
        let board = shown.test_data_mut();
        let remote = board.rows[0].clone();
        let remote_key = remote.key.clone();
        board.rows[0].pr = Some(PrPresentation {
            reviewers: vec!["maintainer".into()],
            ..failing()
        });
        board.host_display.get_mut("builder").unwrap().reviewers = true;
        board.host_reviewers.insert(
            "builder".into(),
            crate::ReviewerPool {
                viewer: "me".into(),
                contributors: (0..100)
                    .map(|index| crate::ContributorView {
                        login: format!("contributor-{index}"),
                        contributions: index,
                    })
                    .collect(),
                fetched_at_ms: 1,
            },
        );
        let entries = board.session_menus[&remote_key].clone();
        for index in 0..49 {
            let key = format!("row-{index}");
            board.rows.push(BoardRow {
                key: key.clone(),
                slug: key.clone(),
                host: None,
                ..remote.clone()
            });
            board.session_menus.insert(key.clone(), entries.clone());
            board.terminals.insert(key, Vec::new());
        }
        board.hosts = vec![
            HostChoice {
                id: None,
                label: "This machine".into(),
                sign_in_needed: Vec::new(),
            },
            HostChoice {
                id: Some("builder".into()),
                label: "Builder".into(),
                sign_in_needed: Vec::new(),
            },
        ];
    }
    let cases: [(&str, KeyEvent); 7] = [
        ("; sessions (remote row)", key(KeyCode::Char(';'))),
        (
            "Shift+F10 stop terminal (remote row)",
            KeyEvent::new(KeyCode::F(10), KeyModifiers::SHIFT),
        ),
        (
            "Shift+F12 harnesses (remote row)",
            KeyEvent::new(KeyCode::F(12), KeyModifiers::SHIFT),
        ),
        ("v reviewers (remote row)", key(KeyCode::Char('v'))),
        ("e mark ready", key(KeyCode::Char('e'))),
        (
            "Shift+N new worktree (remote row)",
            KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT),
        ),
        ("f failed checks (overlay opens)", key(KeyCode::Char('f'))),
    ];
    const RUNS: usize = 2_000;
    for (name, opener) in cases {
        let mut model = Model::default();
        model.apply(shown.clone());
        let mut samples = Vec::with_capacity(RUNS);
        for _ in 0..RUNS {
            let started = std::time::Instant::now();
            let result = model.input(opener, 20);
            samples.push(started.elapsed());
            assert!(
                !matches!(model.interaction, Interaction::None) || model.toast.is_some(),
                "{name}: nothing on screen after the key ({result:?})"
            );
            model.interaction = Interaction::None;
            model.interaction_host = None;
            model.local_menu = None;
            model.pending_request = None;
            model.toast = None;
            model.background.clear();
        }
        samples.sort();
        eprintln!(
            "KEY-TO-FRAME {name}: p50 {:?} p99 {:?}",
            samples[RUNS / 2],
            samples[RUNS * 99 / 100]
        );
    }
}

/// A row the remove menu confirms without asking a host.
fn removable(name: &str) -> BoardRow {
    BoardRow {
        path: format!("/worktrees/{name}"),
        git: crate::GitPresentation {
            head_sha: Some("head".into()),
            tracked_changes: Some(0),
            untracked_files: Some(0),
            ..Default::default()
        },
        ..row(name)
    }
}

fn ignored(model: &Model) -> bool {
    model
        .toast
        .as_ref()
        .is_some_and(|(text, _)| text.contains("key ignored"))
}

/// `v` on a row without a pull request answers with a notice, not the
/// picker. The `d` typed for the picker must not open the remove menu, and
/// the `y` after it must not confirm anything.
#[tokio::test(start_paused = true)]
async fn keys_typed_for_a_menu_that_did_not_open_never_reach_the_board() {
    let board = || {
        live(Board {
            rows: vec![removable("one"), removable("two")],
            display: DisplayPolicy {
                reviewers: true,
                ..Default::default()
            },
            ..Board::default()
        })
    };
    // Each of these keys opens a menu or prompt when it can.
    let openers = [
        key(KeyCode::Char('v')),
        key(KeyCode::Char('e')),
        key(KeyCode::Char('E')),
        key(KeyCode::Char('f')),
        key(KeyCode::Char(';')),
        KeyEvent::new(KeyCode::F(10), KeyModifiers::SHIFT),
        KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL),
        key(KeyCode::Char('Q')),
    ];
    for opener in openers {
        let mut model = Model::default();
        model.apply(board());
        assert_eq!(model.input(opener, 20), InputResult::Draw, "{opener:?}");
        assert!(matches!(model.interaction, Interaction::None));
        let said = model.toast.clone().unwrap().0;
        tokio::time::advance(std::time::Duration::from_millis(30)).await;
        // `q` typed at a menu that never opened must not quit wt.
        for typed in ['d', 'y', 'y', 'q'] {
            let result = ch(&mut model, typed);
            assert_eq!(result, InputResult::Draw, "{opener:?} then {typed}");
            assert!(
                matches!(model.interaction, Interaction::None) && model.yank.is_none(),
                "{opener:?} then {typed} acted on the board"
            );
        }
        assert_eq!(model.selected_row().unwrap().key, "one");
        // The notice stays readable beside the reason keys were ignored.
        let (text, _) = model.toast.clone().unwrap();
        assert!(
            text.starts_with(&said) && text.contains("key ignored"),
            "{text}"
        );
        // A key pressed once the person has seen that nothing opened is
        // their own.
        tokio::time::advance(super::reply::CONFIRM_SETTLE).await;
        assert_eq!(ch(&mut model, 'd'), InputResult::Draw);
        assert!(
            matches!(model.interaction, Interaction::Confirm(_)),
            "{opener:?}: a deliberate key was still ignored"
        );
    }
}

/// `d y y y`: the first `y` confirms the removal and the confirmation is
/// gone. The rest were typed at it. On the board they would open the yank
/// menu and copy the branch.
#[tokio::test(start_paused = true)]
async fn keys_typed_at_a_confirmation_that_just_closed_never_reach_the_board() {
    for (close, commits) in [('y', true), ('n', false), ('q', false)] {
        let mut model = Model::default();
        model.apply(live(Board {
            rows: vec![removable("one"), removable("two")],
            ..Board::default()
        }));
        assert_eq!(ch(&mut model, 'd'), InputResult::Draw);
        assert!(matches!(model.interaction, Interaction::Confirm(_)));
        let closed = ch(&mut model, close);
        assert_eq!(
            matches!(closed, InputResult::Action(UiAction::Remove { .. })),
            commits,
            "{close}: {closed:?}"
        );
        for typed in ['y', 'y', 'n', 'q'] {
            tokio::time::advance(std::time::Duration::from_millis(40)).await;
            let result = ch(&mut model, typed);
            assert_eq!(
                result,
                InputResult::Draw,
                "{close} then {typed}: {result:?}"
            );
            assert!(
                model.yank.is_none(),
                "{close} then {typed} opened the yank menu"
            );
            assert!(matches!(model.interaction, Interaction::None));
            assert!(ignored(&model));
        }
        assert_eq!(model.selected_row().unwrap().key, "one");
        // Esc opens and commits nothing, so it is never held back.
        assert_eq!(model.input(key(KeyCode::Esc), 20), InputResult::Draw);
        assert!(model.keys_settle.is_none());
        assert_eq!(ch(&mut model, 'j'), InputResult::Draw);
        assert_eq!(model.selected_row().unwrap().key, "two");
    }
}

/// Moving the cursor is never held: confirming and moving on is the
/// commonest fast flow, and a move does nothing a person could regret.
#[tokio::test(start_paused = true)]
async fn moving_and_scrolling_pass_through_the_settle_window() {
    let rows = || {
        live(Board {
            rows: ["one", "two", "three", "four"].map(removable).to_vec(),
            ..Board::default()
        })
    };
    // `d y j j j`: the removal is confirmed and the cursor is three rows on.
    let mut model = Model::default();
    model.apply(rows());
    ch(&mut model, 'd');
    assert!(matches!(
        ch(&mut model, 'y'),
        InputResult::Action(UiAction::Remove { .. })
    ));
    for _ in 0..3 {
        assert_eq!(ch(&mut model, 'j'), InputResult::Draw);
        assert!(!ignored(&model), "a move was reported as ignored");
    }
    assert_eq!(model.selected_row().unwrap().key, "four");
    // The window is still open for keys that act.
    assert_eq!(ch(&mut model, 'y'), InputResult::Draw);
    assert!(model.yank.is_none() && ignored(&model));
    assert_ne!(ch(&mut model, 'q'), InputResult::Quit);

    // Every key that only moves, after a menu key that opened nothing.
    let moves = [
        (key(KeyCode::Char('j')), "two"),
        (key(KeyCode::Down), "three"),
        (key(KeyCode::Char('k')), "two"),
        (key(KeyCode::Up), "one"),
        (key(KeyCode::Char('G')), "four"),
        (key(KeyCode::Char('g')), "one"),
        (key(KeyCode::End), "four"),
        (key(KeyCode::Home), "one"),
        (key(KeyCode::PageDown), "three"),
        (key(KeyCode::PageUp), "one"),
    ];
    let mut model = Model::default();
    model.apply(rows());
    // No pull request is shown, so `e` answers with a notice.
    ch(&mut model, 'e');
    assert!(model.keys_settle.is_some());
    for (moved, lands_on) in moves {
        model.input(moved, 4);
        assert_eq!(model.selected_row().unwrap().key, lands_on, "{moved:?}");
        assert!(!ignored(&model), "{moved:?}");
    }
    // Keys that act are still held, and the window has not been spent.
    for acts in [
        key(KeyCode::Char('d')),
        key(KeyCode::Tab),
        key(KeyCode::Char('J')),
        key(KeyCode::Char(' ')),
        key(KeyCode::Enter),
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
    ] {
        let result = model.input(acts, 20);
        assert_eq!(result, InputResult::Draw, "{acts:?} acted: {result:?}");
        assert!(matches!(model.interaction, Interaction::None));
    }
    assert!(ignored(&model));
    assert_eq!(model.selected_row().unwrap().key, "one");
}

/// A removal review that opens as another closes has its own rule: only
/// the keys that commit it wait. Cancelling and scrolling work at once.
#[tokio::test(start_paused = true)]
async fn a_review_that_follows_a_cancelled_one_takes_cancel_and_scroll_at_once() {
    let review = |name: &str| UiReply {
        modal: Some(UiModal::Confirm {
            action: ConfirmAction::Remove {
                key: name.into(),
                force: true,
                revision: Some(crate::RemovalRevision {
                    key: name.into(),
                    path: format!("/worktrees/{name}"),
                    branch: format!("feature/{name}"),
                    head: "head".into(),
                    digest: name.into(),
                    hazards: vec!["Will discard 1 changed file".into()],
                    published_base: None,
                }),
                seen: None,
            },
            title: format!("Review {name}"),
            lines: vec![name.into(), "a".into(), "b".into()],
            cancel_key: Some('d'),
        }),
        ..Default::default()
    };
    let title = |model: &Model| match &model.interaction {
        Interaction::Confirm(confirm) => Some((confirm.title.clone(), confirm.selected)),
        _ => None,
    };
    let mut model = Model::default();
    model.apply(live(Board {
        rows: vec![removable("one"), removable("two"), removable("three")],
        ..Board::default()
    }));
    for name in ["one", "two", "three"] {
        model.reply(review(name));
    }
    assert_eq!(title(&model), Some(("Review one".into(), 0)));
    // Cancel the first: the second is on screen at once.
    ch(&mut model, 'n');
    assert_eq!(title(&model), Some(("Review two".into(), 0)));
    tokio::time::advance(std::time::Duration::from_millis(20)).await;
    // It scrolls at once, its commit keys wait, and it cancels at once.
    ch(&mut model, 'j');
    assert_eq!(title(&model), Some(("Review two".into(), 1)));
    assert!(!ignored(&model));
    assert!(!matches!(ch(&mut model, 'y'), InputResult::Action(_)));
    assert_eq!(title(&model).unwrap().0, "Review two");
    ch(&mut model, 'n');
    assert_eq!(title(&model), Some(("Review three".into(), 0)));
}

/// Keys that open nothing by design are not held: moving on after a plain
/// notice, or inside a menu that stayed open, is immediate.
#[tokio::test(start_paused = true)]
async fn only_a_missing_menu_or_a_closed_confirmation_holds_keys() {
    let mut model = Model::default();
    model.apply(live(Board {
        rows: vec![removable("one"), removable("two")],
        ..Board::default()
    }));
    // Space finds nothing needing attention; `p` has no pull request to open.
    for plain in [' ', 'p'] {
        ch(&mut model, plain);
        assert!(model.keys_settle.is_none(), "{plain:?} held the next key");
    }
    assert_eq!(ch(&mut model, 'j'), InputResult::Draw);
    assert_eq!(model.selected_row().unwrap().key, "two");
    // A confirmation that is still open keeps its keys.
    ch(&mut model, 'd');
    assert_eq!(ch(&mut model, 'j'), InputResult::Draw);
    assert!(matches!(model.interaction, Interaction::Confirm(_)));
    assert!(model.keys_settle.is_none());
    // A pointer acts where it points, whatever was typed before.
    ch(&mut model, 'n');
    assert!(model.keys_settle.is_some());
    model.pointer_input(key(KeyCode::Char('k')), 20);
    assert_eq!(model.selected_row().unwrap().key, "one");
}

/// With GitHub reads switched off no pull request will ever load. The keys
/// say that, and ask for no refresh that cannot help.
#[test]
fn with_github_off_the_pr_keys_and_the_palette_say_so() {
    let mut shown = live(Board {
        rows: vec![row("one")],
        display: DisplayPolicy {
            reviewers: true,
            github_off: true,
            actions: crate::menus::palette::tests::catalog(),
            ..Default::default()
        },
        ..Board::default()
    });
    shown.test_data_mut().rows[0].action_facts.tracker_id = true;
    let off = crate::menus::github::GITHUB_OFF;
    for letter in ['e', 'E', 'v', 'f'] {
        let mut model = Model::default();
        model.apply(shown.clone());
        assert_eq!(ch(&mut model, letter), InputResult::Draw);
        assert_eq!(
            model.toast,
            Some((off.into(), ToastTone::Notice)),
            "{letter}"
        );
        assert!(!model.refresh_requested, "{letter} asked for a read");
        assert!(model.pending_request.is_none());
    }
    // The palette gives one reason for the one fact.
    let mut model = Model::default();
    model.apply(shown);
    ch(&mut model, '!');
    let Interaction::Picker(picker) = &model.interaction else {
        panic!("the palette opens")
    };
    let detail = |id: &str| {
        picker
            .options
            .iter()
            .find(|option| option.value.as_deref() == Some(id))
            .and_then(|option| option.detail.clone())
            .unwrap()
    };
    let said = format!("({off})");
    assert_eq!(detail("rerun-review"), said);
    assert_eq!(detail("ask-review"), said);
    assert_eq!(detail(crate::AUTO_MERGE_ACTION_ID), said);
    // Choosing one neither waits for GitHub nor asks it anything.
    let index = picker
        .options
        .iter()
        .position(|option| option.value.as_deref() == Some("rerun-review"))
        .unwrap();
    for _ in 0..index {
        ch(&mut model, 'j');
    }
    model.refresh_requested = false;
    assert_eq!(model.input(key(KeyCode::Enter), 20), InputResult::Draw);
    assert!(matches!(model.interaction, Interaction::Picker(_)));
    assert!(
        model.palette_wait.is_none(),
        "it waits for a read that never comes"
    );
    assert!(!model.refresh_requested);
    assert!(model.toast.as_ref().unwrap().0.ends_with(off));
}

/// The stop confirmation names one run. When that run ends while the
/// confirmation is open it says so, and Enter closes it without asking a
/// host to stop anything.
#[test]
fn a_stop_confirmation_says_when_its_run_has_finished() {
    let running = |active: bool| {
        let mut shown = live(Board {
            rows: vec![row("one")],
            display: DisplayPolicy {
                actions: crate::menus::palette::tests::catalog(),
                ..Default::default()
            },
            ..Board::default()
        });
        shown.test_data_mut().rows[0].action_facts.active_run =
            active.then(|| crate::ActiveRunView {
                action_key: "one".into(),
                run_id: "run".into(),
                action_name: "check".into(),
            });
        shown
    };
    let mut model = Model::default();
    model.apply(running(true));
    ch(&mut model, '!');
    let text = |model: &Model| match &model.interaction {
        Interaction::Confirm(confirm) => (confirm.title.clone(), confirm.lines.clone()),
        _ => panic!("the stop confirmation is open"),
    };
    assert_eq!(text(&model).0, "Stop check?");
    assert!(text(&model).1[0].contains("is active"));
    // Still running on the next board: nothing changes.
    model.apply(running(true));
    assert_eq!(text(&model).0, "Stop check?");
    model.apply(running(false));
    let (title, lines) = text(&model);
    assert_eq!(title, "check has finished");
    assert!(!lines.concat().contains("is active"), "{lines:?}");
    // It stops nothing any more, and never names another run.
    assert!(matches!(
        &model.interaction,
        Interaction::Confirm(ConfirmPrompt {
            action: ConfirmAction::Finished,
            ..
        })
    ));
    model.confirm_settles_at = None;
    let result = model.input(key(KeyCode::Enter), 20);
    assert_eq!(
        result,
        InputResult::Draw,
        "a finished run was asked to stop"
    );
    assert!(matches!(model.interaction, Interaction::None));
}

fn removes(result: &InputResult) -> Option<&str> {
    match result {
        InputResult::Action(UiAction::Remove { key, .. }) => Some(key),
        _ => None,
    }
}

/// After a confirmation closes only a second tap of its own answer keys is
/// absorbed. The next move, another menu key included, is the person's.
#[tokio::test(start_paused = true)]
async fn after_a_confirmation_closes_everything_but_its_answer_keys_passes() {
    let rows = || {
        let names: Vec<String> = (0..20).map(|index| format!("row-{index:02}")).collect();
        live(Board {
            rows: names.iter().map(|name| removable(name)).collect(),
            ..Board::default()
        })
    };
    let selected = |model: &Model| model.selected_row().unwrap().key.clone();
    // `d y j d y` in one burst removes two rows.
    let mut model = Model::default();
    model.apply(rows());
    ch(&mut model, 'd');
    assert_eq!(removes(&ch(&mut model, 'y')), Some("row-00"));
    ch(&mut model, 'j');
    assert_eq!(ch(&mut model, 'd'), InputResult::Draw);
    assert!(
        matches!(model.interaction, Interaction::Confirm(_)),
        "the second d was swallowed"
    );
    // The confirmation the person just opened answers at once.
    assert!(model.keys_settle.is_none());
    assert_eq!(removes(&ch(&mut model, 'y')), Some("row-01"));

    // `d y` then a held `j` loses no press; `d n j j j` moves three rows.
    for (answer, presses) in [('y', 15), ('n', 3)] {
        let mut model = Model::default();
        model.apply(rows());
        ch(&mut model, 'd');
        ch(&mut model, answer);
        for _ in 0..presses {
            ch(&mut model, 'j');
        }
        assert_eq!(selected(&model), format!("row-{presses:02}"), "d {answer}");
        assert!(!ignored(&model));
    }

    // `d y y y`: the taps after the first open no yank menu and remove
    // nothing more.
    let mut model = Model::default();
    model.apply(rows());
    ch(&mut model, 'd');
    assert!(removes(&ch(&mut model, 'y')).is_some());
    for _ in 0..2 {
        let result = ch(&mut model, 'y');
        assert_eq!(result, InputResult::Draw, "{result:?}");
        assert!(model.yank.is_none() && ignored(&model));
    }
    // Once the window has passed, `y` is the yank menu again.
    tokio::time::advance(super::reply::CONFIRM_SETTLE).await;
    ch(&mut model, 'y');
    assert!(model.yank.is_some());
}

/// After a menu key that opened nothing, keys that act are held and keys
/// that move are not.
#[tokio::test(start_paused = true)]
async fn after_a_menu_that_did_not_open_only_moves_pass() {
    let board = || {
        live(Board {
            rows: ["one", "two", "three", "four"].map(removable).to_vec(),
            display: DisplayPolicy {
                reviewers: true,
                ..Default::default()
            },
            ..Board::default()
        })
    };
    // `v d` on a row without a pull request: no remove menu.
    let mut model = Model::default();
    model.apply(board());
    ch(&mut model, 'v');
    assert_eq!(ch(&mut model, 'd'), InputResult::Draw);
    assert!(matches!(model.interaction, Interaction::None) && ignored(&model));
    // `v j j j` moves three rows.
    let mut model = Model::default();
    model.apply(board());
    ch(&mut model, 'v');
    for _ in 0..3 {
        ch(&mut model, 'j');
    }
    assert_eq!(model.selected_row().unwrap().key, "four");
    assert!(!ignored(&model));
    // `e q` does not quit, and neither does Ctrl+C.
    let mut model = Model::default();
    model.apply(board());
    ch(&mut model, 'e');
    assert_eq!(ch(&mut model, 'q'), InputResult::Draw);
    let interrupt = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert_eq!(model.input(interrupt, 20), InputResult::Draw);
    // Esc is never held, under either rule, and ends the window.
    assert_eq!(model.input(key(KeyCode::Esc), 20), InputResult::Draw);
    assert!(!ignored(&model) || model.keys_settle.is_none());
    assert!(model.keys_settle.is_none());
    assert_eq!(ch(&mut model, 'q'), InputResult::Quit);
}

/// A stale refusal usually arrives before the read it asked for, so the
/// reopened confirmation is built from the board that was just refused.
/// Committing it again could only be refused again: it waits for the newer
/// board, says so, and is rebuilt from it.
#[tokio::test(start_paused = true)]
async fn a_reopened_confirmation_waits_for_a_board_newer_than_the_one_refused() {
    let stop = KeyEvent::new(KeyCode::F(10), KeyModifiers::SHIFT);
    let refusal = "Changed since you looked: shell session changed since confirmation";
    let refuse = |model: &mut Model, id: u64| {
        let InputResult::Action(commit) = model.input(key(KeyCode::Enter), 20) else {
            panic!("the confirmation commits")
        };
        let generation = model.ui_generation;
        model.begin_action(id, &commit);
        model.reply(UiReply {
            operation_id: Some(id),
            ui_generation: Some(generation),
            ..UiReply::stale(refusal)
        });
    };
    let confirm = |model: &Model| match &model.interaction {
        Interaction::Confirm(confirm) => (confirm.lines.clone(), confirm.action.clone()),
        _ => panic!("the confirmation is open"),
    };
    let stops = |action: &ConfirmAction, id: &str| matches!(action, ConfirmAction::StopTerminal { session_id, .. } if session_id == id);
    let mut model = Model::default();
    model.apply(sessions(None));
    model.input(stop, 20);
    refuse(&mut model, 5);
    // Still the terminal that was refused, and it says it is reading.
    let (lines, action) = confirm(&model);
    assert!(stops(&action, "$7"));
    assert_eq!(lines[0], refusal);
    assert_eq!(lines[1], STILL_READING_LINE);
    // A deliberate Enter a second later is not sent to be refused again.
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    assert_eq!(model.input(key(KeyCode::Enter), 20), InputResult::Draw);
    assert!(model.pending_request.is_none());
    assert!(model.toast.as_ref().unwrap().0.contains("still reading"));
    // Cancelling and scrolling work meanwhile.
    assert_eq!(ch(&mut model, 'j'), InputResult::Draw);
    // The newer board arrives: the confirmation now names what is there,
    // keeps saying that it changed, and settles before it commits.
    let mut newer = sessions(None);
    newer.test_data_mut().terminals.get_mut("one").unwrap()[0].id = "$9".into();
    model.apply(newer);
    let (lines, action) = confirm(&model);
    assert!(stops(&action, "$9"), "{action:?}");
    assert_eq!(lines[0], refusal);
    assert!(!lines.iter().any(|line| line == STILL_READING_LINE));
    assert!(!matches!(
        model.input(key(KeyCode::Enter), 20),
        InputResult::Action(_)
    ));
    tokio::time::advance(super::reply::CONFIRM_SETTLE).await;
    assert!(matches!(
        model.input(key(KeyCode::Enter), 20),
        InputResult::Action(UiAction::StopTerminal { session_id, .. }) if session_id == "$9"
    ));

    // A board that does not change is as new as it gets: after the wait
    // the confirmation takes its answer, and its host checks it.
    let mut model = Model::default();
    model.apply(sessions(None));
    model.input(stop, 20);
    refuse(&mut model, 6);
    assert!(model.next_deadline().is_some());
    tokio::time::advance(std::time::Duration::from_secs(6)).await;
    model.tick(tokio::time::Instant::now());
    let (lines, _) = confirm(&model);
    assert!(!lines.iter().any(|line| line == STILL_READING_LINE));
    assert!(matches!(
        model.input(key(KeyCode::Enter), 20),
        InputResult::Action(UiAction::StopTerminal { session_id, .. }) if session_id == "$7"
    ));

    // A terminal that is gone from the newer board closes the confirmation.
    let mut model = Model::default();
    model.apply(sessions(None));
    model.input(stop, 20);
    refuse(&mut model, 7);
    let mut gone = sessions(None);
    gone.test_data_mut()
        .terminals
        .get_mut("one")
        .unwrap()
        .clear();
    model.apply(gone);
    assert!(matches!(model.interaction, Interaction::None));
    assert_eq!(
        model.toast.as_ref().unwrap().0,
        "No shell session is running"
    );
}

const STILL_READING_LINE: &str =
    "Reading what changed. This updates, and takes Enter, in a moment.";
