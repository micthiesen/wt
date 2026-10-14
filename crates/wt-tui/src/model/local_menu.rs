//! Board-built menus: open without a host request, rebuild while open, and
//! reopen when the host reports that the view they showed went stale.

use std::mem::discriminant;

use super::{ConfirmPrompt, InputResult, Interaction, Model, PickerPrompt, ToastTone};
use crate::{LocalMenu, Menu, UiAction, UiModal, UiReply};

impl Model {
    /// Answer a requested action from the board when it is a board menu.
    pub(super) fn open_board_menu(&mut self, action: UiAction) -> InputResult {
        let Menu::Local(menu) = action.menu(&self.board) else {
            return InputResult::Action(action);
        };
        // The opening key is not a commit that the host could find stale.
        self.committed_menu = None;
        self.show_local_menu(action, menu);
        InputResult::Draw
    }

    fn show_local_menu(&mut self, action: UiAction, menu: LocalMenu) {
        match menu {
            LocalMenu::Modal { modal, warm } => {
                self.reply(UiReply {
                    modal: Some(*modal),
                    ..Default::default()
                });
                self.local_menu = Some(action);
                self.background.extend(warm.map(|warm| *warm));
            }
            LocalMenu::Notice(text) => {
                self.local_menu = None;
                self.toast = Some((text, ToastTone::Notice));
            }
        }
    }

    /// Track an open board menu across one key: forget it once its modal
    /// closes, and remember it while the commit it produced is in flight.
    pub(super) fn track_local_menu(&mut self, before: Option<UiAction>, result: &InputResult) {
        if before.is_none() || !matches!(self.interaction, Interaction::None) {
            return;
        }
        if self.local_menu == before {
            self.local_menu = None;
        }
        if matches!(result, InputResult::Action(_)) {
            self.committed_menu = before;
        }
    }

    /// Rebuild the open board menu from the newly presented board. The
    /// selection follows its option value, so a re-sort never moves the
    /// cursor onto a different choice. A board that no longer offers the menu
    /// keeps the last view; the host checks the commit anyway.
    pub(super) fn rebuild_local_menu(&mut self) {
        let Some(action) = &self.local_menu else {
            return;
        };
        if matches!(self.interaction, Interaction::None) {
            self.local_menu = None;
            return;
        }
        let Menu::Local(LocalMenu::Modal { modal, .. }) = action.menu(&self.board) else {
            return;
        };
        match (&mut self.interaction, *modal) {
            (
                Interaction::Picker(picker),
                UiModal::Picker {
                    action,
                    title,
                    options,
                    ..
                },
            ) if discriminant(&picker.action) == discriminant(&action) => {
                let current = picker.options.get(picker.selected);
                let selected = current
                    .and_then(|current| {
                        options
                            .iter()
                            .position(|option| {
                                current.value.is_some() && option.value == current.value
                            })
                            .or_else(|| {
                                options
                                    .iter()
                                    .position(|option| option.label == current.label)
                            })
                    })
                    .unwrap_or(picker.selected)
                    .min(options.len().saturating_sub(1));
                *picker = PickerPrompt {
                    action,
                    title,
                    options,
                    selected,
                };
            }
            (
                Interaction::Confirm(confirm),
                UiModal::Confirm {
                    action,
                    title,
                    lines,
                    cancel_key,
                },
            ) if discriminant(&confirm.action) == discriminant(&action) => {
                *confirm = ConfirmPrompt {
                    action,
                    title,
                    lines,
                    selected: confirm.selected,
                    cancel_key,
                };
            }
            (
                Interaction::Log {
                    title: open_title,
                    lines: open_lines,
                    scroll,
                },
                UiModal::Log { title, lines, .. },
            ) if *open_title == title => {
                *scroll = (*scroll).min(lines.len().saturating_sub(1));
                *open_lines = lines;
            }
            _ => {}
        }
    }

    /// A commit was refused because what its menu showed changed. Reopen the
    /// menu from the current board and ask the sources for fresher data; the
    /// open menu rebuilds when it arrives.
    pub(super) fn reopen_stale_menu(&mut self, operation_id: Option<u64>, message: String) {
        self.refresh_requested = true;
        let Some((id, action)) = self.last_menu.take() else {
            return;
        };
        if Some(id) != operation_id || !matches!(self.interaction, Interaction::None) {
            return;
        }
        if let Menu::Local(menu) = action.menu(&self.board) {
            self.show_local_menu(action, menu);
        }
        self.toast = Some((message, ToastTone::Failed));
    }
}
