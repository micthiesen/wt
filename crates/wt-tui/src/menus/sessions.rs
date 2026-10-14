//! `;` session picker, `Shift+F12` harness picker, and `Shift+F10`/`F11`
//! terminal stop, built from the prepared board.
//!
//! The host prepares [`Board::session_menus`] and [`Board::terminals`] from
//! its discovery snapshot and one tmux inventory. Builders only arrange those
//! entries. A missing entry means the board has no prepared data for that
//! target, so the request goes to the owning host.

use super::{LocalMenu, Menu};
use crate::{
    Board, ConfirmAction, PickerAction, PickerOption, SessionMenuEntry, SessionMode,
    SessionSelection, SessionTarget, UiAction, UiModal, session_menu_key,
};

/// `;`: the resumable conversations and live slots for a worktree or slot,
/// then one `new` row per visible harness.
pub(super) fn sessions_menu(board: &Board, key: Option<&str>, target: SessionTarget) -> Menu {
    let menu_key = session_menu_key(key, target);
    let Some(entries) = board.session_menus.get(&menu_key) else {
        return Menu::Host {
            reason: "has no prepared session data for this target",
        };
    };

    let harnesses = visible_harnesses(board);
    Menu::Local(LocalMenu::Modal {
        modal: Box::new(session_picker(
            key.map(str::to_owned),
            target,
            entries,
            &harnesses,
        )),
        // Discovery finishes after the picker opens; a background read lets
        // saved conversations join it without a loading row.
        warm: Some(Box::new(UiAction::DiscoverSessions {
            key: key.map(str::to_owned),
            target,
        })),
        host: None,
    })
}

/// The picker over prepared entries. Entry order is the board's order; each
/// option value is a stable identity, so a rebuild keeps the same choice.
pub fn session_picker(
    key: Option<String>,
    target: SessionTarget,
    entries: &[SessionMenuEntry],
    harnesses: &[wt_core::HarnessId],
) -> UiModal {
    let mut choices = Vec::new();
    let mut options = Vec::new();
    for entry in entries {
        options.push(PickerOption {
            value: Some(session_value(&entry.selection)),
            label: session_row_label(
                entry.selection.harness,
                &entry.display_name,
                entry.state.as_deref(),
                entry.is_live,
            ),
            chord: None,
            note: None,
            verify_after_merge: None,
            detail: None,
        });
        choices.push(entry.selection.clone());
    }
    // A new session commits the live slots it was offered beside, so the host
    // can refuse one that started after this board was prepared.
    let shown_live: Vec<String> = entries
        .iter()
        .filter(|entry| entry.is_live)
        .filter_map(|entry| entry.selection.live_slot.as_ref())
        .map(|slot| slot.id.clone())
        .collect();
    for &harness in harnesses {
        let selection = SessionSelection {
            key: key.clone(),
            target,
            harness,
            session_id: None,
            live_slot: None,
            managed_name: None,
            mode: SessionMode::New,
            live: false,
            seen_live: Some(crate::Seen(shown_live.clone())),
        };
        options.push(PickerOption {
            value: Some(session_value(&selection)),
            label: format!("new {} session", harness_label(harness)),
            // The letter jumps to this row (the model's sessions key
            // handler); it never commits, as in TS.
            chord: Some(harness_letter(harness)),
            note: None,
            verify_after_merge: None,
            detail: None,
        });
        choices.push(selection);
    }
    UiModal::Picker {
        action: PickerAction::Sessions { choices },
        title: match (&key, target) {
            (Some(key), SessionTarget::Harness) => {
                format!("sessions · {}", wt_core::worktree_ledger_label(key))
            }
            _ => "sessions".into(),
        },
        options,
        selected: 0,
    }
}

/// The stable identity of a picker choice: the conversation, else the live
/// slot, else the harness's next new session. Rebuilds match on this value.
pub fn session_value(selection: &SessionSelection) -> String {
    let harness = selection.harness.as_str();
    match (&selection.mode, &selection.session_id, &selection.live_slot) {
        (SessionMode::New, _, _) => format!("new:{harness}"),
        (_, Some(id), _) => format!("session:{harness}:{id}"),
        (_, None, Some(slot)) => format!("slot:{}", slot.id),
        (_, None, None) => format!(
            "unnamed:{harness}:{}",
            selection.managed_name.as_deref().unwrap_or_default()
        ),
    }
}

/// The choice an option value names. Values, not positions, identify choices.
pub(crate) fn choice_for_value<'a>(
    choices: &'a [SessionSelection],
    value: &str,
) -> Option<&'a SessionSelection> {
    choices.iter().find(|choice| session_value(choice) == value)
}

/// `Shift+F12`: the visible harnesses, cursor on the persisted primary. The
/// primary is this machine's; a remote row's commit still runs on its host.
pub(super) fn harness_menu(board: &Board, key: &str) -> Menu {
    if board.rows.iter().all(|row| row.key != key) {
        return Menu::Local(LocalMenu::Notice(
            "worktree is no longer on the board".into(),
        ));
    }
    let visible = visible_harnesses(board);
    if visible.is_empty() {
        return Menu::Local(LocalMenu::Notice(
            "all agent harnesses are hidden in configuration".into(),
        ));
    }
    Menu::Local(LocalMenu::modal(harness_picker(
        key.to_owned(),
        &visible,
        board.display.primary,
    )))
}

/// The harness chooser over the visible harnesses, starting on `primary`.
pub fn harness_picker(
    key: String,
    visible: &[wt_core::HarnessId],
    primary: Option<wt_core::HarnessId>,
) -> UiModal {
    UiModal::Picker {
        action: PickerAction::Harness { key },
        title: "Start agent".into(),
        selected: primary
            .and_then(|primary| visible.iter().position(|id| *id == primary))
            .unwrap_or(0),
        options: visible
            .iter()
            .map(|&id| PickerOption {
                value: Some(id.as_str().into()),
                label: harness_label(id).into(),
                chord: Some(harness_letter(id)),
                note: None,
                verify_after_merge: None,
                detail: None,
            })
            .collect(),
    }
}

/// `Shift+F10`/`Shift+F11`: confirm stopping the live shell or diff terminal
/// of a worktree, naming the exact tmux identity the stop must match.
pub(super) fn stop_terminal_menu(board: &Board, key: &str, target: SessionTarget) -> Menu {
    if !matches!(target, SessionTarget::Shell | SessionTarget::Diff) {
        return Menu::Local(LocalMenu::Notice(
            "only shell and diff sessions use terminal stop".into(),
        ));
    }
    let row = board.rows.iter().find(|row| row.key == key);
    if row.is_some_and(|row| row.is_main) {
        return Menu::Local(LocalMenu::Notice(
            "terminal stop requires a worktree on this host".into(),
        ));
    }
    // The board lists running local terminals only. A remote row, a terminal
    // whose pane died but is retained for inspection, and a board tmux has
    // not answered yet are all read by the owning host.
    let Some(terminal) = row
        .filter(|row| row.host.is_none())
        .and_then(|_| board.terminals.get(key))
        .and_then(|terminals| terminals.iter().find(|terminal| terminal.target == target))
    else {
        return Menu::Host {
            reason: "reads retained and remote terminals from tmux",
        };
    };
    Menu::Local(LocalMenu::modal(stop_terminal_confirm(
        key,
        target,
        &terminal.name,
        &terminal.id,
        terminal.created_at,
    )))
}

/// The stop confirmation for one exact shell or diff terminal. The board
/// builder and the owning host both answer with this.
pub fn stop_terminal_confirm(
    key: &str,
    target: SessionTarget,
    name: &str,
    session_id: &str,
    created_at: i64,
) -> UiModal {
    UiModal::Confirm {
        title: format!("Stop {} session?", terminal_label(target)),
        lines: vec![
            name.to_owned(),
            if target == SessionTarget::Shell {
                "Processes running in this shell will stop."
            } else {
                "The diff viewer will restart fresh next time."
            }
            .into(),
        ],
        action: ConfirmAction::StopTerminal {
            key: key.to_owned(),
            target,
            session_id: session_id.to_owned(),
            created_at,
        },
        cancel_key: None,
    }
}

/// `shell` or `diff`.
pub fn terminal_label(target: SessionTarget) -> &'static str {
    if target == SessionTarget::Diff {
        "diff"
    } else {
        "shell"
    }
}

fn visible_harnesses(board: &Board) -> Vec<wt_core::HarnessId> {
    board.display.harnesses.clone()
}

/// One resumable session row: the harness, the session name, and the derived
/// state, falling back to live without a state signal. A session whose
/// process is gone reads dead whatever its transcript last said.
fn session_row_label(
    harness: wt_core::HarnessId,
    name: &str,
    state: Option<&str>,
    live: bool,
) -> String {
    let status = match state {
        _ if !live => "dead".into(),
        Some(state) => state.to_ascii_lowercase(),
        None => "live".into(),
    };
    format!("{} / {name} · {status}", harness_label(harness))
}

/// Display name for a harness.
pub fn harness_label(harness: wt_core::HarnessId) -> &'static str {
    match harness {
        wt_core::HarnessId::Claude => "Claude Code",
        wt_core::HarnessId::Codex => "Codex",
        wt_core::HarnessId::Opencode => "OpenCode",
        wt_core::HarnessId::Pi => "Pi",
    }
}

/// Quick-pick letter for each harness.
pub fn harness_letter(harness: wt_core::HarnessId) -> char {
    match harness {
        wt_core::HarnessId::Claude => 'c',
        wt_core::HarnessId::Codex => 'x',
        wt_core::HarnessId::Opencode => 'o',
        wt_core::HarnessId::Pi => 'p',
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BoardRow, DisplayPolicy, TerminalSlotView};
    use wt_core::HarnessId;

    fn entry(harness: HarnessId, id: &str, live: bool, slot: Option<&str>) -> SessionMenuEntry {
        SessionMenuEntry {
            selection: SessionSelection {
                key: Some("one".into()),
                target: SessionTarget::Harness,
                harness,
                session_id: Some(id.into()),
                live_slot: slot.map(|id| crate::LiveSessionIdentity {
                    id: id.into(),
                    created_at: 1,
                }),
                managed_name: None,
                mode: SessionMode::Resume,
                live,
                seen_live: None,
            },
            display_name: id.into(),
            state: live.then(|| "Working".into()),
            is_live: live,
            last_active_ms: Some(1),
        }
    }

    fn board(entries: Vec<SessionMenuEntry>) -> Board {
        Board {
            rows: vec![BoardRow {
                key: "one".into(),
                slug: "one".into(),
                ..Default::default()
            }],
            display: DisplayPolicy {
                harnesses: vec![HarnessId::Claude, HarnessId::Codex],
                primary: Some(HarnessId::Codex),
                ..Default::default()
            },
            session_menus: [("one".to_owned(), entries)].into_iter().collect(),
            ..Default::default()
        }
    }

    fn picker(menu: Menu) -> (UiModal, Option<Box<UiAction>>) {
        let Menu::Local(LocalMenu::Modal { modal, warm, .. }) = menu else {
            panic!("expected a board picker, got {menu:?}")
        };
        (*modal, warm)
    }

    fn values(modal: &UiModal) -> Vec<String> {
        let UiModal::Picker { options, .. } = modal else {
            panic!()
        };
        options
            .iter()
            .map(|option| option.value.clone().unwrap())
            .collect()
    }

    #[test]
    fn empty_board_answers_locally_and_only_sessions_may_ask_the_host() {
        let empty = Board::default();
        // The host serves a picker only when no prepared entries exist.
        assert!(matches!(
            UiAction::PrepareSessions {
                key: Some("one".into()),
                target: SessionTarget::Harness,
            }
            .menu(&empty),
            Menu::Host { .. }
        ));
        let harnesses = UiAction::PrepareHarnesses { key: "one".into() };
        assert!(matches!(
            harnesses.menu(&empty),
            Menu::Local(LocalMenu::Notice(_))
        ));
        assert!(harnesses.is_board_menu());
        // Retained dead panes and remote rows are only visible to the host.
        let stop = UiAction::PrepareStopTerminal {
            key: "one".into(),
            target: SessionTarget::Shell,
        };
        assert!(matches!(stop.menu(&empty), Menu::Host { .. }));
        assert!(!stop.is_board_menu());
    }

    #[test]
    fn picker_lists_entries_then_new_rows_with_stable_values() {
        let live = entry(HarnessId::Codex, "abc", true, Some("$1"));
        let old = entry(HarnessId::Claude, "old", false, None);
        let (modal, warm) = picker(sessions_menu(
            &board(vec![live.clone(), old.clone()]),
            Some("one"),
            SessionTarget::Harness,
        ));
        assert_eq!(
            values(&modal),
            vec![
                "session:codex:abc",
                "session:claude:old",
                "new:claude",
                "new:codex"
            ]
        );
        let UiModal::Picker { options, title, .. } = &modal else {
            panic!()
        };
        assert_eq!(options[0].label, "Codex / abc · working");
        assert_eq!(options[1].label, "Claude Code / old · dead");
        assert_eq!(options[3].chord, Some('x'));
        assert_eq!(
            title,
            &format!("sessions · {}", wt_core::worktree_ledger_label("one"))
        );
        assert_eq!(
            warm.as_deref(),
            Some(&UiAction::DiscoverSessions {
                key: Some("one".into()),
                target: SessionTarget::Harness,
            })
        );
    }

    #[test]
    fn values_survive_reordering_and_liveness_changes() {
        let live = entry(HarnessId::Codex, "abc", true, Some("$1"));
        let (before, _) = picker(sessions_menu(
            &board(vec![live.clone()]),
            Some("one"),
            SessionTarget::Harness,
        ));
        let (after, _) = picker(sessions_menu(
            &board(vec![entry(HarnessId::Claude, "new", true, None), live]),
            Some("one"),
            SessionTarget::Harness,
        ));
        let UiModal::Picker { action, .. } = &before else {
            panic!()
        };
        let PickerAction::Sessions { choices: first } = action else {
            panic!()
        };
        let UiModal::Picker {
            action: PickerAction::Sessions { choices: second },
            ..
        } = &after
        else {
            panic!()
        };
        let value = values(&before)[0].clone();
        assert_eq!(
            choice_for_value(first, &value),
            choice_for_value(second, &value)
        );
        assert_eq!(
            choice_for_value(second, &value)
                .unwrap()
                .session_id
                .as_deref(),
            Some("abc")
        );
        assert!(choice_for_value(second, "session:codex:gone").is_none());
    }

    #[test]
    fn special_slots_use_their_own_menu_key_and_no_key() {
        let mut board = board(vec![]);
        board.session_menus.insert(
            session_menu_key(None, SessionTarget::Manager),
            vec![entry(HarnessId::Codex, "m", false, None)],
        );
        let (modal, warm) = picker(sessions_menu(&board, None, SessionTarget::Manager));
        let UiModal::Picker { title, .. } = &modal else {
            panic!()
        };
        assert_eq!(title, "sessions");
        assert!(values(&modal).contains(&"session:codex:m".to_owned()));
        assert_eq!(
            warm.as_deref(),
            Some(&UiAction::DiscoverSessions {
                key: None,
                target: SessionTarget::Manager,
            })
        );
        assert!(matches!(
            sessions_menu(&board, None, SessionTarget::Dotfiles),
            Menu::Host { .. }
        ));
    }

    #[test]
    fn harness_picker_starts_on_the_persisted_primary() {
        let (modal, _) = match harness_menu(&board(vec![]), "one") {
            Menu::Local(LocalMenu::Modal { modal, warm, .. }) => (*modal, warm),
            other => panic!("{other:?}"),
        };
        let UiModal::Picker {
            action,
            options,
            selected,
            ..
        } = modal
        else {
            panic!()
        };
        assert_eq!(action, PickerAction::Harness { key: "one".into() });
        assert_eq!(selected, 1);
        assert_eq!(
            options
                .iter()
                .map(|option| (option.value.clone().unwrap(), option.chord))
                .collect::<Vec<_>>(),
            vec![("claude".into(), Some('c')), ("codex".into(), Some('x'))]
        );
        assert_eq!(
            harness_menu(&Board::default(), "one"),
            Menu::Local(LocalMenu::Notice(
                "worktree is no longer on the board".into()
            ))
        );
    }

    #[test]
    fn session_rows_name_the_harness_and_state() {
        assert_eq!(
            session_row_label(HarnessId::Claude, "main", Some("Working"), true),
            "Claude Code / main · working"
        );
        assert_eq!(
            session_row_label(HarnessId::Codex, "abc", None, true),
            "Codex / abc · live"
        );
        assert_eq!(
            session_row_label(HarnessId::Opencode, "abc", None, false),
            "OpenCode / abc · dead"
        );
        assert_eq!(
            session_row_label(HarnessId::Claude, "old", Some("Idle"), false),
            "Claude Code / old · dead"
        );
    }

    #[test]
    fn stop_terminal_confirms_the_exact_slot_or_reports_none() {
        let mut board = board(vec![]);
        board.terminals.insert(
            "one".into(),
            vec![TerminalSlotView {
                target: SessionTarget::Shell,
                name: "one-shell".into(),
                id: "$7".into(),
                created_at: 42,
            }],
        );
        let Menu::Local(LocalMenu::Modal { modal, .. }) =
            stop_terminal_menu(&board, "one", SessionTarget::Shell)
        else {
            panic!()
        };
        let UiModal::Confirm {
            title,
            lines,
            action,
            ..
        } = *modal
        else {
            panic!()
        };
        assert_eq!(title, "Stop shell session?");
        assert_eq!(lines[0], "one-shell");
        assert_eq!(
            action,
            ConfirmAction::StopTerminal {
                key: "one".into(),
                target: SessionTarget::Shell,
                session_id: "$7".into(),
                created_at: 42,
            }
        );
        // No running terminal, an unknown row, or no inventory yet: the host
        // reads tmux, which also sees panes retained after their process died.
        for menu in [
            stop_terminal_menu(&board, "one", SessionTarget::Diff),
            stop_terminal_menu(&Board::default(), "one", SessionTarget::Shell),
        ] {
            assert!(matches!(menu, Menu::Host { .. }), "{menu:?}");
        }
        let mut remote = board.clone();
        remote.rows[0].host = Some("builder".into());
        assert!(matches!(
            stop_terminal_menu(&remote, "one", SessionTarget::Shell),
            Menu::Host { .. }
        ));
        board.terminals.clear();
        assert!(matches!(
            stop_terminal_menu(&board, "one", SessionTarget::Shell),
            Menu::Host { .. }
        ));
    }
}
