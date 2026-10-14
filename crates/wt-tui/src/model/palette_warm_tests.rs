//! Prefetched action palettes: they open at once from the board, warm again
//! once the selection rests, and keep fast chords working.

use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent};
use wt_runtime::{SourceSnapshot, SourceState};

use super::{InputResult, Interaction, Model};
use crate::{
    AUTO_MERGE_ACTION_ID, ActionSurface, Board, BoardRow, PickerAction, PickerOption,
    PreparedPalette, SessionTarget, UiAction, UiModal,
};

fn board(keys: &[(&str, Option<&str>)]) -> Board {
    Board {
        rows: keys
            .iter()
            .map(|(key, host)| BoardRow {
                key: (*key).into(),
                title: (*key).into(),
                host: host.map(str::to_owned),
                ..BoardRow::default()
            })
            .collect(),
        ..Board::default()
    }
}

fn live(board: Board) -> SourceSnapshot<Board> {
    SourceSnapshot::test_live(Some(board), SourceState::Ready, wt_core::Domain::Git)
}

fn merge_picker(surface: &ActionSurface) -> UiModal {
    UiModal::Picker {
        action: PickerAction::Actions {
            surface: surface.clone(),
        },
        title: "one actions".into(),
        selected: 0,
        options: vec![PickerOption {
            value: Some(AUTO_MERGE_ACTION_ID.into()),
            label: "github: Toggle merge when ready".into(),
            chord: Some('m'),
            note: None,
            verify_after_merge: None,
            detail: None,
        }],
    }
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::from(code)
}

#[test]
fn prefetched_palette_opens_at_once_and_fast_chords_still_merge() {
    let mut model = Model::default();
    let surface = ActionSurface::Row { key: "one".into() };
    let mut prepared = board(&[("one", None), ("two", None)]);
    prepared.palettes.insert(
        surface.menu_key(),
        PreparedPalette {
            modal: merge_picker(&surface),
        },
    );
    model.apply(live(prepared));

    assert_eq!(
        model.input(key(KeyCode::Char('!')), 20),
        InputResult::Draw,
        "the prefetched palette opens without a host request"
    );
    assert!(model.pending_request.is_none());
    assert_eq!(
        model.background,
        vec![UiAction::WarmPalette {
            surface: surface.clone()
        }],
        "an open palette asks for a fresher copy in the background"
    );
    let InputResult::Action(action) = model.input(key(KeyCode::Char('m')), 20) else {
        panic!("the chord commits the option at once")
    };
    assert_eq!(
        action,
        UiAction::PrepareAction {
            surface,
            id: AUTO_MERGE_ACTION_ID.into(),
            arg: None,
        }
    );
}

#[test]
fn palette_without_a_prefetch_still_asks_the_host() {
    let mut model = Model::default();
    model.apply(live(board(&[("one", None)])));
    let InputResult::Action(action) = model.input(key(KeyCode::Char('!')), 20) else {
        panic!("no prefetched copy: the host builds the palette")
    };
    assert_eq!(
        action,
        UiAction::PrepareActions {
            surface: ActionSurface::Row { key: "one".into() },
        }
    );
    assert!(model.background.is_empty());
}

#[test]
fn selection_warms_its_palettes_only_once_it_rests() {
    let mut model = Model::default();
    model.apply(live(board(&[
        ("one", None),
        ("two", None),
        ("three", None),
    ])));
    let start = tokio::time::Instant::now();
    let warm_rows = |model: &Model| -> Vec<ActionSurface> {
        model
            .background
            .iter()
            .filter_map(|action| match action {
                UiAction::WarmPalette { surface }
                    if matches!(surface, ActionSurface::Row { .. }) =>
                {
                    Some(surface.clone())
                }
                _ => None,
            })
            .collect()
    };

    // Holding the movement key: every step restarts the rest period.
    for step in 0..2u32 {
        let now = start + Duration::from_millis(50 * u64::from(step));
        model.input(key(KeyCode::Down), 20);
        model.plan_palette_warm(now);
        model.fire_palette_warm(now);
    }
    assert!(model.background.is_empty());
    let last_step = start + Duration::from_millis(50);
    model.fire_palette_warm(last_step + Duration::from_millis(199));
    assert!(model.background.is_empty(), "not rested yet");

    model.fire_palette_warm(last_step + Duration::from_millis(200));
    assert_eq!(
        warm_rows(&model),
        vec![ActionSurface::Row {
            key: "three".into()
        }],
        "only the resting row is warmed"
    );
    assert!(
        model.background.iter().any(|action| matches!(
            action,
            UiAction::WarmPalette {
                surface: ActionSurface::Manager { key: Some(key) }
            } if key == "three"
        )),
        "the manager palette for the row is warmed with it"
    );
    for target in [
        SessionTarget::WtSource,
        SessionTarget::Main,
        SessionTarget::Dotfiles,
    ] {
        assert!(model.background.contains(&UiAction::WarmPalette {
            surface: ActionSurface::Slot { target }
        }));
    }

    // Same selection again: nothing new is sent.
    model.background.clear();
    model.plan_palette_warm(last_step + Duration::from_secs(1));
    model.fire_palette_warm(last_step + Duration::from_secs(2));
    assert!(model.background.is_empty());
}

#[test]
fn remote_rows_are_not_prefetched() {
    let mut model = Model::default();
    model.apply(live(board(&[("one", Some("studio"))])));
    let now = tokio::time::Instant::now();
    model.plan_palette_warm(now);
    model.fire_palette_warm(now + Duration::from_secs(1));
    assert!(
        model.background.iter().all(|action| matches!(
            action,
            UiAction::WarmPalette {
                surface: ActionSurface::Slot { .. }
            }
        )),
        "remote palettes come from their own host: {:?}",
        model.background
    );
}

#[test]
fn an_armed_warm_wakes_the_loop_at_its_due_time() {
    let mut model = Model::default();
    model.apply(live(board(&[("one", None)])));
    assert_eq!(model.next_deadline(), None);
    let now = tokio::time::Instant::now();
    model.plan_palette_warm(now);
    assert_eq!(
        model.next_deadline(),
        Some(now + Duration::from_millis(200))
    );
    model.fire_palette_warm(now + Duration::from_millis(200));
    assert_eq!(model.next_deadline(), None);
    assert!(matches!(model.interaction, Interaction::None));
}
