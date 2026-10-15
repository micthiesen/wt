//! `d`: review cached checkout facts without waiting for host I/O.
//! The host checks identity and all removal hazards after confirmation.

use super::{LocalMenu, Menu};
use crate::{Board, ConfirmAction, RemovalSeen, Seen, UiModal, cleanup};

pub(super) fn menu(board: &Board, key: &str) -> Menu {
    let Some(row) = board.rows.iter().find(|row| row.key == key) else {
        return Menu::Host {
            reason: "the worktree is not on the board",
        };
    };
    if row.is_main || row.pending_creation || row.key.starts_with('\0') {
        return Menu::Local(LocalMenu::Notice("This checkout cannot be removed.".into()));
    }
    let mut lines = vec![format!("{} ({})", row.slug, row.branch)];
    if let Some(host) = &row.host {
        lines.push(format!("Host: {host}"));
        if let Some(error) = board.host_errors.get(host) {
            lines.push(format!("Host unavailable: {error}"));
        }
    }
    if let Some(reason) = cleanup::checkout_hold_reason(row, false) {
        lines.push(format!("Last observed: {reason}"));
    }
    if let Some(count) = row.git.tracked_changes.filter(|count| *count > 0) {
        lines.push(format!("Changed tracked files: {count}"));
    }
    if let Some(count) = row.git.untracked_files.filter(|count| *count > 0) {
        lines.push(format!("Untracked files: {count}"));
    }
    if let Some(count) = row.git.ahead.filter(|count| *count > 0) {
        lines.push(format!("Commits ahead of upstream: {count}"));
    }
    if row.environment_live {
        lines.push("A deployed stage may need removal.".into());
    }
    let Some(head) = row.git.head_sha.as_ref().filter(|head| !head.is_empty()) else {
        lines.push(
            "Checkout identity is not available yet. Reopen after the background refresh.".into(),
        );
        return Menu::Local(LocalMenu::modal(UiModal::Log {
            title: "Remove worktree?".into(),
            lines,
            close_key: Some('d'),
            refresh: None,
        }));
    };
    lines.push(
        "Current safety checks run after confirmation. Any hazards need a separate review.".into(),
    );
    Menu::Local(LocalMenu::modal(UiModal::Confirm {
        title: "Remove worktree?".into(),
        lines,
        action: ConfirmAction::Remove {
            key: key.into(),
            force: false,
            revision: None,
            seen: Some(Box::new(Seen(RemovalSeen {
                path: row.path.clone(),
                branch: row.branch.clone(),
                head: head.clone(),
            }))),
        },
        cancel_key: Some('d'),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BoardRow, GitPresentation, LandingKind, UiAction};

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
        assert_eq!(lines[0], "one (feature/one)");
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
    fn hazards_and_remote_rows_still_open_locally_without_authorizing_force() {
        let cases: &[fn(&mut BoardRow)] = &[
            |row| row.git.tracked_changes = Some(1),
            |row| row.git.tracked_changes = None,
            |row| row.git.untracked_files = Some(2),
            |row| row.git.rebasing = true,
            |row| row.git.conflict_files = vec!["a".into()],
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
            |row| row.host = Some("builder".into()),
        ];
        for case in cases {
            let mut row = clean("one");
            case(&mut row);
            let board = Board {
                rows: vec![row],
                ..Default::default()
            };
            let (_, _, action) = confirm(menu(&board, "one"));
            assert!(matches!(
                action,
                ConfirmAction::Remove {
                    force: false,
                    revision: None,
                    seen: Some(_),
                    ..
                }
            ));
        }
    }

    #[test]
    fn missing_head_shows_a_local_menu_without_a_removal_commit() {
        let mut row = clean("one");
        row.git.head_sha = None;
        let board = Board {
            rows: vec![row],
            ..Default::default()
        };
        assert!(
            matches!(menu(&board, "one"), Menu::Local(LocalMenu::Modal { modal, .. })
            if matches!(*modal, UiModal::Log { close_key: Some('d'), refresh: None, .. }))
        );
    }

    #[test]
    fn remote_dirty_menu_preserves_identity_and_lists_cached_hazards() {
        let mut row = clean("one");
        row.host = Some("builder".into());
        row.key = wt_core::remote_worktree_ledger_key("builder", "one");
        row.git.tracked_changes = Some(2);
        row.git.ahead = Some(3);
        let key = row.key.clone();
        let board = Board {
            rows: vec![row],
            ..Default::default()
        };
        let (_, lines, action) = confirm(menu(&board, &key));
        assert!(lines.contains(&"Host: builder".into()));
        assert!(lines.contains(&"Changed tracked files: 2".into()));
        assert!(lines.contains(&"Commits ahead of upstream: 3".into()));
        assert!(
            matches!(action, ConfirmAction::Remove { key: selected, force: false, revision: None, seen: Some(seen) }
            if selected == key && seen.0.head == "head" && seen.0.path == "/worktrees/one")
        );
    }

    #[test]
    fn a_row_the_board_does_not_show_is_planned_by_the_host() {
        assert!(matches!(menu(&Board::default(), "gone"), Menu::Host { .. }));
        // The host must still answer `d`, so the empty board is not a board menu.
        assert!(!UiAction::PrepareRemove { key: "gone".into() }.is_board_menu());
    }
}
