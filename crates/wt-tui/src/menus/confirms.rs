//! Confirmations and prompts built from the board: clearing caches, restoring
//! removed worktrees, checking out a review request, and creating a worktree.

use super::{LocalMenu, Menu};
use crate::{Board, ConfirmAction, PickerAction, PickerOption, TextAction, UiAction, UiModal};

/// Clear derived caches: a fixed confirmation with no data to read.
pub(super) fn hard_refresh() -> LocalMenu {
    LocalMenu::modal(UiModal::Confirm {
        title: "Clear derived caches?".into(),
        lines: vec![
            "Refetch current data and regenerate automatic titles. Saved worktree state and running work are preserved."
                .into(),
        ],
        action: ConfirmAction::HardRefresh,
        cancel_key: None,
    })
}

/// Restore a removed worktree. The confirmation names the ledger key, so the
/// request routes to the host that owns the removal, and carries the removal
/// it showed so the host refuses a different one.
pub(super) fn restore_removed(board: &Board, key: &str) -> LocalMenu {
    let Some(row) = board.removed_history.rows.iter().find(|row| row.key == key) else {
        return LocalMenu::Notice("removed history entry is no longer available".into());
    };
    LocalMenu::modal(UiModal::Confirm {
        title: format!("Restore {}?", row.slug),
        lines: vec![
            format!("Create a worktree for {}", row.branch),
            "The current local/origin branch is used when it still exists; otherwise it is recreated from the configured base.".into(),
        ],
        action: ConfirmAction::RestoreRemoved {
            key: row.key.clone(),
            removed_at: row.removed_at.clone(),
            branch: row.branch.clone(),
        },
        cancel_key: None,
    })
}

/// Check out a review request at the PR head the user saw.
pub(super) fn review_checkout(
    board: &Board,
    url: &str,
    updated_at: &str,
    branch: &str,
    host: Option<&str>,
) -> LocalMenu {
    let Some(row) = board.review_requests.iter().find(|row| {
        row.url == url
            && row.updated_at == updated_at
            && row.branch == branch
            && row.host.as_deref() == host
    }) else {
        return LocalMenu::Notice("review request changed; refresh and select it again".into());
    };
    LocalMenu::Modal {
        modal: Box::new(UiModal::Confirm {
            title: format!("Check out PR #{} for review?", row.number),
            lines: vec![
                row.title.clone(),
                branch.to_owned(),
                "Create a worktree in Reviews at the verified PR head.".into(),
            ],
            action: ConfirmAction::ReviewCheckout {
                url: url.to_owned(),
                updated_at: updated_at.to_owned(),
                branch: branch.to_owned(),
            },
            cancel_key: Some('w'),
        }),
        warm: None,
        host: row.host.clone(),
    }
}

/// `Ctrl+N`: choose a host when remote hosts are configured, then name the
/// worktree. The host list is the board's own, so the choice is local; the
/// chosen host answers the name prompt through its controller.
pub(super) fn create(board: &Board, initial: &str) -> Menu {
    if board.hosts.is_empty() {
        // Without the host list the picker cannot be built; the host answers
        // with the same name prompt.
        return Menu::Host {
            reason: "the host list is not prepared yet",
        };
    }
    if board.hosts.len() <= 1 {
        return Menu::Local(LocalMenu::modal(new_worktree_prompt(initial)));
    }
    let options = board
        .hosts
        .iter()
        .map(|choice| PickerOption {
            value: choice.id.clone(),
            label: choice.label.clone(),
            chord: None,
            note: (!choice.sign_in_needed.is_empty())
                .then(|| format!("sign-in needed: {}", choice.sign_in_needed.join(", "))),
            verify_after_merge: None,
            detail: None,
        })
        .collect();
    Menu::Local(LocalMenu::modal(UiModal::Picker {
        action: PickerAction::Host {
            action: Box::new(UiAction::PrepareCreate {
                initial: initial.to_owned(),
            }),
        },
        title: "Choose host".into(),
        options,
        selected: 0,
    }))
}

/// The name prompt for a new worktree on the host that answers it.
pub fn new_worktree_prompt(initial: &str) -> UiModal {
    UiModal::Text {
        action: TextAction::Create,
        prompt: "New worktree name: ".into(),
        initial: initial.to_owned(),
        allow_empty: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HostChoice, RemovedHistoryRow, ReviewRequestRow};

    fn confirm(menu: LocalMenu) -> (String, ConfirmAction, Vec<String>, Option<char>) {
        let LocalMenu::Modal { modal, .. } = menu else {
            panic!("expected a modal, got {menu:?}");
        };
        let UiModal::Confirm {
            title,
            action,
            lines,
            cancel_key,
        } = *modal
        else {
            panic!("expected a confirmation");
        };
        (title, action, lines, cancel_key)
    }

    #[test]
    fn hard_refresh_confirms_without_board_data() {
        let (title, action, _, cancel_key) = confirm(hard_refresh());
        assert_eq!(title, "Clear derived caches?");
        assert_eq!(action, ConfirmAction::HardRefresh);
        assert_eq!(cancel_key, None);
    }

    #[test]
    fn restore_names_the_ledger_key_and_the_removal_shown() {
        let board = Board {
            removed_history: crate::RemovedHistorySnapshot {
                rows: vec![RemovedHistoryRow {
                    key: "remote:host/one".into(),
                    host: Some("host".into()),
                    slug: "one".into(),
                    branch: "feature/one".into(),
                    removed_at: "2026-10-09T00:00:00Z".into(),
                    ..Default::default()
                }],
            },
            ..Default::default()
        };
        let (title, action, lines, _) = confirm(restore_removed(&board, "remote:host/one"));
        assert_eq!(title, "Restore one?");
        assert_eq!(lines[0], "Create a worktree for feature/one");
        assert_eq!(
            action,
            ConfirmAction::RestoreRemoved {
                key: "remote:host/one".into(),
                removed_at: "2026-10-09T00:00:00Z".into(),
                branch: "feature/one".into(),
            }
        );
        assert!(matches!(
            restore_removed(&Board::default(), "one"),
            LocalMenu::Notice(_)
        ));
    }

    #[test]
    fn review_checkout_matches_the_request_it_describes() {
        let board = Board {
            review_requests: vec![ReviewRequestRow {
                url: "https://example.test/pull/7".into(),
                updated_at: "t1".into(),
                branch: "feature/review".into(),
                title: "Review me".into(),
                number: 7,
                ..Default::default()
            }],
            ..Default::default()
        };
        let (title, action, lines, cancel_key) = confirm(review_checkout(
            &board,
            "https://example.test/pull/7",
            "t1",
            "feature/review",
            None,
        ));
        assert_eq!(title, "Check out PR #7 for review?");
        assert_eq!(lines[0], "Review me");
        assert_eq!(cancel_key, Some('w'));
        assert_eq!(
            action,
            ConfirmAction::ReviewCheckout {
                url: "https://example.test/pull/7".into(),
                updated_at: "t1".into(),
                branch: "feature/review".into(),
            }
        );
        // A moved request no longer matches what the user selected.
        assert!(matches!(
            review_checkout(
                &board,
                "https://example.test/pull/7",
                "t2",
                "feature/review",
                None,
            ),
            LocalMenu::Notice(_)
        ));
        assert!(matches!(
            review_checkout(&Board::default(), "u", "t", "b", None),
            LocalMenu::Notice(_)
        ));
    }

    #[test]
    fn create_asks_for_a_host_only_when_remotes_exist() {
        // Empty board: the host answers.
        assert!(matches!(create(&Board::default(), ""), Menu::Host { .. }));
        // Local only: the name prompt opens directly.
        let local = Board {
            hosts: vec![HostChoice {
                id: None,
                label: "This machine".into(),
                sign_in_needed: Vec::new(),
            }],
            ..Default::default()
        };
        let Menu::Local(LocalMenu::Modal { modal, .. }) = create(&local, "--base main") else {
            panic!("expected the name prompt");
        };
        assert_eq!(
            *modal,
            UiModal::Text {
                action: TextAction::Create,
                prompt: "New worktree name: ".into(),
                initial: "--base main".into(),
                allow_empty: false,
            }
        );
        // With remotes: the picker offers the same choices the host listed,
        // and carries the name so the chosen host opens the same prompt.
        let remote = Board {
            hosts: vec![
                HostChoice {
                    id: None,
                    label: "This machine".into(),
                    sign_in_needed: Vec::new(),
                },
                HostChoice {
                    id: Some("ssh:box".into()),
                    label: "box".into(),
                    sign_in_needed: vec!["Claude".into(), "Codex".into()],
                },
            ],
            ..Default::default()
        };
        let Menu::Local(LocalMenu::Modal { modal, .. }) = create(&remote, "") else {
            panic!("expected the host picker");
        };
        let UiModal::Picker {
            action,
            title,
            options,
            selected,
        } = *modal
        else {
            panic!("expected a picker");
        };
        assert_eq!(title, "Choose host");
        assert_eq!(selected, 0);
        assert_eq!(options[0].note, None);
        assert_eq!(
            options[1].note.as_deref(),
            Some("sign-in needed: Claude, Codex")
        );
        assert_eq!(
            options
                .iter()
                .map(|option| (option.value.clone(), option.label.clone()))
                .collect::<Vec<_>>(),
            vec![
                (None, "This machine".to_owned()),
                (Some("ssh:box".to_owned()), "box".to_owned()),
            ]
        );
        assert_eq!(
            action,
            PickerAction::Host {
                action: Box::new(UiAction::PrepareCreate {
                    initial: String::new(),
                }),
            }
        );
    }
}
