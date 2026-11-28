//! Choosing a palette entry. The model opens the entry's prompt itself and
//! sends the commit with what the palette showed; the owning host checks it
//! again. An entry that only lacks a live GitHub read of its branch waits
//! here, in the open palette, where the person sees it and can leave.

use std::time::Duration;

use tokio::time::Instant;

use super::{InputResult, Interaction, Model, ToastTone};
use crate::menus::palette::{self, Scope};
use crate::{
    AUTO_MERGE_ACTION_ID, ActionEntry, ActionSeen, ActionState, ActionSurface, Blocker,
    DEV_LOGS_ACTION_ID, OPEN_EDITOR_ACTION_ID, PickerAction, Seen, UiAction,
};

/// How long a chosen entry waits for GitHub to confirm its branch.
const GITHUB_WAIT: Duration = Duration::from_secs(12);

/// What the waiting entry's row reads while it waits.
const WAITING: &str = "waiting for GitHub… Esc cancels";

/// An entry chosen while no live GitHub read covered its row's branch. It
/// runs once the board shows one, for as long as its palette stays open.
#[derive(Debug)]
pub(crate) struct PaletteWait {
    surface: ActionSurface,
    id: String,
    until: Instant,
}

/// What choosing an entry did. `keep_open` leaves the palette up.
pub(super) struct Pick {
    pub result: InputResult,
    pub keep_open: bool,
}

impl Pick {
    fn close(result: InputResult) -> Self {
        Self {
            result,
            keep_open: false,
        }
    }
}

impl Model {
    /// Act on the palette entry `id`: send what needs no prompt, open the
    /// prompt of what does, and say why an unavailable entry cannot run.
    pub(super) fn pick_action(&mut self, surface: &ActionSurface, id: Option<&str>) -> Pick {
        self.palette_wait = None;
        let Some(id) = id else {
            return Pick {
                result: InputResult::Unchanged,
                keep_open: true,
            };
        };
        let board = self.board.clone();
        let Some(scope) = Scope::of(&board, surface) else {
            return Pick::close(self.notify("worktree is no longer on the board"));
        };
        if let (Some(reason), ActionSurface::Slot { target }) =
            (scope.slot_blocked(surface), surface)
        {
            return self.unavailable(format!(
                "{} slot unavailable: {reason}",
                crate::slot_name(*target)
            ));
        }
        match (surface, id) {
            (ActionSurface::Row { key }, AUTO_MERGE_ACTION_ID) => {
                let pr = scope.row.and_then(|row| row.pr.as_ref());
                if !pr.is_some_and(|pr| pr.number.is_some()) {
                    return self.unavailable("merge when ready: no PR".into());
                }
                // The direction comes from what the palette showed.
                let armed = pr.is_some_and(|pr| pr.auto_merge_armed || pr.merge_queue.is_some());
                return Pick::close(InputResult::Action(UiAction::GithubSetAutoMerge {
                    key: key.clone(),
                    enable: !armed,
                    pin: None,
                }));
            }
            (ActionSurface::Row { key }, DEV_LOGS_ACTION_ID) => {
                return Pick::close(InputResult::Action(UiAction::PrepareDevLogs {
                    key: key.clone(),
                }));
            }
            (ActionSurface::Slot { target }, OPEN_EDITOR_ACTION_ID) => {
                return Pick::close(InputResult::Action(UiAction::OpenSlotEditor {
                    target: *target,
                }));
            }
            _ => {}
        }
        let Some(entry) = scope.display.actions.entry(surface, id) else {
            return self.unavailable("that action is no longer available".into());
        };
        match palette::blocker(entry, surface, scope.facts()) {
            Some(Blocker::PrUnconfirmed) => {
                self.palette_wait = Some(PaletteWait {
                    surface: surface.clone(),
                    id: id.to_owned(),
                    until: Instant::now() + GITHUB_WAIT,
                });
                self.refresh_requested = true;
                let branch = scope.row.map_or("its branch", |row| row.branch.as_str());
                self.toast = Some((
                    format!(
                        "{}: waiting for GitHub to confirm {branch}… Esc cancels",
                        entry.name
                    ),
                    ToastTone::Info,
                ));
                Pick {
                    result: InputResult::Draw,
                    keep_open: true,
                }
            }
            Some(blocker) => self.unavailable(format!("{}: {}", entry.name, blocker.reason())),
            None => {
                let seen = scope.seen(entry);
                Pick::close(self.proceed(surface, entry, None, seen, scope.state))
            }
        }
    }

    /// The value an action asked for was typed or picked.
    pub(super) fn action_arg(
        &mut self,
        surface: &ActionSurface,
        id: &str,
        seen: Option<Seen<ActionSeen>>,
        arg: String,
    ) -> InputResult {
        let board = self.board.clone();
        let Some(scope) = Scope::of(&board, surface) else {
            return self.notify("worktree is no longer on the board");
        };
        let Some(entry) = scope.display.actions.entry(surface, id) else {
            return self.notify("that action is no longer available");
        };
        let seen = seen.unwrap_or_else(|| scope.seen(entry));
        self.proceed(surface, entry, Some(arg), seen, scope.state)
    }

    /// The next step of an available entry: its value prompt, its extra
    /// instructions, or the commit itself.
    fn proceed(
        &mut self,
        surface: &ActionSurface,
        entry: &ActionEntry,
        arg: Option<String>,
        seen: Seen<ActionSeen>,
        state: &ActionState,
    ) -> InputResult {
        if let (Some(label), None) = (&entry.arg_prompt, &arg) {
            let recent = state
                .recent_args
                .get(&entry.id)
                .map_or(&[][..], Vec::as_slice);
            self.show_modal(
                palette::arg_modal(surface, entry, label, recent, seen),
                None,
            );
            return InputResult::Draw;
        }
        if entry.shell || entry.direct {
            return InputResult::Action(UiAction::RunAction {
                surface: surface.clone(),
                id: entry.id.clone(),
                arg,
                extras: String::new(),
                seen: Some(seen),
            });
        }
        self.show_modal(palette::extras_modal(surface, entry, arg, seen), None);
        InputResult::Draw
    }

    /// An entry the palette lists as unavailable was chosen. The sources
    /// are asked again, in case the board is behind what the person knows.
    fn unavailable(&mut self, text: String) -> Pick {
        self.refresh_requested = true;
        self.toast = Some((text, ToastTone::Notice));
        Pick {
            result: InputResult::Draw,
            keep_open: true,
        }
    }

    /// The palette closed without a choice: nothing waits behind it.
    pub(super) fn palette_closed(&mut self) {
        self.palette_wait = None;
    }

    pub(super) fn palette_wait_deadline(&self) -> Option<Instant> {
        self.palette_wait.as_ref().map(|wait| wait.until)
    }

    /// Show which entry is waiting. The palette is rebuilt from each board,
    /// so this is applied again after every rebuild.
    pub(super) fn mark_palette_wait(&mut self) {
        let (Some(wait), Interaction::Picker(picker)) = (&self.palette_wait, &mut self.interaction)
        else {
            return;
        };
        if !matches!(&picker.action, PickerAction::Actions { surface } if *surface == wait.surface)
        {
            return;
        }
        if let Some(option) = picker
            .options
            .iter_mut()
            .find(|option| option.value.as_deref() == Some(wait.id.as_str()))
        {
            option.detail = Some(WAITING.into());
        }
    }

    /// Look at a newly presented board for the waiting entry: run it once
    /// GitHub has confirmed its branch, say why it cannot run, or give up
    /// when the wait is over.
    pub(super) fn settle_palette_wait(&mut self, now: Instant) {
        let Some(wait) = &self.palette_wait else {
            return;
        };
        let open = matches!(
            &self.interaction,
            Interaction::Picker(picker)
                if matches!(&picker.action, PickerAction::Actions { surface } if *surface == wait.surface)
        );
        if !open {
            self.palette_wait = None;
            return;
        }
        let board = self.board.clone();
        let (surface, id, until) = (wait.surface.clone(), wait.id.clone(), wait.until);
        let found = Scope::of(&board, &surface).and_then(|scope| {
            let entry = scope.display.actions.entry(&surface, &id)?;
            Some((scope, entry))
        });
        let Some((scope, entry)) = found else {
            self.palette_wait = None;
            self.toast = Some((
                "that action is no longer available".into(),
                ToastTone::Notice,
            ));
            return;
        };
        match palette::blocker(entry, &surface, scope.facts()) {
            Some(Blocker::PrUnconfirmed) if now < until => self.mark_palette_wait(),
            Some(blocker) => {
                self.palette_wait = None;
                self.toast = Some((
                    format!("{}: {}", entry.name, blocker.reason()),
                    ToastTone::Notice,
                ));
            }
            // A request already queued behind a reply goes first.
            None if self.follow_up.is_some() => self.mark_palette_wait(),
            None => {
                self.palette_wait = None;
                self.interaction = Interaction::None;
                let menu = self.local_menu.take();
                let seen = scope.seen(entry);
                match self.proceed(&surface, entry, None, seen, scope.state) {
                    InputResult::Action(action) => {
                        // A stale answer reopens the palette it came from.
                        self.committed_menu = menu;
                        self.follow_up = Some(action);
                    }
                    _ => self.local_menu = menu,
                }
            }
        }
    }
}
