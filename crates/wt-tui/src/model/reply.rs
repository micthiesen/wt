//! How a host reply updates the model: its toast, modal and selection.

use super::{
    ConfirmPrompt, Interaction, Model, PickerPrompt, ReviewerPrompt, TextPrompt, ToastTone,
};
use crate::{LineEditor, PickerAction, TextAction, UiModal, UiReply};

impl Model {
    pub fn reply(&mut self, reply: UiReply) -> Option<crate::TerminalHandoff> {
        let selected_create = self.reply_pending(&reply);
        let current = reply
            .ui_generation
            .is_none_or(|generation| generation == self.ui_generation)
            || selected_create;
        if reply.select_when_visible.is_some() && current {
            self.history.active = false;
        }
        if reply.modal.is_some() && current {
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
        self.toast = Some((reply.message, tone));
        self.interaction = match reply.modal.filter(|_| current) {
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
                let prompt = if matches!(action, TextAction::Create) && initial.is_empty() {
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
        if current && let Some(key) = reply.select_when_visible {
            if let Some(index) = self.row_position(&key) {
                self.select(index);
            } else {
                self.pending_selection = Some(key);
            }
        }
        reply.handoff
    }
}
