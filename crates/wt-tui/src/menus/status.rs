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
    let verification = record.and_then(wt_core::WorkStatusRecord::verification);
    // The plain states, the two owed-check rows beside `y`, then clear. The
    // current claim is marked and starts highlighted; a ready claim with an
    // owed check is the row for its stage.
    let current_state = record.map(|record| record.state.as_str());
    let current = match current_state {
        Some("ready") if verification.is_some() => {
            verification.map(|(stage, _)| stage_chord(stage))
        }
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
            verify_in: None,
            detail: None,
        });
        if chord == 'y' {
            for stage in [
                wt_core::VerifyStage::Staging,
                wt_core::VerifyStage::Production,
            ] {
                // Steps prefill only into their own stage's row.
                let steps = verification
                    .filter(|(current, _)| *current == stage)
                    .map(|(_, steps)| steps.to_owned());
                options.push(PickerOption {
                    value: Some("ready".into()),
                    label: format!("ready + verify in {}", stage.as_str()),
                    chord: Some(stage_chord(stage)),
                    note: None,
                    verify_after_merge: Some(steps.unwrap_or_default()),
                    verify_in: Some(stage),
                    detail: None,
                });
            }
        }
    }
    options.push(PickerOption {
        value: None,
        label: "clear status".into(),
        chord: Some('x'),
        note: None,
        verify_after_merge: None,
        verify_in: None,
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
                verify_in: record.verify_in,
            }))),
        },
        title: format!("status for {}", row.slug),
        options,
        selected,
    })
}

fn stage_chord(stage: wt_core::VerifyStage) -> char {
    match stage {
        wt_core::VerifyStage::Staging => 'a',
        wt_core::VerifyStage::Production => 'p',
    }
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
                    verify_in: None,
                }))),
            }
        );
        assert!(matches!(menu(&board, "gone"), LocalMenu::Notice(_)));
    }

    #[test]
    fn a_ready_claim_with_a_production_check_starts_on_its_stage_row() {
        let mut record = wt_core::WorkStatusRecord::new(wt_core::WorkState::Ready, "t");
        record.verify_after_merge = Some("check prod".into());
        record.verify_in = Some(wt_core::VerifyStage::Production);
        let board = Board {
            rows: vec![BoardRow {
                key: "one".into(),
                slug: "one".into(),
                work: Some(WorkPresentation {
                    record: Some(record),
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
            options, selected, ..
        } = *modal
        else {
            panic!()
        };
        assert_eq!(
            options[selected].label,
            "ready + verify in production (current)"
        );
        assert_eq!(options[selected].chord, Some('p'));
        assert_eq!(
            options[selected].verify_after_merge.as_deref(),
            Some("check prod")
        );
        let staging = options.iter().find(|o| o.chord == Some('a')).unwrap();
        assert_eq!(staging.verify_after_merge.as_deref(), Some(""));
        assert_eq!(staging.verify_in, Some(wt_core::VerifyStage::Staging));
    }
}
