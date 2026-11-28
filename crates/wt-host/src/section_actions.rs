//! Controller-side section operations; UI code only handles prepared choices.
use anyhow::{Context, Result, bail};
use wt_core::{ChainMember, StackIndex, build_display_stack_index};
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

/// The stacks as the board draws them: a row whose own work has landed is a
/// unit of its own, so it moves and reorders alone and what was stacked on
/// it stays where it is. Fork-base records and stack ids are not involved.
fn shown_stacks(
    board: &wt_tui::Board,
    trunk: &str,
    listed: impl Fn(&wt_tui::BoardRow) -> bool,
) -> StackIndex {
    let members: Vec<_> = board
        .rows
        .iter()
        .filter(|row| listed(row))
        .map(|row| {
            ChainMember::new(
                &row.key,
                wt_sources::board_layout::branch_key(row, &row.branch),
                row.base_branch
                    .as_deref()
                    .filter(|base| *base != trunk)
                    .map(|base| wt_sources::board_layout::branch_key(row, base)),
            )
        })
        .collect();
    let landed: std::collections::BTreeSet<&str> = board
        .rows
        .iter()
        .filter(|row| row.git.landed_on.is_some())
        .map(|row| row.key.as_str())
        .collect();
    build_display_stack_index(&members, trunk, |member| {
        landed.contains(member.slug.as_str())
    })
}

/// The derived block the board draws a row in, if any. Nothing is filed in
/// one, so a row there has no stored placement a move could change.
fn owed_block_of<'a>(board: &'a wt_tui::Board, key: &str) -> Option<&'a wt_tui::BoardSection> {
    board.sections.iter().find(|section| {
        section.owed_block()
            && section
                .rows
                .iter()
                .any(|&index| board.rows.get(index).is_some_and(|row| row.key == key))
    })
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
    if let Some(block) = owed_block_of(board, &key) {
        return Err(wt_app_core::host_errors::notice(
            wt_tui::owed_block_refusal(&block.title),
        ));
    }
    let (state, archived) = ctx
        .database
        .call(|store| Ok((store.read_wt_state()?, store.read_archived_keys()?)))
        .await?;
    if archived.contains(&row.key) {
        bail!("Restore the archived row before moving it to a section");
    }
    // Fail before recording the write when the menu is already out of date;
    // the same check repeats inside the write's own transaction.
    check_section_witness(&state, &row.key, seen.as_ref())?;
    let destination = section
        .map(|name| {
            if let Some(error) = section::invalid_name(&name) {
                bail!("{error}");
            }
            Ok(section::resolve_section(&state, &name).unwrap_or_else(|| name.trim().to_owned()))
        })
        .transpose()?;
    let stacks = shown_stacks(board, &ctx.config.branch.base, |row| {
        !archived.contains(&row.key)
    });
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
            store.move_worktrees_to_section_checked(&moving, to.as_deref(), |state| {
                check_section_witness(state, &witness_key, seen.as_ref())
            })?
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

/// The stored name of a section rows are filed in (`None` is the Inbox),
/// or the refusal for a group wt derives: nothing in one has an order or a
/// placement a press could change.
fn filed_section(board: &wt_tui::Board, key: &str) -> Result<Option<String>> {
    let title = board
        .sections
        .iter()
        .find(|section| section.key == key)
        .map_or(wt_tui::LANDED_TITLE, |section| section.title.as_str());
    match wt_tui::reorder_refusal(key, title) {
        // A notice, as for the other keys the Landed block answers.
        Some(refusal) if key.starts_with(wt_tui::OWED_BLOCK_PREFIX) => {
            Err(wt_app_core::host_errors::notice(refusal))
        }
        Some(refusal) => bail!(refusal),
        None => Ok((key != wt_sources::board_layout::INBOX).then(|| key.to_owned())),
    }
}

/// A press moves one place: `past` is the entry right behind `moving`
/// going down, right before it going up, in a list that names each once.
fn check_neighbors(shown: &[String], moving: &str, past: &str, down: bool) -> Result<()> {
    let at = |key: &str| shown.iter().position(|entry| entry == key);
    let unique = shown
        .iter()
        .collect::<std::collections::BTreeSet<_>>()
        .len()
        == shown.len();
    let adjacent = match (at(moving), at(past)) {
        (Some(moving), Some(past)) if down => past == moving + 1,
        (Some(moving), Some(past)) => moving == past + 1,
        _ => false,
    };
    if !unique || !adjacent {
        bail!("the move does not match the list it was resolved against");
    }
    Ok(())
}

/// Every unit a press names must be a row this host lists; one that is gone
/// was shown by an older board.
fn check_listed(board: &wt_tui::Board, keys: &[String]) -> Result<()> {
    match keys
        .iter()
        .find(|key| !board.rows.iter().any(|row| row.key == **key))
    {
        Some(gone) => Err(wt_app_core::host_errors::stale(format!(
            "{gone} is no longer listed"
        ))),
        None => Ok(()),
    }
}

fn layout_changed(board: &wt_tui::Board, changed: wt_store::LayoutChanged) -> anyhow::Error {
    let name = |key: &str| {
        if key == wt_sources::board_layout::INBOX {
            return "Inbox".to_owned();
        }
        board
            .rows
            .iter()
            .find(|row| row.key == key)
            .map_or(key, |row| row.slug.as_str())
            .to_owned()
    };
    wt_app_core::host_errors::stale(match changed {
        wt_store::LayoutChanged::Filed(key) => format!("{} is filed elsewhere now", name(&key)),
        wt_store::LayoutChanged::Order(first, second) => {
            format!("{} and {} have changed places", name(&first), name(&second))
        }
        wt_store::LayoutChanged::Section(section) => format!("section {section} is gone"),
        wt_store::LayoutChanged::Ordered(key) => format!("{} has been reordered", name(&key)),
    })
}

/// Store one J or K press. `step` is the move as the person saw it: the
/// units it names are the ones written, and the layout it lists is compared
/// with the stored one inside the write. This host's board is not consulted
/// for either, because it may not show a press this host has already
/// stored; a second press resolved from it would undo the first. The board
/// supplies only what no press changes: which rows exist, how they are
/// stacked, and their status.
/// Stack membership is always inferred from branches; only controller-owned
/// order and section fields are persisted.
pub async fn reorder(
    ctx: &AppContext,
    key: Option<String>,
    section: String,
    down: bool,
    step: wt_tui::ReorderStep,
    board: Option<&wt_tui::Board>,
) -> Result<UiReply> {
    let board = board.context("worktree metadata is still loading")?;
    let from = filed_section(board, &section)?;
    let status_sort = ctx.config.ui.sort == wt_config::UiSort::Status;
    match step {
        wt_tui::ReorderStep::Section { past, sections } => {
            if key.is_some() {
                bail!("a section move names no row");
            }
            filed_section(board, &past)?;
            check_neighbors(&sections, &section, &past, down)?;
            let moved = ctx
                .database
                .call(move |store| Ok(store.move_group_past(&section, &past, !down, &sections)?))
                .await?
                .map_err(|changed| layout_changed(board, changed))?;
            if !moved {
                return Err(wt_app_core::host_errors::notice("section did not move"));
            }
            Ok(UiReply {
                message: format!("moved section {}", if down { "down" } else { "up" }),
                ..Default::default()
            })
        }
        wt_tui::ReorderStep::Pass { unit, past, units } => {
            check_neighbors(&units, &unit, &past, down)?;
            check_listed(board, &units)?;
            if status_sort && wt_tui::unit_rank(board, &unit) != wt_tui::unit_rank(board, &past) {
                return Err(wt_app_core::host_errors::notice(wt_tui::STATUS_SORT_PINS));
            }
            let drawn = shown_units(board, status_sort, &units);
            ctx.database
                .call(move |store| {
                    let shown = wt_store::ShownSection {
                        name: from.as_deref(),
                        drawn: &drawn,
                    };
                    Ok(store.swap_shown_units(shown, &unit, &past)?)
                })
                .await?
                .map_err(|changed| layout_changed(board, changed))?;
            // A swap within the section shows itself; no toast, as in TS.
            Ok(UiReply::default())
        }
        wt_tui::ReorderStep::Cross {
            unit,
            rows,
            units,
            to,
            to_units,
        } => {
            let destination = filed_section(board, &to)?;
            if destination == from || !rows.contains(&unit) || !units.contains(&unit) {
                bail!("the move does not match the list it was resolved against");
            }
            check_listed(board, &units)?;
            check_listed(board, &to_units)?;
            check_unit_rows(board, &unit, &rows)?;
            let drawn = shown_units(board, status_sort, &units);
            let to_drawn = shown_units(board, status_sort, &to_units);
            let message = format!("moved to {}", destination.as_deref().unwrap_or("Inbox"));
            // Recorded before the write, so a read racing its commit already
            // knows the move as this process's own.
            ctx.section_writes
                .record(rows.iter().cloned(), destination.as_deref());
            let (recorded, target) = (rows.clone(), destination.clone());
            let moved = ctx
                .database
                .call(move |store| {
                    let leaving = wt_store::ShownSection {
                        name: from.as_deref(),
                        drawn: &drawn,
                    };
                    let entering = wt_store::ShownSection {
                        name: destination.as_deref(),
                        drawn: &to_drawn,
                    };
                    Ok(store.move_shown_unit(&rows, &unit, leaving, entering, down)?)
                })
                .await?;
            let moved = match moved {
                Ok(moved) => moved,
                Err(changed) => {
                    // Nothing was written: a later move of these rows to
                    // that section by someone else is news again.
                    for row in &recorded {
                        ctx.section_writes.consume(row, target.as_deref());
                    }
                    return Err(layout_changed(board, changed));
                }
            };
            tracing::info!(slugs = ?moved, section = %to, "moved a unit across a section edge");
            Ok(UiReply {
                message,
                ..Default::default()
            })
        }
    }
}

/// The units a press lists, with what decides where the board draws each
/// besides its stored order. Status sort draws by status first, and leaves
/// a unit that has neither a status nor an order behind the unit it was
/// created after. Manual sort draws by stored order alone, and a press
/// there gives every unit of the section one.
fn shown_units(
    board: &wt_tui::Board,
    status_sort: bool,
    units: &[String],
) -> Vec<wt_store::ShownUnit> {
    units
        .iter()
        .map(|key| wt_store::ShownUnit {
            key: key.clone(),
            band: if status_sort {
                wt_tui::unit_rank(board, key).unwrap_or(u8::MAX)
            } else {
                0
            },
            anchored: status_sort && wt_tui::unit_at_creation_position(board, key),
        })
        .collect()
}

/// The rows a press moves together must be one stack as this host knows it:
/// each is the unit's own row or stacked, however deep, on it.
fn check_unit_rows(board: &wt_tui::Board, unit: &str, rows: &[String]) -> Result<()> {
    let parent = |key: &str| {
        board
            .rows
            .iter()
            .find(|row| row.key == key)
            .and_then(|row| row.stack_parent.as_deref())
    };
    for row in rows {
        let mut at = row.as_str();
        // Bounded by the rows listed, so a malformed cycle of parents ends.
        let mut steps = 0;
        while at != unit {
            match parent(at) {
                Some(above) if steps < board.rows.len() => at = above,
                _ => {
                    return Err(wt_app_core::host_errors::stale(format!(
                        "{row} is no longer stacked on {unit}"
                    )));
                }
            }
            steps += 1;
        }
    }
    Ok(())
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
    let writes = ctx.section_writes.clone();
    let moved = ctx
        .database
        .call(move |store| {
            store.remove_section_checked(&name, |state| {
                let mut current = movable
                    .into_iter()
                    .filter(|key| stored_section(state, key).as_deref() == Some(name.as_str()))
                    .collect::<Vec<_>>();
                current.sort();
                if current != seen.0 {
                    return Err(stale(&name));
                }
                // Every row filed here (archived and remote too) lands in the
                // Inbox; none of those moves is news to the user who deleted it.
                let members = ["slugs", "remoteLayouts"]
                    .into_iter()
                    .filter_map(|collection| state[collection].as_object())
                    .flatten()
                    .filter(|(_, entry)| entry["section"].as_str() == Some(name.as_str()))
                    .map(|(key, _)| key.clone());
                writes.record(members, None);
                Ok(())
            })?
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

    /// The fixture's board with `two` stacked on `one`.
    async fn stacked_board(fixture: &wt_app_core::test_support::CommandFixture) -> wt_tui::Board {
        fixture
            .ctx
            .database
            .call(|store| {
                store.set_slug_base("two", Some(("feature/one", None)))?;
                Ok(())
            })
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
        wt_sources::inventory::board(&fixture.ctx.config, &facts, &state, &Default::default())
    }

    async fn sections(
        fixture: &wt_app_core::test_support::CommandFixture,
    ) -> (serde_json::Value, serde_json::Value) {
        let state = fixture
            .ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        (
            state["slugs"]["one"]["section"].clone(),
            state["slugs"]["two"]["section"].clone(),
        )
    }

    #[tokio::test]
    async fn a_landed_head_moves_alone_and_its_unlanded_children_stay() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let mut board = stacked_board(&fixture).await;
        let head = board.rows.iter().position(|row| row.key == "one").unwrap();
        board.rows[head].git.landed_on = Some(wt_tui::LandingKind::Base);
        move_row(
            &fixture.ctx,
            "one".into(),
            Some("Shipped".into()),
            None,
            Some(&board),
        )
        .await
        .unwrap();
        assert_eq!(
            sections(&fixture).await,
            (serde_json::json!("Shipped"), serde_json::Value::Null)
        );
        // What was stacked on it is its own unit now and moves by itself.
        move_row(
            &fixture.ctx,
            "two".into(),
            Some("Next".into()),
            None,
            Some(&board),
        )
        .await
        .unwrap();
        assert_eq!(
            sections(&fixture).await,
            (serde_json::json!("Shipped"), serde_json::json!("Next"))
        );
        // Reordering one past the edge of its section takes only it along.
        fixture
            .ctx
            .database
            .call(|store| {
                store.move_worktrees_to_section(
                    &["one".to_owned(), "two".to_owned()],
                    Some("Next"),
                )?;
                Ok(())
            })
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
            wt_config::UiSort::Manual,
        );
        let last = board
            .sections
            .iter()
            .find(|section| section.key == "Next")
            .and_then(|section| section.rows.last())
            .map(|&index| board.rows[index].key.clone())
            .unwrap();
        let mut ctx = fixture.ctx.clone();
        std::sync::Arc::make_mut(&mut ctx.config).ui.sort = wt_config::UiSort::Manual;
        press(&ctx, &board, &board, Some(last.as_str()), "Next", true)
            .await
            .unwrap_err();
        // An empty named section is on screen and takes the row.
        fixture
            .ctx
            .database
            .call(|store| {
                store.place_slug("ghost", Some("Later"), false)?;
                Ok(())
            })
            .await
            .unwrap();
        board.sections.push(wt_tui::BoardSection {
            key: "Later".into(),
            title: "Later".into(),
            ..Default::default()
        });
        press(&ctx, &board, &board, Some(last.as_str()), "Next", true)
            .await
            .unwrap();
        let (one, two) = sections(&fixture).await;
        let (moved, stayed) = if last == "one" {
            (one, two)
        } else {
            (two, one)
        };
        assert_eq!(
            (moved, stayed),
            (serde_json::json!("Later"), serde_json::json!("Next"))
        );
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn rows_and_blocks_derived_from_what_is_owed_are_not_moved_or_reordered() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        fixture
            .ctx
            .database
            .call(|store| {
                store.place_slug("one", Some("Batch"), false)?;
                store.place_slug("two", Some("Batch"), false)?;
                Ok(())
            })
            .await
            .unwrap();
        let mut board = stacked_board(&fixture).await;
        let head = board.rows.iter().position(|row| row.key == "one").unwrap();
        board.rows[head].git.landed_on = Some(wt_tui::LandingKind::Base);
        board.rows[head].owed_phase = Some(wt_core::OwedPhase::Done);
        board.rows[head].owed_block = Some(wt_core::OwedBlock::Done);
        board.display.owed_sections = true;
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
        let done = wt_tui::LANDED_SECTION.to_owned();
        let refused = |error: anyhow::Error| {
            assert!(
                error
                    .chain()
                    .any(|cause| cause.is::<wt_app_core::host_errors::Notice>()),
                "{error:#}"
            );
            assert_eq!(
                error.to_string(),
                "Landed is derived from what landed rows owe; set [ui] owed_sections = false to file rows by hand"
            );
        };
        // Out of the block, inside it, and the block itself.
        refused(
            move_row(
                &fixture.ctx,
                "one".into(),
                Some("Next".into()),
                None,
                Some(&board),
            )
            .await
            .unwrap_err(),
        );
        for down in [true, false] {
            refused(
                press(&fixture.ctx, &board, &board, Some("one"), &done, down)
                    .await
                    .unwrap_err(),
            );
            refused(
                press(&fixture.ctx, &board, &board, None, &done, down)
                    .await
                    .unwrap_err(),
            );
            // The host refuses on its own account, whatever the request says.
            refused(
                reorder(
                    &fixture.ctx,
                    Some("one".into()),
                    done.clone(),
                    down,
                    wt_tui::ReorderStep::Cross {
                        unit: "one".into(),
                        rows: vec!["one".into()],
                        units: vec!["one".into()],
                        to: "Batch".into(),
                        to_units: vec!["two".into()],
                    },
                    Some(&board),
                )
                .await
                .unwrap_err(),
            );
            refused(
                reorder(
                    &fixture.ctx,
                    Some("two".into()),
                    "Batch".into(),
                    down,
                    wt_tui::ReorderStep::Cross {
                        unit: "two".into(),
                        rows: vec!["two".into()],
                        units: vec!["two".into()],
                        to: done.clone(),
                        to_units: vec!["one".into()],
                    },
                    Some(&board),
                )
                .await
                .unwrap_err(),
            );
        }
        // Into it: a row at the bottom of the last stored section has
        // nowhere further to go, and no section moves past a derived block.
        let order: Vec<&str> = board
            .sections
            .iter()
            .filter(|section| section.drawn())
            .map(|section| section.key.as_str())
            .collect();
        assert_eq!(order, ["Batch", done.as_str()]);
        let bottom = press(&fixture.ctx, &board, &board, Some("two"), "Batch", true)
            .await
            .unwrap_err();
        assert_eq!(bottom.to_string(), "already at bottom");
        let bottom = press(&fixture.ctx, &board, &board, None, "Batch", true)
            .await
            .unwrap_err();
        assert_eq!(bottom.to_string(), "section already at bottom");
        // Nothing stored changed: off again, the row is where it was filed.
        let after = fixture
            .ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        assert_eq!(after["slugs"], state["slugs"]);
        assert_eq!(after["sectionsOrder"], state["sectionsOrder"]);
        assert_eq!(after["slugs"]["one"]["section"], "Batch");
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
        // The move is the user's own, so the attention feed stays quiet.
        assert!(fixture.ctx.section_writes.consume("one", None));
        assert!(
            !state["sectionsOrder"]
                .as_array()
                .unwrap()
                .iter()
                .any(|name| name == "Batch")
        );
        fixture.close().await.unwrap();
    }

    /// Under `[ui] owed_sections` a landed row is drawn in a derived block
    /// and stays filed in its section. Deleting that section moves it too,
    /// and the witness the board builds is the stored placement the host
    /// checks, so the delete goes through.
    #[tokio::test]
    async fn delete_counts_a_landed_row_filed_in_the_section_and_drawn_elsewhere() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let ctx = &fixture.ctx;
        ctx.database
            .call(|store| {
                store.place_slug("one", Some("Batch"), false)?;
                Ok(store.place_slug("two", Some("Batch"), false)?)
            })
            .await
            .unwrap();
        let facts = ctx
            .repository
            .inventory_status(&ctx.cancellation)
            .await
            .unwrap();
        let state = ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        let mut board =
            wt_sources::inventory::board(&ctx.config, &facts, &state, &Default::default());
        board.display.owed_sections = true;
        let one = board.rows.iter().position(|row| row.key == "one").unwrap();
        board.rows[one].git.landed_on = Some(wt_tui::LandingKind::Base);
        board.rows[one].owed_phase = Some(wt_core::OwedPhase::Done);
        board.rows[one].owed_block = Some(wt_core::OwedBlock::Done);
        wt_sources::board_layout::prepare(
            &mut board,
            &state,
            &ctx.config.branch.base,
            ctx.config.ui.sort,
        );
        let drawn = |name: &str| {
            board
                .sections
                .iter()
                .find(|section| section.title == name)
                .map(|section| {
                    section
                        .rows
                        .iter()
                        .map(|&row| board.rows[row].key.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        };
        assert_eq!(drawn("Batch"), ["two"]);
        assert_eq!(drawn("Landed"), ["one"]);
        let seen = board.section_witness("Batch");
        assert_eq!(seen, ["one", "two"]);
        // Only what is drawn there is not what is stored there.
        let partial = delete(ctx, "Batch".into(), Seen(vec!["two".into()]), Some(&board)).await;
        assert!(partial.is_err());
        delete(ctx, "Batch".into(), Seen(seen), Some(&board))
            .await
            .unwrap();
        let state = ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        assert!(state["slugs"]["one"]["section"].is_null());
        assert!(state["slugs"]["two"]["section"].is_null());
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
        let inbox = wt_sources::board_layout::INBOX;
        press(&fixture.ctx, &board, &board, Some("one"), inbox, true)
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
        board.display.status_sort = true;
        let mut ctx = fixture.ctx.clone();
        std::sync::Arc::make_mut(&mut ctx.config).ui.sort = wt_config::UiSort::Status;
        let inbox = wt_sources::board_layout::INBOX;
        let pinned = press(&ctx, &board, &board, Some("two"), inbox, true)
            .await
            .unwrap_err();
        assert_eq!(pinned.to_string(), wt_tui::STATUS_SORT_PINS);
        // The host applies the rule itself to a request that ignores it.
        let pinned = reorder(
            &ctx,
            Some("two".into()),
            inbox.into(),
            true,
            wt_tui::ReorderStep::Pass {
                unit: "two".into(),
                past: "one".into(),
                units: vec!["two".into(), "one".into()],
            },
            Some(&board),
        )
        .await
        .unwrap_err();
        assert_eq!(pinned.to_string(), wt_tui::STATUS_SORT_PINS);
        // An empty derived group is not drawn, so moving down past the last
        // row skips it; an empty named section is on screen and takes the row.
        board.sections.push(wt_tui::BoardSection {
            key: wt_sources::board_layout::REMOVING.into(),
            title: "Removing".into(),
            ..Default::default()
        });
        assert!(
            press(&fixture.ctx, &board, &board, Some("one"), inbox, true)
                .await
                .is_err()
        );
        fixture
            .ctx
            .database
            .call(|store| {
                store.place_slug("ghost", Some("Next"), false)?;
                Ok(())
            })
            .await
            .unwrap();
        board.sections.push(wt_tui::BoardSection {
            key: "Next".into(),
            title: "Next".into(),
            ..Default::default()
        });
        press(&fixture.ctx, &board, &board, Some("one"), inbox, true)
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

    /// Send the press a person looking at `seen` would make to a host whose
    /// own board is `host`. A press that moves nothing is a notice, as the
    /// terminal says it.
    async fn press(
        ctx: &AppContext,
        seen: &wt_tui::Board,
        host: &wt_tui::Board,
        key: Option<&str>,
        section: &str,
        down: bool,
    ) -> Result<UiReply> {
        let step = wt_tui::plan_reorder(seen, key, section, down)
            .map_err(wt_app_core::host_errors::notice)?;
        reorder(
            ctx,
            key.map(str::to_owned),
            section.to_owned(),
            down,
            step,
            Some(host),
        )
        .await
    }

    /// The same press, also shown on `seen` the way the terminal shows it
    /// before the host has answered.
    async fn press_and_show(
        ctx: &AppContext,
        seen: &mut wt_tui::Board,
        host: &wt_tui::Board,
        key: Option<&str>,
        section: &str,
        down: bool,
    ) -> Result<UiReply> {
        use wt_runtime::Mutation;
        let step = wt_tui::plan_reorder(seen, key, section, down)
            .map_err(wt_app_core::host_errors::notice)?;
        let action = wt_tui::UiAction::Reorder {
            key: key.map(str::to_owned),
            section: section.to_owned(),
            down,
            step: step.clone(),
        };
        let wt_tui::Effect::Mutation(shown) = action.effect(seen) else {
            panic!("a reorder changes the board");
        };
        shown.apply(seen);
        reorder(
            ctx,
            key.map(str::to_owned),
            section.to_owned(),
            down,
            step,
            Some(host),
        )
        .await
    }

    async fn stored(fixture: &wt_app_core::test_support::CommandFixture) -> serde_json::Value {
        fixture
            .ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap()
    }

    /// `board` laid out again, under manual sort, from what is stored now.
    async fn relaid(
        fixture: &wt_app_core::test_support::CommandFixture,
        board: &wt_tui::Board,
    ) -> wt_tui::Board {
        let mut board = board.clone();
        // The board source files a remote row from the controller's layout.
        let mut state = stored(fixture).await;
        let remote = state["remoteLayouts"].clone();
        for (key, layout) in remote.as_object().into_iter().flatten() {
            state["slugs"][key] = layout.clone();
        }
        wt_sources::board_layout::prepare(
            &mut board,
            &state,
            &fixture.ctx.config.branch.base,
            wt_config::UiSort::Manual,
        );
        board.display.status_sort = false;
        board
    }

    fn drawn(board: &wt_tui::Board, section: &str) -> Vec<String> {
        board
            .sections
            .iter()
            .find(|drawn| drawn.key == section)
            .map(|drawn| {
                drawn
                    .rows
                    .iter()
                    .map(|&row| board.rows[row].key.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The fixture's board with rows that exist only on it. A reorder
    /// writes placement, which needs no checkout.
    async fn board_with(
        fixture: &wt_app_core::test_support::CommandFixture,
        extra: &[&str],
    ) -> wt_tui::Board {
        let facts = fixture
            .ctx
            .repository
            .inventory_status(&fixture.ctx.cancellation)
            .await
            .unwrap();
        let state = stored(fixture).await;
        let mut board =
            wt_sources::inventory::board(&fixture.ctx.config, &facts, &state, &Default::default());
        for key in extra {
            let mut row = board
                .rows
                .iter()
                .find(|row| row.key == "one")
                .unwrap()
                .clone();
            row.key = (*key).into();
            row.slug = (*key).into();
            row.branch = format!("feature/{key}");
            if let Some(reference) = wt_core::parse_worktree_ledger_key(key)
                && let wt_core::WorktreeRef::Remote { host, .. } = reference
            {
                row.host = Some(host);
            }
            board.rows.push(row);
        }
        relaid(fixture, &board).await
    }

    fn manual(fixture: &wt_app_core::test_support::CommandFixture) -> AppContext {
        let mut ctx = fixture.ctx.clone();
        std::sync::Arc::make_mut(&mut ctx.config).ui.sort = wt_config::UiSort::Manual;
        ctx
    }

    fn is_stale(error: &anyhow::Error) -> bool {
        error
            .chain()
            .any(|cause| cause.is::<wt_app_core::host_errors::Stale>())
    }

    /// The regression this guards: the host chose the neighbour from its own
    /// board, which still showed the order from before its last write, and
    /// swapped the same pair back.
    #[tokio::test]
    async fn a_burst_of_presses_lands_each_one_on_a_host_whose_board_is_behind() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let ctx = manual(&fixture);
        let inbox = wt_sources::board_layout::INBOX;
        // The host's board is never read again here; the person's follows
        // each press.
        let behind = board_with(&fixture, &["four", "three"]).await;
        assert_eq!(drawn(&behind, inbox), ["four", "one", "three", "two"]);
        let mut seen = behind.clone();
        for _ in 0..3 {
            press_and_show(&ctx, &mut seen, &behind, Some("four"), inbox, true)
                .await
                .unwrap();
        }
        assert_eq!(drawn(&seen, inbox), ["one", "three", "two", "four"]);
        assert_eq!(
            drawn(&relaid(&fixture, &behind).await, inbox),
            drawn(&seen, inbox)
        );
        for _ in 0..2 {
            press_and_show(&ctx, &mut seen, &behind, Some("four"), inbox, false)
                .await
                .unwrap();
        }
        assert_eq!(drawn(&seen, inbox), ["one", "four", "three", "two"]);
        assert_eq!(
            drawn(&relaid(&fixture, &behind).await, inbox),
            drawn(&seen, inbox)
        );

        // A press resolved from the board that shows none of it is refused
        // inside the write, and stores nothing.
        let before = stored(&fixture).await;
        let error = press(&ctx, &behind, &behind, Some("four"), inbox, true)
            .await
            .unwrap_err();
        assert!(is_stale(&error), "{error:#}");
        assert_eq!(stored(&fixture).await, before);

        // Another process reorders the section between the person's look
        // and their press.
        fixture
            .ctx
            .database
            .call(|store| {
                let theirs: Vec<String> =
                    ["one", "four", "three", "two"].map(str::to_owned).to_vec();
                let theirs: Vec<wt_store::ShownUnit> = theirs
                    .into_iter()
                    .map(|key| wt_store::ShownUnit {
                        key,
                        band: 0,
                        anchored: false,
                    })
                    .collect();
                let shown = wt_store::ShownSection {
                    name: None,
                    drawn: &theirs,
                };
                Ok(store.swap_shown_units(shown, "three", "two")?)
            })
            .await
            .unwrap()
            .unwrap();
        let error = press(&ctx, &seen, &behind, Some("one"), inbox, true)
            .await
            .unwrap_err();
        assert!(is_stale(&error), "{error:#}");
        assert_eq!(error.to_string(), "three and two have changed places");
        // The list then shows the true order, and a press from it goes through.
        let truth = relaid(&fixture, &behind).await;
        assert_eq!(drawn(&truth, inbox), ["one", "four", "two", "three"]);
        press(&ctx, &truth, &behind, Some("one"), inbox, true)
            .await
            .unwrap();
        assert_eq!(
            drawn(&relaid(&fixture, &behind).await, inbox),
            ["four", "one", "two", "three"]
        );
        // A row the host no longer lists was shown by an older board.
        let mut gone = behind.clone();
        gone.rows.retain(|row| row.key != "three");
        let error = press(&ctx, &truth, &gone, Some("one"), inbox, true)
            .await
            .unwrap_err();
        assert!(is_stale(&error), "{error:#}");
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn a_stack_reorders_as_one_unit_and_remote_rows_keep_their_own_layout() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let ctx = manual(&fixture);
        let inbox = wt_sources::board_layout::INBOX;
        fixture
            .ctx
            .database
            .call(|store| {
                store.set_slug_base("two", Some(("feature/one", None)))?;
                Ok(())
            })
            .await
            .unwrap();
        let remote = "@remote/box/far";
        let behind = board_with(&fixture, &["three", remote]).await;
        // A unit sorts by its root, wherever its members would sort alone.
        assert_eq!(drawn(&behind, inbox), [remote, "one", "two", "three"]);
        let two = behind.rows.iter().find(|row| row.key == "two").unwrap();
        assert_eq!(two.stack_parent.as_deref(), Some("one"));
        let mut seen = behind.clone();
        // From the member: the whole stack passes its neighbour.
        press_and_show(&ctx, &mut seen, &behind, Some("two"), inbox, true)
            .await
            .unwrap();
        assert_eq!(drawn(&seen, inbox), [remote, "three", "one", "two"]);
        assert_eq!(
            drawn(&relaid(&fixture, &behind).await, inbox),
            drawn(&seen, inbox)
        );
        // The remote row's order is the controller's, in its own collection.
        press_and_show(&ctx, &mut seen, &behind, Some(remote), inbox, true)
            .await
            .unwrap();
        assert_eq!(drawn(&seen, inbox), ["three", remote, "one", "two"]);
        assert_eq!(
            drawn(&relaid(&fixture, &behind).await, inbox),
            drawn(&seen, inbox)
        );
        let state = stored(&fixture).await;
        assert!(state["remoteLayouts"][remote]["order"].is_number());
        assert!(state["slugs"].get(remote).is_none());

        // A member filed in another section is a unit there, and crossing
        // back takes only it.
        fixture
            .ctx
            .database
            .call(|store| {
                store.move_worktrees_to_section(&["two".to_owned()], Some("Later"))?;
                Ok(())
            })
            .await
            .unwrap();
        let split = relaid(&fixture, &behind).await;
        assert_eq!(drawn(&split, "Later"), ["two"]);
        assert_eq!(drawn(&split, inbox), ["three", remote, "one"]);
        let mut seen = split.clone();
        // The Inbox is drawn above Later here, so up leaves Later for its
        // bottom.
        press_and_show(&ctx, &mut seen, &split, Some("two"), "Later", false)
            .await
            .unwrap();
        let state = stored(&fixture).await;
        assert!(state["slugs"]["two"]["section"].is_null());
        assert!(state["slugs"]["one"]["section"].is_null());
        assert_eq!(drawn(&seen, inbox), ["three", remote, "one", "two"]);
        assert_eq!(
            drawn(&relaid(&fixture, &split).await, inbox),
            drawn(&seen, inbox)
        );
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn status_sort_compares_and_moves_only_within_one_status() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let mut ctx = fixture.ctx.clone();
        std::sync::Arc::make_mut(&mut ctx.config).ui.sort = wt_config::UiSort::Status;
        let inbox = wt_sources::board_layout::INBOX;
        // Stored manual order: one, three, two.
        fixture
            .ctx
            .database
            .call(|store| {
                for key in ["one", "three", "two"] {
                    store.place_slug(key, None, false)?;
                }
                Ok(())
            })
            .await
            .unwrap();
        // Status sort draws "three" first: its status ranks before theirs.
        let mut board = board_with(&fixture, &["three"]).await;
        assert_eq!(drawn(&board, inbox), ["one", "three", "two"]);
        let index = |board: &wt_tui::Board, key: &str| {
            board.rows.iter().position(|row| row.key == key).unwrap()
        };
        let (one, two, three) = (
            index(&board, "one"),
            index(&board, "two"),
            index(&board, "three"),
        );
        board.rows[three].work_rank = 0;
        board.display.status_sort = true;
        let section = board
            .sections
            .iter_mut()
            .find(|section| section.key == inbox)
            .unwrap();
        section.rows = vec![three, one, two];
        // "one" passes "two"; "three" keeps its place in the manual order
        // although the list drawn does not show it there.
        let mut seen = board.clone();
        press_and_show(&ctx, &mut seen, &board, Some("one"), inbox, true)
            .await
            .unwrap();
        assert_eq!(drawn(&seen, inbox), ["three", "two", "one"]);
        assert_eq!(
            drawn(&relaid(&fixture, &board).await, inbox),
            ["three", "two", "one"]
        );
        // Under manual sort the list drawn is the witness, and this one was
        // never the stored order.
        let error = press(&manual(&fixture), &board, &board, Some("one"), inbox, true)
            .await
            .unwrap_err();
        assert!(is_stale(&error), "{error:#}");
        // A neighbour of another status is not passed.
        let pinned = press(&ctx, &seen, &board, Some("two"), inbox, false)
            .await
            .unwrap_err();
        assert_eq!(pinned.to_string(), wt_tui::STATUS_SORT_PINS);
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn a_section_moves_past_the_one_shown_on_a_host_whose_board_is_behind() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let ctx = manual(&fixture);
        fixture
            .ctx
            .database
            .call(|store| {
                store.place_slug("one", Some("First"), false)?;
                store.place_slug("two", Some("Second"), false)?;
                store.place_slug("ghost", Some("Third"), false)?;
                Ok(())
            })
            .await
            .unwrap();
        let behind = board_with(&fixture, &[]).await;
        let named = |board: &wt_tui::Board| -> Vec<String> {
            board
                .sections
                .iter()
                .filter(|section| !section.key.starts_with('\0'))
                .map(|section| section.key.clone())
                .collect()
        };
        assert_eq!(named(&behind), ["First", "Second", "Third"]);
        let mut seen = behind.clone();
        for _ in 0..2 {
            press_and_show(&ctx, &mut seen, &behind, None, "First", true)
                .await
                .unwrap();
        }
        assert_eq!(named(&seen), ["Second", "Third", "First"]);
        assert_eq!(named(&relaid(&fixture, &behind).await), named(&seen));
        let error = press(&ctx, &behind, &behind, None, "First", true)
            .await
            .unwrap_err();
        assert!(is_stale(&error), "{error:#}");
        assert_eq!(named(&relaid(&fixture, &behind).await), named(&seen));
        // A crossing refused inside the write leaves no claim that the move
        // was this process's own.
        let crossing = |rows: &[&str], to_units: &[&str]| wt_tui::ReorderStep::Cross {
            unit: "two".into(),
            rows: rows.iter().map(|row| (*row).to_owned()).collect(),
            units: vec!["two".into()],
            to: "Third".into(),
            to_units: to_units.iter().map(|unit| (*unit).to_owned()).collect(),
        };
        let cross = |step| {
            reorder(
                &ctx,
                Some("two".into()),
                "Second".into(),
                true,
                step,
                Some(&behind),
            )
        };
        let error = cross(crossing(&["two"], &["one"])).await.unwrap_err();
        assert!(is_stale(&error), "{error:#}");
        assert_eq!(error.to_string(), "one is filed elsewhere now");
        assert!(!fixture.ctx.section_writes.consume("two", Some("Third")));
        // Rows that are not one stack do not move as one unit.
        let error = cross(crossing(&["two", "one"], &[])).await.unwrap_err();
        assert!(is_stale(&error), "{error:#}");
        assert_eq!(stored(&fixture).await["slugs"]["one"]["section"], "First");
        // A row at the edge enters the section the person saw next.
        press_and_show(&ctx, &mut seen, &behind, Some("two"), "Second", true)
            .await
            .unwrap();
        assert_eq!(stored(&fixture).await["slugs"]["two"]["section"], "Third");
        assert!(fixture.ctx.section_writes.consume("two", Some("Third")));
        fixture.close().await.unwrap();
    }

    /// A row of a fabricated board: what is stored for it, and whether the
    /// board lists it. A reorder writes placement, which needs no checkout.
    #[derive(Clone, Debug)]
    struct Fabricated {
        key: String,
        section: &'static str,
        order: Option<f64>,
        /// Created from the board: behind this row, or first when `None`.
        created_after: Option<Option<String>>,
        stacked_on: Option<String>,
        status: Option<wt_core::WorkState>,
        /// Filed in the section and drawn elsewhere.
        hidden: bool,
    }

    impl Fabricated {
        fn new(key: &str, section: &'static str) -> Self {
            Self {
                key: key.to_owned(),
                section,
                order: None,
                created_after: None,
                stacked_on: None,
                status: None,
                hidden: false,
            }
        }
    }

    const FABRICATED_SECTIONS: [&str; 3] = ["S", "T", "U"];

    /// Store `rows` as the whole layout and return the rows a board lists.
    async fn fabricate(
        fixture: &wt_app_core::test_support::CommandFixture,
        rows: &[Fabricated],
    ) -> Vec<wt_tui::BoardRow> {
        let mut slugs = serde_json::Map::new();
        let mut listed = Vec::new();
        for row in rows {
            let record = row
                .status
                .map(|state| wt_core::WorkStatusRecord::new(state, "2026-10-11T00:00:00Z"));
            let mut entry = serde_json::json!({ "section": row.section });
            if let Some(order) = row.order {
                entry["order"] = serde_json::json!(order);
            }
            if let Some(after) = &row.created_after {
                entry["order"] = serde_json::Value::Null;
                entry["creationAfter"] = serde_json::json!(after);
            }
            if let Some(base) = &row.stacked_on {
                entry["baseBranch"] = serde_json::json!(base);
            }
            if let Some(record) = &record {
                entry["work"] = serde_json::to_value(record).unwrap();
                assert!(wt_core::parse_work_status(&entry["work"]).is_some());
            }
            slugs.insert(row.key.clone(), entry);
            if !row.hidden {
                listed.push(wt_tui::BoardRow {
                    key: row.key.clone(),
                    slug: row.key.clone(),
                    title: row.key.clone(),
                    branch: row.key.clone(),
                    base_branch: row.stacked_on.clone(),
                    work_rank: wt_core::work_record_rank(record.as_ref()),
                    work: record.map(|record| wt_tui::WorkPresentation {
                        record: Some(record),
                        ..Default::default()
                    }),
                    ..Default::default()
                });
            }
        }
        let state = serde_json::json!({ "sectionsOrder": FABRICATED_SECTIONS, "slugs": slugs });
        fixture
            .ctx
            .database
            .call(move |store| Ok(store.write_repository_state_json(&state.to_string())?))
            .await
            .unwrap();
        listed
    }

    /// The board the layout draws from what is stored now.
    async fn laid_out(
        fixture: &wt_app_core::test_support::CommandFixture,
        rows: &[wt_tui::BoardRow],
        sort: wt_config::UiSort,
    ) -> wt_tui::Board {
        let mut board = wt_tui::Board {
            rows: rows.to_vec(),
            ..Default::default()
        };
        board.display.status_sort = sort == wt_config::UiSort::Status;
        wt_sources::board_layout::prepare(
            &mut board,
            &stored(fixture).await,
            &fixture.ctx.config.branch.base,
            sort,
        );
        board
    }

    fn sorting(
        fixture: &wt_app_core::test_support::CommandFixture,
        sort: wt_config::UiSort,
    ) -> AppContext {
        let mut ctx = fixture.ctx.clone();
        std::sync::Arc::make_mut(&mut ctx.config).ui.sort = sort;
        ctx
    }

    /// Status sort draws a row that has a status by that status, also one
    /// created from the board that was never ordered. Taken for a row still
    /// behind the row it was created after, it was stored on the near side
    /// of its neighbour: accepted, shown, and drawn back.
    #[tokio::test]
    async fn under_status_sort_a_started_row_created_from_the_board_passes_its_neighbour() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let sort = wt_config::UiSort::Status;
        let ctx = sorting(&fixture, sort);
        let rows = [
            Fabricated {
                order: Some(5.0),
                status: Some(wt_core::WorkState::Ready),
                ..Fabricated::new("p", "S")
            },
            Fabricated {
                order: Some(1.0),
                status: Some(wt_core::WorkState::Review),
                ..Fabricated::new("q", "S")
            },
            Fabricated {
                created_after: Some(Some("p".into())),
                status: Some(wt_core::WorkState::Review),
                ..Fabricated::new("a", "S")
            },
            Fabricated {
                order: Some(2.0),
                status: Some(wt_core::WorkState::Review),
                ..Fabricated::new("z", "S")
            },
        ];
        for (key, down, expected) in [
            ("a", true, ["p", "q", "a", "z"]),
            ("q", false, ["p", "q", "a", "z"]),
            // Neither of the pair is that row, and it still must not move.
            ("q", true, ["p", "a", "z", "q"]),
            ("z", false, ["p", "a", "z", "q"]),
        ] {
            let listed = fabricate(&fixture, &rows).await;
            let board = laid_out(&fixture, &listed, sort).await;
            assert_eq!(drawn(&board, "S"), ["p", "a", "q", "z"]);
            let mut seen = board.clone();
            press_and_show(&ctx, &mut seen, &board, Some(key), "S", down)
                .await
                .unwrap();
            assert_eq!(drawn(&seen, "S"), expected, "{key} down={down}");
            assert_eq!(
                drawn(&laid_out(&fixture, &listed, sort).await, "S"),
                expected,
                "{key} down={down}"
            );
        }
        fixture.close().await.unwrap();
    }

    /// The same sections on every run.
    struct Dice(u64);

    impl Dice {
        fn roll(&mut self, sides: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 33) % sides
        }

        fn chance(&mut self, percent: u64) -> bool {
            self.roll(100) < percent
        }
    }

    fn random_sections(dice: &mut Dice) -> Vec<Fabricated> {
        let count = 3 + dice.roll(7);
        let mut rows: Vec<Fabricated> = Vec::new();
        for index in 0..count {
            let section = FABRICATED_SECTIONS[usize::from(dice.chance(35))];
            let mut row = Fabricated::new(&format!("r{index}"), section);
            match dice.roll(10) {
                0..=4 => row.order = Some(dice.roll(6) as f64),
                5..=6 => {}
                _ => {
                    let peers: Vec<&Fabricated> = rows
                        .iter()
                        .filter(|peer| peer.section == section && !peer.hidden)
                        .collect();
                    row.created_after = Some(if peers.is_empty() || dice.chance(25) {
                        None
                    } else {
                        Some(peers[dice.roll(peers.len() as u64) as usize].key.clone())
                    });
                }
            }
            row.status = [
                None,
                Some(wt_core::WorkState::Ready),
                Some(wt_core::WorkState::Review),
                Some(wt_core::WorkState::Todo),
            ][dice.roll(4) as usize];
            if !rows.is_empty() && dice.chance(25) {
                row.stacked_on = Some(rows[dice.roll(rows.len() as u64) as usize].key.clone());
            }
            if dice.chance(10) {
                row.hidden = true;
                row.created_after = None;
                row.order = Some(dice.roll(6) as f64);
            }
            rows.push(row);
        }
        rows
    }

    /// For sections of every kind the layout draws (ordered rows, rows never
    /// ordered, rows at their creation position, rows filed there and drawn
    /// elsewhere, stacks whole and split, with and without statuses, under
    /// both sorts): a press the host accepts is stored so that the board
    /// draws exactly what the terminal showed at the keypress, and a press
    /// resolved from the board as drawn is never refused as stale.
    #[tokio::test]
    async fn every_accepted_press_redraws_the_board_as_it_was_shown() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let (mut passes, mut crossings) = (0, 0);
        for seed in 0..1500u64 {
            let mut dice = Dice(seed);
            let sort = if seed % 2 == 0 {
                wt_config::UiSort::Status
            } else {
                wt_config::UiSort::Manual
            };
            let ctx = sorting(&fixture, sort);
            let rows = random_sections(&mut dice);
            let listed = fabricate(&fixture, &rows).await;
            for press in 0..4 {
                let board = laid_out(&fixture, &listed, sort).await;
                let layout = |board: &wt_tui::Board| {
                    FABRICATED_SECTIONS.map(|section| (section, drawn(board, section)))
                };
                let before = layout(&board);
                let candidates: Vec<(&str, &String)> = before
                    .iter()
                    .flat_map(|(section, keys)| keys.iter().map(|key| (*section, key)))
                    .collect();
                if candidates.is_empty() {
                    break;
                }
                let (section, key) = candidates[dice.roll(candidates.len() as u64) as usize];
                let down = dice.chance(50);
                let said = format!(
                    "seed {seed} press {press}: {key} {} in {section} under {sort:?}\nstored {}\ndrawn {before:?}",
                    if down { "down" } else { "up" },
                    stored(&fixture).await["slugs"],
                );
                let mut seen = board.clone();
                // Not compared: a split stack member that crosses into the
                // section of the row it is stacked on joins that unit, and
                // under status sort its status can move the whole unit to
                // another status. Where among those is decided by a stored
                // order the board does not carry.
                let joins_its_stack = sort == wt_config::UiSort::Status
                    && matches!(
                        wt_tui::plan_reorder(&board, Some(key), section, down),
                        Ok(wt_tui::ReorderStep::Cross { unit, to, .. })
                            if board
                                .rows
                                .iter()
                                .find(|row| row.key == unit)
                                .and_then(|row| row.stack_parent.as_ref())
                                .is_some_and(|parent| drawn(&board, &to).contains(parent))
                    );
                match press_and_show(&ctx, &mut seen, &board, Some(key), section, down).await {
                    Ok(_) if joins_its_stack => {}
                    Ok(_) => {
                        let after = laid_out(&fixture, &listed, sort).await;
                        assert_eq!(layout(&after), layout(&seen), "{said}");
                        if drawn(&seen, section).contains(key) {
                            passes += 1;
                        } else {
                            crossings += 1;
                        }
                    }
                    // Nothing to do (an edge, a status that pins the row).
                    Err(error) => assert!(
                        error.is::<wt_app_core::host_errors::Notice>(),
                        "{error:#}\n{said}"
                    ),
                }
            }
        }
        // Enough of both went through for the comparison to mean something.
        assert!(
            passes > 600 && crossings > 200,
            "{passes} passes, {crossings} crossings"
        );
        fixture.close().await.unwrap();
    }

    /// Measurement, not a check. Run with
    /// `cargo test -p wt-host timing_of_a_press -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore = "timing measurement of one J or K press"]
    async fn timing_of_a_press_shown_and_stored() {
        use std::time::{Duration, Instant};
        use wt_runtime::Mutation;
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let ctx = manual(&fixture);
        let inbox = wt_sources::board_layout::INBOX;
        let extra: Vec<String> = (0..40).map(|row| format!("r{row:02}")).collect();
        let extra: Vec<&str> = extra.iter().map(String::as_str).collect();
        let behind = board_with(&fixture, &extra).await;
        let mut seen = behind.clone();
        let (mut shown, mut written, mut reread) = (Vec::new(), Vec::new(), Vec::new());
        for press in 0..60 {
            let down = (press / 20) % 2 == 0;
            // What the keypress waits for now: resolving and showing it.
            let started = Instant::now();
            let step = wt_tui::plan_reorder(&seen, Some("one"), inbox, down).unwrap();
            let action = wt_tui::UiAction::Reorder {
                key: Some("one".into()),
                section: inbox.into(),
                down,
                step: step.clone(),
            };
            let wt_tui::Effect::Mutation(change) = action.effect(&seen) else {
                panic!("a reorder changes the board");
            };
            change.apply(&mut seen);
            shown.push(started.elapsed());
            let started = Instant::now();
            reorder(
                &ctx,
                Some("one".into()),
                inbox.into(),
                down,
                step,
                Some(&behind),
            )
            .await
            .unwrap();
            written.push(started.elapsed());
            // What it waited for before, at the least: the write, then the
            // stored state read again and the board laid out from it.
            let again = relaid(&fixture, &behind).await;
            reread.push(started.elapsed());
            assert_eq!(drawn(&again, inbox), drawn(&seen, inbox));
        }
        let report = |name: &str, samples: &mut Vec<Duration>| {
            samples.sort();
            println!(
                "{name}: p50 {:?}, p90 {:?}, max {:?}",
                samples[samples.len() / 2],
                samples[samples.len() * 9 / 10],
                samples[samples.len() - 1]
            );
        };
        report("shown at the keypress", &mut shown);
        report("stored by the host", &mut written);
        report("stored, read again and laid out", &mut reread);
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
                store.set_slug_title("two", "Pinned", wt_store::TitleSource::User, None)?;
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
