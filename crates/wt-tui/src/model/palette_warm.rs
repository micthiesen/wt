//! Prefetch of the action palettes the selection can open. A request goes
//! out once the selection has rested, so holding a movement key does not
//! flood the host with one warm-up per row it passes.

use std::time::Duration;

use tokio::time::Instant;

use super::Model;
use crate::{ActionSurface, SessionTarget, UiAction};

/// How long the selection must stay put before its palettes are prefetched.
const REST: Duration = Duration::from_millis(200);

/// Slot palettes do not follow the selection, so they are prefetched once.
const SLOTS: [SessionTarget; 3] = [
    SessionTarget::WtSource,
    SessionTarget::Main,
    SessionTarget::Dotfiles,
];

#[derive(Debug, Default)]
pub(crate) struct PaletteWarm {
    /// Surfaces the current selection can open.
    wanted: Vec<ActionSurface>,
    /// When `wanted` last changed, plus the rest period; `None` once sent.
    due: Option<Instant>,
    /// Surfaces the host was last asked to prefetch.
    sent: Vec<ActionSurface>,
    slots_sent: bool,
}

impl Model {
    /// Note what the selection can open. Any change restarts the rest timer.
    pub(crate) fn plan_palette_warm(&mut self, now: Instant) {
        let wanted = self.palette_surfaces();
        if wanted == self.palette_warm.wanted {
            return;
        }
        self.palette_warm.wanted = wanted;
        self.palette_warm.due = Some(now + REST);
    }

    /// Queue the prefetch once the selection has rested. Nothing is queued
    /// for a selection that already has its palettes prefetched.
    pub(crate) fn fire_palette_warm(&mut self, now: Instant) {
        let Some(due) = self.palette_warm.due else {
            return;
        };
        if due > now {
            return;
        }
        self.palette_warm.due = None;
        if self.palette_warm.wanted == self.palette_warm.sent && self.palette_warm.slots_sent {
            return;
        }
        let mut surfaces = self.palette_warm.wanted.clone();
        if !self.palette_warm.slots_sent {
            surfaces.extend(SLOTS.map(|target| ActionSurface::Slot { target }));
            self.palette_warm.slots_sent = true;
        }
        self.background.extend(
            surfaces
                .into_iter()
                .map(|surface| UiAction::WarmPalette { surface }),
        );
        self.palette_warm.sent = self.palette_warm.wanted.clone();
    }

    /// A background request could not be queued. A palette prefetch is
    /// retried after the rest period instead of being treated as sent.
    pub(crate) fn background_not_sent(&mut self, action: &UiAction, now: Instant) {
        let UiAction::WarmPalette { surface } = action else {
            return;
        };
        if matches!(surface, ActionSurface::Slot { .. }) {
            self.palette_warm.slots_sent = false;
        } else {
            self.palette_warm.sent.retain(|sent| sent != surface);
        }
        self.palette_warm.due.get_or_insert(now + REST);
    }

    /// When the selection's prefetch is due, so the loop wakes for it.
    pub(super) fn palette_warm_deadline(&self) -> Option<Instant> {
        self.palette_warm.due
    }

    /// The surfaces the selection opens: the selected row's `!` palette and
    /// the `M` palette scoped to it. Surfaces the board cannot prefetch are
    /// left out.
    fn palette_surfaces(&self) -> Vec<ActionSurface> {
        let row = self.actionable_row().map(|row| row.key.clone());
        let manager = ActionSurface::Manager {
            key: (!self.history.active).then(|| row.clone()).flatten(),
        };
        let mut surfaces: Vec<ActionSurface> = row
            .into_iter()
            .map(|key| ActionSurface::Row { key })
            .collect();
        surfaces.push(manager);
        surfaces.retain(|surface| crate::menus::palette::prefetchable(&self.board, surface));
        surfaces
    }
}
