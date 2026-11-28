//! `;` session picker, `Shift+F12` harness picker, and `Shift+F10`/`F11`
//! terminal stop, built from the prepared board.
//!
//! Each host prepares [`Board::session_menus`] and [`Board::terminals`] from
//! its discovery snapshot and one tmux inventory, and the fleet board lists
//! a remote host's under its rows' keys. Builders only arrange those
//! entries. A row without an entry is one whose host has not read its
//! sessions yet, which the menu says instead of guessing that none run.

use super::{LocalMenu, host_unknown, row_display};
use crate::{
    Board, ConfirmAction, PickerAction, PickerOption, SessionMenuEntry, SessionMode,
    SessionSelection, SessionTarget, UiAction, UiModal, session_menu_key,
};

const ROW_GONE: &str = "worktree is no longer on the board";

/// `;`: the resumable conversations and live slots for a worktree or slot,
/// then one `new` row per visible harness.
pub(super) fn sessions_menu(board: &Board, key: Option<&str>, target: SessionTarget) -> LocalMenu {
    // A worktree's picker belongs to its row; a slot's to this machine.
    let row = match (target, key) {
        (SessionTarget::Harness, Some(key)) => {
            let Some(row) = board.rows.iter().find(|row| row.key == key) else {
                return LocalMenu::Notice(ROW_GONE.into());
            };
            if row.is_main {
                return LocalMenu::Notice("the main checkout has its own session: press .".into());
            }
            Some(row)
        }
        _ => None,
    };
    let menu_key = session_menu_key(key, target);
    let Some(entries) = board.session_menus.get(&menu_key) else {
        return match row {
            Some(row) => host_unknown(board, row, "session list"),
            None => LocalMenu::NotYet("session list not loaded yet".into()),
        };
    };

    let harnesses = match row {
        Some(row) => row_display(board, row).harnesses.clone(),
        None => board.display.harnesses.clone(),
    };
    LocalMenu::Modal {
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
    }
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
            verify_in: None,
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
            verify_in: None,
            detail: None,
        });
        choices.push(selection);
    }
    UiModal::Picker {
        action: PickerAction::Sessions { choices },
        title: match (&key, target) {
            (Some(key), SessionTarget::Harness) => {
                format!("Sessions · {}", wt_core::worktree_ledger_label(key))
            }
            _ => "Sessions".into(),
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

/// `Shift+F12`: the harnesses visible on the row's host, cursor on that
/// host's persisted primary.
pub(super) fn harness_menu(board: &Board, key: &str) -> LocalMenu {
    let Some(row) = board.rows.iter().find(|row| row.key == key) else {
        return LocalMenu::Notice(ROW_GONE.into());
    };
    let display = row_display(board, row);
    if display.harnesses.is_empty() {
        return LocalMenu::Notice("all agent harnesses are hidden in configuration".into());
    }
    LocalMenu::modal(harness_picker(
        key.to_owned(),
        &display.harnesses,
        display.primary,
    ))
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
                verify_in: None,
                detail: None,
            })
            .collect(),
    }
}

/// `Shift+F10`/`Shift+F11`: confirm stopping the shell or diff terminal of a
/// worktree, naming the exact tmux identity the stop must match. A terminal
/// whose process exited, kept by tmux for inspection, is listed too.
pub(super) fn stop_terminal_menu(board: &Board, key: &str, target: SessionTarget) -> LocalMenu {
    if !matches!(target, SessionTarget::Shell | SessionTarget::Diff) {
        return LocalMenu::Notice("only shell and diff sessions use terminal stop".into());
    }
    let Some(row) = board.rows.iter().find(|row| row.key == key) else {
        return LocalMenu::Notice(ROW_GONE.into());
    };
    if row.is_main {
        return LocalMenu::Notice("terminal stop requires a worktree".into());
    }
    let Some(terminals) = board.terminals.get(key) else {
        return host_unknown(board, row, "terminal list");
    };
    match terminals.iter().find(|terminal| terminal.target == target) {
        Some(terminal) => LocalMenu::modal(stop_terminal_confirm(key, terminal)),
        None => LocalMenu::Notice(format!("No {} session is running", terminal_label(target))),
    }
}

/// The stop confirmation for one exact shell or diff terminal.
fn stop_terminal_confirm(key: &str, terminal: &crate::TerminalSlotView) -> UiModal {
    let target = terminal.target;
    UiModal::Confirm {
        title: format!("Stop {} session?", terminal_label(target)),
        lines: vec![
            terminal.name.clone(),
            if terminal.retained {
                "Its process has exited; closing it discards the output kept for inspection."
            } else if target == SessionTarget::Shell {
                "Processes running in this shell will stop."
            } else {
                "The diff viewer will restart fresh next time."
            }
            .into(),
        ],
        action: ConfirmAction::StopTerminal {
            key: key.to_owned(),
            target,
            session_id: terminal.id.clone(),
            created_at: terminal.created_at,
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

    /// A row of the host `builder` as the fleet board lists it, with that
    /// host's own policy.
    fn remote(mut board: Board) -> Board {
        board.rows[0].host = Some("builder".into());
        board.host_display.insert(
            "builder".into(),
            DisplayPolicy {
                harnesses: vec![HarnessId::Pi],
                primary: Some(HarnessId::Pi),
                ..Default::default()
            },
        );
        board
    }

    fn picker(menu: LocalMenu) -> (UiModal, Option<Box<UiAction>>) {
        let LocalMenu::Modal { modal, warm, .. } = menu else {
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

    fn shell(retained: bool) -> TerminalSlotView {
        TerminalSlotView {
            target: SessionTarget::Shell,
            name: "one-shell".into(),
            id: "$7".into(),
            created_at: 42,
            retained,
        }
    }

    #[test]
    fn every_board_is_answered_without_a_host() {
        let sessions = UiAction::PrepareSessions {
            key: Some("one".into()),
            target: SessionTarget::Harness,
        };
        let slot = UiAction::PrepareSessions {
            key: None,
            target: SessionTarget::Manager,
        };
        let harnesses = UiAction::PrepareHarnesses { key: "one".into() };
        let stop = UiAction::PrepareStopTerminal {
            key: "one".into(),
            target: SessionTarget::Shell,
        };
        let empty = Board::default();
        // A row with nothing prepared: its host has not read tmux yet.
        let unread = Board {
            session_menus: Default::default(),
            ..board(vec![])
        };
        for action in [&sessions, &slot, &harnesses, &stop] {
            assert!(action.is_board_menu(), "{action:?}");
            for board in [&empty, &unread, &remote(unread.clone())] {
                assert!(
                    matches!(action.menu(board), crate::Menu::Local(_)),
                    "{action:?}"
                );
            }
        }
        assert_eq!(
            sessions_menu(&empty, Some("one"), SessionTarget::Harness),
            LocalMenu::Notice(ROW_GONE.into())
        );
        assert_eq!(
            sessions_menu(&empty, None, SessionTarget::Manager),
            LocalMenu::NotYet("session list not loaded yet".into())
        );
        assert_eq!(
            sessions_menu(&unread, Some("one"), SessionTarget::Harness),
            LocalMenu::NotYet("session list not loaded yet".into())
        );
        assert_eq!(
            stop_terminal_menu(&unread, "one", SessionTarget::Shell),
            LocalMenu::NotYet("terminal list not loaded yet".into())
        );
        // A host that cannot be reached is named, not waited for.
        let mut lost = remote(unread.clone());
        lost.host_errors
            .insert("builder".into(), "SSH disconnected".into());
        assert_eq!(
            sessions_menu(&lost, Some("one"), SessionTarget::Harness),
            LocalMenu::Notice("session list unknown: SSH disconnected".into())
        );
        assert_eq!(
            stop_terminal_menu(&lost, "one", SessionTarget::Diff),
            LocalMenu::Notice("terminal list unknown: SSH disconnected".into())
        );
        let mut main = unread;
        main.rows[0].is_main = true;
        assert!(matches!(
            sessions_menu(&main, Some("one"), SessionTarget::Harness),
            LocalMenu::Notice(_)
        ));
        assert!(matches!(
            stop_terminal_menu(&main, "one", SessionTarget::Shell),
            LocalMenu::Notice(_)
        ));
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
            &format!("Sessions · {}", wt_core::worktree_ledger_label("one"))
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
    fn a_remote_rows_menus_use_its_hosts_entries_and_harnesses() {
        let live = entry(HarnessId::Pi, "abc", true, Some("$1"));
        let board = remote(board(vec![live]));
        let (modal, warm) = picker(sessions_menu(&board, Some("one"), SessionTarget::Harness));
        // That host hides every harness but Pi; this machine's list is not
        // offered for its rows.
        assert_eq!(values(&modal), vec!["session:pi:abc", "new:pi"]);
        let UiModal::Picker {
            action: PickerAction::Sessions { choices },
            ..
        } = &modal
        else {
            panic!()
        };
        // A new session beside the live slot it was shown with.
        assert_eq!(
            choices[1].seen_live,
            Some(crate::Seen(vec!["$1".to_owned()]))
        );
        // The background read is routed to the row's host by its key.
        assert_eq!(
            warm.as_deref(),
            Some(&UiAction::DiscoverSessions {
                key: Some("one".into()),
                target: SessionTarget::Harness,
            })
        );
        let (modal, _) = picker(harness_menu(&board, "one"));
        let UiModal::Picker {
            options, selected, ..
        } = modal
        else {
            panic!()
        };
        assert_eq!(selected, 0);
        assert_eq!(
            options
                .iter()
                .map(|option| option.value.clone().unwrap())
                .collect::<Vec<_>>(),
            ["pi"]
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
        assert_eq!(title, "Sessions");
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
            LocalMenu::NotYet(_)
        ));
    }

    #[test]
    fn harness_picker_starts_on_the_persisted_primary() {
        let (modal, _) = picker(harness_menu(&board(vec![]), "one"));
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
            LocalMenu::Notice(ROW_GONE.into())
        );
        let mut hidden = board(vec![]);
        hidden.display.harnesses.clear();
        assert!(matches!(harness_menu(&hidden, "one"), LocalMenu::Notice(_)));
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
        board.terminals.insert("one".into(), vec![shell(false)]);
        let confirm = |board: &Board| {
            let LocalMenu::Modal { modal, .. } =
                stop_terminal_menu(board, "one", SessionTarget::Shell)
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
            (title, lines, action)
        };
        let (title, lines, action) = confirm(&board);
        assert_eq!(title, "Stop shell session?");
        assert_eq!(lines[0], "one-shell");
        assert_eq!(lines[1], "Processes running in this shell will stop.");
        let stop = ConfirmAction::StopTerminal {
            key: "one".into(),
            target: SessionTarget::Shell,
            session_id: "$7".into(),
            created_at: 42,
        };
        assert_eq!(action, stop);
        // A remote row confirms the identity its own host listed.
        assert_eq!(confirm(&remote(board.clone())).2, stop);
        // A pane kept after its process exited stops by the same identity.
        board.terminals.insert("one".into(), vec![shell(true)]);
        let (_, lines, action) = confirm(&board);
        assert!(lines[1].starts_with("Its process has exited"), "{lines:?}");
        assert_eq!(action, stop);
        // The inventory was read and holds no such terminal.
        assert_eq!(
            stop_terminal_menu(&board, "one", SessionTarget::Diff),
            LocalMenu::Notice("No diff session is running".into())
        );
        assert_eq!(
            stop_terminal_menu(&board, "one", SessionTarget::Harness),
            LocalMenu::Notice("only shell and diff sessions use terminal stop".into())
        );
    }
}
