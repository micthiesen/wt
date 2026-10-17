//! How a host reply updates the model: its toast, modal and selection.

use super::{
    ConfirmPrompt, Interaction, Model, PickerPrompt, ReviewerPrompt, TextPrompt, ToastTone,
};
use crate::{LineEditor, PickerAction, TextAction, UiModal, UiReply};

impl Model {
    pub(crate) fn show_next_removal_review(&mut self) {
        if !matches!(self.interaction, Interaction::None)
            || self.help
            || self.show_perf
            || self.history.active
            || self.title_prompt.is_some()
            || self
                .pending_request
                .as_ref()
                .is_some_and(|request| request.preparation)
        {
            return;
        }
        if let Some((prompt, host)) = self.removal_reviews.pop_front() {
            self.interaction_host = host;
            self.interaction = Interaction::Confirm(prompt);
        }
    }
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
        let mut completed_request = if self
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
            .map(|request| {
                (
                    request.typeahead.clone(),
                    request.started,
                    request.typeahead_overflow,
                )
            })
            .filter(|(keys, _, _)| !keys.is_empty());
        let selected_create = self.reply_pending(&reply);
        let (stale, stale_id, stale_message) =
            (reply.stale, reply.operation_id, reply.message.clone());
        // Removal reviews belong to accepted requests, not the current cursor.
        // Keep each review until it can be shown without replacing another menu.
        if matches!(
            &reply.modal,
            Some(UiModal::Confirm {
                action: crate::ConfirmAction::Remove {
                    revision: Some(_),
                    ..
                },
                ..
            })
        ) && let Some(UiModal::Confirm {
            action,
            title,
            lines,
            cancel_key,
        }) = reply.modal.take()
        {
            self.removal_reviews.push_back((
                ConfirmPrompt {
                    action,
                    title,
                    lines,
                    selected: 0,
                    cancel_key,
                },
                reply.modal_host.take(),
            ));
        }
        let presentation_current = reply
            .ui_generation
            .is_none_or(|generation| generation == self.ui_generation);
        if reply.handoff.is_some() && !presentation_current {
            // Dropping the unacknowledged ticket releases its controller wait
            // without suspending this newer interaction or attaching a client.
            return None;
        }
        if reply.handoff.is_some()
            && let Some(request) = &mut completed_request
            && let Some(guard) = request._cancel_on_drop.take()
        {
            // The terminal now owns the session. Completing the UI request
            // must not cancel its attach client.
            guard.disarm();
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
                let selected = Self::picker_default(
                    self.last_section_target.as_ref(),
                    &action,
                    &options,
                    selected,
                );
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
        if let Some((keys, started, overflow)) = typeahead {
            if presentation_current
                && !reply.failed
                && !matches!(self.interaction, Interaction::None)
                && started.elapsed() <= super::input::TYPEAHEAD_TTL
            {
                self.replay_typeahead(keys);
                // A replayed commit closes the menu and reports its own
                // result; the overflow only matters to a menu still open.
                if overflow && !matches!(self.interaction, Interaction::None) {
                    self.append_typeahead_notice(&format!(
                        "only the first {} keys typed while the menu loaded were used",
                        super::input::TYPEAHEAD_LIMIT
                    ));
                }
            } else if !reply.failed {
                self.append_typeahead_notice("keys typed while the menu loaded were dropped");
            }
        }
        if reply.handoff.is_none() {
            self.show_next_removal_review();
        }
        reply.handoff
    }

    /// Report what happened to typeahead without hiding the reply's own
    /// message; the keys are secondary.
    fn append_typeahead_notice(&mut self, notice: &str) {
        let text = match self.toast.take() {
            Some((text, _)) if !text.trim().is_empty() => format!("{text}; {notice}"),
            _ => {
                let mut chars = notice.chars();
                chars
                    .next()
                    .map(|first| first.to_uppercase().chain(chars).collect())
                    .unwrap_or_default()
            }
        };
        self.toast = Some((text, ToastTone::Notice));
    }

    /// Where a picker's cursor starts: the section picker on the section
    /// last moved to, every other picker on its builder's default.
    pub(super) fn picker_default(
        last_section_target: Option<&Option<String>>,
        action: &PickerAction,
        options: &[crate::PickerOption],
        selected: usize,
    ) -> usize {
        if matches!(action, PickerAction::Section { .. }) {
            last_section_target
                .and_then(|target| options.iter().position(|option| &option.value == target))
                .unwrap_or(selected)
        } else {
            selected
        }
        .min(options.len().saturating_sub(1))
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

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent};
    use wt_runtime::{SourceSnapshot, SourceState};

    use super::super::{InputResult, Interaction, Model, ToastTone, input::TYPEAHEAD_LIMIT};
    use crate::{ActionSurface, Board, BoardRow, PickerAction, PickerOption, UiModal, UiReply};

    /// Open the row's action menu with `keys` typed while it loaded and a
    /// reply carrying `message`.
    fn overflowing_menu(first: KeyCode, message: &str) -> Model {
        let mut model = Model::default();
        model.apply(SourceSnapshot::test_live(
            Some(Board {
                rows: vec![BoardRow {
                    key: "one".into(),
                    title: "one".into(),
                    ..BoardRow::default()
                }],
                ..Board::default()
            }),
            SourceState::Ready,
            wt_core::Domain::Git,
        ));
        let InputResult::Action(action) = model.input(KeyEvent::from(KeyCode::Char('!')), 20)
        else {
            panic!("`!` opens the action menu")
        };
        let generation = model.ui_generation;
        model.begin_action(42, &action);
        model.input(KeyEvent::from(first), 20);
        for _ in 0..TYPEAHEAD_LIMIT {
            model.input(KeyEvent::from(KeyCode::Down), 20);
        }
        model.reply(UiReply {
            operation_id: Some(42),
            ui_generation: Some(generation),
            message: message.into(),
            modal: Some(UiModal::Picker {
                action: PickerAction::Actions {
                    surface: ActionSurface::Row { key: "one".into() },
                },
                title: "Actions".into(),
                selected: 0,
                options: vec![PickerOption {
                    value: Some("a".into()),
                    label: "a".into(),
                    chord: None,
                    note: None,
                    verify_after_merge: None,
                    detail: None,
                }],
            }),
            ..Default::default()
        });
        model
    }

    #[test]
    fn typeahead_overflow_keeps_the_reply_message() {
        let model = overflowing_menu(KeyCode::Down, "Loaded from cache");
        assert!(matches!(model.interaction, Interaction::Picker(_)));
        let (text, tone) = model.toast.clone().unwrap();
        assert_eq!(tone, ToastTone::Notice);
        assert!(
            text.starts_with("Loaded from cache; only the first"),
            "{text}"
        );
    }

    #[test]
    fn typeahead_overflow_is_not_reported_after_replay_closes_the_menu() {
        let model = overflowing_menu(KeyCode::Esc, "");
        assert!(matches!(model.interaction, Interaction::None));
        assert!(
            model
                .toast
                .as_ref()
                .is_none_or(|(text, _)| !text.to_lowercase().contains("only the first")),
            "{:?}",
            model.toast
        );
    }
}
