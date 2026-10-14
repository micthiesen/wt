//! `u`: the work status picker, from the row's stored claim.

use super::LocalMenu;
use crate::{Board, PickerAction, PickerOption, Seen, StatusClaim, UiModal};

const STATUS_CHORDS: [(&str, char); 8] = [
    ("todo", 't'),
    ("working", 'w'),
    ("review", 'r'),
    ("needs-testing", 'n'),
    ("needs-human", 'h'),
    ("ready", 'y'),
    ("verified", 'v'),
    ("dropped", 'd'),
];

pub(super) fn menu(board: &Board, key: &str) -> LocalMenu {
    let Some(row) = board.rows.iter().find(|row| row.key == key) else {
        return LocalMenu::Notice("worktree is no longer on the board".into());
    };
    let record = row.work.as_ref().and_then(|work| work.record.as_ref());
    let note = record.and_then(|record| record.note.clone());
    let verify = record.and_then(|record| record.verify_after_merge.clone());
    // TS order: the plain states, `a` beside `y`, then clear. The current
    // claim is marked and starts highlighted; a ready claim with an owed
    // check is the `a` row.
    let current_state = record.map(|record| record.state.as_str());
    let current = match current_state {
        Some("ready") if verify.is_some() => Some('a'),
        Some(state) => STATUS_CHORDS
            .iter()
            .find(|(name, _)| *name == state)
            .map(|(_, chord)| *chord),
        // No claim yet starts on todo, the usual first claim, as in TS.
        None => Some('t'),
    };
    let mut options = Vec::new();
    for (state, chord) in STATUS_CHORDS {
        options.push(PickerOption {
            value: Some(state.into()),
            label: state.into(),
            chord: Some(chord),
            note: note.clone(),
            verify_after_merge: None,
            detail: None,
        });
        if chord == 'y' {
            options.push(PickerOption {
                value: Some("ready".into()),
                label: "ready + verify after merge".into(),
                chord: Some('a'),
                note: None,
                verify_after_merge: Some(verify.clone().unwrap_or_default()),
                detail: None,
            });
        }
    }
    options.push(PickerOption {
        value: None,
        label: "clear status".into(),
        chord: Some('x'),
        note: None,
        verify_after_merge: None,
        detail: None,
    });
    let selected = options
        .iter()
        .position(|option| option.chord == current)
        .unwrap_or(0);
    if current_state.is_some() {
        options[selected].label.push_str(" (current)");
    }
    LocalMenu::modal(UiModal::Picker {
        action: PickerAction::Status {
            key: key.to_owned(),
            seen: Some(Seen(record.map(|record| StatusClaim {
                state: record.state.as_str().to_owned(),
                note: record.note.clone(),
                verify_after_merge: record.verify_after_merge.clone(),
            }))),
        },
        title: format!("status for {}", row.slug),
        options,
        selected,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BoardRow, WorkPresentation};

    #[test]
    fn marks_the_current_claim_and_records_what_was_shown() {
        let board = Board {
            rows: vec![BoardRow {
                key: "one".into(),
                slug: "one".into(),
                work: Some(WorkPresentation {
                    record: Some(wt_core::WorkStatusRecord::new(
                        wt_core::WorkState::Review,
                        "2026-10-14T00:00:00Z",
                    )),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        let LocalMenu::Modal { modal, .. } = menu(&board, "one") else {
            panic!()
        };
        let UiModal::Picker {
            action,
            options,
            selected,
            ..
        } = *modal
        else {
            panic!()
        };
        assert_eq!(options[selected].label, "review (current)");
        assert_eq!(
            action,
            PickerAction::Status {
                key: "one".into(),
                seen: Some(Seen(Some(StatusClaim {
                    state: "review".into(),
                    note: None,
                    verify_after_merge: None,
                }))),
            }
        );
        assert!(matches!(menu(&board, "gone"), LocalMenu::Notice(_)));
    }
}
