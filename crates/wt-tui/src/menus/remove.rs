//! `d`: remove a worktree. A clean checkout confirms from the board; any
//! hazard, or any doubt, is planned by the host so its exact lines show.

use super::{LocalMenu, Menu};
use crate::{Board, BoardRow, ConfirmAction, RemovalSeen, Seen, UiModal, cleanup};

pub(super) fn menu(board: &Board, key: &str) -> Menu {
    let Some(row) = board.rows.iter().find(|row| row.key == key) else {
        // Not on the board: the host answers authoritatively.
        return Menu::Host {
            reason: "the worktree is not on the board",
        };
    };
    let Some(seen) = clean_checkout(row) else {
        return Menu::Host {
            reason: "the checkout has hazards or unknown state",
        };
    };
    Menu::Local(LocalMenu::modal(UiModal::Confirm {
        title: "Remove worktree?".into(),
        lines: vec![format!("{} ({})", row.slug, row.branch)],
        action: ConfirmAction::Remove {
            key: key.into(),
            force: false,
            revision: None,
            seen: Some(Box::new(Seen(seen))),
        },
        cancel_key: Some('d'),
    }))
}

/// The identity to witness when the board shows no removal hazard. Every
/// doubt returns `None`, so the host plans the row instead.
fn clean_checkout(row: &BoardRow) -> Option<RemovalSeen> {
    // Temporary creation rows have no checkout, and the main checkout is
    // never removable. Remote rows reach their host through its own routing.
    if row.key.starts_with('\0')
        || row.is_main
        || row.detached
        || row.archived
        || row.host.is_some()
        || row.environment_live
        || row.slug.is_empty()
        || row.branch.is_empty()
        || cleanup::checkout_hold_reason(row, false).is_some()
    {
        return None;
    }
    Some(RemovalSeen {
        path: row.path.clone(),
        branch: row.branch.clone(),
        head: row.git.head_sha.clone()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GitPresentation, LandingKind, UiAction};

    fn clean(slug: &str) -> BoardRow {
        BoardRow {
            key: slug.into(),
            slug: slug.into(),
            path: format!("/worktrees/{slug}"),
            branch: format!("feature/{slug}"),
            git: GitPresentation {
                head_sha: Some("head".into()),
                landed_on: Some(LandingKind::Base),
                tracked_changes: Some(0),
                untracked_files: Some(0),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn confirm(menu: Menu) -> (String, Vec<String>, ConfirmAction) {
        let Menu::Local(LocalMenu::Modal { modal, .. }) = menu else {
            panic!("clean rows confirm from the board");
        };
        let UiModal::Confirm {
            title,
            lines,
            action,
            cancel_key,
        } = *modal
        else {
            panic!("confirmation expected");
        };
        assert_eq!(cancel_key, Some('d'));
        (title, lines, action)
    }

    #[test]
    fn clean_row_confirms_from_the_board_with_its_witness() {
        let board = Board {
            rows: vec![clean("one")],
            ..Default::default()
        };
        let (title, lines, action) = confirm(menu(&board, "one"));
        assert_eq!(title, "Remove worktree?");
        assert_eq!(lines, ["one (feature/one)"]);
        assert_eq!(
            action,
            ConfirmAction::Remove {
                key: "one".into(),
                force: false,
                revision: None,
                seen: Some(Box::new(Seen(RemovalSeen {
                    path: "/worktrees/one".into(),
                    branch: "feature/one".into(),
                    head: "head".into(),
                }))),
            }
        );
    }

    #[test]
    fn any_hazard_or_doubt_defers_to_the_host() {
        let cases: &[fn(&mut BoardRow)] = &[
            |row| row.git.tracked_changes = Some(1),
            |row| row.git.tracked_changes = None,
            |row| row.git.untracked_files = Some(2),
            |row| row.git.rebasing = true,
            |row| row.git.conflict_files = vec!["a".into()],
            |row| row.git.head_sha = None,
            |row| row.git.error = Some("boom".into()),
            |row| row.path_missing = true,
            |row| row.busy = Some(Default::default()),
            |row| {
                row.work = Some(crate::WorkPresentation {
                    verification_owed: true,
                    ..Default::default()
                })
            },
            |row| row.environment_live = true,
            |row| row.is_main = true,
            |row| row.host = Some("builder".into()),
        ];
        for case in cases {
            let mut row = clean("one");
            case(&mut row);
            let board = Board {
                rows: vec![row],
                ..Default::default()
            };
            assert!(
                matches!(menu(&board, "one"), Menu::Host { .. }),
                "hazard must be planned by the host"
            );
        }
    }

    #[test]
    fn a_row_the_board_does_not_show_is_planned_by_the_host() {
        assert!(matches!(menu(&Board::default(), "gone"), Menu::Host { .. }));
        // The host must still answer `d`, so the empty board is not a board menu.
        assert!(!UiAction::PrepareRemove { key: "gone".into() }.is_board_menu());
    }
}
