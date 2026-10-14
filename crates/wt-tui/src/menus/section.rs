//! `l`: the move-to-section picker, from the board's manual section order and
//! the row's current placement.

use super::LocalMenu;
use crate::{Board, PickerAction, PickerOption, Seen, UiModal};

/// Picker value of the "+ new section" row. A leading NUL is reserved for
/// derived groups, so no real section can collide with it.
pub(crate) const NEW_SECTION: &str = "\0new-section";

pub(super) fn menu(board: &Board, key: &str) -> LocalMenu {
    let Some(index) = board.rows.iter().position(|row| row.key == key) else {
        return LocalMenu::Notice("worktree is no longer on the board".into());
    };
    let row = &board.rows[index];
    if row.archived {
        return LocalMenu::Notice("Restore the archived row before moving it to a section".into());
    }
    let current = current_section(board, index);
    // TS order: the Inbox (only when the row is in a section), every other
    // manual section, then the "+ new section" row on its `n` chord.
    let mut options = Vec::new();
    if current.is_some() {
        options.push(option(None, "Inbox".into(), None));
    }
    options.extend(
        board
            .section_order
            .iter()
            .filter(|name| Some(name.as_str()) != current.as_deref())
            .map(|name| option(Some(name.clone()), name.clone(), None)),
    );
    options.push(option(
        Some(NEW_SECTION.into()),
        "+ new section".into(),
        Some('n'),
    ));
    LocalMenu::modal(UiModal::Picker {
        action: PickerAction::Section {
            key: key.to_owned(),
            seen: Some(Seen(current)),
        },
        title: format!("Move {} to section · n new", row.slug),
        options,
        selected: 0,
    })
}

/// The section a row is filed under, or `None` for the Inbox. Derived groups
/// (the Inbox and any reserved `\0` key) are not named sections.
fn current_section(board: &Board, index: usize) -> Option<String> {
    board
        .sections
        .iter()
        .find(|section| section.rows.contains(&index))
        .map(|section| section.key.as_str())
        .filter(|key| !key.starts_with('\0'))
        .map(str::to_owned)
}

fn option(value: Option<String>, label: String, chord: Option<char>) -> PickerOption {
    PickerOption {
        value,
        label,
        chord,
        note: None,
        verify_after_merge: None,
        detail: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BoardRow, BoardSection};

    fn board(archived: bool, placed_in: Option<&str>) -> Board {
        let row = BoardRow {
            key: "one".into(),
            slug: "one".into(),
            archived,
            ..Default::default()
        };
        let section = |key: &str, rows: Vec<usize>| BoardSection {
            key: key.into(),
            title: key.into(),
            rows,
            ..Default::default()
        };
        Board {
            rows: vec![row],
            sections: match placed_in {
                Some(name) => vec![section("\0inbox", vec![]), section(name, vec![0])],
                None => vec![section("\0inbox", vec![0])],
            },
            section_order: vec!["Review".into(), "Next".into()],
            ..Default::default()
        }
    }

    type Choice = (Option<String>, String, Option<char>);

    fn picker(menu: LocalMenu) -> (PickerAction, Vec<Choice>, String) {
        let LocalMenu::Modal { modal, .. } = menu else {
            panic!("expected a modal")
        };
        let UiModal::Picker {
            action,
            title,
            options,
            ..
        } = *modal
        else {
            panic!("expected a picker")
        };
        (
            action,
            options
                .into_iter()
                .map(|option| (option.value, option.label, option.chord))
                .collect(),
            title,
        )
    }

    #[test]
    fn empty_board_and_unknown_rows_get_a_notice() {
        assert!(matches!(
            menu(&Board::default(), "one"),
            LocalMenu::Notice(_)
        ));
        assert!(matches!(
            menu(&board(false, None), "gone"),
            LocalMenu::Notice(_)
        ));
    }

    #[test]
    fn inbox_row_lists_every_section_and_witnesses_the_inbox() {
        let (action, options, title) = picker(menu(&board(false, None), "one"));
        assert_eq!(title, "Move one to section · n new");
        assert_eq!(
            action,
            PickerAction::Section {
                key: "one".into(),
                seen: Some(Seen(None)),
            }
        );
        assert_eq!(
            options,
            [
                (Some("Review".into()), "Review".into(), None),
                (Some("Next".into()), "Next".into(), None),
                (Some(NEW_SECTION.into()), "+ new section".into(), Some('n')),
            ]
        );
    }

    #[test]
    fn row_in_a_section_offers_inbox_and_the_other_sections() {
        let (action, options, _) = picker(menu(&board(false, Some("Review")), "one"));
        assert_eq!(
            action,
            PickerAction::Section {
                key: "one".into(),
                seen: Some(Seen(Some("Review".into()))),
            }
        );
        assert_eq!(
            options,
            [
                (None, "Inbox".into(), None),
                (Some("Next".into()), "Next".into(), None),
                (Some(NEW_SECTION.into()), "+ new section".into(), Some('n')),
            ]
        );
    }

    #[test]
    fn archived_rows_refuse_with_a_notice() {
        let LocalMenu::Notice(text) = menu(&board(true, Some("Review")), "one") else {
            panic!("archived rows must not open a picker")
        };
        assert_eq!(
            text,
            "Restore the archived row before moving it to a section"
        );
    }
}
