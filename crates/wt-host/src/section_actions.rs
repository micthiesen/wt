//! Controller-side section operations; UI code only handles prepared choices.
use anyhow::{Context, Result, bail};
use wt_core::{ChainMember, build_stack_index};
use wt_tui::{Seen, UiReply};

use wt_app_core::context::AppContext;
use wt_ops::commands::section;

/// The section a row is stored under (`None` = Inbox). Remote rows keep their
/// placement in the controller's `remoteLayouts`, as the board reads it.
fn stored_section(state: &serde_json::Value, key: &str) -> Option<String> {
    let collection = if wt_core::is_remote_worktree_ledger_key(key) {
        "remoteLayouts"
    } else {
        "slugs"
    };
    state[collection][key]["section"]
        .as_str()
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

/// The menu showed the row's section; a change since then (another client
/// or the agent moved it) must not be overwritten unseen.
fn check_section_witness(
    state: &serde_json::Value,
    key: &str,
    seen: Option<&Seen<Option<String>>>,
) -> Result<()> {
    if let Some(Seen(seen)) = seen {
        let current = stored_section(state, key);
        if current != *seen {
            return Err(wt_app_core::host_errors::stale(format!(
                "section is now {}",
                current.as_deref().unwrap_or("Inbox")
            )));
        }
    }
    Ok(())
}

pub async fn move_row(
    ctx: &AppContext,
    key: String,
    section: Option<String>,
    seen: Option<Seen<Option<String>>>,
    board: Option<&wt_tui::Board>,
) -> Result<UiReply> {
    let board = board.context("worktree metadata is still loading")?;
    let row = board
        .rows
        .iter()
        .find(|row| row.key == key)
        .context("selected worktree is no longer visible")?;
    let (state, archived) = ctx
        .database
        .call(|store| Ok((store.read_wt_state()?, store.read_archived_keys()?)))
        .await?;
    if archived.contains(&row.key) {
        bail!("Restore the archived row before moving it to a section");
    }
    // Fail before recording the write when the menu is already out of date;
    // the same check repeats inside the write's own database call.
    check_section_witness(&state, &row.key, seen.as_ref())?;
    let destination = section
        .map(|name| {
            if let Some(error) = section::invalid_name(&name) {
                bail!("{error}");
            }
            Ok(section::resolve_section(&state, &name).unwrap_or_else(|| name.trim().to_owned()))
        })
        .transpose()?;
    let members: Vec<_> = board
        .rows
        .iter()
        .filter(|row| !archived.contains(&row.key))
        .map(|row| {
            ChainMember::new(
                &row.key,
                wt_sources::board_layout::branch_key(row, &row.branch),
                row.base_branch
                    .as_deref()
                    .filter(|base| *base != ctx.config.branch.base)
                    .map(|base| wt_sources::board_layout::branch_key(row, base)),
            )
        })
        .collect();
    let stacks = build_stack_index(&members, &ctx.config.branch.base);
    let moving: Vec<_> = stacks
        .by_branch
        .get(&wt_sources::board_layout::branch_key(row, &row.branch))
        .map(|entry| {
            stacks.layouts[entry.layout_index]
                .nodes
                .iter()
                .map(|node| node.slug.clone())
                .collect()
        })
        .unwrap_or_else(|| vec![row.key.clone()]);
    let to = destination.clone();
    ctx.section_writes
        .record(moving.iter().cloned(), destination.as_deref());
    let witness_key = row.key.clone();
    let changed = ctx
        .database
        .call(move |store| {
            check_section_witness(&store.read_wt_state()?, &witness_key, seen.as_ref())?;
            Ok(store.move_worktrees_to_section(&moving, to.as_deref())?)
        })
        .await?;
    tracing::info!(slugs = ?changed, section = ?destination, "moved worktrees to section");
    Ok(UiReply {
        message: format!(
            "Moved {} to {}",
            row.slug,
            destination.as_deref().unwrap_or("Inbox")
        ),
        ..Default::default()
    })
}

/// Switch the repository to explicit sections once, keeping only those that
/// hold a live, unarchived worktree. Live means listed in Git's inventory now;
/// an inventory failure, or one listing no worktrees, adopts nothing and the
/// next start tries again.
pub async fn adopt_explicit_sections(ctx: &AppContext) -> Result<bool> {
    let live = ctx
        .repository
        .inventory(&ctx.cancellation)
        .await?
        .into_iter()
        .filter(|row| !row.is_main)
        .map(|row| row.target.slug().to_owned())
        .collect::<std::collections::BTreeSet<_>>();
    // An inventory with no worktrees is more likely a misread than a fleet
    // with nothing in any section; adopting from it would drop every section
    // for good. Leave the state alone and try again on the next start.
    if live.is_empty() {
        return Ok(false);
    }
    ctx.database
        .call(move |store| {
            let archived = store.read_archived_keys()?;
            let live = live
                .into_iter()
                .filter(|slug| !archived.contains(slug))
                .collect();
            Ok(store.adopt_explicit_sections(&live)?)
        })
        .await
}

/// Move the displayed unit. Stack membership is always inferred from branches;
/// only controller-owned order/section fields are persisted.
pub async fn reorder(
    ctx: &AppContext,
    key: Option<String>,
    section: String,
    down: bool,
    board: Option<&wt_tui::Board>,
) -> Result<UiReply> {
    let board = board.context("worktree metadata is still loading")?;
    if section == wt_sources::board_layout::ARCHIVED {
        bail!("Archived rows do not reorder; use a to restore");
    }
    let group_index = board
        .sections
        .iter()
        .position(|group| group.key == section)
        .context("section disappeared")?;
    if key.is_none() || section.starts_with("\0stack:") {
        let order = board
            .sections
            .iter()
            .filter(|group| group.key != wt_sources::board_layout::ARCHIVED && group.drawn())
            .map(|group| group.key.clone())
            .collect::<Vec<_>>();
        let current = order
            .iter()
            .position(|group| group == &section)
            .context("section is empty")?;
        let neighbor = if down {
            current.checked_add(1)
        } else {
            current.checked_sub(1)
        }
        .and_then(|index| order.get(index))
        .cloned()
        .ok_or_else(|| {
            wt_app_core::host_errors::notice(if down {
                "section already at bottom"
            } else {
                "section already at top"
            })
        })?;
        ctx.database
            .call(move |store| Ok(store.move_group_past(&section, &neighbor, !down, &order)?))
            .await?;
        return Ok(UiReply {
            message: format!("moved section {}", if down { "down" } else { "up" }),
            ..Default::default()
        });
    }
    let key = key.expect("checked row key");
    let members = board
        .rows
        .iter()
        .filter(|row| !row.archived)
        .map(|row| {
            ChainMember::new(
                &row.key,
                wt_sources::board_layout::branch_key(row, &row.branch),
                row.base_branch
                    .as_deref()
                    .filter(|base| *base != ctx.config.branch.base)
                    .map(|base| wt_sources::board_layout::branch_key(row, base)),
            )
        })
        .collect::<Vec<_>>();
    let stacks = build_stack_index(&members, &ctx.config.branch.base);
    let section_rows = board.sections[group_index]
        .rows
        .iter()
        .filter_map(|index| board.rows.get(*index))
        .collect::<Vec<_>>();
    // A stack moves as one block under its root, but only within the
    // root's section: a split member shown here under another section's
    // root moves on its own, so a swap never drags the root across.
    let unit_key = |row: &wt_tui::BoardRow| {
        stacks
            .by_branch
            .get(&wt_sources::board_layout::branch_key(row, &row.branch))
            .and_then(|entry| stacks.layouts[entry.layout_index].nodes.first())
            .map(|root| root.slug.clone())
            .filter(|root| section_rows.iter().any(|row| &row.key == root))
            .unwrap_or_else(|| row.key.clone())
    };
    let row = board
        .rows
        .iter()
        .find(|row| row.key == key)
        .context("selected worktree disappeared")?;
    let mover = unit_key(row);
    let mut units = Vec::new();
    for row in &section_rows {
        let key = unit_key(row);
        if !units.contains(&key) {
            units.push(key);
        }
    }
    let index = units
        .iter()
        .position(|unit| unit == &mover)
        .context("selected row left this section")?;
    let neighbor = if down {
        index.checked_add(1)
    } else {
        index.checked_sub(1)
    }
    .and_then(|index| units.get(index))
    .cloned();
    let state = ctx
        .database
        .call(|store| Ok(store.read_wt_state()?))
        .await?;
    if let Some(neighbor) = neighbor {
        let rank = |unit: &str| {
            section_rows
                .iter()
                .filter(|row| unit_key(row) == unit)
                .map(|row| row.work_rank)
                .min()
                .unwrap_or(99)
        };
        if ctx.config.ui.sort == wt_config::UiSort::Status && rank(&mover) != rank(&neighbor) {
            return Err(wt_app_core::host_errors::notice(
                "status sort pins this row; reorder within the same status or use manual sort",
            ));
        }
        let manual_order = |key: &str| {
            let collection = if wt_core::is_remote_worktree_ledger_key(key) {
                "remoteLayouts"
            } else {
                "slugs"
            };
            state[collection][key]["order"]
                .as_f64()
                .filter(|value| value.is_finite())
                .unwrap_or(f64::NEG_INFINITY)
        };
        units.sort_by(|a, b| {
            manual_order(a)
                .total_cmp(&manual_order(b))
                .then_with(|| a.cmp(b))
        });
        let section = (!section.starts_with('\0')).then_some(section);
        ctx.database
            .call(move |store| {
                Ok(store.swap_orders(&mover, &neighbor, section.as_deref(), &units)?)
            })
            .await?;
    } else {
        let destination = if down {
            board
                .sections
                .iter()
                .skip(group_index + 1)
                .collect::<Vec<_>>()
        } else {
            board.sections[..group_index]
                .iter()
                .rev()
                .collect::<Vec<_>>()
        }
        .into_iter()
        // A row only crosses into a section on screen: a named section even
        // while empty, a derived group (the Inbox) only with rows.
        .find(|group| {
            group.key != wt_sources::board_layout::ARCHIVED
                && !group.key.starts_with("\0stack:")
                && group.drawn()
        })
        .ok_or_else(|| {
            wt_app_core::host_errors::notice(if down {
                "already at bottom"
            } else {
                "already at top"
            })
        })?;
        let section =
            (destination.key != wt_sources::board_layout::INBOX).then(|| destination.key.clone());
        let message = format!("moved to {}", section.as_deref().unwrap_or("Inbox"));
        let moving = section_rows
            .iter()
            .filter(|row| unit_key(row) == mover)
            .map(|row| row.key.clone())
            .collect::<Vec<_>>();
        ctx.section_writes
            .record(moving.iter().cloned(), section.as_deref());
        ctx.database
            .call(move |store| {
                store.move_worktrees_to_section(&moving, section.as_deref())?;
                store.place_slug(&mover, section.as_deref(), down)?;
                Ok(())
            })
            .await?;
        return Ok(UiReply {
            message,
            ..Default::default()
        });
    }
    // A swap within the section shows itself; no toast, as in TS.
    Ok(UiReply::default())
}

/// Delete a named section; its rows (live, archived and remote) return to
/// the Inbox. `seen` is the sorted keys of the live rows the user saw in it,
/// rechecked against stored placement inside the write.
pub async fn delete(
    ctx: &AppContext,
    section: String,
    seen: Seen<Vec<String>>,
    board: Option<&wt_tui::Board>,
) -> Result<UiReply> {
    if section.starts_with('\0') {
        bail!("This group is named by wt and cannot be deleted");
    }
    let board = board.context("worktree metadata is still loading")?;
    let stale = |name: &str| {
        wt_app_core::host_errors::stale(format!(
            "{name} changed since you asked; look again before deleting it"
        ))
    };
    if board.section_witness(&section) != seen.0 {
        return Err(stale(&section));
    }
    // Under the write, the rows the user saw must still be filed there, and
    // no other row the board shows as movable may have been filed since.
    let movable = board
        .rows
        .iter()
        .filter(|row| !row.archived && row.removal.is_none() && !row.pending_creation)
        .map(|row| row.key.clone())
        .collect::<Vec<_>>();
    let name = section.clone();
    let moved = ctx
        .database
        .call(move |store| {
            let state = store.read_wt_state()?;
            let mut current = movable
                .into_iter()
                .filter(|key| stored_section(&state, key).as_deref() == Some(name.as_str()))
                .collect::<Vec<_>>();
            current.sort();
            if current != seen.0 {
                return Err(stale(&name));
            }
            Ok(store.remove_section(&name)?)
        })
        .await?;
    tracing::info!(%section, moved, "deleted section");
    Ok(UiReply {
        message: format!("Deleted section {section}"),
        ..Default::default()
    })
}

pub async fn rename(ctx: &AppContext, old: String, new: String) -> Result<UiReply> {
    if old.starts_with('\0') {
        bail!("This group is named by wt and cannot be renamed");
    }
    if let Some(error) = section::invalid_name(&new) {
        bail!("{error}");
    }
    let state = ctx
        .database
        .call(|store| Ok(store.read_wt_state()?))
        .await?;
    let from = section::resolve_section(&state, &old).context("Section no longer exists")?;
    let new = new.trim().to_owned();
    let to = section::resolve_section(&state, &new).unwrap_or(new);
    let target = to.clone();
    let members = ["slugs", "remoteLayouts"]
        .into_iter()
        .filter_map(|collection| state[collection].as_object())
        .flatten()
        .filter(|(_, entry)| entry["section"].as_str() == Some(from.as_str()))
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();
    ctx.section_writes.record(members, Some(&target));
    ctx.database
        .call(move |store| Ok(store.rename_section(&from, &target)?))
        .await?;
    Ok(UiReply {
        message: format!("Section renamed to {to}"),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn move_refuses_when_the_section_the_menu_showed_changed() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let facts = fixture
            .ctx
            .repository
            .inventory_status(&fixture.ctx.cancellation)
            .await
            .unwrap();
        let state = fixture
            .ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        let board =
            wt_sources::inventory::board(&fixture.ctx.config, &facts, &state, &Default::default());
        // The menu showed the Inbox; another writer files the row under Release.
        fixture
            .ctx
            .database
            .call(|store| {
                store.move_worktrees_to_section(&["one".to_owned()], Some("Release"))?;
                Ok(())
            })
            .await
            .unwrap();
        let error = move_row(
            &fixture.ctx,
            "one".into(),
            Some("Next".into()),
            Some(Seen(None)),
            Some(&board),
        )
        .await
        .unwrap_err();
        assert!(
            error
                .chain()
                .any(|cause| cause.is::<wt_app_core::host_errors::Stale>())
        );
        let state = fixture
            .ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        assert_eq!(state["slugs"]["one"]["section"], "Release");
        // The same move from the menu that saw Release goes through.
        move_row(
            &fixture.ctx,
            "one".into(),
            Some("Next".into()),
            Some(Seen(Some("Release".into()))),
            Some(&board),
        )
        .await
        .unwrap();
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn adoption_keeps_sections_of_listed_unarchived_worktrees() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        fixture
            .ctx
            .database
            .call(|store| {
                store.place_slug("one", Some("Live"), false)?;
                store.place_slug("two", Some("Shelved"), false)?;
                store.place_slug("gone", Some("Stale"), false)?;
                store.set_archived("two", true)?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(adopt_explicit_sections(&fixture.ctx).await.unwrap());
        assert!(!adopt_explicit_sections(&fixture.ctx).await.unwrap());
        let state = fixture
            .ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        assert_eq!(
            state["sectionsOrder"],
            serde_json::json!([wt_sources::board_layout::INBOX, "Live"])
        );
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn delete_refuses_a_section_that_changed_and_moves_rows_to_the_inbox() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        fixture
            .ctx
            .database
            .call(|store| Ok(store.place_slug("one", Some("Batch"), false)?))
            .await
            .unwrap();
        let facts = fixture
            .ctx
            .repository
            .inventory_status(&fixture.ctx.cancellation)
            .await
            .unwrap();
        let state = fixture
            .ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        let board =
            wt_sources::inventory::board(&fixture.ctx.config, &facts, &state, &Default::default());
        // The user saw Batch empty, but `one` is in it now.
        let stale = delete(&fixture.ctx, "Batch".into(), Seen(vec![]), Some(&board)).await;
        assert!(stale.is_err());
        delete(
            &fixture.ctx,
            "Batch".into(),
            Seen(vec!["one".into()]),
            Some(&board),
        )
        .await
        .unwrap();
        let state = fixture
            .ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        assert!(state["slugs"]["one"]["section"].is_null());
        assert!(
            !state["sectionsOrder"]
                .as_array()
                .unwrap()
                .iter()
                .any(|name| name == "Batch")
        );
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn reorder_swaps_only_same_rank_units_and_crosses_into_visible_sections() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let facts = fixture
            .ctx
            .repository
            .inventory_status(&fixture.ctx.cancellation)
            .await
            .unwrap();
        let state = fixture
            .ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        let mut board =
            wt_sources::inventory::board(&fixture.ctx.config, &facts, &state, &Default::default());
        reorder(
            &fixture.ctx,
            Some("one".into()),
            wt_sources::board_layout::INBOX.into(),
            true,
            Some(&board),
        )
        .await
        .unwrap();
        let state = fixture
            .ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        wt_sources::board_layout::prepare(
            &mut board,
            &state,
            &fixture.ctx.config.branch.base,
            fixture.ctx.config.ui.sort,
        );
        let inbox = board
            .sections
            .iter()
            .find(|section| section.key == wt_sources::board_layout::INBOX)
            .unwrap();
        assert_eq!(board.rows[inbox.rows[0]].key, "two");
        let first = inbox.rows[0];
        board.rows[first].work_rank = 0;
        let mut ctx = fixture.ctx.clone();
        std::sync::Arc::make_mut(&mut ctx.config).ui.sort = wt_config::UiSort::Status;
        assert!(
            reorder(
                &ctx,
                Some("two".into()),
                wt_sources::board_layout::INBOX.into(),
                true,
                Some(&board)
            )
            .await
            .is_err()
        );
        // An empty derived group is not drawn, so moving down past the last
        // row skips it; an empty named section is on screen and takes the row.
        board.sections.push(wt_tui::BoardSection {
            key: wt_sources::board_layout::REMOVING.into(),
            title: "Removing".into(),
            ..Default::default()
        });
        assert!(
            reorder(
                &fixture.ctx,
                Some("one".into()),
                wt_sources::board_layout::INBOX.into(),
                true,
                Some(&board),
            )
            .await
            .is_err()
        );
        board.sections.push(wt_tui::BoardSection {
            key: "Next".into(),
            title: "Next".into(),
            ..Default::default()
        });
        reorder(
            &fixture.ctx,
            Some("one".into()),
            wt_sources::board_layout::INBOX.into(),
            true,
            Some(&board),
        )
        .await
        .unwrap();
        let state = fixture
            .ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        assert_eq!(state["slugs"]["one"]["section"], "Next");
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn moving_files_the_stack_and_rename_preserves_unrelated_state() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        fixture
            .ctx
            .database
            .call(|store| {
                store.set_slug_base("two", Some(("feature/one", None)))?;
                store.set_slug_manual_title("two", "Pinned", None)?;
                Ok(())
            })
            .await
            .unwrap();
        let rows = fixture
            .ctx
            .repository
            .inventory(&fixture.ctx.cancellation)
            .await
            .unwrap();
        let row = rows.iter().find(|row| row.target.slug() == "one").unwrap();
        let key = wt_core::worktree_target_key(&row.target);
        let facts = fixture
            .ctx
            .repository
            .inventory_status(&fixture.ctx.cancellation)
            .await
            .unwrap();
        let state = fixture
            .ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        let board =
            wt_sources::inventory::board(&fixture.ctx.config, &facts, &state, &Default::default());
        move_row(
            &fixture.ctx,
            key.clone(),
            Some("Release".into()),
            None,
            Some(&board),
        )
        .await
        .unwrap();
        rename(&fixture.ctx, "Release".into(), "Today".into())
            .await
            .unwrap();
        let state = fixture
            .ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        assert_eq!(state["slugs"]["one"]["section"], "Today");
        assert_eq!(state["slugs"]["two"]["section"], "Today");
        assert_eq!(state["slugs"]["two"]["manualTitle"], "Pinned");
        fixture
            .ctx
            .database
            .call(|store| {
                store.set_archived("one", true)?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(
            move_row(&fixture.ctx, key, None, None, Some(&board))
                .await
                .is_err()
        );
        assert!(
            rename(&fixture.ctx, "\0inbox".into(), "Renamed".into())
                .await
                .is_err()
        );
        fixture.close().await.unwrap();
    }
}
