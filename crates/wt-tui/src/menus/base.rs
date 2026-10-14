//! `b`: the fork base picker, from the board's rows and recorded bases.

use super::{LocalMenu, Menu};
use crate::{Board, BoardRow, PickerAction, PickerOption, Seen, UiModal};
use wt_core::{ForkBaseCandidate, ForkBaseChooser, fork_base_choices, recorded_fork_base};

pub(super) fn menu(board: &Board, key: &str) -> Menu {
    let Some(row) = board.rows.iter().find(|row| row.key == key) else {
        return Menu::Local(LocalMenu::Notice(
            "worktree is no longer on the board".into(),
        ));
    };
    let trunk = board.display.trunk.as_str();
    let recorded = recorded_fork_base(row.base_branch.as_deref(), trunk);
    // Offer bases in the order the board shows them, as TS did. Candidates
    // come from the row's own host: a remote row's siblings live there.
    let shown = board
        .sections
        .iter()
        .flat_map(|section| section.rows.iter())
        .filter_map(|index| board.rows.get(*index))
        .map(|row| row.key.as_str())
        .collect::<Vec<_>>();
    let mut siblings = board
        .rows
        .iter()
        .filter(|other| other.host == row.host && !other.pending_creation)
        .collect::<Vec<&BoardRow>>();
    siblings.sort_by_key(|other| {
        shown
            .iter()
            .position(|key| *key == other.key)
            .unwrap_or(usize::MAX)
    });
    let candidates = siblings
        .iter()
        .map(|other| ForkBaseCandidate {
            slug: &other.slug,
            branch: &other.branch,
            base: other.base_branch.as_deref(),
            is_main: other.is_main,
            detached: other.detached,
            archived: other.archived,
        })
        .collect::<Vec<_>>();
    let choices = fork_base_choices(
        trunk,
        ForkBaseChooser {
            slug: &row.slug,
            branch: &row.branch,
        },
        row.base_branch.as_deref(),
        &candidates,
    );
    let option = |value: Option<String>, label: String| PickerOption {
        value,
        label,
        chord: None,
        note: None,
        verify_after_merge: None,
        detail: None,
    };
    let mut options = vec![option(None, format!("none (diff against {trunk})"))];
    options.extend(choices.into_iter().map(|branch| {
        let label = if recorded == Some(branch.as_str()) {
            format!("{branch} (current)")
        } else {
            branch.clone()
        };
        option(Some(branch), label)
    }));
    let selected = recorded
        .and_then(|recorded| {
            options
                .iter()
                .position(|option| option.value.as_deref() == Some(recorded))
        })
        .unwrap_or(0);
    Menu::Local(LocalMenu::modal(UiModal::Picker {
        action: PickerAction::Base {
            key: key.to_owned(),
            seen: Some(Seen(recorded.map(str::to_owned))),
        },
        title: format!("fork base for {}", row.slug),
        options,
        selected,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DisplayPolicy;

    fn row(slug: &str, branch: &str, base: Option<&str>) -> BoardRow {
        BoardRow {
            key: slug.into(),
            slug: slug.into(),
            branch: branch.into(),
            base_branch: base.map(str::to_owned),
            ..Default::default()
        }
    }

    fn board(rows: Vec<BoardRow>) -> Board {
        Board {
            sections: vec![crate::BoardSection {
                rows: (0..rows.len()).collect(),
                ..Default::default()
            }],
            rows,
            display: DisplayPolicy {
                trunk: "main".into(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn picker(menu: Menu) -> (PickerAction, Vec<String>, usize) {
        let Menu::Local(LocalMenu::Modal { modal, .. }) = menu else {
            panic!("expected a modal")
        };
        let UiModal::Picker {
            action,
            options,
            selected,
            ..
        } = *modal
        else {
            panic!("expected a picker")
        };
        (
            action,
            options.into_iter().map(|option| option.label).collect(),
            selected,
        )
    }

    #[test]
    fn empty_board_and_missing_rows_answer_with_a_notice() {
        assert_eq!(
            menu(&Board::default(), "one"),
            Menu::Local(LocalMenu::Notice(
                "worktree is no longer on the board".into()
            ))
        );
    }

    #[test]
    fn offers_siblings_and_preselects_the_recorded_base() {
        let board = board(vec![
            row("one", "feature/one", Some("feature/three")),
            row("two", "feature/two", None),
            row("three", "feature/three", None),
        ]);
        let (action, labels, selected) = picker(menu(&board, "one"));
        assert_eq!(
            labels,
            [
                "none (diff against main)",
                "feature/two",
                "feature/three (current)",
            ]
        );
        assert_eq!(selected, 2);
        assert_eq!(
            action,
            PickerAction::Base {
                key: "one".into(),
                seen: Some(Seen(Some("feature/three".into()))),
            }
        );
    }

    #[test]
    fn trunk_and_missing_records_select_none() {
        let board = board(vec![
            row("one", "feature/one", Some("main")),
            row("two", "feature/two", None),
        ]);
        let (action, labels, selected) = picker(menu(&board, "one"));
        assert_eq!(labels, ["none (diff against main)", "feature/two"]);
        assert_eq!(selected, 0);
        assert_eq!(
            action,
            PickerAction::Base {
                key: "one".into(),
                seen: Some(Seen(None)),
            }
        );
    }

    #[test]
    fn excludes_descendants_archived_and_detached_rows() {
        let mut archived = row("parked", "feature/parked", None);
        archived.archived = true;
        let mut detached = row("loose", "", None);
        detached.detached = true;
        let board = board(vec![
            row("one", "feature/one", None),
            row("child", "feature/child", Some("feature/one")),
            row("grandchild", "feature/grandchild", Some("feature/child")),
            row("sibling", "feature/sibling", Some("feature/grandchild")),
            row("other", "feature/other", None),
            archived,
            detached,
        ]);
        let (_, labels, _) = picker(menu(&board, "one"));
        assert_eq!(labels, ["none (diff against main)", "feature/other"]);
    }

    #[test]
    fn a_recorded_base_whose_worktree_is_gone_stays_listed() {
        let board = board(vec![
            row("child", "feature/child", Some("feature/gone")),
            row("two", "feature/two", None),
        ]);
        let (_, labels, selected) = picker(menu(&board, "child"));
        assert_eq!(
            labels,
            [
                "none (diff against main)",
                "feature/gone (current)",
                "feature/two",
            ]
        );
        assert_eq!(selected, 1);
    }

    #[test]
    fn remote_rows_offer_only_their_own_hosts_siblings() {
        let mut remote = row("far", "feature/far", None);
        remote.host = Some("box".into());
        let mut far_sibling = row("far-two", "feature/far-two", None);
        far_sibling.host = Some("box".into());
        let board = board(vec![
            row("local", "feature/local", None),
            remote,
            far_sibling,
        ]);
        let (_, labels, _) = picker(menu(&board, "far"));
        assert_eq!(labels, ["none (diff against main)", "feature/far-two"]);
    }
}
