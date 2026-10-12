use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use wt_runtime::{SourceSnapshot, SourceState};

use super::*;
use crate::{
    ConfirmAction, PickerAction, PickerOption, SessionTarget, TextAction, UiAction, UiModal,
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
fn delayed_modal_and_background_completion_preserve_newer_input() {
    let mut model = Model::default();
    model.apply(snapshot(&["one"]));
    model.input(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE), 20);
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
    assert_eq!(
        model.input(key('l'), 20),
        InputResult::Action(UiAction::PrepareSection { key: "one".into() })
    );
    model.reply(UiReply {
        modal: Some(UiModal::Picker {
            action: PickerAction::Section { key: "one".into() },
            title: "Section".into(),
            options: vec![],
            selected: 0,
        }),
        ..Default::default()
    });
    assert_eq!(model.input(key('n'), 20), InputResult::Draw);
    model.paste("Release");
    assert_eq!(
        model.input(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), 20),
        InputResult::Action(UiAction::MoveSection {
            key: "one".into(),
            section: Some("Release".into())
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
            action: PickerAction::Section { key: "two".into() },
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
            section: Some("Release".into())
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

#[test]
fn pending_creation_queues_f12_and_selects_the_real_row() {
    let mut model = Model::default();
    model.apply(snapshot(&["old"]));
    let generation = model.ui_generation;
    model.begin_action(
        1,
        &UiAction::Create {
            input: "new".into(),
        },
    );
    assert!(model.selected_row().unwrap().key.starts_with("\0creating:"));
    assert_eq!(
        model.input(KeyEvent::from(KeyCode::Char('d')), 20),
        InputResult::Draw
    );
    assert_eq!(
        model.input(KeyEvent::from(KeyCode::F(12)), 20),
        InputResult::Draw
    );
    model.reply(UiReply {
        operation_id: Some(1),
        ui_generation: Some(generation),
        select_when_visible: Some("new".into()),
        outcome: Some(crate::ReplyOutcome::Accepted(wt_runtime::Stamp::now())),
        ..Default::default()
    });
    assert_eq!(
        model.follow_up,
        Some(UiAction::OnHost {
            host: None,
            action: Box::new(UiAction::Session {
                key: Some("new".into()),
                target: SessionTarget::Harness
            })
        })
    );
    model.apply(snapshot(&["old", "new"]));
    assert_eq!(model.selected_row().unwrap().key, "new");
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
    assert_eq!(model.input(key('j'), 10), InputResult::Draw);
    assert_eq!(model.help_scroll, 3);
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
    assert!(
        crate::help::filtered_lines(&model.help_query.text())
            .iter()
            .any(|line| line.contains("Refresh"))
    );
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
            input: "ENG-123 --attach --gh 12 --base origin/main".into()
        })
    );
    model.input(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::SHIFT), 20);
    assert_eq!(
        model.input(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), 20),
        InputResult::Action(UiAction::Create {
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
        InputResult::Action(UiAction::PrepareRemove {
            key: "second".into()
        })
    );
    model.reply(UiReply {
        modal: Some(UiModal::Confirm {
            action: ConfirmAction::Remove {
                key: "second".into(),
                force: true,
                revision: removal_revision("second"),
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
                revision: removal_revision("second"),
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
            revision: removal_revision("second"),
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
            action: PickerAction::Status { key: "row".into() },
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
        })
    );
    model.input(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE), 20);
    model.reply(UiReply {
        modal: Some(UiModal::Picker {
            action: PickerAction::Status { key: "row".into() },
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
        })
    );
    model.input(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE), 20);
    model.reply(UiReply {
        modal: Some(UiModal::Picker {
            action: PickerAction::Status { key: "row".into() },
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
