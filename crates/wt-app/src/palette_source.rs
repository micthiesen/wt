//! Action palettes the host prepared while the selection rested, keyed by
//! `ActionSurface::menu_key`. Live process state (running actions, checkout
//! and GitHub confirmation at the time of the warm-up), so it is never
//! persisted; a missing entry only means the palette asks the host.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use wt_core::Domain;
use wt_runtime::{
    LeafHandle, LeafPublisher, Persistence, SourceHandle, SourceSpec, Stamp, TaskScope,
    derived_channel, leaf_channel,
};
use wt_tui::{Board, PreparedPalette, UiModal};

/// The palettes currently prepared, by surface key.
pub type PaletteMap = BTreeMap<String, UiModal>;

#[derive(Clone)]
pub struct PaletteCommands {
    state: Arc<Mutex<PaletteState>>,
}

struct PaletteState {
    palettes: PaletteMap,
    publisher: LeafPublisher<PaletteMap>,
}

impl PaletteCommands {
    /// Store the palette for `key`, or drop it when the host could not
    /// prepare one (`None`). The surface then asks the host again.
    pub fn publish(&self, key: String, modal: Option<UiModal>) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        match modal {
            Some(modal) => {
                state.palettes.insert(key, modal);
            }
            None => {
                state.palettes.remove(&key);
            }
        }
        let palettes = state.palettes.clone();
        state.publisher.publish_live(palettes, Stamp::now());
    }
}

pub struct PaletteSource {
    pub commands: PaletteCommands,
    pub snapshot: LeafHandle<PaletteMap>,
}

pub fn start(scope: &TaskScope) -> PaletteSource {
    let (snapshot, publisher) = leaf_channel(
        scope,
        SourceSpec {
            name: "palettes",
            domain: Domain::Actions,
            persistence: Persistence::Never("live host state; warmed again on selection"),
            policy: Default::default(),
        },
    );
    PaletteSource {
        commands: PaletteCommands {
            state: Arc::new(Mutex::new(PaletteState {
                palettes: PaletteMap::new(),
                publisher,
            })),
        },
        snapshot,
    }
}

/// Attach the prepared palettes to each board the presentation receives.
pub fn overlay(
    scope: &TaskScope,
    board: SourceHandle<Board>,
    palettes: SourceHandle<PaletteMap>,
) -> SourceHandle<Board> {
    let (output, mut publisher) = derived_channel(scope, "palette-board");
    let cancellation = scope.token();
    scope.spawn(async move {
        let mut boards = board.subscribe();
        let mut prepared = palettes.subscribe();
        boards.mark_changed();
        prepared.mark_changed();
        loop {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => break,
                request = publisher.requested() => {
                    if request.is_none() { break; }
                    board.refresh();
                    continue;
                }
                changed = boards.changed() => if changed.is_err() { break; },
                changed = prepared.changed() => if changed.is_err() { break; },
            }
            let base = boards.borrow_and_update().clone();
            let snapshot = prepared.borrow_and_update().clone();
            let data = base.data().map(|board| {
                let mut board = board.as_ref().clone();
                if let Some(palettes) = snapshot.data() {
                    board.palettes = palettes
                        .iter()
                        .map(|(key, modal)| {
                            (
                                key.clone(),
                                PreparedPalette {
                                    modal: modal.clone(),
                                },
                            )
                        })
                        .collect();
                }
                Arc::new(board)
            });
            publisher.publish(base.with_data(data).with_input(&snapshot));
        }
    });
    output
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use wt_runtime::SourceHandle;

    use super::*;

    fn picker() -> UiModal {
        UiModal::Picker {
            action: wt_tui::PickerAction::Actions {
                surface: wt_tui::ActionSurface::Manager { key: None },
            },
            title: "Manager commands".into(),
            options: Vec::new(),
            selected: 0,
        }
    }

    async fn wait_for(board: &SourceHandle<Board>, ready: impl Fn(&Board) -> bool) {
        let mut updates = board.subscribe();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let found = updates
                    .borrow_and_update()
                    .data()
                    .is_some_and(|board| ready(board));
                if found {
                    return;
                }
                updates.changed().await.unwrap();
            }
        })
        .await
        .expect("the board shows the palette change");
    }

    #[tokio::test]
    async fn published_palettes_reach_the_board_until_withdrawn() {
        let scope = TaskScope::new();
        let (base, base_publisher) = wt_runtime::leaf_channel(
            &scope,
            wt_runtime::SourceSpec {
                name: "test-board",
                domain: Domain::Git,
                persistence: Persistence::Never("test fixture"),
                policy: Default::default(),
            },
        );
        base_publisher.publish_live(Board::default(), Stamp::now());
        let source = start(&scope);
        let board = overlay(&scope, base.view(), source.snapshot.view());
        let key = wt_tui::ActionSurface::Manager { key: None }.menu_key();

        source.commands.publish(key.clone(), Some(picker()));
        let expected = key.clone();
        wait_for(&board, move |board| {
            board
                .palettes
                .get(&expected)
                .map(|prepared| &prepared.modal)
                == Some(&picker())
        })
        .await;

        // A failed or refused warm-up withdraws the copy rather than keeping it.
        source.commands.publish(key.clone(), None);
        wait_for(&board, |board| board.palettes.is_empty()).await;
        // The base board is untouched by palette changes.
        assert!(
            board
                .snapshot()
                .data()
                .is_some_and(|board| board.rows.is_empty())
        );
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
    }
}
