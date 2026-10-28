//! `l`: the move-to-section picker, from the board's manual section order and
//! the row's current placement.

use super::LocalMenu;
use crate::{Board, PickerAction, PickerOption, Seen, UiModal};

/// Picker value of the "+ new section" row. A leading NUL is reserved for
/// derived groups, so no real section can collide with it.
pub(crate) const NEW_SECTION: &str = "\0new-section";

/// How many empty sections stay offered after the occupied ones.
const RECENT_EMPTY: usize = 5;

/// Detail on an empty section's row; the picker mutes these under "recent".
pub(crate) const EMPTY_DETAIL: &str = "empty";

pub(super) fn menu(board: &Board, key: &str) -> LocalMenu {
    let Some(index) = board.rows.iter().position(|row| row.key == key) else {
        return LocalMenu::Notice("worktree is no longer on the board".into());
    };
    let row = &board.rows[index];
    if row.archived {
        return LocalMenu::Notice("Restore the archived row before moving it to a section".into());
    }
    let current = current_section(board, index);
    // The Inbox (only when the row is in a section), the sections that hold
    // live rows in board order, "+ new section" on its `n` chord, then a few
    // recently created empty sections, muted.
    let mut options = Vec::new();
    if current.is_some() {
        options.push(option(None, "Inbox".into(), None));
    }
    let (occupied, empty): (Vec<&String>, Vec<&String>) = board
        .section_order
        .iter()
        .filter(|name| Some(name.as_str()) != current.as_deref())
        .partition(|name| live_rows(board, name) > 0);
    options.extend(
        occupied
            .into_iter()
            .map(|name| option(Some(name.clone()), name.clone(), None)),
    );
    options.push(option(
        Some(NEW_SECTION.into()),
        "+ new section".into(),
        Some('n'),
    ));
    // Sections are appended on creation, so the newest empty ones are last.
    options.extend(empty.into_iter().rev().take(RECENT_EMPTY).map(|name| {
        let mut recent = option(Some(name.clone()), name.clone(), None);
        recent.detail = Some(EMPTY_DETAIL.into());
        recent
    }));
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

/// Rows still on the board (not archived) filed under `name`.
fn live_rows(board: &Board, name: &str) -> usize {
    board
        .sections
        .iter()
        .filter(|section| section.key == name)
        .flat_map(|section| &section.rows)
        .filter(|&&row| board.rows.get(row).is_some_and(|row| !row.archived))
        .count()
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
    /// `three` is archived in Shipped, and E1..E6 are empty, oldest first.
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

    /// Value, label, chord, and whether the row is a muted recent section.
    type Choice = (Option<String>, String, Option<char>, bool);

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
                .map(|option| {
                    let recent = option.detail.as_deref() == Some(EMPTY_DETAIL);
                    (option.value, option.label, option.chord, recent)
                })
                .collect(),
            title,
        )
    }

    fn named(name: &str, recent: bool) -> Choice {
        (Some(name.into()), name.into(), None, recent)
    }

    fn new_section() -> Choice {
        (
            Some(NEW_SECTION.into()),
            "+ new section".into(),
            Some('n'),
            false,
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
    fn inbox_row_lists_occupied_sections_then_five_recent_empty_ones() {
        let (action, options, title) = picker(menu(&board(false, None), "one"));
        assert_eq!(title, "Move one to section · n new");
        assert_eq!(
            action,
            PickerAction::Section {
                key: "one".into(),
                seen: Some(Seen(None)),
            }
        );
        // Shipped holds only an archived row, so it counts as empty and is
        // the oldest; the cap drops it along with E1.
        assert_eq!(
            options,
            [
                named("Busy", false),
                new_section(),
                named("E6", true),
                named("E5", true),
                named("E4", true),
                named("E3", true),
                named("E2", true),
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
            options[..3],
            [
                (None, "Inbox".into(), None, false),
                named("Busy", false),
                new_section()
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
