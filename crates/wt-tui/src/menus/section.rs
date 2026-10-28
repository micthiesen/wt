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
    // The Inbox (only when the row is in a section), every other section in
    // board order with its size, then "+ new section" on its `n` chord.
    let mut options = Vec::new();
    if current.is_some() {
        options.push(option(None, "Inbox".into(), None));
    }
    options.extend(
        board
            .section_order
            .iter()
            .filter(|name| Some(name.as_str()) != current.as_deref())
            .map(|name| {
                let mut choice = option(Some(name.clone()), name.clone(), None);
                choice.detail = Some(size(live_rows(board, name)));
                choice
            }),
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

fn size(rows: usize) -> String {
    match rows {
        0 => "empty".into(),
        1 => "1 worktree".into(),
        n => format!("{n} worktrees"),
    }
}

/// Rows still on the board (not archived) filed under `name`.
fn live_rows(board: &Board, name: &str) -> usize {
    section_rows(board, name).len()
}

/// Confirm deleting a section that still has rows: they move to the Inbox.
/// The confirmation carries the rows it listed, as the host's witness.
pub(super) fn delete_confirm(board: &Board, name: &str) -> LocalMenu {
    let Some(section) = board.sections.iter().find(|section| section.key == name) else {
        return LocalMenu::Notice("section is no longer on the board".into());
    };
    let seen = board.section_witness(name);
    let slug = |key: &String| {
        board
            .rows
            .iter()
            .find(|row| &row.key == key)
            .map_or_else(|| key.clone(), |row| row.slug.clone())
    };
    let mut lines = vec![format!("Its {} move to the Inbox:", size(seen.len()))];
    lines.extend(seen.iter().take(8).map(|key| format!("  {}", slug(key))));
    if seen.len() > 8 {
        lines.push(format!("  … and {} more", seen.len() - 8));
    }
    LocalMenu::modal(UiModal::Confirm {
        title: format!("Delete section {}?", section.title),
        lines,
        action: crate::ConfirmAction::DeleteSection {
            section: name.to_owned(),
            seen: Seen(seen),
        },
        cancel_key: None,
    })
}

/// The live (unarchived) rows filed under a section, in board order.
fn section_rows<'a>(board: &'a Board, name: &str) -> Vec<&'a crate::BoardRow> {
    board
        .sections
        .iter()
        .filter(|section| section.key == name)
        .flat_map(|section| &section.rows)
        .filter_map(|&row| board.rows.get(row))
        .filter(|row| !row.archived)
        .collect()
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

    /// Row `one` sits in `placed_in` (or the Inbox), `two` is live in Busy,
    /// `three` is archived in Shipped, and E1..E6 are empty.
    fn board(archived: bool, placed_in: Option<&str>) -> Board {
        let row = |key: &str, archived: bool| BoardRow {
            key: key.into(),
            slug: key.into(),
            archived,
            ..Default::default()
        };
        let section = |key: &str, rows: Vec<usize>| BoardSection {
            key: key.into(),
            title: key.into(),
            rows,
            ..Default::default()
        };
        let mut sections = vec![section("Busy", vec![1]), section("Shipped", vec![2])];
        let mut section_order = vec!["Busy".to_owned(), "Shipped".to_owned()];
        match placed_in {
            Some(name) => {
                sections.insert(0, section("\0inbox", vec![]));
                sections.push(section(name, vec![0]));
                section_order.push(name.into());
            }
            None => sections.insert(0, section("\0inbox", vec![0])),
        }
        section_order.extend((1..=6).map(|n| format!("E{n}")));
        Board {
            rows: vec![row("one", archived), row("two", false), row("three", true)],
            sections,
            section_order,
            ..Default::default()
        }
    }

    /// Value, label, chord, and the dim size detail.
    type Choice = (Option<String>, String, Option<char>, Option<String>);

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
                .map(|option| (option.value, option.label, option.chord, option.detail))
                .collect(),
            title,
        )
    }

    fn named(name: &str, size: &str) -> Choice {
        (Some(name.into()), name.into(), None, Some(size.into()))
    }

    fn new_section() -> Choice {
        (
            Some(NEW_SECTION.into()),
            "+ new section".into(),
            Some('n'),
            None,
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
    fn inbox_row_lists_every_section_in_order_with_its_size() {
        let (action, options, title) = picker(menu(&board(false, None), "one"));
        assert_eq!(title, "Move one to section · n new");
        assert_eq!(
            action,
            PickerAction::Section {
                key: "one".into(),
                seen: Some(Seen(None)),
            }
        );
        // Shipped holds only an archived row, so it reads as empty.
        let mut expected = vec![named("Busy", "1 worktree"), named("Shipped", "empty")];
        expected.extend((1..=6).map(|n| named(&format!("E{n}"), "empty")));
        expected.push(new_section());
        assert_eq!(options, expected);
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
            options[..2],
            [
                (None, "Inbox".into(), None, None),
                named("Busy", "1 worktree")
            ]
        );
        assert!(!options.iter().any(|option| option.1 == "Review"));
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
