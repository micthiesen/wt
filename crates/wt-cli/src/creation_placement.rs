//! Controller-owned layout for a creation on any host.
use anyhow::{Context, Result};
use wt_tui::{Board, CreatePlacement, CreationPlacementAccepted};

use crate::context::AppContext;

pub(crate) async fn accept(
    context: &AppContext,
    board: Option<&Board>,
    key: String,
    placement: CreatePlacement,
) -> Result<CreationPlacementAccepted> {
    let board = board.context("section layout is still loading")?;
    let section_key = placement
        .section
        .as_deref()
        .unwrap_or(crate::board_layout::INBOX);
    let section = board
        .sections
        .iter()
        .find(|section| section.key == section_key)
        .ok_or_else(|| crate::host_errors::stale("the creation section disappeared"))?;
    if let Some(anchor) = &placement.after {
        let shown = section
            .rows
            .iter()
            .filter_map(|index| board.rows.get(*index))
            .any(|row| row.key == *anchor && !row.archived && row.removal.is_none());
        if !shown {
            return Err(crate::host_errors::stale(
                "the creation row left its section",
            ));
        }
    }
    let stored_key = key.clone();
    let stored_placement = placement.clone();
    let changed = context
        .database
        .call(move |store| {
            Ok(store.place_new_worktree(
                &stored_key,
                stored_placement.section.as_deref(),
                stored_placement.after.as_deref(),
            )?)
        })
        .await?;
    if !changed {
        return Err(crate::host_errors::stale(
            "the creation section changed; reopen the create prompt",
        ));
    }
    context
        .section_writes
        .record([key.clone()], placement.section.as_deref());
    wt_runtime::hold_for_settlement(std::time::Duration::from_secs(30));
    Ok(CreationPlacementAccepted {
        key,
        placement,
        accepted_at: wt_runtime::Stamp::now(),
    })
}
