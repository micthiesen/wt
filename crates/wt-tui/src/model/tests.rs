use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use wt_runtime::{SourceSnapshot, SourceState};

use super::*;
use crate::{
    ConfirmAction, PickerAction, PickerOption, Seen, SessionTarget, TextAction, UiAction, UiModal,
    UiReply, UrlKind,
};

#[test]
fn first_inventory_selects_a_visible_row_after_empty_inbox() {
    let mut model = Model::default();
    let mut empty = snapshot(&[]);
    empty.test_data_mut().sections = vec![BoardSection {
        key: "\0inbox".into(),
        title: "Inbox".into(),
        ..Default::default()
    }];
    model.apply(empty);
    let mut populated = snapshot(&["one", "two"]);
    populated.test_data_mut().sections = vec![BoardSection {
        key: "\0inbox".into(),
        title: "Inbox".into(),
        rows: vec![0, 1],
        ..Default::default()
    }];
    model.apply(populated);
    assert_eq!(model.selected_row().unwrap().key, "one");
    model.input(KeyEvent::from(KeyCode::Char('j')), 20);
    assert_eq!(model.selected_row().unwrap().key, "two");
}

#[test]
fn github_keys_capture_target_and_require_confirmation() {
    let mut model = Model::default();
    model.apply(snapshot(&["one", "two"]));
    assert_eq!(
        model.input(KeyEvent::from(KeyCode::Char('e')), 20),
        InputResult::Action(UiAction::PrepareGithub {
            key: "one".into(),
            ship: false
        })
    );
    model.reply(UiReply {
        modal: Some(UiModal::Confirm {
            action: ConfirmAction::Github {
                key: "one".into(),
                ship: true,
                pin: Some(crate::PrPin {
                    number: 7,
                    head_sha: Some("abc".into()),
                }),
            },
            title: "Ship?".into(),
            lines: vec![],
            cancel_key: Some('E'),
        }),
        ..Default::default()
    });
    assert_eq!(
        model.input(KeyEvent::from(KeyCode::Enter), 20),
        InputResult::Action(UiAction::GithubShip {
            key: "one".into(),
            pin: Some(crate::PrPin {
                number: 7,
                head_sha: Some("abc".into()),
            }),
        })
    );
}

#[test]
fn delayed_terminal_handoff_preserves_newer_prompt_or_selection() {
    for newer_input in [KeyCode::Char('#'), KeyCode::Down] {
        let mut model = Model::default();
        model.apply(snapshot(&["one", "two"]));
        model.input(KeyEvent::from(KeyCode::F(12)), 20);
        let generation = model.ui_generation;
        model.input(KeyEvent::from(newer_input), 20);
        let selected = model.selected;
        let toast = model.toast.clone();
        let (ready, mut suspended) = tokio::sync::oneshot::channel();
        let (resume, resumed) = tokio::sync::oneshot::channel();
        assert!(
            model
                .reply(UiReply {
                    operation_id: Some(42),
                    ui_generation: Some(generation),
                    handoff: Some(crate::TerminalHandoff { ready, resumed }),
                    ..Default::default()
                })
                .is_none()
        );
        assert!(matches!(
            suspended.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Closed)
        ));
        assert!(resume.is_closed());
        assert_eq!(model.selected, selected);
        assert_eq!(model.toast, toast);
        if newer_input == KeyCode::Char('#') {
            assert!(matches!(model.interaction, Interaction::Text(_)));
        }
    }
}

#[test]
fn ignored_input_keeps_current_terminal_handoff_available() {
    let mut model = Model::default();
    model.apply(snapshot(&["one"]));
    model.input(KeyEvent::from(KeyCode::F(12)), 20);
    let generation = model.ui_generation;
    model.input(KeyEvent::from(KeyCode::Null), 20);
    let (ready, mut suspended) = tokio::sync::oneshot::channel();
    let (_resume, resumed) = tokio::sync::oneshot::channel();
    let ticket = model
        .reply(UiReply {
            operation_id: Some(42),
            ui_generation: Some(generation),
            handoff: Some(crate::TerminalHandoff { ready, resumed }),
            ..Default::default()
        })
        .unwrap();
    ticket.ready.send(()).unwrap();
    assert_eq!(suspended.try_recv(), Ok(()));
}

#[test]
fn stale_special_session_completion_cannot_erase_pending_creation_feedback() {
    let mut model = Model::default();
    model.apply(snapshot(&["old"]));
    model.begin_action(
        1,
        &UiAction::Create {
            placement: None,
            input: "new".into(),
        },
    );
    let InputResult::Action(action) = model.input(KeyEvent::from(KeyCode::Char(',')), 20) else {
        panic!("special session remains available during creation")
    };
    model.begin_action(2, &action);
    let generation = model.ui_generation;
    model.toast = Some(("Working…".into(), ToastTone::Info));
    assert_eq!(
        model.input(KeyEvent::from(KeyCode::F(12)), 20),
        InputResult::Draw
    );
    let feedback = model.toast.clone();
    assert_eq!(
        feedback.as_ref().unwrap().0,
        "Worktree is still being created"
    );
    let (ready, mut suspended) = tokio::sync::oneshot::channel();
    let (_resume, resumed) = tokio::sync::oneshot::channel();
    assert!(
        model
            .reply(UiReply {
                operation_id: Some(2),
                ui_generation: Some(generation),
                handoff: Some(crate::TerminalHandoff { ready, resumed }),
                ..Default::default()
            })
            .is_none()
    );
    assert!(matches!(
        suspended.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Closed)
    ));
    model.reply(UiReply {
        operation_id: Some(2),
        ui_generation: Some(generation),
        outcome: Some(crate::ReplyOutcome::NoChange),
        ..Default::default()
    });
    assert_eq!(model.toast, feedback);
}

#[test]
fn navigating_away_from_a_pending_connection_explains_cancellation() {
    let mut model = Model::default();
    model.apply(snapshot(&["one", "two"]));
    let InputResult::Action(action) = model.input(KeyEvent::from(KeyCode::F(12)), 20) else {
        panic!("session action")
    };
    model.begin_action(7, &action);
    model.toast = Some(("Working…".into(), ToastTone::Info));
    model.input(KeyEvent::from(KeyCode::Down), 20);
    assert_eq!(
        model.toast.as_ref().unwrap().0,
        "Session connection cancelled"
    );
    model.reply(UiReply {
        operation_id: Some(7),
        outcome: Some(crate::ReplyOutcome::NoChange),
        ..Default::default()
    });
    assert_eq!(
        model.toast.as_ref().unwrap().0,
        "Session connection cancelled"
    );
}

#[test]
fn delayed_modal_and_background_completion_preserve_newer_input() {
    let mut model = Model::default();
    model.apply(snapshot(&["one"]));
    model.input(KeyEvent::new(KeyCode::F(12), KeyModifiers::NONE), 20);
    let generation = model.ui_generation;
    model.input(KeyEvent::new(KeyCode::Char('#'), KeyModifiers::NONE), 20);
    assert!(matches!(model.interaction, Interaction::Text(_)));
    model.reply(UiReply {
        ui_generation: Some(generation),
        modal: Some(UiModal::Log {
            title: "Old result".into(),
            lines: vec![],
            close_key: None,
            refresh: None,
        }),
        ..Default::default()
    });
    assert!(matches!(model.interaction, Interaction::Text(_)));
    model.reply(UiReply {
        message: "Background command finished".into(),
        ..Default::default()
    });
    assert!(matches!(model.interaction, Interaction::Text(_)));
}

fn grouped(keys: &[&str], folded: bool) -> SourceSnapshot<Board> {
    let mut state = snapshot(keys);
    let board = state.test_data_mut();
    board.sections = vec![BoardSection {
        key: "Batch".into(),
        title: "Batch".into(),
        folded,
        rows: (0..keys.len()).collect(),
        ..Default::default()
    }];
    state
}

#[test]
fn delayed_palette_keeps_fast_chords_and_ignores_unrelated_replies() {
    for host in [None, Some("maxbook".to_owned())] {
        let mut model = Model::default();
        model.apply(snapshot(&["one", "two"]));
        let InputResult::Action(action) = model.input(KeyEvent::from(KeyCode::Char('!')), 20)
        else {
            panic!()
        };
        let generation = model.ui_generation;
        model.begin_action(42, &action);
        assert_eq!(
            model.input(KeyEvent::from(KeyCode::Char('m')), 20),
            InputResult::Draw
        );
        assert_eq!(model.ui_generation, generation);
        assert!(model.follow_up.is_none());
        model.reply(UiReply {
            operation_id: Some(41),
            message: "other work finished".into(),
            ..Default::default()
        });
        assert!(model.pending_request.is_some());
        model.apply(snapshot(&["two", "one"]));
        let surface = crate::ActionSurface::Row { key: "one".into() };
        model.reply(UiReply {
            operation_id: Some(42),
            ui_generation: Some(generation),
            modal_host: host.clone(),
            modal: Some(UiModal::Picker {
                action: PickerAction::Actions {
                    surface: surface.clone(),
                },
                title: "Actions".into(),
                selected: 0,
                options: vec![PickerOption {
                    value: Some(crate::AUTO_MERGE_ACTION_ID.into()),
                    label: "Merge".into(),
                    chord: Some('m'),
                    note: None,
                    verify_after_merge: None,
                    detail: None,
                }],
            }),
            ..Default::default()
        });
        assert!(model.pending_request.is_none());
        let expected = UiAction::PrepareAction {
            surface,
            id: crate::AUTO_MERGE_ACTION_ID.into(),
            arg: None,
        };
        assert_eq!(
            model.follow_up,
            Some(match host {
                Some(host) => UiAction::OnHost {
                    host: Some(host),
                    action: Box::new(expected)
                },
                None => expected,
            })
        );
    }
}

#[test]
fn keys_typed_while_a_menu_loads_replay_into_it_and_escape_cancels() {
    for key in [KeyCode::Null, KeyCode::Esc, KeyCode::Down] {
        let mut model = Model::default();
        model.apply(snapshot(&["one", "two"]));
        let InputResult::Action(action) = model.input(KeyEvent::from(KeyCode::Char('!')), 20)
        else {
            panic!()
        };
        let generation = model.ui_generation;
        model.begin_action(42, &action);
        model.input(KeyEvent::from(key), 20);
        model.reply(UiReply {
            operation_id: Some(42),
            ui_generation: Some(generation),
            modal: Some(UiModal::Picker {
                action: PickerAction::Actions {
                    surface: crate::ActionSurface::Row { key: "one".into() },
                },
                title: "Actions".into(),
                selected: 0,
                options: ["a", "b"]
                    .into_iter()
                    .map(|value| PickerOption {
                        value: Some(value.into()),
                        label: value.into(),
                        chord: None,
                        note: None,
                        verify_after_merge: None,
                        detail: None,
                    })
                    .collect(),
            }),
            ..Default::default()
        });
        match &model.interaction {
            Interaction::Picker(picker) => {
                assert_ne!(key, KeyCode::Esc);
                // Down moved the menu's selection, not the board cursor.
                assert_eq!(picker.selected, usize::from(key == KeyCode::Down));
            }
            _ => assert_eq!(key, KeyCode::Esc),
        }
        assert_eq!(model.selected_row().unwrap().key, "one");
        assert!(model.pending_request.is_none());
        assert!(model.follow_up.is_none());
    }
}

#[test]
fn fast_session_close_never_reaches_board_remove() {
    let mut model = Model::default();
    model.apply(snapshot(&["one"]));
    let InputResult::Action(action) = model.input(KeyEvent::from(KeyCode::Char(';')), 20) else {
        panic!()
    };
    let generation = model.ui_generation;
    model.begin_action(7, &action);
    // `d` on the board would request removal; while sessions load it waits.
    assert_eq!(
        model.input(KeyEvent::from(KeyCode::Char('d')), 20),
        InputResult::Draw
    );
    assert!(model.pending_request.is_some());
    let selection = crate::SessionSelection {
        key: Some("one".into()),
        target: crate::SessionTarget::Harness,
        harness: wt_core::HarnessId::Claude,
        session_id: Some("s1".into()),
        live_slot: None,
        managed_name: None,
        mode: crate::SessionMode::Resume,
        live: true,
        seen_live: None,
    };
    model.reply(UiReply {
        operation_id: Some(7),
        ui_generation: Some(generation),
        modal: Some(UiModal::Picker {
            action: PickerAction::Sessions {
                choices: vec![selection.clone()],
            },
            title: "sessions".into(),
            selected: 0,
            options: vec![PickerOption {
                value: Some(crate::session_value(&selection)),
                label: "Claude · live".into(),
                chord: None,
                note: None,
                verify_after_merge: None,
                detail: None,
            }],
        }),
        ..Default::default()
    });
    assert!(
        matches!(&model.follow_up, Some(UiAction::StopSession { selection: stopped }) if *stopped == selection),
        "{:?}",
        model.follow_up
    );
}

#[tokio::test(start_paused = true)]
async fn slow_menus_drop_typeahead_instead_of_replaying_it() {
    let mut model = Model::default();
    model.apply(snapshot(&["one"]));
    let InputResult::Action(action) = model.input(KeyEvent::from(KeyCode::Char('!')), 20) else {
        panic!()
    };
    let generation = model.ui_generation;
    model.begin_action(42, &action);
    model.input(KeyEvent::from(KeyCode::Char('m')), 20);
    tokio::time::advance(super::input::TYPEAHEAD_TTL + std::time::Duration::from_millis(1)).await;
    model.reply(UiReply {
        operation_id: Some(42),
        ui_generation: Some(generation),
        modal: Some(UiModal::Picker {
            action: PickerAction::Actions {
                surface: crate::ActionSurface::Row { key: "one".into() },
            },
            title: "Actions".into(),
            selected: 0,
            options: vec![PickerOption {
                value: Some(crate::AUTO_MERGE_ACTION_ID.into()),
                label: "Merge".into(),
                chord: Some('m'),
                note: None,
                verify_after_merge: None,
                detail: None,
            }],
        }),
        ..Default::default()
    });
    assert!(matches!(model.interaction, Interaction::Picker(_)));
    assert!(model.follow_up.is_none());
    assert_eq!(model.toast.as_ref().unwrap().1, ToastTone::Notice);
}

#[test]
fn replay_stops_when_the_menu_closes_so_keys_never_fall_through() {
    let mut model = Model::default();
    model.apply(snapshot(&["one", "two"]));
    let InputResult::Action(action) = model.input(KeyEvent::from(KeyCode::Char('!')), 20) else {
        panic!()
    };
    let generation = model.ui_generation;
    model.begin_action(42, &action);
    model.pending_request.as_mut().unwrap().typeahead = vec![
        (KeyEvent::from(KeyCode::Esc), 20),
        (KeyEvent::from(KeyCode::Char('j')), 20),
    ];
    model.reply(UiReply {
        operation_id: Some(42),
        ui_generation: Some(generation),
        modal: Some(UiModal::Picker {
            action: PickerAction::Actions {
                surface: crate::ActionSurface::Row { key: "one".into() },
            },
            title: "Actions".into(),
            selected: 0,
            options: vec![],
        }),
        ..Default::default()
    });
    assert!(matches!(model.interaction, Interaction::None));
    assert_eq!(model.selected_row().unwrap().key, "one");
}

#[test]
fn pending_palette_cancellation_keys_discard_early_chord_immediately() {
    for key in [
        KeyEvent::from(KeyCode::Char('q')),
        KeyEvent::from(KeyCode::Esc),
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
    ] {
        let mut model = Model::default();
        model.apply(snapshot(&["one"]));
        let InputResult::Action(action) = model.input(KeyEvent::from(KeyCode::Char('!')), 20)
        else {
            panic!()
        };
        let generation = model.ui_generation;
        model.begin_action(42, &action);
        model.input(KeyEvent::from(KeyCode::Char('m')), 20);
        assert_eq!(model.input(key, 20), InputResult::Draw);
        assert!(model.pending_request.is_none());
        assert_eq!(
            model.toast,
            Some(("Menu cancelled".into(), ToastTone::Info))
        );
        model.reply(UiReply {
            operation_id: Some(42),
            ui_generation: Some(generation),
            modal: Some(UiModal::Picker {
                action: PickerAction::Actions {
                    surface: crate::ActionSurface::Row { key: "one".into() },
                },
                title: "Delayed menu".into(),
                selected: 0,
                options: vec![],
            }),
            ..Default::default()
        });
        assert!(matches!(model.interaction, Interaction::None));
        assert!(model.follow_up.is_none());
    }
}

#[test]
fn palette_failure_clears_buffer_and_never_runs_board_chord() {
    let mut model = Model::default();
    model.apply(snapshot(&["one"]));
    let InputResult::Action(action) = model.input(KeyEvent::from(KeyCode::Char('!')), 20) else {
        panic!()
    };
    model.begin_action(42, &action);
    model.input(KeyEvent::from(KeyCode::Char('f')), 20);
    model.reply(UiReply {
        operation_id: Some(42),
        ..UiReply::failure("remote unavailable")
    });
    assert!(model.pending_request.is_none());
    assert!(model.follow_up.is_none());
    assert_eq!(
        model.toast,
        Some(("remote unavailable".into(), ToastTone::Failed))
    );
}

#[test]
fn action_palette_chords_and_arguments_keep_the_original_target() {
    let mut model = Model::default();
    model.apply(snapshot(&["one", "two"]));
    let surface = crate::ActionSurface::Row { key: "one".into() };
    assert_eq!(
        model.input(KeyEvent::from(KeyCode::Char('!')), 20),
        InputResult::Action(UiAction::PrepareActions {
            surface: surface.clone()
        })
    );
    model.reply(UiReply {
        modal: Some(UiModal::Picker {
            action: PickerAction::Actions {
                surface: surface.clone(),
            },
            title: "Actions".into(),
            selected: 0,
            options: vec![PickerOption {
                value: Some("continue".into()),
                label: "Continue".into(),
                chord: Some('g'),
                note: None,
                verify_after_merge: None,
                detail: None,
            }],
        }),
        ..Default::default()
    });
    model.apply(snapshot(&["two", "one"]));
    assert_eq!(
        model.input(KeyEvent::from(KeyCode::Char('g')), 20),
        InputResult::Action(UiAction::PrepareAction {
            surface: surface.clone(),
            id: "continue".into(),
            arg: None
        })
    );
    model.reply(UiReply {
        modal: Some(UiModal::Text {
            action: TextAction::ActionExtras {
                surface: surface.clone(),
                id: "continue".into(),
                arg: Some("saved".into()),
            },
            prompt: "Extra instructions".into(),
            initial: "go".into(),
            allow_empty: true,
        }),
        ..Default::default()
    });
    model.input(KeyEvent::from(KeyCode::Char('q')), 20);
    assert_eq!(
        model.input(KeyEvent::from(KeyCode::Enter), 20),
        InputResult::Action(UiAction::RunAction {
            surface,
            id: "continue".into(),
            arg: Some("saved".into()),
            extras: "goq".into()
        })
    );
}

#[test]
fn special_palettes_open_without_a_selected_worktree() {
    let mut model = Model::default();
    assert_eq!(
        model.input(KeyEvent::from(KeyCode::Char('M')), 20),
        InputResult::Action(UiAction::PrepareActions {
            surface: crate::ActionSurface::Manager { key: None }
        })
    );
    for (key, target) in [
        ('<', SessionTarget::WtSource),
        ('>', SessionTarget::Main),
        ('\\', SessionTarget::Dotfiles),
    ] {
        assert_eq!(
            model.input(KeyEvent::from(KeyCode::Char(key)), 20),
            InputResult::Action(UiAction::PrepareActions {
                surface: crate::ActionSurface::Slot { target }
            })
        );
    }
}

#[test]
fn folding_keeps_the_group_selected_and_hidden_rows_cannot_receive_actions() {
    let mut model = Model::default();
    model.apply(grouped(&["one", "two"], false));
    assert_eq!(model.selected_row().unwrap().key, "one");
    assert_eq!(
        model.input(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), 10),
        InputResult::Action(UiAction::FoldSection {
            key: "Batch".into(),
            folded: true
        })
    );
    model.apply(grouped(&["one", "two"], true));
    assert_eq!(model.selected, Some(0));
    assert_eq!(model.item_count(), 1);
    assert!(model.selected_row().is_none());
    assert_eq!(
        model.input(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE), 10),
        InputResult::Draw
    );
    assert_eq!(
        model.toast.as_ref().map(|(text, _)| text.as_str()),
        Some("select a worktree first")
    );
    assert_eq!(model.yank_choices()[0].2, "Batch");
    // Unfolding expands in place: the first row takes the header's stop.
    model.apply(grouped(&["one", "two"], false));
    assert_eq!(model.selected_row().unwrap().key, "one");
    model.input(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), 10);
    assert_eq!(model.selected_row().unwrap().key, "two");
}

#[test]
fn section_picker_creates_from_empty_list_and_remembers_last_target() {
    let mut model = Model::default();
    model.apply(grouped(&["one", "two"], false));
    let key = |ch| KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);
    // The section menu is built from the board, so `l` opens it without a
    // host request, witnessing the section the row shows.
    assert_eq!(model.input(key('l'), 20), InputResult::Draw);
    let Interaction::Picker(picker) = &model.interaction else {
        panic!("the section menu should be open")
    };
    assert_eq!(
        picker.action,
        PickerAction::Section {
            key: "one".into(),
            seen: Some(Seen(Some("Batch".into()))),
        }
    );
    assert_eq!(model.input(key('n'), 20), InputResult::Draw);
    model.paste("Release");
    assert_eq!(
        model.input(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), 20),
        InputResult::Action(UiAction::MoveSection {
            key: "one".into(),
            section: Some("Release".into()),
            seen: Some(Seen(Some("Batch".into()))),
        })
    );
    let options = ["Other", "Release"]
        .map(|name| PickerOption {
            value: Some(name.into()),
            label: name.into(),
            chord: None,
            note: None,
            verify_after_merge: None,
            detail: None,
        })
        .to_vec();
    model.reply(UiReply {
        modal: Some(UiModal::Picker {
            action: PickerAction::Section {
                key: "two".into(),
                seen: None,
            },
            title: "Section".into(),
            options,
            selected: 0,
        }),
        ..Default::default()
    });
    assert_eq!(
        model.input(key('l'), 20),
        InputResult::Action(UiAction::MoveSection {
            key: "two".into(),
            section: Some("Release".into()),
            seen: None,
        })
    );
}

#[test]
fn moved_row_keeps_cursor_in_original_section_and_restore_follows_row() {
    let mut model = Model::default();
    model.apply(grouped(&["one", "two"], false));
    let mut moved = grouped(&["one", "two"], false);
    let board = moved.test_data_mut();
    board.sections[0].rows = vec![1];
    board.sections.push(BoardSection {
        key: "\0archived".into(),
        title: "Archived".into(),
        rows: vec![0],
        folded: false,
        ..Default::default()
    });
    model.apply(moved);
    assert_eq!(model.selected_row().unwrap().key, "two");
    model.select(3);
    assert_eq!(model.selected_row().unwrap().key, "one");
    model.apply(grouped(&["one", "two"], false));
    assert_eq!(model.selected_row().unwrap().key, "one");
}

#[test]
fn folded_manual_section_can_be_renamed_without_affecting_inbox() {
    let mut model = Model::default();
    model.apply(grouped(&["one"], true));
    let key = KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT);
    assert_eq!(model.input(key, 20), InputResult::Draw);
    assert!(matches!(
        model.interaction,
        Interaction::Text(TextPrompt {
            action: TextAction::RenameSection { .. },
            ..
        })
    ));
    model.input(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), 20);
    let mut inbox = grouped(&["one"], true);
    inbox.test_data_mut().sections[0].key = "\0inbox".into();
    model.apply(inbox);
    assert_eq!(model.input(key, 20), InputResult::Unchanged);
}

#[test]
fn creation_selection_waits_until_the_new_row_is_visible_in_its_expanded_section() {
    let mut model = Model::default();
    model.apply(grouped(&["old"], true));
    model.reply(UiReply {
        select_when_visible: Some("new".into()),
        ..Default::default()
    });
    model.apply(grouped(&["old", "new"], true));
    assert_eq!(model.selected_row().unwrap().key, "new");
    assert!(model.pending_selection.is_none());
    model.apply(grouped(&["old", "new"], false));
    assert_eq!(model.selected_row().unwrap().key, "new");
    assert!(model.pending_selection.is_none());
    model.apply(grouped(&["new", "old"], false));
    assert_eq!(model.selected_row().unwrap().key, "new");
}

#[test]
fn removed_group_member_selects_its_visual_neighbor_instead_of_the_header() {
    let mut model = Model::default();
    model.apply(grouped(&["one", "two"], false));
    model.input(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), 10);
    assert_eq!(model.selected_row().unwrap().key, "two");
    model.apply(grouped(&["one"], false));
    assert_eq!(model.selected_row().unwrap().key, "one");
}

fn snapshot(keys: &[&str]) -> SourceSnapshot<Board> {
    SourceSnapshot::test_live(
        Some(Board {
            rows: keys
                .iter()
                .map(|key| BoardRow {
                    key: (*key).into(),
                    title: (*key).into(),
                    ..BoardRow::default()
                })
                .collect(),
            ..Board::default()
        }),
        SourceState::Ready,
        wt_core::Domain::Git,
    )
}

fn bind_creation(model: &mut Model, id: u64, key: &str) {
    let (ready, mut received) = tokio::sync::oneshot::channel();
    model.reply(UiReply {
        operation_id: Some(id),
        creation_prepared: Some(crate::CreationPrepared {
            already_exists: false,
            key: key.into(),
            ready,
        }),
        ..Default::default()
    });
    assert_eq!(received.try_recv(), Ok(()));
}

#[test]
fn creation_immediately_selects_one_pending_row_and_f12_never_queues_entry() {
    let mut model = Model::default();
    model.apply(snapshot(&["old"]));
    model.begin_action(
        1,
        &UiAction::Create {
            placement: None,
            input: "new".into(),
        },
    );
    assert_eq!(model.board.rows.len(), 2);
    assert_eq!(model.selected_row().unwrap().title, "new");
    assert!(model.selected_row().unwrap().pending_creation);
    assert_eq!(
        model.input(KeyEvent::from(KeyCode::F(12)), 20),
        InputResult::Draw
    );
    assert!(model.follow_up.is_none());
    assert_eq!(
        model.input(KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT), 20),
        InputResult::Draw
    );
    assert!(matches!(model.interaction, Interaction::None));
    bind_creation(&mut model, 1, "new");
    assert_eq!(model.selected_row().unwrap().key, "new");
    model.apply(snapshot(&["old", "new"]));
    assert_eq!(model.board.rows.len(), 2);
    assert!(model.selected_row().unwrap().pending_creation);
    model.reply(UiReply {
        operation_id: Some(1),
        select_when_visible: Some("new".into()),
        outcome: Some(crate::ReplyOutcome::Accepted(wt_runtime::Stamp::now())),
        ..Default::default()
    });
    assert!(!model.selected_row().unwrap().pending_creation);
    assert!(model.follow_up.is_none());
    assert_eq!(
        model.input(KeyEvent::from(KeyCode::F(12)), 20),
        InputResult::Action(UiAction::Session {
            key: Some("new".into()),
            target: SessionTarget::Harness,
        })
    );
}

/// A remote row's archive runs on the controller: its accept stamp is
/// this process's, so this process's own reads confirm it.
#[tokio::test(start_paused = true)]
async fn a_controller_owned_write_on_a_remote_row_settles_on_local_reads() {
    let mut model = Model::default();
    let mut board = Board {
        rows: vec![BoardRow {
            key: "@box/alpha".into(),
            host: Some("box".into()),
            title: "alpha".into(),
            ..BoardRow::default()
        }],
        ..Board::default()
    };
    model.apply(SourceSnapshot::test_live(
        Some(board.clone()),
        SourceState::Ready,
        wt_core::Domain::Metadata,
    ));
    let action = UiAction::ToggleArchive {
        key: "@box/alpha".into(),
        seen: Some(crate::Seen(false)),
    };
    model.begin_action(1, &action);
    assert!(model.board.rows[0].archived, "shown at once");
    tokio::time::advance(std::time::Duration::from_millis(5)).await;
    model.reply(UiReply {
        operation_id: Some(1),
        outcome: Some(crate::ReplyOutcome::Accepted(wt_runtime::Stamp::now())),
        ..Default::default()
    });
    tokio::time::advance(std::time::Duration::from_millis(5)).await;
    board.rows[0].archived = true;
    model.apply(SourceSnapshot::test_live(
        Some(board),
        SourceState::Ready,
        wt_core::Domain::Metadata,
    ));
    assert!(
        model.pending.is_empty(),
        "a local read after the write settles it"
    );
}

#[test]
fn pending_creation_does_not_block_quit() {
    let mut model = Model::default();
    model.apply(snapshot(&["old"]));
    model.begin_action(
        1,
        &UiAction::Create {
            placement: None,
            input: "new".into(),
        },
    );
    assert_eq!(
        model.input(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL), 20),
        InputResult::Quit
    );
}

fn removal_revision(key: &str) -> crate::RemovalRevision {
    crate::RemovalRevision {
        key: key.into(),
        path: format!("/worktrees/{key}"),
        branch: format!("feature/{key}"),
        head: "abc123".into(),
        digest: "deadbeef".into(),
        hazards: vec![],
        published_base: None,
    }
}

#[test]
fn creation_waits_for_real_row_and_manual_navigation_cancels_old_intent() {
    let mut model = Model::default();
    model.apply(snapshot(&["first", "second"]));
    model.reply(UiReply {
        message: "Created".into(),
        failed: false,
        select_when_visible: Some("new".into()),
        ..UiReply::default()
    });
    model.apply(snapshot(&["first", "second"]));
    assert_eq!(model.selected_row().unwrap().key, "first");
    model.apply(snapshot(&["first", "second", "new"]));
    assert_eq!(model.selected_row().unwrap().key, "new");
    assert!(model.pending_selection.is_none());
    model.reply(UiReply {
        message: "Created".into(),
        failed: false,
        select_when_visible: Some("later".into()),
        ..UiReply::default()
    });
    model.input(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE), 20);
    model.apply(snapshot(&["first", "second", "new", "later"]));
    assert_eq!(model.selected_row().unwrap().key, "second");
}

#[test]
fn title_prompt_keeps_target_identity_and_consumes_global_keys() {
    let mut model = Model::default();
    model.apply(snapshot(&["first", "second"]));
    model.input(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE), 20);
    model.apply(snapshot(&["second", "first"]));
    assert_eq!(
        model.input(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), 20),
        InputResult::Draw
    );
    assert_eq!(
        model.input(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), 20),
        InputResult::Action(UiAction::SetTitle {
            key: "first".into(),
            title: "firstq".into()
        })
    );
    model.input(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE), 20);
    model.input(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL), 20);
    assert_eq!(
        model.input(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), 20),
        InputResult::Unchanged
    );
    assert!(model.title_prompt.is_some());
}

#[test]
fn selection_tracks_identity_across_reorder_and_neighbor_after_removal() {
    let mut model = Model::default();
    model.apply(snapshot(&["a", "b", "c"]));
    model.select(1);
    model.details_scroll = 12;
    model.apply(snapshot(&["c", "a", "b"]));
    assert_eq!(model.selected, Some(2));
    assert_eq!(model.details_scroll, 12);
    model.apply(snapshot(&["c", "a"]));
    assert_eq!(model.selected_row().unwrap().key, "a");
    assert_eq!(model.details_scroll, 0);
    model.apply(snapshot(&[]));
    assert_eq!(model.selected, None);
}

#[test]
fn viewport_keeps_selection_visible_after_shrinking() {
    let mut model = Model::default();
    model.apply(snapshot(&["a", "b", "c", "d", "e", "f", "g", "h", "i"]));
    model.select(6);
    for height in [8, 3, 1, 0, 6] {
        model.keep_selection_visible(height);
        let selected = model.selected.unwrap();
        assert!(selected >= model.offset);
        assert!(height == 0 || selected < model.offset + height);
    }
}

#[test]
fn overlays_consume_navigation_and_cancel_before_global_keys() {
    let mut model = Model::default();
    model.apply(snapshot(&["a", "b"]));
    let key = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
    model.input(key('?'), 10);
    // The overlay's draw publishes how far it can scroll.
    model.help_max_scroll = 40;
    assert_eq!(model.input(key('j'), 10), InputResult::Draw);
    assert_eq!(model.help_scroll, 3);
    // Scrolling stops where the drawn overlay ends.
    model.help_max_scroll = 4;
    model.input(key('j'), 10);
    assert_eq!(model.help_scroll, 4);
    model.help_scroll = 3;
    assert_eq!(model.selected, Some(0));
    assert_eq!(model.input(key('q'), 10), InputResult::Draw);
    assert!(!model.help);
    model.input(key('P'), 10);
    assert_eq!(
        model.input(key('q'), 10),
        InputResult::Action(UiAction::SetPerf {
            active: false,
            continuous: false,
            refresh: false
        })
    );
    assert!(!model.show_perf);
    assert_eq!(model.input(key('q'), 10), InputResult::Quit);
    assert_eq!(
        model.input(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::ALT), 10),
        InputResult::Unchanged
    );
}

#[test]
fn help_search_filters_rows_and_esc_clears_before_closing() {
    let mut model = Model {
        help: true,
        ..Default::default()
    };
    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
    assert_eq!(model.input(key(KeyCode::Char('/')), 20), InputResult::Draw);
    assert!(model.help_searching);
    model.input(key(KeyCode::Char('r')), 20);
    model.input(key(KeyCode::Char('e')), 20);
    assert_eq!(model.help_query.text(), "re");
    assert_eq!(model.input(key(KeyCode::Esc), 20), InputResult::Draw);
    assert!(!model.help_searching);
    assert!(model.help_query.text().is_empty());
    assert!(model.help);
    assert_eq!(model.input(key(KeyCode::Esc), 20), InputResult::Draw);
    assert!(!model.help);
}

#[test]
fn attention_toggle_and_seen_mark_preserve_the_feed_contract() {
    let mut model = Model {
        board: Arc::new(Board {
            attention: (0..20)
                .map(|at_ms| crate::AttentionLine {
                    at_ms,
                    source: "test".into(),
                    text: at_ms.to_string(),
                })
                .collect(),
            ..Default::default()
        }),
        ..Default::default()
    };
    let key = |character| KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE);
    model.output_view(4, 80);
    model.output.scroll(true);
    assert!(model.output.is_scrolled());
    assert!(matches!(
        model.input(key('x'), 20),
        InputResult::Action(UiAction::SetAttentionSeen { at_ms }) if at_ms > 0
    ));
    assert!(!model.output.is_scrolled());
    assert_eq!(model.input(key('"'), 20), InputResult::Draw);
    assert_eq!(model.output.target, crate::output::OutputTarget::Activity);
    assert_eq!(model.input(key('"'), 20), InputResult::Draw);
    assert_eq!(model.output.target, crate::output::OutputTarget::Attention);
}

#[test]
fn create_prompt_preserves_raw_cli_text_and_shift_n_seeds_base() {
    let mut model = Model::default();
    model.apply(snapshot(&["first"]));
    model.board = Arc::new(Board {
        rows: vec![BoardRow {
            key: "first".into(),
            branch: "feature/base".into(),
            ..BoardRow::default()
        }],
        ..Board::default()
    });
    model.input(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE), 20);
    let Interaction::Text(prompt) = &model.interaction else {
        panic!("create prompt did not open");
    };
    assert_eq!(prompt.prompt, "New worktree name: ");
    assert!(prompt.editor.text().is_empty());
    for ch in "ENG-123 --attach --gh 12 --base origin/main".chars() {
        model.input(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), 20);
    }
    assert_eq!(
        model.input(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), 20),
        InputResult::Action(UiAction::Create {
            placement: None,
            input: "ENG-123 --attach --gh 12 --base origin/main".into()
        })
    );
    model.input(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::SHIFT), 20);
    assert_eq!(
        model.input(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), 20),
        InputResult::Action(UiAction::Create {
            placement: None,
            input: "--base feature/base".into()
        })
    );
}

#[test]
fn remove_confirm_captures_target_and_cancel_key_never_mutates() {
    let mut model = Model::default();
    model.apply(snapshot(&["first", "second"]));
    model.select(1);
    assert_eq!(
        model.input(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE), 20),
        InputResult::Draw
    );
    model.reply(UiReply {
        modal: Some(UiModal::Confirm {
            action: ConfirmAction::Remove {
                key: "second".into(),
                force: true,
                revision: Some(removal_revision("second")),
                seen: None,
            },
            title: "remove second".into(),
            lines: vec!["dirty changes will be lost".into()],
            cancel_key: Some('d'),
        }),
        ..UiReply::default()
    });
    assert_eq!(
        model.input(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE), 20),
        InputResult::Draw
    );
    model.reply(UiReply {
        modal: Some(UiModal::Confirm {
            action: ConfirmAction::Remove {
                key: "second".into(),
                force: true,
                revision: Some(removal_revision("second")),
                seen: None,
            },
            title: "remove second".into(),
            lines: vec!["dirty changes will be lost".into()],
            cancel_key: Some('d'),
        }),
        ..UiReply::default()
    });
    assert_eq!(
        model.input(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), 20),
        InputResult::Action(UiAction::Remove {
            key: "second".into(),
            force: true,
            revision: Some(removal_revision("second")),
            seen: None,
        })
    );
}

#[test]
fn cleanup_confirmation_keeps_the_prepared_candidate_set() {
    let mut model = Model::default();
    model.apply(snapshot(&["a", "b"]));
    model.input(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), 20);
    model.reply(UiReply {
        modal: Some(UiModal::Confirm {
            action: ConfirmAction::Cleanup {
                revisions: vec![removal_revision("a")],
            },
            title: "clean merged".into(),
            lines: vec!["a".into(), "kept b: dirty".into()],
            cancel_key: Some('c'),
        }),
        ..UiReply::default()
    });
    assert_eq!(
        model.input(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE), 20),
        InputResult::Action(UiAction::Cleanup {
            revisions: vec![removal_revision("a")]
        })
    );
}

#[test]
fn cleanup_key_opens_cached_menu_and_requests_background_refresh() {
    let mut model = Model::default();
    model.apply(snapshot(&["one"]));
    assert_eq!(
        model.input(KeyEvent::from(KeyCode::Char('c')), 20),
        InputResult::Refresh
    );
    assert!(matches!(model.interaction, Interaction::Log { .. }));
    assert_eq!(
        model.input(KeyEvent::from(KeyCode::Char('c')), 20),
        InputResult::Draw
    );
    assert!(matches!(model.interaction, Interaction::None));
    let mut board = Board::default();
    board.rows.push(BoardRow {
        key: "one".into(),
        slug: "one".into(),
        path: "/worktrees/one".into(),
        branch: "feature/one".into(),
        git: GitPresentation {
            head_sha: Some("old-head".into()),
            landed_on: Some(LandingKind::Base),
            tracked_changes: Some(0),
            untracked_files: Some(0),
            ..Default::default()
        },
        ..Default::default()
    });
    model.board = Arc::new(board);
    assert_eq!(
        model.input(KeyEvent::from(KeyCode::Char('c')), 20),
        InputResult::Refresh
    );
    assert!(matches!(model.interaction, Interaction::Confirm(_)));
    // Later source results cannot expand or change this confirmation.
    model.apply(snapshot(&["two"]));
    model.input(KeyEvent::from(KeyCode::Char('l')), 20);
    assert_eq!(
        model.input(KeyEvent::from(KeyCode::Enter), 20),
        InputResult::Action(UiAction::CleanupTargets {
            targets: vec![crate::CleanupTarget {
                key: "one".into(),
                path: "/worktrees/one".into(),
                branch: "feature/one".into(),
                head: "old-head".into()
            }]
        })
    );
}

#[test]
fn status_picker_supports_chords_notes_and_verify_steps() {
    let mut model = Model::default();
    model.apply(snapshot(&["row"]));
    model.input(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE), 20);
    model.reply(UiReply {
        modal: Some(UiModal::Picker {
            action: PickerAction::Status {
                key: "row".into(),
                seen: None,
            },
            title: "status".into(),
            options: vec![
                PickerOption {
                    value: Some("ready".into()),
                    label: "ready".into(),
                    chord: Some('y'),
                    note: Some("old note".into()),
                    verify_after_merge: None,
                    detail: None,
                },
                PickerOption {
                    value: Some("ready".into()),
                    label: "ready + verify after merge".into(),
                    chord: Some('a'),
                    note: None,
                    verify_after_merge: Some("probe".into()),
                    detail: None,
                },
                PickerOption {
                    value: None,
                    label: "clear".into(),
                    chord: Some('x'),
                    note: None,
                    verify_after_merge: None,
                    detail: None,
                },
            ],
            selected: 0,
        }),
        ..UiReply::default()
    });
    assert_eq!(
        model.input(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE), 20),
        InputResult::Draw
    );
    model.input(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL), 20);
    for ch in "needs review".chars() {
        model.input(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), 20);
    }
    assert_eq!(
        model.input(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), 20),
        InputResult::Action(UiAction::SetStatus {
            key: "row".into(),
            state: Some("ready".into()),
            note: Some("needs review".into()),
            verify_after_merge: None,
            seen: None,
        })
    );
    model.input(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE), 20);
    model.reply(UiReply {
        modal: Some(UiModal::Picker {
            action: PickerAction::Status {
                key: "row".into(),
                seen: None,
            },
            title: "status".into(),
            options: vec![PickerOption {
                value: Some("ready".into()),
                label: "ready".into(),
                chord: Some('y'),
                note: Some("old note".into()),
                verify_after_merge: None,
                detail: None,
            }],
            selected: 0,
        }),
        ..UiReply::default()
    });
    assert_eq!(
        model.input(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE), 20),
        InputResult::Action(UiAction::SetStatus {
            key: "row".into(),
            state: Some("ready".into()),
            note: None,
            verify_after_merge: None,
            seen: None,
        })
    );
    model.input(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE), 20);
    model.reply(UiReply {
        modal: Some(UiModal::Picker {
            action: PickerAction::Status {
                key: "row".into(),
                seen: None,
            },
            title: "status".into(),
            options: vec![PickerOption {
                value: Some("ready".into()),
                label: "ready + verify".into(),
                chord: Some('a'),
                note: None,
                verify_after_merge: Some("probe".into()),
                detail: None,
            }],
            selected: 0,
        }),
        ..UiReply::default()
    });
    assert_eq!(
        model.input(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE), 20),
        InputResult::Draw
    );
    assert_eq!(
        model.input(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), 20),
        InputResult::Action(UiAction::SetStatus {
            key: "row".into(),
            state: Some("ready".into()),
            note: None,
            verify_after_merge: Some("probe".into()),
            seen: None,
        })
    );
}

#[test]
fn row_urls_and_session_keys_only_emit_for_available_targets() {
    let mut model = Model::default();
    model.apply(snapshot(&["row"]));
    assert_eq!(
        model.input(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE), 20),
        InputResult::Draw
    );
    assert_eq!(
        model.toast.as_ref().map(|(text, _)| text.as_str()),
        Some("no PR for this branch")
    );
    model.board = Arc::new(Board {
        rows: vec![BoardRow {
            key: "row".into(),
            pr_url: Some("https://example.test/pr/1".into()),
            ..BoardRow::default()
        }],
        ..Board::default()
    });
    assert_eq!(
        model.input(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE), 20),
        InputResult::Action(UiAction::OpenUrl {
            key: "row".into(),
            kind: UrlKind::PullRequest
        })
    );
    assert_eq!(
        model.input(KeyEvent::new(KeyCode::F(12), KeyModifiers::NONE), 20),
        InputResult::Action(UiAction::Session {
            key: Some("row".into()),
            target: SessionTarget::Harness
        })
    );
    assert_eq!(
        model.input(KeyEvent::new(KeyCode::Char(','), KeyModifiers::NONE), 20),
        InputResult::Action(UiAction::Session {
            key: None,
            target: SessionTarget::WtSource
        })
    );
}

#[test]
fn remote_inventory_before_create_reply_never_duplicates_rows_or_blocks_attention() {
    let host = "maxbook/config";
    let key = wt_core::remote_worktree_ledger_key(host, "new");
    let mut model = Model::default();
    model.apply(snapshot(&["old", "new"]));
    model.begin_action(
        1,
        &UiAction::OnHost {
            host: Some(host.into()),
            action: Box::new(UiAction::Create {
                placement: None,
                input: "42 --no-install".into(),
            }),
        },
    );
    assert_eq!(model.board.rows.len(), 3);
    assert_eq!(model.selected_row().unwrap().host.as_deref(), Some(host));
    bind_creation(&mut model, 1, &key);
    let mut arrived = snapshot(&["old", "new", &key]);
    arrived.test_data_mut().rows[2].host = Some(host.into());
    arrived
        .test_data_mut()
        .attention
        .push(crate::AttentionLine {
            at_ms: 1,
            source: "test".into(),
            text: "event".into(),
        });
    model.apply(arrived);
    assert_eq!(model.board.rows.len(), 3);
    assert_eq!(
        model.board.rows.iter().filter(|row| row.key == key).count(),
        1
    );
    assert_eq!(model.selected_row().unwrap().key, key);
    assert!(matches!(
        model.input(KeyEvent::from(KeyCode::Char('X')), 20),
        InputResult::Action(UiAction::SetAttentionSeen { .. })
    ));
    model.reply(UiReply {
        operation_id: Some(1),
        select_when_visible: Some(key.clone()),
        outcome: Some(crate::ReplyOutcome::Accepted(wt_runtime::Stamp::now())),
        ..Default::default()
    });
    assert_eq!(model.board.rows.len(), 3);
    assert!(!model.selected_row().unwrap().pending_creation);
    assert!(model.follow_up.is_none());
}

#[test]
fn manual_navigation_survives_delayed_creation_plan_reply_and_inventory() {
    let mut model = Model::default();
    model.apply(snapshot(&["first", "second"]));
    let generation = model.ui_generation;
    model.begin_action(
        1,
        &UiAction::Create {
            placement: None,
            input: "42".into(),
        },
    );
    assert_eq!(model.selected_row().unwrap().title, "42");
    model.input(KeyEvent::from(KeyCode::Char('k')), 20);
    assert_eq!(model.selected_row().unwrap().key, "second");
    bind_creation(&mut model, 1, "resolved-issue-slug");
    assert_eq!(model.selected_row().unwrap().key, "second");
    model.reply(UiReply {
        operation_id: Some(1),
        ui_generation: Some(generation),
        select_when_visible: Some("resolved-issue-slug".into()),
        outcome: Some(crate::ReplyOutcome::Accepted(wt_runtime::Stamp::now())),
        ..Default::default()
    });
    model.apply(snapshot(&["first", "second", "resolved-issue-slug"]));
    assert_eq!(model.selected_row().unwrap().key, "second");
    assert_eq!(model.board.rows.len(), 3);
    assert!(model.follow_up.is_none());
}

#[test]
fn failed_or_expired_creation_does_not_acknowledge_a_late_plan() {
    let mut model = Model::default();
    model.apply(snapshot(&["old"]));
    model.begin_action(
        1,
        &UiAction::Create {
            placement: None,
            input: "new".into(),
        },
    );
    model.reply(UiReply {
        operation_id: Some(1),
        failed: true,
        ..Default::default()
    });
    assert_eq!(model.board.rows.len(), 1);
    let (ready, mut received) = tokio::sync::oneshot::channel();
    model.reply(UiReply {
        operation_id: Some(1),
        creation_prepared: Some(crate::CreationPrepared {
            already_exists: false,
            key: "new".into(),
            ready,
        }),
        ..Default::default()
    });
    assert!(matches!(
        received.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Closed)
    ));
    assert_eq!(model.selected_row().unwrap().key, "old");
}

#[test]
fn failed_creation_cannot_replace_a_new_prompt_on_the_same_selected_row() {
    let mut model = Model::default();
    model.apply(snapshot(&["old"]));
    model.begin_action(
        1,
        &UiAction::Create {
            placement: None,
            input: "first".into(),
        },
    );
    let generation = model.ui_generation;
    model.input(KeyEvent::from(KeyCode::Char('n')), 20);
    assert!(model.paste("second"));
    model.reply(UiReply {
        operation_id: Some(1),
        ui_generation: Some(generation),
        failed: true,
        modal: Some(UiModal::Text {
            action: TextAction::Create { placement: None },
            prompt: "retry".into(),
            initial: "first".into(),
            allow_empty: false,
        }),
        ..Default::default()
    });
    assert!(
        matches!(&model.interaction, Interaction::Text(prompt) if prompt.editor.text() == "second")
    );
}

#[test]
fn paste_invalidates_pending_presentation_but_ignored_paste_does_not() {
    let mut model = Model::default();
    model.input(KeyEvent::from(KeyCode::Char('n')), 20);
    model.begin_action(
        1,
        &UiAction::PrepareCreate {
            placement: None,
            initial: String::new(),
        },
    );
    let generation = model.ui_generation;
    assert!(!model.paste(""));
    assert_eq!(model.ui_generation, generation);
    assert!(model.pending_request.is_some());
    assert!(model.paste("typed"));
    assert!(model.pending_request.is_none());
    model.reply(UiReply {
        operation_id: Some(1),
        ui_generation: Some(generation),
        modal: Some(UiModal::Text {
            action: TextAction::Create { placement: None },
            prompt: "stale".into(),
            initial: "stale".into(),
            allow_empty: false,
        }),
        ..Default::default()
    });
    assert!(
        matches!(&model.interaction, Interaction::Text(prompt) if prompt.editor.text() == "typed")
    );
}

#[test]
fn creation_completion_does_not_close_newer_history_view() {
    let mut model = Model::default();
    model.apply(snapshot(&["old"]));
    model.begin_action(
        1,
        &UiAction::Create {
            placement: None,
            input: "new".into(),
        },
    );
    let generation = model.ui_generation;
    bind_creation(&mut model, 1, "new");
    model.input(KeyEvent::from(KeyCode::Char('h')), 20);
    assert!(model.history.active);
    model.reply(UiReply {
        operation_id: Some(1),
        ui_generation: Some(generation),
        select_when_visible: Some("new".into()),
        outcome: Some(crate::ReplyOutcome::Accepted(wt_runtime::Stamp::now())),
        ..Default::default()
    });
    assert!(model.history.active);
}

#[test]
fn resolved_creation_of_existing_checkout_reuses_its_row_until_acceptance() {
    let mut model = Model::default();
    model.apply(snapshot(&["existing"]));
    model.begin_action(
        1,
        &UiAction::Create {
            placement: None,
            input: "issue-alias".into(),
        },
    );
    let deadline = model.next_deadline();
    bind_creation(&mut model, 1, "existing");
    assert_eq!(model.next_deadline(), deadline);
    assert_eq!(model.board.rows.len(), 1);
    assert!(model.selected_row().unwrap().pending_creation);
    assert_eq!(
        model.input(KeyEvent::from(KeyCode::F(12)), 20),
        InputResult::Draw
    );
    model.reply(UiReply {
        operation_id: Some(1),
        select_when_visible: Some("existing".into()),
        outcome: Some(crate::ReplyOutcome::Accepted(wt_runtime::Stamp::now())),
        ..Default::default()
    });
    assert_eq!(model.board.rows.len(), 1);
    assert!(!model.selected_row().unwrap().pending_creation);
}

fn board_with_status(state: Option<wt_core::WorkState>) -> SourceSnapshot<Board> {
    let mut snapshot = snapshot(&["one"]).data().unwrap().as_ref().clone();
    snapshot.rows[0].work = state.map(|state| crate::WorkPresentation {
        record: Some(wt_core::WorkStatusRecord::new(
            state,
            "2026-10-14T00:00:00Z",
        )),
        ..Default::default()
    });
    SourceSnapshot::test_live(Some(snapshot), SourceState::Ready, wt_core::Domain::Git)
}

#[test]
fn board_menus_open_without_a_request_and_commit_what_they_showed() {
    let mut model = Model::default();
    model.apply(board_with_status(Some(wt_core::WorkState::Review)));
    assert_eq!(
        model.input(KeyEvent::from(KeyCode::Char('u')), 20),
        InputResult::Draw
    );
    assert!(model.pending_request.is_none());
    let Interaction::Picker(picker) = &model.interaction else {
        panic!("status menu opens at once")
    };
    assert_eq!(picker.options[picker.selected].label, "review (current)");
    let InputResult::Action(action) = model.input(KeyEvent::from(KeyCode::Char('w')), 20) else {
        panic!()
    };
    assert_eq!(
        action,
        UiAction::SetStatus {
            key: "one".into(),
            state: Some("working".into()),
            note: None,
            verify_after_merge: None,
            seen: Some(crate::Seen(Some(crate::StatusClaim {
                state: "review".into(),
                note: None,
                verify_after_merge: None,
            }))),
        }
    );
}

#[test]
fn open_board_menus_rebuild_from_newer_boards_and_keep_their_choice() {
    let mut model = Model::default();
    model.apply(board_with_status(None));
    model.input(KeyEvent::from(KeyCode::Char('u')), 20);
    model.input(KeyEvent::from(KeyCode::Down), 20);
    model.input(KeyEvent::from(KeyCode::Down), 20);
    let chosen = |model: &Model| match &model.interaction {
        Interaction::Picker(picker) => (
            picker.options[picker.selected].value.clone(),
            picker.action.clone(),
        ),
        _ => panic!("menu stays open"),
    };
    let (before, _) = chosen(&model);
    model.apply(board_with_status(Some(wt_core::WorkState::Working)));
    let (after, action) = chosen(&model);
    assert_eq!(before, after);
    assert_eq!(
        action,
        PickerAction::Status {
            key: "one".into(),
            seen: Some(crate::Seen(Some(crate::StatusClaim {
                state: "working".into(),
                note: None,
                verify_after_merge: None,
            }))),
        }
    );
}

#[test]
fn stale_commits_reopen_their_menu_from_the_board() {
    let mut model = Model::default();
    model.apply(board_with_status(Some(wt_core::WorkState::Review)));
    model.input(KeyEvent::from(KeyCode::Char('u')), 20);
    let InputResult::Action(action) = model.input(KeyEvent::from(KeyCode::Char('w')), 20) else {
        panic!()
    };
    let generation = model.ui_generation;
    model.begin_action(5, &action);
    assert!(matches!(model.interaction, Interaction::None));
    model.apply(board_with_status(Some(wt_core::WorkState::Ready)));
    model.reply(UiReply {
        operation_id: Some(5),
        ui_generation: Some(generation),
        ..UiReply::stale("Changed since you looked: status is now ready")
    });
    let Interaction::Picker(picker) = &model.interaction else {
        panic!("the menu reopens")
    };
    assert_eq!(picker.options[picker.selected].label, "ready (current)");
    assert_eq!(model.toast.as_ref().unwrap().1, ToastTone::Failed);
    assert!(model.refresh_requested);
    // A stale reply for another request does not reopen anything.
    model.interaction = Interaction::None;
    model.reply(UiReply {
        operation_id: Some(6),
        ..UiReply::stale("other")
    });
    assert!(matches!(model.interaction, Interaction::None));
}

#[test]
fn an_untouched_board_menu_follows_the_newer_default() {
    let mut model = Model::default();
    model.apply(board_with_status(None));
    model.input(KeyEvent::from(KeyCode::Char('u')), 20);
    model.apply(board_with_status(Some(wt_core::WorkState::Review)));
    let Interaction::Picker(picker) = &model.interaction else {
        panic!("menu stays open")
    };
    // Nobody chose `todo`; it was only the older board's default.
    assert_eq!(picker.options[picker.selected].label, "review (current)");
    model.apply(board_with_status(Some(wt_core::WorkState::Ready)));
    let Interaction::Picker(picker) = &model.interaction else {
        panic!("menu stays open")
    };
    assert_eq!(picker.options[picker.selected].label, "ready (current)");
}

#[test]
fn an_unsent_request_leaves_nothing_for_the_next_one() {
    let mut model = Model::default();
    model.apply(snapshot(&["one"]));
    model.carry_typeahead = vec![(KeyEvent::from(KeyCode::Char('m')), 20)];
    model.committed_menu = Some(UiAction::PrepareStatus { key: "one".into() });
    let InputResult::Action(_) = model.input(KeyEvent::from(KeyCode::Char('!')), 20) else {
        panic!()
    };
    model.request_not_sent();
    let InputResult::Action(action) = model.input(KeyEvent::from(KeyCode::Char('!')), 20) else {
        panic!()
    };
    model.begin_action(9, &action);
    let request = model.pending_request.as_ref().unwrap();
    assert!(request.typeahead.is_empty());
    assert!(model.last_menu.is_none());
}

#[test]
fn enter_never_becomes_the_key_that_cancels_a_loading_menu() {
    let mut model = Model::default();
    model.apply(snapshot(&["one"]));
    model.last_key = Some(KeyEvent::from(KeyCode::Enter));
    model.begin_action(3, &UiAction::PrepareCleanup);
    // A double-tapped Enter is typeahead for the menu, not a cancel.
    assert_eq!(
        model.input(KeyEvent::from(KeyCode::Enter), 20),
        InputResult::Draw
    );
    assert_eq!(model.pending_request.as_ref().unwrap().typeahead.len(), 1);
}

#[test]
fn a_stale_reply_refreshes_even_after_the_user_moved_on() {
    let mut model = Model::default();
    model.apply(snapshot(&["one", "two"]));
    let generation = model.ui_generation;
    model.input(KeyEvent::from(KeyCode::Char('j')), 20);
    assert_ne!(model.ui_generation, generation);
    model.reply(UiReply {
        operation_id: Some(4),
        ui_generation: Some(generation),
        ..UiReply::stale("Changed since you looked: status is now ready")
    });
    assert!(model.refresh_requested);
}

#[test]
fn a_menu_reply_message_survives_dropped_typeahead() {
    let mut model = Model::default();
    model.apply(snapshot(&["one"]));
    let generation = model.ui_generation;
    model.begin_action(5, &UiAction::PrepareCleanup);
    model.input(KeyEvent::from(KeyCode::Char('y')), 20);
    model.reply(UiReply {
        operation_id: Some(5),
        ui_generation: Some(generation),
        ..UiReply::message("Nothing safe to clean")
    });
    let (text, _) = model.toast.clone().unwrap();
    assert!(text.starts_with("Nothing safe to clean"), "{text}");
}

#[test]
fn a_rebuild_keeps_the_status_row_chosen_among_rows_sharing_a_value() {
    // `ready` and `ready + verify after merge` both carry the value `ready`.
    let ready_with_steps = || {
        let mut snapshot = board_with_status(Some(wt_core::WorkState::Ready));
        let record = snapshot.test_data_mut().rows[0]
            .work
            .as_mut()
            .unwrap()
            .record
            .as_mut()
            .unwrap();
        record.verify_after_merge = Some("check prod".into());
        snapshot
    };
    let label = |model: &Model| match &model.interaction {
        Interaction::Picker(picker) => picker.options[picker.selected].label.clone(),
        _ => panic!("menu stays open"),
    };
    // Moved from plain `ready` down to the `a` row.
    let mut model = Model::default();
    model.apply(board_with_status(Some(wt_core::WorkState::Ready)));
    model.input(KeyEvent::from(KeyCode::Char('u')), 20);
    assert_eq!(label(&model), "ready (current)");
    model.input(KeyEvent::from(KeyCode::Down), 20);
    assert_eq!(label(&model), "ready + verify after merge");
    model.apply(board_with_status(Some(wt_core::WorkState::Ready)));
    assert_eq!(label(&model), "ready + verify after merge");
    // Moved from the current `a` row up to plain `ready`.
    let mut model = Model::default();
    model.apply(ready_with_steps());
    model.input(KeyEvent::from(KeyCode::Char('u')), 20);
    assert_eq!(label(&model), "ready + verify after merge (current)");
    model.input(KeyEvent::from(KeyCode::Up), 20);
    assert_eq!(label(&model), "ready");
    model.apply(ready_with_steps());
    assert_eq!(label(&model), "ready");
}

#[test]
fn a_rebuild_keeps_the_section_picker_on_the_last_target() {
    let mut model = Model::default();
    let board = || {
        let mut snapshot = grouped(&["one"], false);
        snapshot.test_data_mut().section_order =
            vec!["Batch".into(), "Next".into(), "Later".into()];
        snapshot
    };
    model.apply(board());
    model.last_section_target = Some(Some("Later".into()));
    model.input(KeyEvent::from(KeyCode::Char('l')), 20);
    let value = |model: &Model| match &model.interaction {
        Interaction::Picker(picker) => picker.options[picker.selected].value.clone(),
        _ => panic!("menu stays open"),
    };
    assert_eq!(value(&model).as_deref(), Some("Later"));
    // A snapshot arriving while the picker is open does not move the cursor.
    model.apply(board());
    assert_eq!(value(&model).as_deref(), Some("Later"));
}

#[test]
fn keys_past_the_typeahead_limit_are_reported_when_the_menu_opens() {
    let mut model = Model::default();
    model.apply(snapshot(&["one"]));
    let InputResult::Action(action) = model.input(KeyEvent::from(KeyCode::Char('!')), 20) else {
        panic!()
    };
    let generation = model.ui_generation;
    model.begin_action(42, &action);
    for _ in 0..=super::input::TYPEAHEAD_LIMIT {
        model.input(KeyEvent::from(KeyCode::Down), 20);
    }
    model.reply(UiReply {
        operation_id: Some(42),
        ui_generation: Some(generation),
        modal: Some(UiModal::Picker {
            action: PickerAction::Actions {
                surface: crate::ActionSurface::Row { key: "one".into() },
            },
            title: "Actions".into(),
            selected: 0,
            options: vec![PickerOption {
                value: Some("a".into()),
                label: "a".into(),
                chord: None,
                note: None,
                verify_after_merge: None,
                detail: None,
            }],
        }),
        ..Default::default()
    });
    assert!(matches!(model.interaction, Interaction::Picker(_)));
    assert_eq!(model.toast.as_ref().unwrap().1, ToastTone::Notice);
}

#[test]
fn archive_and_automation_toggles_carry_the_state_the_user_saw() {
    let mut model = Model::default();
    let mut shown = snapshot(&["one"]);
    shown.test_data_mut().rows[0].automations_paused = true;
    shown.test_data_mut().display.automations = true;
    model.apply(shown);
    assert_eq!(
        model.input(KeyEvent::from(KeyCode::Char('a')), 20),
        InputResult::Action(UiAction::ToggleArchive {
            key: "one".into(),
            seen: Some(Seen(false)),
        })
    );
    assert_eq!(
        model.input(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL), 20),
        InputResult::Action(UiAction::ToggleAutomations {
            key: Some("one".into()),
            seen: Some(Seen(true)),
        })
    );
    assert_eq!(
        model.input(KeyEvent::from(KeyCode::Char('A')), 20),
        InputResult::Action(UiAction::ToggleAutomations {
            key: None,
            seen: Some(Seen(false)),
        })
    );
}

#[test]
fn delayed_removal_reviews_survive_navigation_and_keep_each_host_and_witness() {
    for cancel_first in [true, false] {
        let mut model = Model::default();
        model.apply(snapshot(&["one", "two"]));
        model.ui_generation = 10;
        model.interaction = Interaction::Log {
            title: "Another menu".into(),
            lines: vec![],
            scroll: 0,
        };
        for (key, host) in [("one", None), ("two", Some("remote"))] {
            model.reply(UiReply {
                ui_generation: Some(1),
                modal_host: host.map(str::to_owned),
                modal: Some(UiModal::Confirm {
                    action: ConfirmAction::Remove {
                        key: key.into(),
                        force: true,
                        seen: None,
                        revision: Some(crate::RemovalRevision {
                            key: key.into(),
                            path: format!("/trees/{key}"),
                            branch: key.into(),
                            head: "head".into(),
                            digest: key.into(),
                            hazards: vec!["dirty".into()],
                            published_base: None,
                        }),
                    },
                    title: format!("Review {key}"),
                    lines: vec!["dirty".into()],
                    cancel_key: Some('d'),
                }),
                ..Default::default()
            });
        }
        assert!(
            matches!(&model.interaction, Interaction::Log { title, .. } if title == "Another menu")
        );
        assert_eq!(model.removal_reviews.len(), 2);
        model.input(KeyEvent::from(KeyCode::Esc), 20);
        assert!(
            matches!(&model.interaction, Interaction::Confirm(prompt) if prompt.title == "Review one")
        );
        // Cancelling the first review must neither discard nor approve the second.
        let result = model.input(
            KeyEvent::from(KeyCode::Char(if cancel_first { 'n' } else { 'y' })),
            20,
        );
        if !cancel_first {
            assert!(
                matches!(result, InputResult::Action(UiAction::Remove { ref key, .. }) if key == "one")
            );
        }
        assert!(
            matches!(&model.interaction, Interaction::Confirm(prompt) if prompt.title == "Review two")
        );
        assert_eq!(model.interaction_host.as_deref(), Some("remote"));
        let InputResult::Action(UiAction::OnHost { host, action }) =
            model.input(KeyEvent::from(KeyCode::Char('y')), 20)
        else {
            panic!("second review must target its own host");
        };
        assert_eq!(host.as_deref(), Some("remote"));
        assert!(
            matches!(*action, UiAction::Remove { ref key, revision: Some(ref revision), force: true, .. } if key == "two" && revision.digest == "two")
        );
        assert!(model.removal_reviews.is_empty());
    }
}

#[test]
fn create_captures_the_cursor_section_and_projects_below_its_row() {
    let mut model = Model::default();
    let mut initial = snapshot(&["one", "two"]);
    initial.test_data_mut().sections = vec![BoardSection {
        key: "Batch".into(),
        title: "Batch".into(),
        rows: vec![0, 1],
        ..Default::default()
    }];
    initial.test_data_mut().hosts = vec![
        crate::HostChoice {
            id: None,
            label: "local".into(),
            sign_in_needed: vec![],
        },
        crate::HostChoice {
            id: Some("box".into()),
            label: "box".into(),
            sign_in_needed: vec![],
        },
    ];
    model.apply(initial);
    model.input(KeyEvent::from(KeyCode::Char('n')), 20);
    model.input(KeyEvent::from(KeyCode::Tab), 20);
    model.paste("new");
    let InputResult::Action(action) = model.input(KeyEvent::from(KeyCode::Enter), 20) else {
        panic!("expected creation");
    };
    let UiAction::OnHost {
        host,
        action: inner,
    } = &action
    else {
        panic!("missing selected host");
    };
    assert_eq!(host.as_deref(), Some("box"));
    let UiAction::Create {
        placement: Some(placement),
        ..
    } = inner.as_ref()
    else {
        panic!("missing creation position");
    };
    assert_eq!(placement.section.as_deref(), Some("Batch"));
    assert_eq!(placement.after.as_deref(), Some("one"));
    let placement = placement.clone();
    model.begin_action(1, &action);
    let section = model
        .board
        .sections
        .iter()
        .find(|section| section.key == "Batch")
        .unwrap();
    assert_eq!(
        section
            .rows
            .iter()
            .map(|&index| model.board.rows[index].title.as_str())
            .collect::<Vec<_>>(),
        ["one", "new", "two"]
    );
    let key = wt_core::remote_worktree_ledger_key("box", "new");
    bind_creation(&mut model, 1, &key);
    model.reply(UiReply {
        operation_id: Some(1),
        creation_placement: Some(crate::CreationPlacementAccepted {
            key,
            placement: placement.clone(),
            accepted_at: wt_runtime::Stamp::now(),
        }),
        ..Default::default()
    });
    assert_eq!(
        model.pending.mutation(1).unwrap().target.host.as_deref(),
        Some("box")
    );
    let layout = model
        .pending
        .mutation(2)
        .expect("layout has its own pending mutation");
    assert_eq!(layout.target.host, None);
    assert!(matches!(
        layout.kind,
        crate::MutationKind::Layout(crate::LayoutChange::Creation { .. })
    ));
}

#[test]
fn creation_uses_inbox_and_a_folded_section_header_without_guessing_an_anchor() {
    for section_key in [INBOX_SECTION, "Batch"] {
        let mut model = Model::default();
        let mut source = snapshot(&["one"]);
        source.test_data_mut().sections = vec![BoardSection {
            key: section_key.into(),
            title: "Group".into(),
            rows: vec![0],
            folded: true,
            ..Default::default()
        }];
        model.apply(source);
        model.input(KeyEvent::from(KeyCode::Char('n')), 20);
        model.paste("new");
        let InputResult::Action(UiAction::Create {
            placement: Some(placement),
            ..
        }) = model.input(KeyEvent::from(KeyCode::Enter), 20)
        else {
            panic!("missing header placement");
        };
        assert_eq!(
            placement.section.as_deref(),
            (section_key != INBOX_SECTION).then_some(section_key)
        );
        assert_eq!(placement.after, None);
        model.begin_action(
            1,
            &UiAction::Create {
                input: "new".into(),
                placement: Some(placement),
            },
        );
        let section = model
            .board
            .sections
            .iter()
            .find(|section| section.key == section_key)
            .unwrap();
        assert!(!section.folded);
        assert_eq!(model.board.rows[section.rows[0]].title, "new");
    }
}

#[test]
fn resolving_an_existing_worktree_does_not_move_it_into_the_create_section() {
    let mut model = Model::default();
    let mut source = snapshot(&["origin", "existing"]);
    source.test_data_mut().sections = vec![
        BoardSection {
            key: "Batch".into(),
            title: "Batch".into(),
            rows: vec![0],
            ..Default::default()
        },
        BoardSection {
            key: "Other".into(),
            title: "Other".into(),
            rows: vec![1],
            ..Default::default()
        },
    ];
    model.apply(source);
    model.begin_action(
        1,
        &UiAction::Create {
            input: "existing".into(),
            placement: Some(crate::CreatePlacement {
                section: Some("Batch".into()),
                after: Some("origin".into()),
            }),
        },
    );
    let (ready, mut received) = tokio::sync::oneshot::channel();
    model.reply(UiReply {
        operation_id: Some(1),
        creation_prepared: Some(crate::CreationPrepared {
            key: "existing".into(),
            already_exists: true,
            ready,
        }),
        ..Default::default()
    });
    assert_eq!(received.try_recv(), Ok(()));
    let other = model
        .board
        .sections
        .iter()
        .find(|section| section.key == "Other")
        .unwrap();
    assert_eq!(
        other
            .rows
            .iter()
            .map(|&index| model.board.rows[index].key.as_str())
            .collect::<Vec<_>>(),
        ["existing"]
    );
}

#[test]
fn z_opens_the_recorded_guide_only_when_guides_are_enabled() {
    let model = |guides: bool, guide: Option<crate::GuideView>| Model {
        board: std::sync::Arc::new(Board {
            rows: vec![BoardRow {
                key: "one".into(),
                guide,
                ..BoardRow::default()
            }],
            display: crate::DisplayPolicy {
                guides,
                ..Default::default()
            },
            ..Board::default()
        }),
        selected: Some(0),
        ..Model::default()
    };
    let guide = || {
        Some(crate::GuideView {
            url: "https://guides.example/g".into(),
            current: Some(false),
        })
    };
    let z = || KeyEvent::from(KeyCode::Char('z'));
    assert_eq!(
        model(true, guide()).input(z(), 20),
        InputResult::Action(UiAction::OpenUrl {
            key: "one".into(),
            kind: crate::UrlKind::Guide,
        })
    );
    let mut missing = model(true, None);
    assert_ne!(
        missing.input(z(), 20),
        InputResult::Action(UiAction::OpenUrl {
            key: "one".into(),
            kind: crate::UrlKind::Guide,
        })
    );
    assert_eq!(model(false, guide()).input(z(), 20), InputResult::Unchanged);
}

#[test]
fn empty_named_section_is_a_stop_and_shift_d_deletes_with_a_witness() {
    let key = KeyEvent::new(KeyCode::Char('D'), KeyModifiers::SHIFT);
    let mut model = Model::default();
    let mut board = grouped(&["one"], false);
    board.test_data_mut().sections.push(BoardSection {
        key: "Later".into(),
        title: "Later".into(),
        ..Default::default()
    });
    model.apply(board);
    // The row, then the empty section's own line.
    assert_eq!(model.item_count(), 2);
    model.input(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), 20);
    assert_eq!(model.selected_section().unwrap().key, "Later");
    // Nothing to lose: deleting an empty section needs no confirmation.
    assert_eq!(
        model.input(key, 20),
        InputResult::Action(UiAction::DeleteSection {
            section: "Later".into(),
            seen: Seen(vec![]),
        })
    );
    // A section with rows confirms first, naming the rows it will move.
    model.input(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), 20);
    assert_eq!(model.input(key, 20), InputResult::Draw);
    let Interaction::Confirm(confirm) = &model.interaction else {
        panic!("deleting a section with rows should confirm")
    };
    assert_eq!(
        confirm.action,
        ConfirmAction::DeleteSection {
            section: "Batch".into(),
            seen: Seen(vec!["one".into()]),
        }
    );
    model.input(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), 20);
    // An empty section has nothing to fold and only its name to copy.
    model.input(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), 20);
    assert_eq!(
        model.input(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), 20),
        InputResult::Draw
    );
    assert_eq!(
        model.toast.as_ref().map(|(text, _)| text.as_str()),
        Some("nothing to fold in an empty section")
    );
    assert_eq!(
        model.yank_choices(),
        vec![('n', "section", "Later".to_owned())]
    );
    // The Inbox is wt's, not the user's.
    let mut inbox = grouped(&["one"], false);
    inbox.test_data_mut().sections[0].key = "\0inbox".into();
    inbox.test_data_mut().sections[0].title = "Inbox".into();
    model.apply(inbox);
    assert_eq!(model.input(key, 20), InputResult::Draw);
    assert_eq!(
        model.toast.as_ref().map(|(text, _)| text.as_str()),
        Some("Inbox is not a section you can delete")
    );
}

#[test]
fn a_section_witness_counts_only_rows_a_delete_would_move() {
    let mut board = grouped(&["live", "archived", "removing", "creating"], false);
    {
        let board = board.test_data_mut();
        board.rows[1].archived = true;
        board.rows[2].removal = Some(Default::default());
        board.rows[3].pending_creation = true;
    }
    assert_eq!(board.data().unwrap().section_witness("Batch"), ["live"]);
}
