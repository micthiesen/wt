//! Key sequence -> action/selection/toast pins for TS interaction parity.
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use wt_runtime::{SourceSnapshot, SourceState};

use super::*;
use crate::{ActionSurface, SessionMode, SessionSelection};

fn press(model: &mut Model, code: KeyCode) -> InputResult {
    model.input(KeyEvent::from(code), 20)
}

fn press_with(model: &mut Model, code: KeyCode, modifiers: KeyModifiers) -> InputResult {
    model.input(KeyEvent::new(code, modifiers), 20)
}

fn ch(model: &mut Model, letter: char) -> InputResult {
    press(model, KeyCode::Char(letter))
}

fn toast(model: &Model) -> Option<&str> {
    model.toast.as_ref().map(|(text, _)| text.as_str())
}

fn board(keys: &[&str]) -> Board {
    Board {
        rows: keys
            .iter()
            .map(|key| BoardRow {
                key: (*key).into(),
                slug: (*key).into(),
                title: (*key).into(),
                branch: format!("branch-{key}"),
                ..BoardRow::default()
            })
            .collect(),
        ..Board::default()
    }
}

fn ready(board: Board) -> SourceSnapshot<Board> {
    SourceSnapshot {
        data: Some(Arc::new(board)),
        state: SourceState::Ready,
        updated_at: None,
        revision: 0,
    }
}

fn section(key: &str, rows: Vec<usize>, folded: bool) -> BoardSection {
    BoardSection {
        key: key.into(),
        title: key.into(),
        folded,
        rows,
        ..Default::default()
    }
}

fn model_with(keys: &[&str]) -> Model {
    let mut model = Model::default();
    model.apply(ready(board(keys)));
    model
}

fn selection(harness: wt_core::HarnessId, mode: SessionMode, live: bool) -> SessionSelection {
    SessionSelection {
        key: Some("one".into()),
        target: SessionTarget::Harness,
        harness,
        session_id: (mode == SessionMode::Resume).then(|| "id".into()),
        managed_name: None,
        mode,
        live,
    }
}

fn option(index: usize, label: &str) -> PickerOption {
    PickerOption {
        value: Some(index.to_string()),
        label: label.into(),
        chord: None,
        note: None,
        verify_after_merge: None,
        detail: None,
    }
}

/// Sessions picker: live Claude, dead Codex, then the three `New` rows.
fn open_sessions(model: &mut Model) -> Vec<SessionSelection> {
    use wt_core::HarnessId::*;
    let choices = vec![
        selection(Claude, SessionMode::Resume, true),
        selection(Codex, SessionMode::Resume, false),
        selection(Claude, SessionMode::New, false),
        selection(Codex, SessionMode::New, false),
        selection(Opencode, SessionMode::New, false),
    ];
    let options = (0..choices.len()).map(|i| option(i, "row")).collect();
    model.reply(UiReply {
        modal: Some(UiModal::Picker {
            action: PickerAction::Sessions {
                choices: choices.clone(),
            },
            title: "Sessions".into(),
            options,
            selected: 0,
        }),
        ..Default::default()
    });
    choices
}

fn picker_selected(model: &Model) -> Option<usize> {
    match &model.interaction {
        Interaction::Picker(picker) => Some(picker.selected),
        _ => None,
    }
}

#[test]
fn sessions_d_closes_live_directly_and_refuses_dead_rows() {
    let mut model = model_with(&["one"]);
    let choices = open_sessions(&mut model);
    assert_eq!(
        ch(&mut model, 'd'),
        InputResult::Action(UiAction::StopSession {
            selection: choices[0].clone()
        })
    );
    assert!(matches!(model.interaction, Interaction::None));

    let mut model = model_with(&["one"]);
    open_sessions(&mut model);
    ch(&mut model, 'j');
    assert_eq!(ch(&mut model, 'd'), InputResult::Draw);
    assert_eq!(toast(&model), Some("session isn't live, nothing to close"));
    assert_eq!(picker_selected(&model), Some(1));
}

#[test]
fn sessions_x_kills_session_rows_and_jumps_elsewhere() {
    let mut model = model_with(&["one"]);
    let choices = open_sessions(&mut model);
    assert_eq!(
        ch(&mut model, 'x'),
        InputResult::Action(UiAction::KillSession {
            selection: choices[0].clone()
        })
    );

    // A dead Codex row cannot be killed from wt.
    let mut model = model_with(&["one"]);
    open_sessions(&mut model);
    ch(&mut model, 'j');
    ch(&mut model, 'x');
    assert_eq!(
        toast(&model),
        Some("Codex session is dead; remove via codex CLI")
    );
    assert_eq!(picker_selected(&model), Some(1));

    // On a `New` row the harness letters move the cursor without
    // committing: `o` lands on new OpenCode, `x` on new Codex.
    ch(&mut model, 'o');
    assert_eq!(picker_selected(&model), Some(4));
    assert_eq!(ch(&mut model, 'x'), InputResult::Draw);
    assert_eq!(picker_selected(&model), Some(3));
    ch(&mut model, 'c');
    assert_eq!(picker_selected(&model), Some(2));
}

#[test]
fn sessions_digits_count_session_rows_and_space_never_confirms() {
    let mut model = model_with(&["one"]);
    let choices = open_sessions(&mut model);
    assert_eq!(ch(&mut model, ' '), InputResult::Unchanged);
    assert!(matches!(model.interaction, Interaction::Picker(_)));
    // `3` would be the new-Claude row by position; only two session rows
    // exist, so it does nothing.
    assert_eq!(ch(&mut model, '3'), InputResult::Unchanged);
    assert_eq!(
        ch(&mut model, '2'),
        InputResult::Action(UiAction::SelectSession {
            selection: choices[1].clone()
        })
    );
}

#[test]
fn new_claude_name_prompt_allows_auto_name_filters_and_backs_out() {
    let mut model = model_with(&["one"]);
    open_sessions(&mut model);
    ch(&mut model, 'c');
    press(&mut model, KeyCode::Enter);
    assert!(matches!(model.interaction, Interaction::Text(_)));
    // Only the Claude name charset is accepted.
    ch(&mut model, '!');
    ch(&mut model, 'a');
    let Interaction::Text(prompt) = &model.interaction else {
        panic!("name prompt closed");
    };
    assert_eq!(prompt.editor.text(), "a");
    press(&mut model, KeyCode::Backspace);
    // Backspace on empty input returns to the sessions picker.
    assert_eq!(
        press(&mut model, KeyCode::Backspace),
        InputResult::Action(UiAction::PrepareSessions {
            key: Some("one".into()),
            target: SessionTarget::Harness,
        })
    );

    let mut model = model_with(&["one"]);
    open_sessions(&mut model);
    ch(&mut model, 'c');
    press(&mut model, KeyCode::Enter);
    let InputResult::Action(UiAction::SelectSession { selection }) =
        press(&mut model, KeyCode::Enter)
    else {
        panic!("empty name did not start a session");
    };
    assert_eq!(selection.managed_name, None);
    assert_eq!(selection.mode, SessionMode::New);
}

#[test]
fn new_section_prompt_esc_returns_to_the_section_picker() {
    let mut model = model_with(&["one"]);
    model.reply(UiReply {
        modal: Some(UiModal::Picker {
            action: PickerAction::Section { key: "one".into() },
            title: "Section".into(),
            options: vec![option(0, "Inbox")],
            selected: 0,
        }),
        ..Default::default()
    });
    ch(&mut model, 'n');
    assert_eq!(
        press(&mut model, KeyCode::Esc),
        InputResult::Action(UiAction::PrepareSection { key: "one".into() })
    );
}

#[test]
fn confirm_cancels_on_n() {
    let mut model = model_with(&["one"]);
    model.reply(UiReply {
        modal: Some(UiModal::Confirm {
            action: ConfirmAction::HardRefresh,
            title: "Clear?".into(),
            lines: vec![],
            cancel_key: None,
        }),
        ..Default::default()
    });
    assert_eq!(ch(&mut model, 'n'), InputResult::Draw);
    assert!(matches!(model.interaction, Interaction::None));
}

#[test]
fn palettes_confirm_on_their_own_opener_and_ignore_digits() {
    for (surface, opener) in [
        (ActionSurface::Manager { key: None }, 'M'),
        (
            ActionSurface::Slot {
                target: SessionTarget::WtSource,
            },
            '<',
        ),
        (ActionSurface::Row { key: "one".into() }, '!'),
    ] {
        let mut model = model_with(&["one"]);
        model.reply(UiReply {
            modal: Some(UiModal::Picker {
                action: PickerAction::Actions {
                    surface: surface.clone(),
                },
                title: "Commands".into(),
                options: vec![PickerOption {
                    value: Some("digest".into()),
                    label: "Digest".into(),
                    chord: Some('d'),
                    note: None,
                    verify_after_merge: None,
                    detail: None,
                }],
                selected: 0,
            }),
            ..Default::default()
        });
        assert_eq!(ch(&mut model, '1'), InputResult::Unchanged);
        assert_eq!(
            ch(&mut model, opener),
            InputResult::Action(UiAction::PrepareAction {
                surface,
                id: "digest".into(),
                arg: None,
            })
        );
    }
}

#[test]
fn shift_f12_opens_the_harness_picker_and_letters_or_f12_commit() {
    let mut model = model_with(&["one"]);
    assert_eq!(
        press_with(&mut model, KeyCode::F(12), KeyModifiers::SHIFT),
        InputResult::Action(UiAction::PrepareHarnesses { key: "one".into() })
    );
    let harness_picker = || UiReply {
        modal: Some(UiModal::Picker {
            action: PickerAction::Harness { key: "one".into() },
            title: "Start agent".into(),
            options: ["claude", "codex", "opencode"]
                .into_iter()
                .zip(['c', 'x', 'o'])
                .map(|(value, chord)| PickerOption {
                    value: Some(value.into()),
                    label: value.into(),
                    chord: Some(chord),
                    note: None,
                    verify_after_merge: None,
                    detail: None,
                })
                .collect(),
            selected: 0,
        }),
        ..Default::default()
    };
    model.reply(harness_picker());
    assert_eq!(
        ch(&mut model, 'x'),
        InputResult::Action(UiAction::EnterHarness {
            key: "one".into(),
            harness: wt_core::HarnessId::Codex,
        })
    );
    model.reply(harness_picker());
    ch(&mut model, 'j');
    ch(&mut model, 'j');
    assert_eq!(
        press_with(&mut model, KeyCode::F(12), KeyModifiers::SHIFT),
        InputResult::Action(UiAction::EnterHarness {
            key: "one".into(),
            harness: wt_core::HarnessId::Opencode,
        })
    );
}

#[test]
fn reorder_across_sections_keeps_the_cursor_on_the_moved_row() {
    let mut model = Model::default();
    let mut first = board(&["a", "b", "c"]);
    first.sections = vec![
        section("One", vec![0, 1], false),
        section("Two", vec![2], false),
    ];
    model.apply(ready(first.clone()));
    ch(&mut model, 'j');
    assert_eq!(model.selected_row().unwrap().key, "b");
    assert!(matches!(
        ch(&mut model, 'J'),
        InputResult::Action(UiAction::Reorder { .. })
    ));
    let mut moved = first;
    moved.sections = vec![
        section("One", vec![0], false),
        section("Two", vec![1, 2], false),
    ];
    model.apply(ready(moved));
    assert_eq!(model.selected_row().unwrap().key, "b");
}

#[test]
fn tab_unfold_lands_on_the_first_row_and_rows_outside_sections_toast() {
    let mut model = Model::default();
    let mut folded = board(&["a", "b"]);
    folded.sections = vec![section("One", vec![0, 1], true)];
    model.apply(ready(folded.clone()));
    assert!(matches!(
        model.selected_item(),
        Some(VisualItem::Section(0))
    ));
    assert_eq!(
        press(&mut model, KeyCode::Tab),
        InputResult::Action(UiAction::FoldSection {
            key: "One".into(),
            folded: false,
        })
    );
    let mut open = folded;
    open.sections[0].folded = false;
    model.apply(ready(open));
    assert_eq!(model.selected_row().unwrap().key, "a");

    let mut model = model_with(&["a"]);
    assert_eq!(press(&mut model, KeyCode::Tab), InputResult::Draw);
    assert_eq!(toast(&model), Some("no section here to fold"));
}

#[test]
fn n_is_always_local_and_ctrl_n_needs_a_remote() {
    let mut remote = board(&["one"]);
    remote.hosts = vec![
        HostChoice {
            id: None,
            label: "local".into(),
        },
        HostChoice {
            id: Some("builder".into()),
            label: "builder".into(),
        },
    ];
    let mut model = Model::default();
    model.apply(ready(remote));
    assert_eq!(ch(&mut model, 'n'), InputResult::Draw);
    assert!(matches!(
        &model.interaction,
        Interaction::Text(TextPrompt {
            action: TextAction::Create,
            ..
        })
    ));
    press(&mut model, KeyCode::Esc);
    assert_eq!(
        press_with(&mut model, KeyCode::Char('n'), KeyModifiers::CONTROL),
        InputResult::Action(UiAction::PrepareCreate {
            initial: String::new()
        })
    );

    let mut model = model_with(&["one"]);
    assert_eq!(
        press_with(&mut model, KeyCode::Char('n'), KeyModifiers::CONTROL),
        InputResult::Draw
    );
    assert_eq!(toast(&model), Some("[remote] is not configured"));
}

#[test]
fn esc_returns_output_focus_and_o_opens_the_main_clone() {
    let mut model = model_with(&["one"]);
    ch(&mut model, '"');
    assert_ne!(model.output.target, crate::output::OutputTarget::Attention);
    assert_eq!(press(&mut model, KeyCode::Esc), InputResult::Draw);
    assert_eq!(model.output.target, crate::output::OutputTarget::Attention);
    assert_eq!(
        ch(&mut model, 'O'),
        InputResult::Action(UiAction::OpenSlotEditor {
            target: SessionTarget::Main
        })
    );
    model.history.active = true;
    assert_eq!(
        ch(&mut model, 'O'),
        InputResult::Action(UiAction::OpenSlotEditor {
            target: SessionTarget::Main
        })
    );
}

fn review_board() -> Board {
    let mut value = board(&["a"]);
    value.sections = vec![section("Inbox", vec![0], false)];
    value.review_requests = (1..=3)
        .map(|number| ReviewRequestRow {
            url: format!("https://github.com/a/b/pull/{number}"),
            number,
            updated_at: "rev".into(),
            branch: format!("review-{number}"),
            ..Default::default()
        })
        .collect();
    value
}

#[test]
fn review_rows_open_at_pr_target_arm_chords_and_dismiss_to_the_next() {
    let mut model = Model::default();
    model.apply(ready(review_board()));
    ch(&mut model, 'j');
    ch(&mut model, 'j');
    assert_eq!(model.selected_review().unwrap().number, 2);
    assert_eq!(
        press(&mut model, KeyCode::Enter),
        InputResult::Action(UiAction::OpenPrDefault {
            url: "https://github.com/a/b/pull/2".into()
        })
    );
    assert_eq!(ch(&mut model, 'l'), InputResult::Unchanged);
    assert_eq!(
        ch(&mut model, 'p'),
        InputResult::Action(UiAction::OpenPrLink {
            url: "https://github.com/a/b/pull/2".into(),
            linear: true,
        })
    );
    assert!(matches!(
        ch(&mut model, 'd'),
        InputResult::Action(UiAction::OnHost { .. })
    ));
    let mut dismissed = review_board();
    dismissed.review_requests.remove(1);
    model.apply(ready(dismissed));
    assert_eq!(model.selected_review().unwrap().number, 3);
}

#[test]
fn review_fold_keeps_the_cursor_on_the_header() {
    let mut model = Model::default();
    model.apply(ready(review_board()));
    ch(&mut model, 'j');
    assert_eq!(press(&mut model, KeyCode::Tab), InputResult::Draw);
    assert!(matches!(
        model.selected_item(),
        Some(VisualItem::ReviewHeader)
    ));
    press(&mut model, KeyCode::Tab);
    assert_eq!(model.selected_review().unwrap().number, 1);
}

#[test]
fn verification_steps_are_per_row_and_toast_when_absent() {
    let mut value = board(&["a", "b"]);
    value.rows[0].verify_steps = Some("check prod".into());
    value.rows[0].work = Some(WorkPresentation {
        verification_owed: true,
        ..Default::default()
    });
    let mut model = Model::default();
    model.apply(ready(value));
    assert!(model.show_verification, "owed steps start open");
    ch(&mut model, 'V');
    assert!(!model.show_verification);
    ch(&mut model, 'j');
    assert!(!model.show_verification);
    assert_eq!(ch(&mut model, 'V'), InputResult::Draw);
    assert_eq!(
        toast(&model),
        Some("no verify-after-merge steps on this row")
    );
    ch(&mut model, 'k');
    assert!(model.show_verification, "returning resets to the default");
}

#[test]
fn perf_i_investigates_and_c_toggles_continuous() {
    let mut model = model_with(&["a"]);
    ch(&mut model, 'P');
    assert_eq!(ch(&mut model, 'i'), InputResult::Draw);
    assert_eq!(toast(&model), Some("no perf sample yet"));
    let mut value = board(&["a"]);
    value.perf = vec!["cpu 90%".into()];
    model.apply(ready(value));
    assert_eq!(
        ch(&mut model, 'i'),
        InputResult::Action(UiAction::PerfInvestigate {
            report: vec!["cpu 90%".into()]
        })
    );
    assert_eq!(
        ch(&mut model, 'c'),
        InputResult::Action(UiAction::SetPerf {
            active: true,
            continuous: true,
            refresh: false,
        })
    );
}

#[test]
fn feedback_toasts_replace_silent_no_ops() {
    let mut model = model_with(&["a"]);
    assert_eq!(ch(&mut model, ' '), InputResult::Draw);
    assert_eq!(toast(&model), Some("nothing needs you"));
    ch(&mut model, 'p');
    assert_eq!(toast(&model), Some("no PR for this branch"));
    ch(&mut model, 's');
    assert_eq!(
        toast(&model),
        Some("no stage deployed or dev server running")
    );
    ch(&mut model, 'x');
    assert_eq!(toast(&model), Some("attention feed is empty"));
    ch(&mut model, 'A');
    assert_eq!(toast(&model), Some("no [[automations]] configured"));

    let mut model = Model::default();
    assert_eq!(ch(&mut model, '!'), InputResult::Draw);
    assert_eq!(toast(&model), Some("select a worktree first"));
}

#[test]
fn archive_refuses_a_locked_row() {
    let mut value = board(&["a"]);
    value.rows[0].busy = Some(BusyView {
        op: "remove".into(),
        label: "removing".into(),
        age: None,
    });
    let mut model = Model::default();
    model.apply(ready(value));
    assert_eq!(ch(&mut model, 'a'), InputResult::Draw);
    assert_eq!(
        toast(&model),
        Some("a is removing; can't change archive state")
    );
}

#[test]
fn yank_keeps_unavailable_entries_in_place() {
    let mut value = board(&["a"]);
    value.rows[0].stage_name = Some("dev-a".into());
    let mut model = Model::default();
    model.apply(ready(value));
    ch(&mut model, 'y');
    let letters: Vec<_> = model.yank_choices().iter().map(|(key, ..)| *key).collect();
    assert_eq!(letters, ['b', 's', 'S', 'd', 'p', 'n', 'i', 'I', 'r']);
    assert_eq!(
        ch(&mut model, 's'),
        InputResult::Action(UiAction::Copy {
            value: "dev-a".into(),
            label: "stage name".into(),
        })
    );
    ch(&mut model, 'y');
    assert_eq!(ch(&mut model, '3'), InputResult::Draw);
    assert_eq!(toast(&model), Some("nothing to yank"));
}

#[test]
fn status_note_starts_empty() {
    let mut model = model_with(&["a"]);
    model.reply(UiReply {
        modal: Some(UiModal::Picker {
            action: PickerAction::Status { key: "a".into() },
            title: "Status".into(),
            options: vec![PickerOption {
                value: Some("working".into()),
                label: "working".into(),
                chord: Some('w'),
                note: Some("old note".into()),
                verify_after_merge: None,
                detail: None,
            }],
            selected: 0,
        }),
        ..Default::default()
    });
    ch(&mut model, 'm');
    let Interaction::Text(prompt) = &model.interaction else {
        panic!("note prompt missing");
    };
    assert_eq!(prompt.editor.text(), "");
}

#[test]
fn live_log_closes_on_its_key_and_refresh_keeps_scroll() {
    let mut model = model_with(&["a"]);
    let refresh = UiAction::PrepareAction {
        surface: ActionSurface::Row { key: "a".into() },
        id: "dev-logs".into(),
        arg: None,
    };
    let log = |lines: usize| UiReply {
        modal: Some(UiModal::Log {
            title: "a dev logs".into(),
            lines: (0..lines).map(|line| line.to_string()).collect(),
            close_key: Some('l'),
            refresh: Some(Box::new(refresh.clone())),
        }),
        ..Default::default()
    };
    model.reply(log(50));
    ch(&mut model, 'j');
    ch(&mut model, 'j');
    model.reply(log(60));
    assert!(matches!(
        model.interaction,
        Interaction::Log { scroll: 6, .. }
    ));
    assert_eq!(model.log_refresh, Some(refresh));
    ch(&mut model, 'l');
    assert!(matches!(model.interaction, Interaction::None));
    assert!(model.log_refresh.is_none());
}

#[test]
fn edges_scroll_the_list_and_wheel_never_moves_the_cursor() {
    let mut model = model_with(&["a", "b"]);
    ch(&mut model, 'j');
    assert_eq!(ch(&mut model, 'j'), InputResult::Draw);
    assert!(model.list_free_scroll);
    assert_eq!(model.selected_row().unwrap().key, "b");
    ch(&mut model, 'k');
    assert!(!model.list_free_scroll);

    model.offset = 0;
    let area = ratatui::layout::Rect::new(0, 0, 100, 30);
    let wheel = crossterm::event::MouseEvent {
        kind: crossterm::event::MouseEventKind::ScrollDown,
        column: 5,
        row: 5,
        modifiers: KeyModifiers::NONE,
    };
    assert!(crate::mouse::scroll(&mut model, wheel, area));
    assert_eq!(model.selected_row().unwrap().key, "a");
    assert!(model.list_free_scroll);
    assert_eq!(model.offset, 3);
}

#[test]
fn drag_selects_inside_the_pane_and_copies_trimmed_text() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    use ratatui::buffer::Buffer;
    let mut model = model_with(&["a"]);
    let area = ratatui::layout::Rect::new(0, 0, 100, 30);
    let at = |kind, column, row| MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    };
    let list = crate::render::panes(area, false).list;
    let (x, y) = (list.x + 2, list.y + 1);
    crate::mouse::select(
        &mut model,
        at(MouseEventKind::Down(MouseButton::Left), x, y),
        area,
    );
    assert!(crate::mouse::select(
        &mut model,
        at(MouseEventKind::Drag(MouseButton::Left), x + 3, y + 1),
        area
    ));
    assert!(crate::mouse::select(
        &mut model,
        at(MouseEventKind::Up(MouseButton::Left), x + 3, y + 1),
        area
    ));
    let selection = model
        .mouse_selection
        .expect("selection kept for the next frame");
    assert!(selection.finished);
    let mut buffer = Buffer::empty(area);
    buffer.set_string(list.x + 1, y, " hello world", ratatui::style::Style::new());
    buffer.set_string(list.x + 1, y + 1, " second", ratatui::style::Style::new());
    assert_eq!(
        crate::mouse::extract(&buffer, &selection),
        "hello world\n seco"
    );

    // A plain click selects nothing.
    let mut model = model_with(&["a"]);
    crate::mouse::select(
        &mut model,
        at(MouseEventKind::Down(MouseButton::Left), x, y),
        area,
    );
    crate::mouse::select(
        &mut model,
        at(MouseEventKind::Up(MouseButton::Left), x, y),
        area,
    );
    assert!(model.mouse_selection.is_none());
}
