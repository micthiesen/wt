//! Board-built menus: open without a host request, rebuild while open, and
//! reopen when the host reports that the view they showed went stale.

use std::mem::discriminant;

use super::{ConfirmPrompt, InputResult, Interaction, Model, PickerPrompt, ToastTone};
use crate::{LocalMenu, Menu, PickerOption, UiAction, UiModal};

/// What identifies a picker choice across rebuilds: its value, plus its
/// chord for options that share a value.
pub(crate) type OptionIdentity = (Option<String>, Option<char>);

fn identity(option: &PickerOption) -> OptionIdentity {
    (option.value.clone(), option.chord)
}

/// How a host's stale refusal reads (`wt_app_core::host_errors`), and the
/// first line of a confirmation reopened after one.
const CHANGED: &str = "Changed since you looked";

/// The second line of a reopened confirmation while the board it was built
/// from is the one that was refused.
const STILL_READING: &str = "Reading what changed. This updates, and takes Enter, in a moment.";

/// How long a reopened confirmation waits for a newer board. Every local
/// source has read again by then (the session inventory polls at this
/// interval), so a board unchanged after it is current.
const STALE_HOLD: std::time::Duration = std::time::Duration::from_secs(5);

fn changed_line(message: &str) -> String {
    if message.starts_with(CHANGED) {
        message.to_owned()
    } else {
        format!("{CHANGED}: {message}")
    }
}

impl Model {
    /// Answer a requested action from the board when it is a board menu.
    pub(super) fn open_board_menu(&mut self, action: UiAction) -> InputResult {
        let menu = match action.menu(&self.board) {
            Menu::Local(menu) => menu,
            // Content its host must fetch: the overlay is on screen from
            // this key on, and the request the terminal now sends fills it.
            Menu::Overlay(overlay) => {
                self.committed_menu = None;
                self.local_menu = None;
                self.interaction_host = None;
                self.interaction = Interaction::Loading(super::LoadingPrompt {
                    title: overlay.title,
                    close_key: overlay.close_key,
                    action: action.clone(),
                    request: None,
                    started: tokio::time::Instant::now(),
                });
                return InputResult::Action(action);
            }
            Menu::NotMenu | Menu::Host { .. } => return InputResult::Action(action),
        };
        // The opening key is not a commit that the host could find stale.
        self.committed_menu = None;
        self.menu_refused = !matches!(menu, LocalMenu::Modal { .. });
        self.show_local_menu(action, menu);
        InputResult::Draw
    }

    fn show_local_menu(&mut self, action: UiAction, menu: LocalMenu) {
        match menu {
            LocalMenu::Modal { modal, warm, host } => {
                self.show_modal(*modal, host);
                self.local_menu = Some(action);
                self.local_menu_default = self.picker_identity();
                self.picker_choice_lost = false;
                self.background.extend(warm.map(|warm| *warm));
            }
            LocalMenu::Notice(text) => {
                self.local_menu = None;
                self.toast = Some((text, ToastTone::Notice));
            }
            LocalMenu::NotYet(text) => {
                self.local_menu = None;
                self.toast = Some((text, ToastTone::Notice));
                self.refresh_requested = true;
            }
            LocalMenu::Ask { text, read } => {
                self.local_menu = None;
                self.toast = Some((text, ToastTone::Notice));
                self.background.push(*read);
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

    /// The identity of the highlighted picker option, if a picker is open.
    fn picker_identity(&self) -> Option<OptionIdentity> {
        match &self.interaction {
            Interaction::Picker(picker) => picker.options.get(picker.selected).map(identity),
            _ => None,
        }
    }

    /// Rebuild the open board menu from the newly presented board. A cursor
    /// the user moved follows its option value, so a re-sort never moves it
    /// onto a different choice. If that option is gone the cursor keeps its
    /// place and the next commit key is dropped. A cursor still on the builder's default
    /// follows the new default: a menu opened from an older board must not
    /// keep a choice nobody made. A board that no longer offers the menu
    /// keeps the last view; the host checks the commit anyway.
    pub(super) fn rebuild_local_menu(&mut self) {
        let Some(action) = &self.local_menu else {
            return;
        };
        if matches!(self.interaction, Interaction::None) {
            self.local_menu = None;
            self.picker_choice_lost = false;
            return;
        }
        let menu = action.menu(&self.board);
        self.settle_stale_hold(&menu);
        let Menu::Local(LocalMenu::Modal { modal, .. }) = menu else {
            return;
        };
        // The run a stop confirmation names has ended, so its palette is a
        // list again. The confirmation still names that run and no other;
        // it says the run is over, and committing it then only closes it.
        if let Interaction::Confirm(confirm) = &mut self.interaction
            && let crate::ConfirmAction::KillAction { run_id, .. } = &confirm.action
            && !self.board.run_active(run_id)
            && let Some(name) = confirm
                .title
                .strip_prefix("Stop ")
                .and_then(|title| title.strip_suffix('?'))
        {
            confirm.title = format!("{name} has finished");
            confirm.lines = vec!["Nothing is left to stop.".into()];
            confirm.selected = 0;
            confirm.action = crate::ConfirmAction::Finished;
            return;
        }
        match (&mut self.interaction, *modal) {
            (
                Interaction::Picker(picker),
                UiModal::Picker {
                    action,
                    title,
                    options,
                    selected: default,
                },
            ) if discriminant(&picker.action) == discriminant(&action) => {
                let current = picker.options.get(picker.selected);
                let untouched = current.map(identity) == self.local_menu_default;
                let default = Self::picker_default(
                    self.last_section_target.as_ref(),
                    &action,
                    &options,
                    default,
                );
                let found = current.filter(|_| !untouched).and_then(|current| {
                    // Two options may share a value (`ready` and `ready +
                    // verify in <stage>`); their chord tells them apart.
                    options
                        .iter()
                        .position(|option| {
                            current.value.is_some() && identity(option) == identity(current)
                        })
                        .or_else(|| {
                            options.iter().position(|option| {
                                current.value.is_some() && option.value == current.value
                            })
                        })
                        .or_else(|| {
                            options
                                .iter()
                                .position(|option| option.label == current.label)
                        })
                });
                // The cursor stays near where it was, but that row is now a
                // different choice: a commit key already on its way must not
                // act on it.
                if let Some(current) = current.filter(|_| !untouched && found.is_none()) {
                    self.picker_choice_lost = true;
                    self.toast = Some((
                        format!("{} is no longer available", current.label),
                        ToastTone::Notice,
                    ));
                }
                let selected = found
                    .unwrap_or(if untouched { default } else { picker.selected })
                    .min(options.len().saturating_sub(1));
                if untouched {
                    self.local_menu_default = options.get(selected).map(identity);
                }
                *picker = PickerPrompt {
                    action,
                    title,
                    options,
                    selected,
                };
            }
            // A confirmation never changes what it will do while it is open:
            // only its wording follows the board. When its subject changed
            // (another run in the slot, another pull request or checkout)
            // the person keeps the one they are reading and the host refuses.
            (
                Interaction::Confirm(confirm),
                UiModal::Confirm {
                    action,
                    title,
                    lines,
                    cancel_key,
                },
            ) if confirm.action == action => {
                // A confirmation reopened after a refusal keeps saying so.
                let mut lines = lines;
                let kept = confirm
                    .lines
                    .iter()
                    .take(2)
                    .filter(|line| line.starts_with(CHANGED) || *line == STILL_READING)
                    .cloned()
                    .collect::<Vec<_>>();
                lines.splice(0..0, kept);
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
    ///
    /// The reopened menu answers a reply, not a key, and names the newer
    /// state. A confirmation says so in its first line and waits out the
    /// settle delay, so a key already on its way cannot commit what the
    /// person has not read: that would be the retry with newer state the
    /// refusal exists to prevent.
    pub(super) fn reopen_stale_menu(&mut self, operation_id: Option<u64>, message: String) {
        let Some((id, action)) = self.last_menu.take() else {
            return;
        };
        if Some(id) != operation_id || !matches!(self.interaction, Interaction::None) {
            return;
        }
        let refused = self.committed_confirm.take();
        if let Menu::Local(menu) = action.menu(&self.board) {
            self.show_local_menu(action, menu);
            if let Interaction::Confirm(confirm) = &mut self.interaction {
                let now = tokio::time::Instant::now();
                confirm.lines.insert(0, changed_line(&message));
                self.confirm_settles_at = Some(now + super::reply::CONFIRM_SETTLE);
                // The refusal usually arrives before the read it asked for.
                // The board then still shows what was refused, and so does
                // this confirmation: it waits for the newer board.
                if refused.as_ref() == Some(&confirm.action) {
                    confirm.lines.insert(1, STILL_READING.into());
                    self.stale_hold = Some((confirm.action.clone(), now + STALE_HOLD));
                }
            }
        }
        self.toast = Some((message, ToastTone::Failed));
    }

    pub(super) fn stale_hold_deadline(&self) -> Option<tokio::time::Instant> {
        self.stale_hold.as_ref().map(|(_, until)| *until)
    }

    /// Look at the newly presented board for a confirmation held after a
    /// stale refusal. Once the board shows something other than what was
    /// refused, the confirmation is rebuilt from it, says so no longer, and
    /// settles again before it can commit. A board that has not changed by
    /// the deadline is as new as it gets, and its host checks the commit.
    fn settle_stale_hold(&mut self, menu: &Menu) {
        let Some((refused, until)) = self.stale_hold.clone() else {
            return;
        };
        let Interaction::Confirm(confirm) = &mut self.interaction else {
            self.stale_hold = None;
            return;
        };
        if confirm.action != refused {
            self.stale_hold = None;
            return;
        }
        let now = tokio::time::Instant::now();
        match menu {
            Menu::Local(LocalMenu::Modal { modal, .. }) => {
                if let UiModal::Confirm {
                    action,
                    title,
                    lines,
                    cancel_key,
                } = &**modal
                    && *action != refused
                {
                    let mut lines = lines.clone();
                    if let Some(changed) = confirm.lines.first() {
                        lines.insert(0, changed.clone());
                    }
                    *confirm = ConfirmPrompt {
                        action: action.clone(),
                        title: title.clone(),
                        lines,
                        selected: 0,
                        cancel_key: *cancel_key,
                    };
                    self.confirm_settles_at = Some(now + super::reply::CONFIRM_SETTLE);
                    self.stale_hold = None;
                } else if now >= until {
                    confirm.lines.retain(|line| line != STILL_READING);
                    self.stale_hold = None;
                }
            }
            // What it asked about is gone from the newer board.
            Menu::Local(LocalMenu::Notice(text) | LocalMenu::NotYet(text))
            | Menu::Local(LocalMenu::Ask { text, .. }) => {
                self.toast = Some((text.clone(), ToastTone::Notice));
                self.interaction = Interaction::None;
                self.local_menu = None;
                self.stale_hold = None;
            }
            _ => {
                if now >= until {
                    confirm.lines.retain(|line| line != STILL_READING);
                    self.stale_hold = None;
                }
            }
        }
    }
}
