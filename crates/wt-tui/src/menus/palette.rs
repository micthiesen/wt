//! `!`, `M`, and the slot palettes: the action picker the host prefetches
//! onto the board. A prefetched copy opens at once and refreshes while open;
//! without one, the host builds the list as before.

use super::{LocalMenu, Menu};
use crate::{ActionSurface, Board, UiAction};

pub(super) fn menu(board: &Board, surface: &ActionSurface) -> Menu {
    if !prefetchable(board, surface) {
        return host_menu();
    }
    let Some(prepared) = board.palettes.get(&surface.menu_key()) else {
        return host_menu();
    };
    Menu::Local(LocalMenu::Modal {
        modal: Box::new(prepared.modal.clone()),
        warm: Some(Box::new(UiAction::WarmPalette {
            surface: surface.clone(),
        })),
        host: None,
    })
}

/// Whether the host that owns this surface can publish its palette onto
/// the local board. Remote rows' palettes are built by their own host and do
/// not flow back, so their surfaces always ask the host.
pub(crate) fn prefetchable(board: &Board, surface: &ActionSurface) -> bool {
    let row_key = match surface {
        ActionSurface::Row { key } => Some(key),
        ActionSurface::Manager { key } => key.as_ref(),
        ActionSurface::Slot { .. } => return true,
    };
    match row_key {
        // A manager palette without a row is the fleet-level list.
        None => matches!(surface, ActionSurface::Manager { .. }),
        Some(key) => board
            .rows
            .iter()
            .find(|row| &row.key == key)
            .is_some_and(|row| row.host.is_none()),
    }
}

fn host_menu() -> Menu {
    Menu::Host {
        reason: "reads action definitions, running actions and checkout state",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BoardRow, PreparedPalette, UiModal};

    fn picker() -> UiModal {
        UiModal::Picker {
            action: crate::PickerAction::Actions {
                surface: ActionSurface::Manager { key: None },
            },
            title: "Manager commands".into(),
            options: Vec::new(),
            selected: 0,
        }
    }

    fn board_with_row(host: Option<&str>) -> Board {
        Board {
            rows: vec![BoardRow {
                key: "one".into(),
                slug: "one".into(),
                host: host.map(str::to_owned),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn empty_board_asks_the_host_for_every_surface() {
        let board = Board::default();
        for surface in [
            ActionSurface::Row { key: "one".into() },
            ActionSurface::Manager { key: None },
            ActionSurface::Slot {
                target: crate::SessionTarget::Main,
            },
        ] {
            assert!(matches!(menu(&board, &surface), Menu::Host { .. }));
        }
    }

    #[test]
    fn prefetched_palette_opens_locally_with_a_warm_request() {
        let mut board = board_with_row(None);
        let surface = ActionSurface::Row { key: "one".into() };
        board
            .palettes
            .insert(surface.menu_key(), PreparedPalette { modal: picker() });
        assert_eq!(
            menu(&board, &surface),
            Menu::Local(LocalMenu::Modal {
                modal: Box::new(picker()),
                warm: Some(Box::new(UiAction::WarmPalette { surface })),
                host: None,
            })
        );
    }

    #[test]
    fn missing_prefetch_falls_back_to_the_host() {
        let board = board_with_row(None);
        let surface = ActionSurface::Row { key: "one".into() };
        assert!(matches!(menu(&board, &surface), Menu::Host { .. }));
        // A palette for another surface does not answer this one.
        let mut other = board.clone();
        other.palettes.insert(
            ActionSurface::Row { key: "two".into() }.menu_key(),
            PreparedPalette { modal: picker() },
        );
        assert!(matches!(menu(&other, &surface), Menu::Host { .. }));
    }

    #[test]
    fn remote_rows_and_vanished_rows_always_ask_the_host() {
        let mut board = board_with_row(Some("studio"));
        let surface = ActionSurface::Row { key: "one".into() };
        board
            .palettes
            .insert(surface.menu_key(), PreparedPalette { modal: picker() });
        assert!(matches!(menu(&board, &surface), Menu::Host { .. }));
        let gone = ActionSurface::Row { key: "gone".into() };
        board
            .palettes
            .insert(gone.menu_key(), PreparedPalette { modal: picker() });
        assert!(matches!(menu(&board, &gone), Menu::Host { .. }));
    }

    #[test]
    fn slot_palettes_prefetch_without_a_row() {
        let mut board = Board::default();
        let surface = ActionSurface::Slot {
            target: crate::SessionTarget::Dotfiles,
        };
        board
            .palettes
            .insert(surface.menu_key(), PreparedPalette { modal: picker() });
        assert!(matches!(
            menu(&board, &surface),
            Menu::Local(LocalMenu::Modal { .. })
        ));
    }
}
