//! Palettes are menus of the board: they open with their key, an entry
//! commits or prompts without a host preparation, and what the board cannot
//! offer yet is said, waited for in the open palette, or refused as stale.

use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent};
use wt_runtime::{SourceSnapshot, SourceState};

use super::{InputResult, Interaction, Model, ToastTone};
use crate::menus::palette::tests::board;
use crate::{
    AUTO_MERGE_ACTION_ID, ActionFacts, ActionRowSeen, ActionSeen, ActionSurface, ActiveRunView,
    ArgValue, Board, BoardRow, CUSTOM_ACTION_ID, ConfirmAction, DEV_LOGS_ACTION_ID,
    OPEN_EDITOR_ACTION_ID, PickerAction, PrFact, PrPresentation, Seen, SessionTarget, TextAction,
    UiAction, UiReply,
};

fn live(board: Board) -> SourceSnapshot<Board> {
    SourceSnapshot::test_live(Some(board), SourceState::Ready, wt_core::Domain::Git)
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::from(code)
}

fn start(board: Board) -> Model {
    let mut model = Model::default();
    model.apply(live(board));
    model
}

fn row() -> ActionSurface {
    ActionSurface::Row { key: "one".into() }
}

fn seen(pr: Option<u64>) -> Option<Seen<ActionSeen>> {
    Some(Seen(ActionSeen {
        row: Some(ActionRowSeen {
            path: "/work/one".into(),
            branch: "feature/one".into(),
        }),
        pr,
    }))
}

/// A board whose one row has every fact its actions ask for.
fn ready() -> Board {
    let mut board = board();
    board.rows[0].action_facts = ActionFacts {
        pr: PrFact::Present {
            open: true,
            draft: false,
        },
        deployed: true,
        tracker_id: true,
        active_run: None,
    };
    board.rows[0].pr = Some(PrPresentation {
        number: Some(7),
        head_sha: Some("abc".into()),
        ..Default::default()
    });
    board
}

/// Move the open palette's cursor to `id` and confirm it.
fn pick(model: &mut Model, id: &str) -> InputResult {
    let Interaction::Picker(picker) = &mut model.interaction else {
        panic!("no palette is open")
    };
    picker.selected = picker
        .options
        .iter()
        .position(|option| option.value.as_deref() == Some(id))
        .unwrap_or_else(|| panic!("{id} is not listed"));
    model.input(key(KeyCode::Enter), 20)
}

fn detail(model: &Model, id: &str) -> String {
    let Interaction::Picker(picker) = &model.interaction else {
        panic!("no palette is open")
    };
    picker
        .options
        .iter()
        .find(|option| option.value.as_deref() == Some(id))
        .and_then(|option| option.detail.clone())
        .unwrap_or_else(|| panic!("{id} is not listed"))
}

fn open_palette(model: &Model) -> Option<&ActionSurface> {
    match &model.interaction {
        Interaction::Picker(picker) => match &picker.action {
            PickerAction::Actions { surface } => Some(surface),
            _ => None,
        },
        _ => None,
    }
}

fn type_text(model: &mut Model, text: &str) {
    for ch in text.chars() {
        model.input(key(KeyCode::Char(ch)), 20);
    }
}

#[test]
fn every_palette_key_opens_its_menu_in_the_same_input_without_a_request() {
    for (code, surface) in [
        ('!', row()),
        (
            'M',
            ActionSurface::Manager {
                key: Some("one".into()),
            },
        ),
        (
            '<',
            ActionSurface::Slot {
                target: SessionTarget::WtSource,
            },
        ),
        (
            '>',
            ActionSurface::Slot {
                target: SessionTarget::Main,
            },
        ),
        (
            '\\',
            ActionSurface::Slot {
                target: SessionTarget::Dotfiles,
            },
        ),
    ] {
        let mut model = start(board());
        assert_eq!(
            model.input(key(KeyCode::Char(code)), 20),
            InputResult::Draw,
            "{code} asked the host"
        );
        assert_eq!(open_palette(&model), Some(&surface));
        assert!(model.pending_request.is_none());
        assert!(model.background.is_empty());
        assert!(!model.refresh_requested);
    }
}

#[test]
fn a_shell_entry_commits_at_once_with_what_the_palette_showed() {
    let mut model = start(ready());
    model.input(key(KeyCode::Char('!')), 20);
    assert_eq!(
        pick(&mut model, "teardown"),
        InputResult::Action(UiAction::RunAction {
            surface: row(),
            id: "teardown".into(),
            arg: None,
            extras: String::new(),
            seen: seen(None),
        })
    );
    assert!(matches!(model.interaction, Interaction::None));
    // An action that needs a pull request names the one it was shown.
    model.input(key(KeyCode::Char('!')), 20);
    let InputResult::Action(UiAction::RunAction {
        id, seen: shown, ..
    }) = pick(&mut model, "rerun-review")
    else {
        panic!("the entry did not commit")
    };
    assert_eq!((id.as_str(), shown), ("rerun-review", seen(Some(7))));
}

#[test]
fn a_prompt_entry_opens_its_instructions_here_and_then_commits() {
    let mut model = start(ready());
    model.input(key(KeyCode::Char('!')), 20);
    // The chord works like the cursor: no request for the prompt.
    assert_eq!(model.input(key(KeyCode::Char('g')), 20), InputResult::Draw);
    let Interaction::Text(prompt) = &model.interaction else {
        panic!("expected the instructions prompt")
    };
    assert_eq!(
        prompt.action,
        TextAction::ActionExtras {
            surface: row(),
            id: "agent-continue".into(),
            arg: None,
            seen: seen(None),
        }
    );
    assert_eq!(
        prompt.prompt,
        "Agent: continue work · extra instructions (optional)"
    );
    assert!(prompt.allow_empty);
    assert!(model.pending_request.is_none());
    type_text(&mut model, "be brief");
    assert_eq!(
        model.input(key(KeyCode::Enter), 20),
        InputResult::Action(UiAction::RunAction {
            surface: row(),
            id: "agent-continue".into(),
            arg: None,
            extras: "be brief".into(),
            seen: seen(None),
        })
    );

    // The custom prompt has no default to send.
    model.input(key(KeyCode::Char('!')), 20);
    assert_eq!(model.input(key(KeyCode::Char('c')), 20), InputResult::Draw);
    let Interaction::Text(prompt) = &model.interaction else {
        panic!("expected the custom prompt")
    };
    assert_eq!(prompt.prompt, "Prompt");
    assert_eq!(model.input(key(KeyCode::Enter), 20), InputResult::Unchanged);
    type_text(&mut model, "x");
    assert!(matches!(
        model.input(key(KeyCode::Enter), 20),
        InputResult::Action(UiAction::RunAction { ref id, ref extras, .. })
            if id == CUSTOM_ACTION_ID && extras == "x"
    ));
}

#[test]
fn an_entry_that_takes_a_value_asks_for_it_from_the_boards_recent_values() {
    // No history: a text prompt.
    let mut model = start(ready());
    model.input(key(KeyCode::Char('!')), 20);
    assert_eq!(pick(&mut model, "ask-review"), InputResult::Draw);
    let Interaction::Text(prompt) = &model.interaction else {
        panic!("expected the value prompt")
    };
    assert_eq!(prompt.prompt, "Reviewer");
    assert_eq!(
        prompt.action,
        TextAction::ActionArg {
            surface: row(),
            id: "ask-review".into(),
            seen: seen(Some(7)),
        }
    );
    type_text(&mut model, "bob");
    assert_eq!(model.input(key(KeyCode::Enter), 20), InputResult::Draw);
    assert!(matches!(
        &model.interaction,
        Interaction::Text(prompt) if prompt.action == TextAction::ActionExtras {
            surface: row(),
            id: "ask-review".into(),
            arg: Some("bob".into()),
            seen: seen(Some(7)),
        }
    ));

    // With history: a picker of the values, which is sent back unchanged.
    let mut board = ready();
    board.actions.recent_args.insert(
        "ask-review".into(),
        vec![ArgValue {
            value: "alice".into(),
            shown: "alice".into(),
            label: Some("Alice".into()),
        }],
    );
    let mut model = start(board);
    model.input(key(KeyCode::Char('!')), 20);
    assert_eq!(pick(&mut model, "ask-review"), InputResult::Draw);
    let Interaction::Picker(picker) = &model.interaction else {
        panic!("expected the value picker")
    };
    assert_eq!(picker.title, "Reviewer");
    let labels: Vec<_> = picker.options.iter().map(|o| o.label.as_str()).collect();
    assert_eq!(labels, ["Enter a new value…", "Alice  alice"]);
    model.input(key(KeyCode::Down), 20);
    assert_eq!(model.input(key(KeyCode::Enter), 20), InputResult::Draw);
    assert_eq!(
        model.input(key(KeyCode::Enter), 20),
        InputResult::Action(UiAction::RunAction {
            surface: row(),
            id: "ask-review".into(),
            arg: Some("alice".into()),
            extras: String::new(),
            seen: seen(Some(7)),
        })
    );
}

#[test]
fn an_unavailable_entry_says_why_and_asks_the_sources_again() {
    let mut board = ready();
    board.rows[0].action_facts.deployed = false;
    let mut model = start(board);
    model.input(key(KeyCode::Char('!')), 20);
    assert_eq!(pick(&mut model, "teardown"), InputResult::Draw);
    assert_eq!(
        model.toast,
        Some((
            "Tear down stage: no stage deployed".into(),
            ToastTone::Notice
        ))
    );
    assert_eq!(open_palette(&model), Some(&row()), "the palette stays open");
    assert!(model.refresh_requested);
    assert!(model.follow_up.is_none() && model.pending_request.is_none());
}

#[test]
fn an_entry_waits_in_its_palette_for_github_and_runs_once_the_board_confirms() {
    let mut unconfirmed = ready();
    unconfirmed.rows[0].action_facts.pr = PrFact::Unknown;
    let mut model = start(unconfirmed.clone());
    model.input(key(KeyCode::Char('!')), 20);
    assert_eq!(
        detail(&model, "rerun-review"),
        "(GitHub data is not confirmed)"
    );
    assert_eq!(pick(&mut model, "rerun-review"), InputResult::Draw);
    // Nothing is sent: the wait is the board's, shown where it was chosen.
    assert!(model.follow_up.is_none() && model.pending_request.is_none());
    assert!(model.refresh_requested, "GitHub is asked to read again");
    assert_eq!(
        detail(&model, "rerun-review"),
        "waiting for GitHub… Esc cancels"
    );
    assert_eq!(
        model.toast.as_ref().map(|(text, _)| text.as_str()),
        Some("Re-run review: waiting for GitHub to confirm feature/one… Esc cancels")
    );
    // A board that still has no read keeps waiting, and says so.
    model.apply(live(unconfirmed.clone()));
    assert_eq!(
        detail(&model, "rerun-review"),
        "waiting for GitHub… Esc cancels"
    );
    assert!(model.follow_up.is_none());

    model.apply(live(ready()));
    assert_eq!(
        model.follow_up,
        Some(UiAction::RunAction {
            surface: row(),
            id: "rerun-review".into(),
            arg: None,
            extras: String::new(),
            seen: seen(Some(7)),
        })
    );
    assert!(matches!(model.interaction, Interaction::None));
    assert_eq!(
        model.committed_menu,
        Some(UiAction::PrepareActions { surface: row() }),
        "a stale answer reopens the palette"
    );

    // The read can also say the action cannot run.
    let mut model = super::Model::default();
    model.apply(live(unconfirmed));
    model.input(key(KeyCode::Char('!')), 20);
    pick(&mut model, "rerun-review");
    let mut absent = ready();
    absent.rows[0].action_facts.pr = PrFact::Absent;
    absent.rows[0].pr = None;
    model.apply(live(absent.clone()));
    assert!(model.follow_up.is_none());
    assert_eq!(
        model.toast,
        Some(("Re-run review: no PR".into(), ToastTone::Notice))
    );
    assert_eq!(detail(&model, "rerun-review"), "(no PR)");
    // That answer ended the wait: a later read does not run it after all.
    model.apply(live(ready()));
    assert!(model.follow_up.is_none());
}

#[tokio::test(start_paused = true)]
async fn the_github_wait_ends_when_its_palette_closes_or_its_time_is_up() {
    let mut unconfirmed = ready();
    unconfirmed.rows[0].action_facts.pr = PrFact::Unknown;

    let mut model = start(unconfirmed.clone());
    model.input(key(KeyCode::Char('!')), 20);
    pick(&mut model, "rerun-review");
    model.input(key(KeyCode::Esc), 20);
    // Reopened before the read arrives: the earlier choice is gone.
    model.input(key(KeyCode::Char('!')), 20);
    model.apply(live(ready()));
    assert!(model.follow_up.is_none(), "a cancelled wait ran its action");
    assert_eq!(detail(&model, "rerun-review"), "$ rerun-review");

    let mut model = super::Model::default();
    model.apply(live(unconfirmed.clone()));
    model.input(key(KeyCode::Char('!')), 20);
    pick(&mut model, "rerun-review");
    // Choosing something else replaces the wait.
    pick(&mut model, "teardown");
    model.apply(live(ready()));
    assert!(model.follow_up.is_none());

    let mut model = super::Model::default();
    model.apply(live(unconfirmed));
    model.input(key(KeyCode::Char('!')), 20);
    pick(&mut model, "rerun-review");
    let deadline = model.next_deadline().expect("the wait has a deadline");
    tokio::time::advance(Duration::from_secs(11)).await;
    model.tick(tokio::time::Instant::now());
    assert_eq!(
        detail(&model, "rerun-review"),
        "waiting for GitHub… Esc cancels"
    );
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(deadline <= tokio::time::Instant::now());
    model.tick(tokio::time::Instant::now());
    assert_eq!(
        model.toast,
        Some((
            "Re-run review: GitHub data is not confirmed".into(),
            ToastTone::Notice
        ))
    );
    assert_eq!(
        detail(&model, "rerun-review"),
        "(GitHub data is not confirmed)"
    );
    assert!(model.next_deadline().is_none());
    model.apply(live(ready()));
    assert!(model.follow_up.is_none(), "an expired wait ran its action");
}

#[test]
fn a_commit_the_host_finds_stale_reopens_the_palette_from_the_newer_board() {
    let mut model = start(ready());
    model.input(key(KeyCode::Char('!')), 20);
    let InputResult::Action(action) = pick(&mut model, "teardown") else {
        panic!("the entry did not commit")
    };
    model.begin_action(7, &action);
    // The board catches up with what the host already knew.
    let mut newer = ready();
    newer.rows[0].action_facts.deployed = false;
    model.apply(live(newer));
    model.reply(UiReply {
        operation_id: Some(7),
        message: "Tear down stage: no stage deployed".into(),
        failed: true,
        stale: true,
        ..Default::default()
    });
    assert_eq!(open_palette(&model), Some(&row()));
    assert_eq!(detail(&model, "teardown"), "(no stage deployed)");
    assert_eq!(
        model.toast,
        Some((
            "Tear down stage: no stage deployed".into(),
            ToastTone::Failed
        ))
    );
    // The same holds for a commit sent from the instructions prompt.
    let mut model = super::Model::default();
    model.apply(live(ready()));
    model.input(key(KeyCode::Char('!')), 20);
    model.input(key(KeyCode::Char('g')), 20);
    let InputResult::Action(action) = model.input(key(KeyCode::Enter), 20) else {
        panic!("the prompt did not commit")
    };
    model.begin_action(8, &action);
    model.reply(UiReply {
        operation_id: Some(8),
        message: "one is not the checkout the palette showed".into(),
        failed: true,
        stale: true,
        ..Default::default()
    });
    assert_eq!(open_palette(&model), Some(&row()));
}

#[test]
fn stopping_an_action_names_the_run_the_board_showed_and_settles_first() {
    let mut board = ready();
    board.rows[0].action_facts.active_run = Some(ActiveRunView {
        action_key: "one".into(),
        run_id: "run-9".into(),
        action_name: "Lint".into(),
    });
    let mut model = start(board);
    assert_eq!(model.input(key(KeyCode::Char('!')), 20), InputResult::Draw);
    let Interaction::Confirm(confirm) = &model.interaction else {
        panic!("expected the stop confirmation")
    };
    assert_eq!(confirm.title, "Stop Lint?");
    // The key asked for a list. Enter typed for its first entry must not
    // stop the action that turned out to be running.
    assert!(!matches!(
        model.input(key(KeyCode::Enter), 20),
        InputResult::Action(_)
    ));
    assert!(matches!(model.interaction, Interaction::Confirm(_)));
    model.confirm_settles_at = None;
    assert_eq!(
        model.input(key(KeyCode::Enter), 20),
        InputResult::Action(UiAction::KillAction {
            action_key: "one".into(),
            run_id: "run-9".into(),
        })
    );
    assert!(matches!(
        ConfirmAction::KillAction {
            action_key: "one".into(),
            run_id: "run-9".into(),
        },
        ConfirmAction::KillAction { .. }
    ));
}

#[test]
fn merge_when_ready_takes_its_direction_and_pin_from_the_board() {
    let mut board = ready();
    board.rows[0].pr.as_mut().unwrap().auto_merge_armed = true;
    let mut model = start(board);
    model.input(key(KeyCode::Char('!')), 20);
    let InputResult::Action(action) = model.input(key(KeyCode::Char('m')), 20) else {
        panic!("the chord did not commit")
    };
    assert_eq!(
        model.resolve(action),
        UiAction::GithubSetAutoMerge {
            key: "one".into(),
            enable: false,
            pin: Some(crate::PrPin {
                number: 7,
                head_sha: Some("abc".into()),
            }),
        }
    );
    let mut board = ready();
    board.rows[0].pr = None;
    let mut model = super::Model::default();
    model.apply(live(board));
    model.input(key(KeyCode::Char('!')), 20);
    assert_eq!(pick(&mut model, AUTO_MERGE_ACTION_ID), InputResult::Draw);
    assert_eq!(
        model.toast,
        Some(("merge when ready: no PR".into(), ToastTone::Notice))
    );
}

#[test]
fn the_fixed_rows_send_their_own_requests() {
    let mut model = start(ready());
    model.input(key(KeyCode::Char('!')), 20);
    assert_eq!(
        pick(&mut model, DEV_LOGS_ACTION_ID),
        InputResult::Action(UiAction::PrepareDevLogs { key: "one".into() })
    );
    model.input(key(KeyCode::Char('>')), 20);
    assert_eq!(
        model.input(key(KeyCode::Char('z')), 20),
        InputResult::Action(UiAction::OpenSlotEditor {
            target: SessionTarget::Main
        })
    );
    // A slot whose checkout is missing runs nothing, its editor included.
    let mut board = ready();
    board
        .actions
        .slots_blocked
        .insert("dotfiles".into(), "dotfiles checkout missing".into());
    let mut model = super::Model::default();
    model.apply(live(board));
    model.input(key(KeyCode::Char('\\')), 20);
    for id in [OPEN_EDITOR_ACTION_ID, "slot-continue", CUSTOM_ACTION_ID] {
        assert_eq!(pick(&mut model, id), InputResult::Draw);
        assert_eq!(
            model.toast,
            Some((
                "dotfiles slot unavailable: dotfiles checkout missing".into(),
                ToastTone::Notice
            ))
        );
    }
}

#[test]
fn a_remote_rows_palette_opens_and_commits_like_a_local_one() {
    let key_far = wt_core::remote_worktree_ledger_key("studio", "far");
    let local = ready();
    let mut board = Board {
        rows: vec![BoardRow {
            key: key_far.clone(),
            host: Some("studio".into()),
            slug: "far".into(),
            title: "far".into(),
            ..local.rows[0].clone()
        }],
        ..Board::default()
    };
    board
        .host_display
        .insert("studio".into(), local.display.clone());
    let mut model = start(board);
    let surface = ActionSurface::Row {
        key: key_far.clone(),
    };
    assert_eq!(model.input(key(KeyCode::Char('!')), 20), InputResult::Draw);
    assert_eq!(open_palette(&model), Some(&surface));
    assert!(model.pending_request.is_none());
    // The commit routes by the row's key; no host wrapper is needed.
    assert_eq!(
        pick(&mut model, "teardown"),
        InputResult::Action(UiAction::RunAction {
            surface,
            id: "teardown".into(),
            arg: None,
            extras: String::new(),
            seen: seen(None),
        })
    );
}
