//! How a host reply updates the model: its toast, modal and selection.

use super::{
    ConfirmPrompt, Interaction, Model, PickerPrompt, ReviewerPrompt, TextPrompt, ToastTone,
};
use crate::{LineEditor, PickerAction, TextAction, UiModal, UiReply};

/// How long a confirmation the user did not ask for ignores the keys that
/// commit it. It can appear under a key already on its way to the board.
pub(crate) const CONFIRM_SETTLE: std::time::Duration = std::time::Duration::from_millis(400);

/// How many unanswered requests the model remembers having begun. A reply
/// to one forgotten is treated like a background read's.
pub(crate) const BEGUN_LIMIT: usize = 32;

impl Model {
    /// Something other than the board takes the next key: a menu, overlay or
    /// prompt that is open, loading ones included. A confirm opened here by
    /// a reply would take a key typed for that layer.
    pub(crate) fn layer_open(&self) -> bool {
        !matches!(self.interaction, Interaction::None)
            || self.yank.is_some()
            || self.help
            || self.show_perf
            || self.history.active
            || self.title_prompt.is_some()
    }

    pub(crate) fn show_next_removal_review(&mut self) {
        if self.layer_open() {
            return;
        }
        if let Some((prompt, host)) = self.removal_reviews.pop_front() {
            self.interaction_host = host;
            self.interaction = self.reply_confirm(prompt, false);
        }
    }

    /// A confirmation that arrived in a reply. One the user did not ask for
    /// (a hazard review, a dialog answering some other request) can appear
    /// under a key meant for the board, so the keys that commit it wait out
    /// [`CONFIRM_SETTLE`]. Cancelling and scrolling work at once.
    fn reply_confirm(&mut self, prompt: ConfirmPrompt, requested: bool) -> Interaction {
        self.confirm_settles_at =
            (!requested).then(|| tokio::time::Instant::now() + CONFIRM_SETTLE);
        Interaction::Confirm(prompt)
    }

    /// The user is back from a session. A confirmation that opened while
    /// they were away is new to them now.
    pub(crate) fn restart_confirm_settle(&mut self) {
        if self.confirm_settles_at.is_some() {
            self.confirm_settles_at = Some(tokio::time::Instant::now() + CONFIRM_SETTLE);
        }
    }

    pub fn reply(&mut self, reply: UiReply) -> Option<crate::TerminalHandoff> {
        self.reply_from(reply, false)
    }

    /// Open a modal built from the board for the key being dispatched.
    pub(super) fn show_modal(&mut self, modal: UiModal, host: Option<String>) {
        self.reply_from(
            UiReply {
                modal: Some(modal),
                modal_host: host,
                ..Default::default()
            },
            true,
        );
    }

    fn reply_from(&mut self, mut reply: UiReply, by_key: bool) -> Option<crate::TerminalHandoff> {
        if let Some(progress) = reply.progress.take() {
            if self.pending_request.as_ref().is_some_and(|request| {
                Some(request.id) == reply.operation_id
                    && Some(request.generation) == reply.ui_generation
                    && request.generation == self.ui_generation
            }) {
                // An earlier request's failure stays beside the progress of
                // the one in flight until the user acts again.
                self.toast = Some(match &self.earlier_failure {
                    Some(failure) => (format!("{failure}; {progress}"), ToastTone::Failed),
                    None => (progress, ToastTone::Info),
                });
            }
            return None;
        }
        if let Some(accepted) = reply.creation_placement.take() {
            // The user has since filed the row elsewhere; showing where it
            // was created would move it back until that move runs.
            if self.creation_move_owed(&accepted.key) {
                return None;
            }
            let now = tokio::time::Instant::now();
            let id = self.next_op_id();
            let mutation = crate::UiMutation {
                target: wt_runtime::Target::row(None, accepted.key),
                kind: crate::LayoutChange::Creation {
                    placement: accepted.placement,
                }
                .into(),
            };
            self.pending.begin(id, mutation, now);
            self.pending
                .accept_with(id, accepted.accepted_at, now, |_| {});
            self.reproject(now);
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
        // Whether the user asked for this: a request the model began, not a
        // read the terminal sent on its own.
        let begun = reply
            .operation_id
            .is_some_and(|id| self.begun.contains(&id));
        if reply.handoff.is_none() {
            self.begun.retain(|id| Some(*id) != reply.operation_id);
        }
        // The overlay this reply answers. Its content replaces it, and an
        // answer without content closes it; either way it stops loading.
        let filled = match &self.interaction {
            Interaction::Loading(loading)
                if loading.request.is_some() && loading.request == reply.operation_id =>
            {
                match std::mem::take(&mut self.interaction) {
                    Interaction::Loading(loading) => Some(loading),
                    _ => None,
                }
            }
            _ => None,
        };
        // A confirm is the user's own when the key they pressed opened it.
        let requested_menu = by_key;
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
            // A dialog that takes a loading overlay's place ends its load:
            // nothing is left on screen for that request to fill.
            if let Interaction::Loading(loading) = &self.interaction
                && self
                    .pending_request
                    .as_ref()
                    .is_some_and(|request| Some(request.id) == loading.request)
            {
                self.pending_request = None;
            }
            // A dialog answering the request in flight must not discard a
            // removal review already on screen; the review waits its turn.
            if matches!(
                &self.interaction,
                Interaction::Confirm(ConfirmPrompt {
                    action: crate::ConfirmAction::Remove {
                        revision: Some(_),
                        ..
                    },
                    ..
                })
            ) && let Interaction::Confirm(prompt) = std::mem::take(&mut self.interaction)
            {
                self.removal_reviews
                    .push_front((prompt, self.interaction_host.take()));
            }
            self.interaction_host = reply.modal_host;
            self.confirm_settles_at = None;
            // A picker that lost its highlighted choice is being replaced;
            // its guard must not eat a key meant for the new dialog.
            self.picker_choice_lost = false;
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
        // The request in flight owns the toast, except that the failure,
        // refusal or notice of an earlier request the user made must still
        // be said: its optimistic change reverts now, and nothing else
        // explains why. A background read's reply never interrupts.
        let speaks_over_pending = completed_request.is_none()
            && begun
            && (reply.failed || reply.notice)
            && !reply.message.trim().is_empty();
        if speaks_over_pending && reply.failed && self.pending_request.is_some() {
            self.earlier_failure = Some(reply.message.clone());
        }
        // That failure is still unacknowledged when the request in flight
        // succeeds, so the success joins it instead of replacing it.
        let earlier_failure = completed_request
            .as_ref()
            .and_then(|_| self.earlier_failure.take())
            .filter(|_| !reply.failed);
        if (self.pending_request.is_none() || speaks_over_pending)
            && !preserves_failure
            && !empty_unrelated_completion
        {
            let message = if reply.message.is_empty()
                && reply.modal.is_none()
                && reply.handoff.is_none()
                && completed_request.is_some()
            {
                if reply.failed {
                    "command failed; see attention".into()
                } else if filled.is_some() {
                    "nothing to show".into()
                } else {
                    "done".into()
                }
            } else {
                reply.message
            };
            self.toast = Some(match earlier_failure {
                Some(failure) if message.trim().is_empty() => (failure, ToastTone::Failed),
                Some(failure) => (format!("{failure}; {message}"), ToastTone::Failed),
                None => (message, tone),
            });
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
                let open = match &self.interaction {
                    Interaction::Log {
                        title: open,
                        scroll,
                        ..
                    } if refresh.is_some() && *open == title => Some(*scroll),
                    _ => None,
                };
                let scroll = open.map_or(0, |scroll| scroll.min(lines.len().saturating_sub(1)));
                self.log_close_key = close_key;
                // A live overlay asks again with the request that opened
                // it, which names its row as this board does; the one a
                // host sends back names the row by that host's own key.
                // So the first fill keeps the opener's request, and a
                // refresh of the open overlay keeps asking as it has been.
                self.log_refresh = match (&filled, refresh) {
                    (Some(loading), Some(_)) => Some(loading.action.clone()),
                    (None, Some(_)) if open.is_some() && self.log_refresh.is_some() => {
                        self.log_refresh.take()
                    }
                    (_, refresh) => refresh.map(|action| *action),
                };
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
            }) => {
                // A palette key asks for a list. The stop confirmation that
                // opens in its place while an action runs was not asked for.
                let requested =
                    requested_menu && !matches!(action, crate::ConfirmAction::KillAction { .. });
                self.reply_confirm(
                    ConfirmPrompt {
                        action,
                        title,
                        lines,
                        selected: 0,
                        cancel_key,
                    },
                    requested,
                )
            }
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
                let prompt = if matches!(action, TextAction::Create { .. }) {
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
        if let Some(loading) = &filled {
            tracing::info!(
                operation_id = loading.request,
                load_ms = u64::try_from(loading.started.elapsed().as_millis()).unwrap_or(u64::MAX),
                filled = matches!(self.interaction, Interaction::Log { .. }),
                "host overlay answered"
            );
        }
        if reply.handoff.is_none() {
            self.show_next_removal_review();
        }
        reply.handoff
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
}
