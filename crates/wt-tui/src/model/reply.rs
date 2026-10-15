//! How a host reply updates the model: its toast, modal and selection.

use super::{
    ConfirmPrompt, Interaction, Model, PickerPrompt, ReviewerPrompt, TextPrompt, ToastTone,
};
use crate::{LineEditor, PickerAction, TextAction, UiModal, UiReply};

impl Model {
    pub fn reply(&mut self, mut reply: UiReply) -> Option<crate::TerminalHandoff> {
        if let Some(progress) = reply.progress.take() {
            if self.pending_request.as_ref().is_some_and(|request| {
                Some(request.id) == reply.operation_id
                    && Some(request.generation) == reply.ui_generation
                    && request.generation == self.ui_generation
            }) {
                self.toast = Some((progress, ToastTone::Info));
            }
            return None;
        }
        if let Some(prepared) = reply.creation_prepared.take() {
            self.creation_prepared(reply.operation_id, prepared);
            return None;
        }
        let completed_request = if self
            .pending_request
            .as_ref()
            .is_some_and(|request| Some(request.id) == reply.operation_id)
        {
            self.pending_request.take()
        } else {
            None
        };
        let typeahead = completed_request
            .as_ref()
            .filter(|request| request.generation == self.ui_generation && request.preparation)
            .map(|request| (request.typeahead.clone(), request.started))
            .filter(|(keys, _)| !keys.is_empty());
        let selected_create = self.reply_pending(&reply);
        let (stale, stale_id, stale_message) =
            (reply.stale, reply.operation_id, reply.message.clone());
        let presentation_current = reply
            .ui_generation
            .is_none_or(|generation| generation == self.ui_generation);
        if reply.handoff.is_some() && !presentation_current {
            // Dropping the unacknowledged ticket releases its controller wait
            // without suspending this newer interaction or attaching a client.
            return None;
        }
        let selection_current = presentation_current
            || (selected_create
                && !self.history.active
                && !self.help
                && !self.show_perf
                && self.title_prompt.is_none()
                && matches!(self.interaction, Interaction::None));
        if reply.select_when_visible.is_some() && selection_current {
            self.history.active = false;
        }
        if reply.modal.is_some() && presentation_current {
            self.interaction_host = reply.modal_host;
        }
        if reply.handoff.is_some() {
            // Entering a session (perf `i` included) closes the overlays so
            // they do not paint back over the board on return.
            self.show_perf = false;
            self.help = false;
        }
        let tone = if reply.failed {
            ToastTone::Failed
        } else if reply.notice {
            ToastTone::Notice
        } else {
            ToastTone::Info
        };
        // A different operation finishing cannot clear the request the user
        // is waiting on. Failures are already persisted in the attention feed.
        let preserves_failure = completed_request.is_none()
            && self
                .toast
                .as_ref()
                .is_some_and(|(_, tone)| *tone == ToastTone::Failed)
            && !reply.failed;
        let empty_unrelated_completion = completed_request.is_none()
            && reply.message.trim().is_empty()
            && reply.modal.is_none()
            && reply.handoff.is_none()
            && !reply.failed;
        if self.pending_request.is_none() && !preserves_failure && !empty_unrelated_completion {
            let message = if reply.message.is_empty()
                && reply.modal.is_none()
                && reply.handoff.is_none()
                && completed_request.is_some()
            {
                if reply.failed {
                    "Command failed; see attention".into()
                } else if completed_request
                    .as_ref()
                    .is_some_and(|request| request.preparation)
                {
                    "Request finished without opening a menu".into()
                } else {
                    "Done".into()
                }
            } else {
                reply.message
            };
            self.toast = Some((message, tone));
        }
        self.interaction = match reply.modal.filter(|_| presentation_current) {
            Some(UiModal::Reviewers {
                key,
                pr_number,
                original,
                candidates,
            }) => Interaction::Reviewers(ReviewerPrompt {
                key,
                pr_number,
                original,
                candidates,
                selected: 0,
            }),
            Some(UiModal::Log {
                title,
                lines,
                close_key,
                refresh,
            }) => {
                // A refreshed overlay keeps its scroll position.
                let scroll = match &self.interaction {
                    Interaction::Log {
                        title: open,
                        scroll,
                        ..
                    } if refresh.is_some() && *open == title => {
                        (*scroll).min(lines.len().saturating_sub(1))
                    }
                    _ => 0,
                };
                self.log_close_key = close_key;
                self.log_refresh = refresh.map(|action| *action);
                Interaction::Log {
                    title,
                    lines,
                    scroll,
                }
            }
            Some(UiModal::Confirm {
                action,
                title,
                lines,
                cancel_key,
            }) => Interaction::Confirm(ConfirmPrompt {
                action,
                title,
                lines,
                selected: 0,
                cancel_key,
            }),
            Some(UiModal::Picker {
                action,
                title,
                options,
                selected,
            }) => {
                let selected = if matches!(action, PickerAction::Section { .. }) {
                    self.last_section_target
                        .as_ref()
                        .and_then(|target| {
                            options.iter().position(|option| &option.value == target)
                        })
                        .unwrap_or(selected)
                } else {
                    selected
                }
                .min(options.len().saturating_sub(1));
                Interaction::Picker(PickerPrompt {
                    action,
                    title,
                    options,
                    selected,
                })
            }
            Some(UiModal::Text {
                action,
                prompt,
                initial,
                allow_empty,
            }) => {
                // A plain create prompt names its host like `n` does, so a
                // host picked in the Ctrl+N picker is visible and Tab works.
                let prompt = if matches!(action, TextAction::Create) {
                    self.create_prompt(self.interaction_host.as_deref())
                } else {
                    prompt
                };
                Interaction::Text(TextPrompt {
                    action,
                    prompt,
                    editor: LineEditor::new(&initial),
                    allow_empty,
                })
            }
            None => std::mem::take(&mut self.interaction),
        };
        if selection_current && let Some(key) = reply.select_when_visible {
            if let Some(index) = self.row_position(&key) {
                self.select(index);
            } else {
                self.pending_selection = Some(key);
            }
        }
        if stale {
            // Refresh even when the user has moved on; only the reopen needs
            // the view the commit came from.
            self.refresh_requested = true;
            if presentation_current {
                self.reopen_stale_menu(stale_id, stale_message);
            }
        }
        if let Some((keys, started)) = typeahead {
            if presentation_current
                && !reply.failed
                && !matches!(self.interaction, Interaction::None)
                && started.elapsed() <= super::input::TYPEAHEAD_TTL
            {
                self.replay_typeahead(keys);
            } else if !reply.failed {
                // Keep the reply's own message; the dropped keys are secondary.
                let text = match self.toast.take() {
                    Some((text, _)) if !text.trim().is_empty() => {
                        format!("{text}; keys typed while it loaded were dropped")
                    }
                    _ => "Keys typed while the menu loaded were dropped".into(),
                };
                self.toast = Some((text, ToastTone::Notice));
            }
        }
        reply.handoff
    }

    /// Feed keys typed during loading to the menu that just opened. Replay
    /// stops when the menu closes, so a typed `q` or Esc can never fall
    /// through to the board. A key that requests another menu carries the
    /// remaining keys to that request.
    fn replay_typeahead(&mut self, keys: Vec<(crossterm::event::KeyEvent, usize)>) {
        let mut keys = keys.into_iter();
        while let Some((key, height)) = keys.next() {
            if matches!(self.interaction, Interaction::None) {
                break;
            }
            match self.input(key, height) {
                super::InputResult::Action(action) => {
                    self.follow_up = Some(action);
                    self.carry_typeahead = keys.collect();
                    return;
                }
                super::InputResult::Quit => break,
                _ => {}
            }
        }
        self.last_key = None;
    }
}
