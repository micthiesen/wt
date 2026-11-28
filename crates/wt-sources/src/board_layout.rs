//! Pure preparation of section placement and stack rails from the current
//! inventory and durable layout. No separate stack membership is persisted.
use std::collections::BTreeMap;

use serde_json::Value;
use wt_config::UiSort;
use wt_core::{
    ChainMember, SpineMember, build_display_stack_index, parse_work_status, spine_layout,
    work_record_rank,
};
use wt_tui::{Board, BoardRow, BoardSection, LANDED_SECTION};

/// Apply explicit positions for unstarted rows. Invalid cycles retain every row.
fn apply_creation_positions(rows: &mut Vec<usize>, positions: &[(usize, Option<usize>)]) {
    if positions.is_empty() {
        return;
    }
    let anchored = positions
        .iter()
        .map(|(row, _)| *row)
        .collect::<std::collections::BTreeSet<_>>();
    let mut children = BTreeMap::<Option<usize>, Vec<usize>>::new();
    for &(row, anchor) in positions {
        children.entry(anchor).or_default().push(row);
    }
    let mut roots = children.get(&None).cloned().unwrap_or_default();
    roots.extend(rows.iter().copied().filter(|row| !anchored.contains(row)));
    // A malformed cycle has no root. These fallbacks keep the function total.
    roots.extend(rows.iter().copied());
    let mut pending = roots.into_iter().rev().collect::<Vec<_>>();
    let mut visited = std::collections::BTreeSet::new();
    let mut placed = Vec::with_capacity(rows.len());
    while let Some(row) = pending.pop() {
        if !visited.insert(row) {
            continue;
        }
        placed.push(row);
        if let Some(next) = children.get(&Some(row)) {
            pending.extend(next.iter().rev().copied());
        }
    }
    *rows = placed;
}

pub const INBOX: &str = "\0inbox";
pub const REMOVING: &str = "\0removing";
pub const ARCHIVED: &str = "\0archived";

/// Whether `[ui] owed_sections` draws a row in the Landed block instead of
/// its stored section: only a local row on the board whose own work has
/// landed, whose only remaining meaning is what it owes. False before the
/// owed overlay has prepared the row.
fn in_landed_block(board: &Board, row: &BoardRow) -> bool {
    board.display.owed_sections
        && row.host.is_none()
        && !row.is_main
        && !row.pending_creation
        && ((row.git.landed_on.is_some() && row.owed_block.is_some())
            // A removal deletes the checkout, and the Git facts with it,
            // before the row goes: the record says where it was drawn.
            || row.removal.as_ref().is_some_and(|removal| removal.landed))
}

/// When the row's work landed, which orders the Landed block, and when wt
/// first saw that, which breaks a tie. The first recorded landing says
/// both: the merge time it recorded, or for an event from before it
/// recorded one the time it was seen. Several landings seen in one refresh
/// share that second time, so the first is what orders them. Without an
/// event, the newest commit of the branch (nothing is committed after a
/// landing), else the checkout's creation. None of these changes with what
/// the row owes or with a later refresh, so a row keeps its place.
fn landed_at_ms(board: &Board, row: &BoardRow) -> (u64, u64) {
    let recorded = board.row_history.get(&row.key).and_then(|events| {
        events.iter().find_map(|event| match event.event {
            wt_core::WorktreeEventKind::Landed { landed_at_ms, .. } => {
                Some((landed_at_ms.unwrap_or(event.at_ms), event.at_ms))
            }
            _ => None,
        })
    });
    recorded.unwrap_or_else(|| {
        let at = row
            .git
            .last_commit_ms
            .or(row.git.created_ms)
            .unwrap_or(u64::MAX);
        (at, at)
    })
}

/// Lay the board out from the stored placement. With
/// `board.display.owed_sections`, landed rows are drawn in the one Landed
/// block, oldest landing first; nothing stored is read differently or
/// written, so the option can be turned off again.
pub fn prepare(board: &mut Board, state: &Value, trunk: &str, sort: UiSort) {
    let members = board
        .rows
        .iter()
        .map(|row| {
            ChainMember::new(
                &row.key,
                branch_key(row, &row.branch),
                state["slugs"][&row.key]["baseBranch"]
                    .as_str()
                    .filter(|base| *base != trunk)
                    .map(|base| branch_key(row, base)),
            )
        })
        .collect::<Vec<_>>();
    // Display only: a member whose own work landed is no longer a parent
    // here, so it sorts and moves alone. Stack ids, the stack pause and
    // automations keep the stored index.
    let landed: std::collections::BTreeSet<&str> = board
        .rows
        .iter()
        .filter(|row| row.git.landed_on.is_some())
        .map(|row| row.key.as_str())
        .collect();
    let stacks = build_display_stack_index(&members, trunk, |member| {
        landed.contains(member.slug.as_str())
    });
    let nodes: Vec<_> = board
        .rows
        .iter()
        .map(|row| stacks.by_branch.get(&branch_key(row, &row.branch)).cloned())
        .collect();
    let node_of = |index: usize| nodes[index].as_ref();
    // Each row lives in its own stored section. Moving a stack writes every
    // member's placement, and `--only` deliberately splits one off, so the
    // read side must not pull members back to the root.
    let stored_keys: Vec<String> = board
        .rows
        .iter()
        .map(|row| {
            let section = state["slugs"][&row.key]["section"]
                .as_str()
                .filter(|name| !name.is_empty());
            // With the Landed block a row being removed stays where it is
            // drawn and says so itself; no section appears for it and goes.
            if row.removal.is_some() && !board.display.owed_sections {
                REMOVING.to_owned()
            } else if row
                .removal
                .as_ref()
                // A removal archives its row while it runs.
                .map_or(row.archived, |removal| removal.was_archived)
            {
                ARCHIVED.to_owned()
            } else {
                section.map_or_else(|| INBOX.to_owned(), str::to_owned)
            }
        })
        .collect();
    for (index, row) in board.rows.iter_mut().enumerate() {
        let entry = &state["slugs"][&row.key];
        row.other_prs = wt_core::stored_other_branches(entry)
            .into_iter()
            .map(|other| wt_tui::OtherPrView {
                branch: other.branch,
                number: other.number,
                state: other.state,
                draft: other.draft,
            })
            .collect();
        row.filed_in = entry["section"]
            .as_str()
            .filter(|name| !name.is_empty())
            .map(str::to_owned);
        row.creation_placement = entry
            .get("creationAfter")
            .filter(|_| !entry["order"].is_number())
            .map(|after| wt_tui::CreatePlacement {
                section: (stored_keys[index] != INBOX).then(|| stored_keys[index].clone()),
                after: after.as_str().map(str::to_owned),
            });
    }
    // Where each row is drawn: its stored section, or the Landed block.
    let section_keys: Vec<String> = board
        .rows
        .iter()
        .zip(stored_keys)
        .map(|(row, stored)| {
            if stored != REMOVING && stored != ARCHIVED && in_landed_block(board, row) {
                LANDED_SECTION.to_owned()
            } else {
                stored
            }
        })
        .collect();
    let index_by_branch: BTreeMap<String, usize> = board
        .rows
        .iter()
        .enumerate()
        .map(|(index, row)| (branch_key(row, &row.branch), index))
        .collect();
    // A member still names the branch it was forked from after that branch
    // landed; only a parent in the same shown stack is one here.
    let parent_of = |index: usize| {
        let entry = node_of(index)?;
        entry
            .node
            .parent_branch
            .as_ref()
            .and_then(|branch| index_by_branch.get(branch).copied())
            .filter(|&parent| {
                node_of(parent).is_some_and(|parent| parent.node.stack_id == entry.node.stack_id)
            })
    };
    // A stack sorts as one unit inside a section, anchored at its
    // shallowest member in that section; members filed elsewhere form
    // their own units there.
    let anchor_of = |index: usize| {
        let mut anchor = index;
        while let Some(parent) = parent_of(anchor) {
            if section_keys[parent] != section_keys[index] {
                break;
            }
            anchor = parent;
        }
        anchor
    };
    let mut buckets = BTreeMap::<String, Vec<usize>>::new();
    buckets.insert(INBOX.into(), Vec::new());
    for (index, key) in section_keys.iter().enumerate() {
        buckets.entry(key.clone()).or_default().push(index);
    }
    // Keep explicitly named empty sections available for the move/rename UI.
    let order = state["sectionsOrder"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    for section in order.iter().filter_map(Value::as_str) {
        if !section.starts_with('\0') {
            buckets.entry(section.to_owned()).or_default();
        }
    }
    let rank =
        |slug: &str| work_record_rank(parse_work_status(&state["slugs"][slug]["work"]).as_ref());
    let unit = |index: usize| {
        let anchor = anchor_of(index);
        let slug = board.rows[anchor].key.as_str();
        let status = if sort == UiSort::Status {
            board
                .rows
                .iter()
                .enumerate()
                .filter(|&(member, _)| {
                    section_keys[member] == section_keys[index] && anchor_of(member) == anchor
                })
                .map(|(_, row)| rank(&row.key))
                .min()
                .unwrap_or(99)
        } else {
            0
        };
        // An unordered row sorts before ordered ones, as in the TS sorter;
        // the first reorder writes explicit orders for the whole bucket.
        let order = state["slugs"][slug]["order"]
            .as_f64()
            .filter(|n| n.is_finite())
            .unwrap_or(f64::NEG_INFINITY);
        // Ties break on one stable identity per stack (its root branch),
        // as in the TS sorter, so tied units stay contiguous.
        // A unit left of a stack whose first row was removed keeps the
        // identity the store kept for it.
        let identity = state["slugs"][slug]["unitIdentity"]
            .as_str()
            .unwrap_or_else(|| node_of(anchor).map_or(slug, |entry| entry.node.stack_id.as_str()));
        (
            status,
            order,
            identity,
            node_of(index).map_or(0, |entry| entry.node.index),
        )
    };
    for (key, indices) in &mut buckets {
        if key == LANDED_SECTION {
            // A stable order: nothing a row owes, and no stored order or
            // creation anchor, moves it here.
            indices.sort_by(|&a, &b| {
                let (a, b) = (&board.rows[a], &board.rows[b]);
                landed_at_ms(board, a)
                    .cmp(&landed_at_ms(board, b))
                    .then_with(|| a.key.cmp(&b.key))
            });
            continue;
        }
        indices.sort_by(|&a, &b| {
            let a = unit(a);
            let b = unit(b);
            a.0.cmp(&b.0)
                .then_with(|| a.1.total_cmp(&b.1))
                .then_with(|| a.2.cmp(b.2))
                .then_with(|| a.3.cmp(&b.3))
        });
        // Apply a creation anchor to whole stack units. A child cannot be
        // emitted before a root that has moved below its own anchor.
        let mut units = Vec::new();
        let mut members_by_unit = BTreeMap::<usize, Vec<usize>>::new();
        for &index in indices.iter() {
            let root = anchor_of(index);
            if !members_by_unit.contains_key(&root) {
                units.push(root);
            }
            members_by_unit.entry(root).or_default().push(index);
        }
        // The units still at their creation position.
        let anchored: Vec<usize> = units
            .iter()
            .copied()
            .filter(|root| {
                let entry = &state["slugs"][&board.rows[*root].key];
                let started = members_by_unit[root].iter().any(|&member| {
                    parse_work_status(&state["slugs"][&board.rows[member].key]["work"]).is_some()
                });
                entry.get("creationAfter").is_some()
                    && !(entry["order"].is_number() || (sort == UiSort::Status && started))
            })
            .collect();
        // Where a row that is not drawn here would be: behind the last unit
        // drawn by its own order that does not sort after it. `status` is
        // `None` for a row whose status is no longer known.
        let behind = |status: Option<u8>, order: f64, identity: &str| {
            let mut placed = units
                .iter()
                .copied()
                .filter(|root| !anchored.contains(root));
            let by_order = |root: usize| {
                let key = unit(root);
                key.1.total_cmp(&order).then_with(|| key.2.cmp(identity))
            };
            match status {
                Some(status) => {
                    placed.rfind(|&root| unit(root).0.cmp(&status).then(by_order(root)).is_le())
                }
                None => placed
                    .filter(|&root| by_order(root).is_le())
                    .max_by(|&a, &b| {
                        let (a, b) = (unit(a), unit(b));
                        a.1.total_cmp(&b.1).then_with(|| a.2.cmp(b.2))
                    }),
            }
        };
        // Whether a row is filed in this section, drawn in it or not. A
        // removed worktree's record stays, and so does where it was filed.
        let stored_here = |slug: &str| {
            let stored = &state["slugs"][slug];
            let filed = stored["section"]
                .as_str()
                .filter(|name| !name.is_empty())
                .unwrap_or(INBOX);
            stored.is_object() && filed == key.as_str()
        };
        // What each anchored unit is drawn behind. The row it was created
        // after may no longer be drawn here. Archived, landed, being removed
        // or removed, it is still filed here; filed elsewhere or forgotten,
        // the store kept where it was (`creationSlot`). Such a row stands
        // in as a unit of no rows, where it would be drawn, so that the
        // units that followed it, and the ones that follow those, keep
        // their order among everything else when it goes.
        let first_stand_in = board.rows.len();
        let mut stand_ins: Vec<String> = Vec::new();
        let mut stand_in = |name: String| {
            let known = stand_ins.iter().position(|known| *known == name);
            let new = known.is_none();
            if new {
                stand_ins.push(name);
            }
            (first_stand_in + known.unwrap_or(stand_ins.len() - 1), new)
        };
        // Stand-ins drawn by their own stored order: status, order, name.
        let mut ordered_stand_ins: Vec<(usize, Option<u8>, f64, &str)> = Vec::new();
        // Anchored units and stand-ins: what they sort by among those that
        // follow the same unit, themselves, and what they follow.
        let mut links: Vec<((u8, &str), usize, Option<usize>)> = Vec::new();
        // Each with the record that says what it follows.
        let mut unresolved: Vec<(usize, &Value, (u8, &str))> = anchored
            .iter()
            .rev()
            .map(|&root| {
                let entry = &state["slugs"][&board.rows[root].key];
                // In the place of a row that left, it sorts as that row did
                // among the rows created after the same row.
                let name = entry["creationAs"].as_str().unwrap_or(unit(root).2);
                (root, entry, (unit(root).0, name))
            })
            .collect();
        while let Some((id, entry, sorts_by)) = unresolved.pop() {
            let named = entry["creationAfter"].as_str();
            let drawn = named.and_then(|after| {
                indices
                    .iter()
                    .copied()
                    .find(|&index| board.rows[index].key == after)
            });
            // The row it follows when that is not drawn here: its record,
            // or what the store kept of it, under a name of its own.
            let gone = if drawn.is_some() {
                None
            } else if let Some(after) = named.filter(|&after| stored_here(after)) {
                Some((after.to_owned(), after, &state["slugs"][after]))
            } else {
                let slot = &entry["creationSlot"];
                let key = slot["key"].as_str().unwrap_or_default();
                slot.is_object().then(|| (format!("\0{key}"), key, slot))
            };
            let follows = match (drawn, gone) {
                (Some(drawn), _) => Some(anchor_of(drawn)),
                (None, None) => None,
                (None, Some((known_as, key, gone))) => {
                    let (follows, new) = stand_in(known_as);
                    let work = parse_work_status(&gone["work"]);
                    // A removed stack's unit sorted by its first branch.
                    let name = gone["unitIdentity"].as_str().unwrap_or(key);
                    // A remote row's status is its host's to know: gone from
                    // there, it is not known here.
                    let status = if sort != UiSort::Status {
                        Some(0)
                    } else if gone.get("work").is_none()
                        && wt_core::is_remote_worktree_ledger_key(key)
                    {
                        None
                    } else {
                        Some(work_record_rank(work.as_ref()))
                    };
                    let anchored = gone.get("creationAfter").is_some()
                        && !gone["order"].is_number()
                        && !(sort == UiSort::Status && work.is_some());
                    if new && anchored {
                        let status = status.unwrap_or(work_record_rank(None));
                        unresolved.push((follows, gone, (status, name)));
                    } else if new {
                        ordered_stand_ins.push((
                            follows,
                            status,
                            gone["order"].as_f64().unwrap_or(f64::NEG_INFINITY),
                            name,
                        ));
                    }
                    Some(follows)
                }
            };
            if follows != Some(id) {
                links.push((sorts_by, id, follows));
            }
        }
        links.sort_by(|a, b| a.0.cmp(&b.0));
        let positions: Vec<(usize, Option<usize>)> = links
            .iter()
            .map(|&(_, id, follows)| (id, follows))
            .collect();
        // Each ordered stand-in goes behind the unit it would be drawn
        // behind, several of them there in their own order.
        let mut behind_units: Vec<(Option<usize>, f64, &str, usize)> = ordered_stand_ins
            .iter()
            .map(|&(id, status, order, name)| (behind(status, order, name), order, name, id))
            .collect();
        behind_units.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.2.cmp(b.2)));
        let stand_ins_behind = |unit: Option<usize>| {
            behind_units
                .iter()
                .filter(move |stand_in| stand_in.0 == unit)
                .map(|stand_in| stand_in.3)
        };
        let mut with_stand_ins: Vec<usize> = stand_ins_behind(None).collect();
        for &root in &units {
            with_stand_ins.push(root);
            with_stand_ins.extend(stand_ins_behind(Some(root)));
        }
        let mut units = with_stand_ins;
        apply_creation_positions(&mut units, &positions);
        let mut placed = Vec::with_capacity(indices.len());
        for root in units {
            placed.extend(members_by_unit.get(&root).into_iter().flatten().copied());
        }
        *indices = placed;
    }
    board.section_order = wt_ops::commands::section::manual_sections(state);
    let folded = state["foldedSections"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    board.sections = buckets
        .into_iter()
        .map(|(key, rows)| BoardSection {
            folded: folded.iter().any(|value| value.as_str() == Some(&key)),
            title: section_title(&key),
            key,
            rows,
        })
        .collect();
    board.sections.sort_by(|a, b| {
        let position = |section: &BoardSection| {
            if section.key == ARCHIVED {
                usize::MAX
            } else if section.key == REMOVING {
                usize::MAX - 1
            } else if section.key == LANDED_SECTION {
                // After every stored section and the Inbox.
                usize::MAX - 2
            } else {
                order
                    .iter()
                    .position(|key| key.as_str() == Some(&section.key))
                    .unwrap_or(order.len())
            }
        };
        position(a)
            .cmp(&position(b))
            .then_with(|| a.key.cmp(&b.key))
    });
    for section in &board.sections {
        // Only stacked rows draw a rail, and only to neighbours drawn in the
        // same contiguous group.
        let members = section
            .rows
            .iter()
            .filter(|&&index| node_of(index).is_some())
            .map(|&index| {
                let row = &board.rows[index];
                SpineMember {
                    key: row.key.clone(),
                    branch: branch_key(row, &row.branch),
                    parent_branch: node_of(index)
                        .and_then(|entry| entry.node.parent_branch.clone()),
                }
            })
            .collect::<Vec<_>>();
        let spine = spine_layout(&members);
        for &index in &section.rows {
            let lane = node_of(index).map_or(0, |entry| entry.node.lane.min(255) as u8);
            let split = parent_of(index)
                .filter(|&parent| section_keys[parent] != section_keys[index])
                .map(|parent| section_title(&section_keys[parent]));
            let parent = parent_of(index).map(|parent| board.rows[parent].key.clone());
            let row = &mut board.rows[index];
            row.stack_lane = lane;
            row.split_parent_section = split;
            row.stack_parent = parent;
            row.stack_index = node_of(index).map_or(0, |entry| entry.node.index);
            row.stack_prefix = spine
                .get(&row.key)
                .map(|cell| {
                    (0..=cell.col.min(16))
                        .map(|column| {
                            if column == cell.col {
                                cell.glyph
                            } else if cell.trail.get(column).copied().unwrap_or(false) {
                                '│'
                            } else {
                                ' '
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();
        }
    }
}

fn section_title(key: &str) -> String {
    match key {
        INBOX => "Inbox".to_owned(),
        ARCHIVED => "Archived".to_owned(),
        REMOVING => "Removing".to_owned(),
        LANDED_SECTION => wt_tui::LANDED_TITLE.to_owned(),
        _ => wt_core::sanitize_terminal_text(key),
    }
}

/// Identical branch names on different machines do not form a shared stack.
pub fn branch_key(row: &wt_tui::BoardRow, branch: &str) -> String {
    if wt_core::is_remote_worktree_ledger_key(&row.key) {
        let namespace = row
            .key
            .rsplit_once('/')
            .map(|(prefix, _)| prefix)
            .unwrap_or(&row.key);
        format!("{namespace}/{branch}")
    } else {
        branch.to_owned()
    }
}

/// Expand the section containing a completed creation before its selection is
/// consumed. Inventory publication, not mutation completion, proves visibility.
pub fn section_for(board: &Board, key: &str) -> Option<String> {
    let index = board.rows.iter().position(|row| row.key == key)?;
    board
        .sections
        .iter()
        .find(|section| section.rows.contains(&index))
        .map(|section| section.key.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wt_tui::{BoardRow, OwedBlock};

    fn row(slug: &str) -> BoardRow {
        BoardRow {
            slug: slug.into(),
            key: slug.into(),
            branch: slug.into(),
            title: slug.into(),
            ..BoardRow::default()
        }
    }

    #[test]
    fn creation_stays_after_the_selected_stack_until_status_or_manual_order_changes() {
        let mut board = Board {
            rows: vec![row("new"), row("root"), row("child"), row("other")],
            ..Default::default()
        };
        let mut state = json!({"sectionsOrder":["Batch"],"slugs":{
            "root":{"section":"Batch","order":1,"work":{"state":"working","at":"now"}},
            "child":{"section":"Batch","baseBranch":"root"},
            "other":{"section":"Batch","order":2,"work":{"state":"working","at":"now"}},
            "new":{"section":"Batch","order":null,"creationAfter":"root"}
        }});
        let keys = |board: &Board| {
            board
                .sections
                .iter()
                .find(|section| section.key == "Batch")
                .unwrap()
                .rows
                .iter()
                .map(|&index| board.rows[index].key.as_str())
                .collect::<Vec<_>>()
                .join(",")
        };
        prepare(&mut board, &state, "main", UiSort::Status);
        assert_eq!(keys(&board), "root,child,new,other");
        state["slugs"]["new"]["work"] = json!({"state":"todo","at":"now"});
        prepare(&mut board, &state, "main", UiSort::Status);
        assert_eq!(keys(&board), "root,child,other,new");
        prepare(&mut board, &state, "main", UiSort::Manual);
        assert_eq!(keys(&board), "root,child,new,other");
        state["slugs"]["new"]["order"] = json!(3);
        prepare(&mut board, &state, "main", UiSort::Manual);
        assert_eq!(keys(&board), "root,child,other,new");
    }

    #[test]
    fn a_new_anchored_stack_keeps_its_children_together() {
        let mut board = Board {
            rows: vec![
                row("new-root"),
                row("new-child"),
                row("anchor"),
                row("other"),
            ],
            ..Default::default()
        };
        let mut state = json!({"sectionsOrder":["Batch"],"slugs":{
            "anchor":{"section":"Batch","order":1},
            "other":{"section":"Batch","order":2},
            "new-root":{"section":"Batch","order":null,"creationAfter":"anchor"},
            "new-child":{"section":"Batch","baseBranch":"new-root","order":null,"creationAfter":"new-root"}
        }});
        prepare(&mut board, &state, "main", UiSort::Manual);
        let section = board
            .sections
            .iter()
            .find(|section| section.key == "Batch")
            .unwrap();
        let keys = section
            .rows
            .iter()
            .map(|&index| board.rows[index].key.as_str())
            .collect::<Vec<_>>();
        assert_eq!(keys, ["anchor", "new-root", "new-child", "other"]);
        state["slugs"]["new-child"]["work"] = json!({"state":"working","at":"now"});
        prepare(&mut board, &state, "main", UiSort::Status);
        let section = board
            .sections
            .iter()
            .find(|section| section.key == "Batch")
            .unwrap();
        let keys = section
            .rows
            .iter()
            .map(|&index| board.rows[index].key.as_str())
            .collect::<Vec<_>>();
        assert_eq!(keys, ["new-root", "new-child", "anchor", "other"]);
    }

    #[test]
    fn creation_positions_handle_empty_sections_chains_and_invalid_cycles() {
        let mut rows = vec![0, 1, 2, 3];
        apply_creation_positions(&mut rows, &[(1, Some(0)), (2, Some(1)), (3, None)]);
        assert_eq!(rows, [3, 0, 1, 2]);
        let mut rows = vec![0, 1, 2];
        apply_creation_positions(&mut rows, &[(0, Some(1)), (1, Some(0))]);
        assert_eq!(
            rows.iter()
                .copied()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            3
        );
    }

    #[test]
    fn stacks_sort_as_a_unit_and_a_split_member_points_at_its_parents_section() {
        let mut board = Board {
            rows: vec![row("child"), row("root"), row("other"), row("split")],
            ..Board::default()
        };
        let state = json!({"slugs": {
            "root": {"section": "Batch", "baseBranch": "main", "order": 1},
            "child": {"section": "Batch", "baseBranch": "root"},
            "other": {"section": "Batch", "order": 0},
            "split": {"baseBranch": "root"}
        }, "sectionsOrder": [INBOX, "Batch"], "foldedSections": ["Batch"]});
        prepare(&mut board, &state, "main", UiSort::Manual);
        let batch = board
            .sections
            .iter()
            .find(|section| section.key == "Batch")
            .unwrap();
        assert!(batch.folded);
        // `other` sorts first by order; the stack follows as one unit, root
        // before child.
        assert_eq!(batch.rows, [2, 1, 0]);
        assert_eq!(board.rows[1].stack_prefix, "┌");
        assert_eq!(board.rows[0].stack_prefix, "└");
        assert_eq!(section_for(&board, "child").as_deref(), Some("Batch"));
        // A member filed apart from its parent stays where it was put, draws
        // no rail, and names the section its parent went to.
        let inbox = board.sections.iter().find(|s| s.key == INBOX).unwrap();
        assert_eq!(inbox.rows, [3]);
        assert_eq!(board.rows[3].stack_prefix, "");
        assert_eq!(board.rows[3].split_parent_section.as_deref(), Some("Batch"));
        assert_eq!(board.rows[0].split_parent_section, None);
        board.rows[1].title = "Renamed to sort first".into();
        prepare(&mut board, &state, "main", UiSort::Manual);
        assert_eq!(
            board
                .sections
                .iter()
                .find(|section| section.key == "Batch")
                .unwrap()
                .rows,
            [2, 1, 0]
        );
    }

    fn landed(slug: &str) -> BoardRow {
        let mut row = row(slug);
        row.git.landed_on = Some(wt_tui::LandingKind::Base);
        row
    }

    fn owing(
        slug: &str,
        phase: wt_core::OwedPhase,
        stage: Option<wt_core::VerifyStage>,
    ) -> BoardRow {
        let mut row = landed(slug);
        row.owed_phase = Some(phase);
        if phase != wt_core::OwedPhase::Done {
            let state = if phase == wt_core::OwedPhase::Due {
                wt_core::OwedState::Due
            } else {
                wt_core::OwedState::Waiting
            };
            let mut item = wt_tui::OwedView::new(wt_core::OwedKind::Step, state, "x");
            item.stage = stage;
            row.owes = vec![item];
        }
        // As the owed overlay files it.
        row.owed_block = Some(match (phase, stage) {
            (wt_core::OwedPhase::Due, _) => OwedBlock::Due,
            (wt_core::OwedPhase::Done, _) => OwedBlock::Done,
            (_, Some(wt_core::VerifyStage::Staging)) => OwedBlock::WaitingStaging,
            (_, Some(wt_core::VerifyStage::Production)) => OwedBlock::WaitingProduction,
            (_, None) => OwedBlock::WaitingOther,
        });
        row
    }

    /// A landed row under Scheduled whose next item comes due at `at_ms`.
    fn scheduled(slug: &str, at_ms: u64) -> BoardRow {
        let mut row = owing(slug, wt_core::OwedPhase::Waiting, None);
        row.owes[0].next_at_ms = Some(at_ms);
        row.owed_block = Some(OwedBlock::Scheduled);
        row
    }

    fn layout(board: &Board) -> Vec<(String, String, Vec<String>, bool)> {
        board
            .sections
            .iter()
            .filter(|section| section.drawn())
            .map(|section| {
                (
                    section.key.clone(),
                    section.title.clone(),
                    section
                        .rows
                        .iter()
                        .map(|&index| board.rows[index].key.clone())
                        .collect(),
                    section.folded,
                )
            })
            .collect()
    }

    #[test]
    fn owed_sections_show_landed_rows_by_what_they_owe_and_leave_the_stored_layout() {
        use wt_core::{OwedPhase, VerifyStage};
        let mut archived = owing("archived", OwedPhase::Due, None);
        archived.archived = true;
        let mut remote = owing("remote", OwedPhase::Due, None);
        remote.key = wt_core::remote_worktree_ledger_key("builder", "remote");
        remote.host = Some("builder".into());
        let mut removing = owing("removing", OwedPhase::Done, None);
        removing.removal = Some(wt_tui::RemovalView::default());
        // Landed, but the owed overlay has not prepared it yet.
        let unprepared = landed("unprepared");
        let mut board = Board {
            rows: vec![
                row("open"),
                owing("done", OwedPhase::Done, None),
                owing("wait-other", OwedPhase::Waiting, None),
                owing(
                    "wait-prod",
                    OwedPhase::Waiting,
                    Some(VerifyStage::Production),
                ),
                owing(
                    "wait-staging",
                    OwedPhase::Waiting,
                    Some(VerifyStage::Staging),
                ),
                owing("due", OwedPhase::Due, Some(VerifyStage::Staging)),
                scheduled("later", 2_000),
                scheduled("sooner", 1_000),
                archived,
                remote,
                removing,
                unprepared,
            ],
            ..Board::default()
        };
        let state = json!({"slugs": {
            "open": {"section": "Batch", "order": 1},
            "done": {"section": "Batch", "order": 2},
            "due": {"section": "Batch", "order": 3},
            "wait-staging": {"section": "Verify in Staging"},
        }, "sectionsOrder": ["Batch", "Verify in Staging", "Empty"],
           "foldedSections": [LANDED_SECTION]});
        let before = state.clone();
        prepare(&mut board, &state, "main", UiSort::Manual);
        let stored = layout(&board);
        let keys = |shown: &[(String, String, Vec<String>, bool)]| -> Vec<(String, Vec<String>)> {
            shown
                .iter()
                .map(|(_, title, rows, _)| (title.clone(), rows.clone()))
                .collect()
        };
        let named = |pairs: &[(&str, &[&str])]| -> Vec<(String, Vec<String>)> {
            pairs
                .iter()
                .map(|(title, rows)| {
                    (
                        (*title).to_owned(),
                        rows.iter().map(|row| (*row).to_owned()).collect(),
                    )
                })
                .collect()
        };
        assert_eq!(
            keys(&stored),
            named(&[
                ("Batch", &["open", "done", "due"]),
                ("Verify in Staging", &["wait-staging"]),
                ("Empty", &[]),
                (
                    "Inbox",
                    &[
                        "@remote/builder/remote",
                        "later",
                        "sooner",
                        "unprepared",
                        "wait-other",
                        "wait-prod"
                    ]
                ),
                ("Removing", &["removing"]),
                ("Archived", &["archived"]),
            ])
        );
        // On: only landed local rows on the board move, into the one Landed
        // block after the stored sections and the Inbox. With no landing
        // time known they are drawn by slug.
        board.display.owed_sections = true;
        prepare(&mut board, &state, "main", UiSort::Manual);
        let shown = layout(&board);
        assert_eq!(
            keys(&shown),
            named(&[
                ("Batch", &["open"]),
                ("Verify in Staging", &[]),
                ("Empty", &[]),
                ("Inbox", &["@remote/builder/remote", "unprepared"]),
                (
                    "Landed",
                    &[
                        "done",
                        "due",
                        "later",
                        "removing",
                        "sooner",
                        "wait-other",
                        "wait-prod",
                        "wait-staging"
                    ]
                ),
                ("Archived", &["archived"]),
            ])
        );
        let landed = shown.iter().find(|section| section.1 == "Landed").unwrap();
        assert_eq!((landed.0.as_str(), landed.3), (LANDED_SECTION, true));
        assert_eq!(
            board
                .sections
                .iter()
                .filter(|section| section.owed_block())
                .count(),
            1
        );
        // A row drawn in the block is still filed where it was, and still
        // counts as its section's: deleting "Batch" is witnessed by all
        // three rows stored there, not only the one drawn in it.
        assert_eq!(board.section_witness("Batch"), ["done", "due", "open"]);
        assert_eq!(board.section_members("Verify in Staging").len(), 1);
        let due = board.rows.iter().find(|row| row.key == "due").unwrap();
        assert_eq!(due.filed_in.as_deref(), Some("Batch"));
        // Nothing was written, the manual sections are still what a move
        // may target, and off again is the layout it was.
        assert_eq!(state, before);
        assert_eq!(board.section_order, ["Batch", "Verify in Staging", "Empty"]);
        board.display.owed_sections = false;
        prepare(&mut board, &state, "main", UiSort::Manual);
        assert_eq!(layout(&board), stored);
        // No landed row, no block.
        board.display.owed_sections = true;
        board.rows.retain(|row| row.key == "open");
        prepare(&mut board, &state, "main", UiSort::Manual);
        assert_eq!(
            keys(&layout(&board)),
            named(&[
                ("Batch", &["open"]),
                ("Verify in Staging", &[]),
                ("Empty", &[]),
            ])
        );
    }

    #[test]
    fn landed_rows_are_drawn_oldest_landing_first_and_never_move_with_what_they_owe() {
        use wt_core::{LandedOn, OwedPhase, VerifyStage, WorktreeEvent, WorktreeEventKind};
        let landing = |at_ms: u64, on: LandedOn| WorktreeEvent {
            at_ms,
            event: WorktreeEventKind::Landed {
                on,
                head: None,
                landed_at_ms: None,
            },
        };
        let mut committed = owing("committed", OwedPhase::Waiting, None);
        committed.git.last_commit_ms = Some(2_500);
        // Neither a landing nor a commit time: its creation orders it.
        let mut created = owing("created", OwedPhase::Done, None);
        created.git.created_ms = Some(10);
        created.git.last_commit_ms = None;
        let mut board = Board {
            rows: vec![
                owing("newest", OwedPhase::Due, None),
                owing("b-tied", OwedPhase::Done, None),
                owing("a-tied", OwedPhase::Waiting, Some(VerifyStage::Staging)),
                committed,
                owing("oldest", OwedPhase::Waiting, Some(VerifyStage::Production)),
                created,
                owing("unknown", OwedPhase::Due, None),
            ],
            ..Board::default()
        };
        board.display.owed_sections = true;
        board.row_history = [
            ("newest", vec![landing(4_000, LandedOn::Base)]),
            ("a-tied", vec![landing(3_000, LandedOn::Base)]),
            ("b-tied", vec![landing(3_000, LandedOn::Base)]),
            // Its first landing counts, not the later release.
            (
                "oldest",
                vec![
                    landing(1_000, LandedOn::Base),
                    landing(9_000, LandedOn::Production),
                ],
            ),
        ]
        .into_iter()
        .map(|(key, events)| (key.to_owned(), events))
        .collect();
        // A stored order or status sort does not reach into the block.
        let state = json!({"slugs": {"newest": {"order": 1}, "oldest": {"order": 9}}});
        let drawn = |board: &Board| -> Vec<String> {
            board
                .sections
                .iter()
                .find(|section| section.key == LANDED_SECTION)
                .unwrap()
                .rows
                .iter()
                .map(|&index| board.rows[index].key.clone())
                .collect()
        };
        prepare(&mut board, &state, "main", UiSort::Manual);
        let order = [
            "created",
            "oldest",
            "committed",
            "a-tied",
            "b-tied",
            "newest",
            "unknown",
        ];
        assert_eq!(drawn(&board), order);
        // What each row owes changes; none of them moves.
        for row in &mut board.rows {
            let (phase, block) = match row.owed_phase {
                Some(OwedPhase::Due) => (OwedPhase::Done, wt_tui::OwedBlock::Done),
                Some(OwedPhase::Done) => (OwedPhase::Waiting, wt_tui::OwedBlock::Scheduled),
                _ => (OwedPhase::Due, wt_tui::OwedBlock::Due),
            };
            row.owed_phase = Some(phase);
            row.owed_block = Some(block);
            for (index, item) in row.owes.iter_mut().enumerate() {
                item.next_at_ms = Some(50 - index as u64);
                item.due_at_ms = Some(index as u64);
            }
        }
        prepare(&mut board, &state, "main", UiSort::Status);
        assert_eq!(drawn(&board), order);
        // A new landing appends at the bottom.
        let mut fresh = owing("a-fresh", OwedPhase::Due, None);
        fresh.git.last_commit_ms = Some(5_000);
        board.rows.insert(0, fresh);
        prepare(&mut board, &state, "main", UiSort::Manual);
        let mut appended = vec![
            "created",
            "oldest",
            "committed",
            "a-tied",
            "b-tied",
            "newest",
        ];
        appended.extend(["a-fresh", "unknown"]);
        assert_eq!(drawn(&board), appended);
    }

    /// Several merges seen by one refresh share the time wt saw them. The
    /// time each recorded as its merge orders them; an event from before
    /// that was recorded keeps the time it was seen.
    #[test]
    fn landings_seen_in_one_refresh_are_ordered_by_when_each_merged() {
        use wt_core::{LandedOn, OwedPhase, WorktreeEvent, WorktreeEventKind};
        const SEEN: u64 = 9_000;
        let merged = [
            ("f-first", 1_000),
            ("e-second", 2_000),
            ("d-third", 3_000),
            ("c-fourth", 4_000),
            ("b-fifth", 5_000),
            ("a-sixth", 6_000),
        ];
        let mut board = Board {
            rows: merged
                .iter()
                .rev()
                .map(|(slug, _)| owing(slug, OwedPhase::Waiting, None))
                .collect(),
            ..Board::default()
        };
        board.display.owed_sections = true;
        let landing = |seen: u64, landed_at_ms: Option<u64>| {
            vec![WorktreeEvent {
                at_ms: seen,
                event: WorktreeEventKind::Landed {
                    on: LandedOn::Base,
                    head: None,
                    landed_at_ms,
                },
            }]
        };
        board.row_history = merged
            .iter()
            .map(|(slug, at)| ((*slug).to_owned(), landing(SEEN, Some(*at))))
            .collect();
        let drawn = |board: &Board| -> Vec<String> {
            board
                .sections
                .iter()
                .find(|section| section.key == LANDED_SECTION)
                .unwrap()
                .rows
                .iter()
                .map(|&index| board.rows[index].key.clone())
                .collect()
        };
        let state = json!({"slugs": {}});
        prepare(&mut board, &state, "main", UiSort::Manual);
        assert_eq!(drawn(&board), merged.map(|(slug, _)| slug));
        // A later refresh, and a later landing in production, move nothing.
        board
            .row_history
            .get_mut("a-sixth")
            .unwrap()
            .push(WorktreeEvent {
                at_ms: SEEN + 1,
                event: WorktreeEventKind::Landed {
                    on: LandedOn::Production,
                    head: None,
                    landed_at_ms: Some(500),
                },
            });
        prepare(&mut board, &state, "main", UiSort::Status);
        assert_eq!(drawn(&board), merged.map(|(slug, _)| slug));
        // Old events have no merge time: the time seen orders them among
        // the rest, and then the slug, as before.
        for slug in ["c-fourth", "d-third"] {
            board
                .row_history
                .insert(slug.to_owned(), landing(3_500, None));
        }
        prepare(&mut board, &state, "main", UiSort::Manual);
        assert_eq!(
            drawn(&board),
            [
                "f-first", "e-second", "c-fourth", "d-third", "b-fifth", "a-sixth"
            ]
        );
        // The same merge time falls back to when each was seen.
        board
            .row_history
            .insert("b-fifth".to_owned(), landing(SEEN - 1, Some(6_000)));
        board
            .row_history
            .insert("a-sixth".to_owned(), landing(SEEN, Some(6_000)));
        prepare(&mut board, &state, "main", UiSort::Manual);
        assert_eq!(drawn(&board)[4..], ["b-fifth", "a-sixth"]);
    }

    #[test]
    fn a_landed_head_is_its_own_unit_and_what_was_stacked_on_it_stays_a_stack() {
        let section_rows = |board: &Board, key: &str| -> Vec<String> {
            board
                .sections
                .iter()
                .find(|section| section.key == key)
                .map(|section| {
                    section
                        .rows
                        .iter()
                        .map(|&index| board.rows[index].key.clone())
                        .collect()
                })
                .unwrap_or_default()
        };
        let state = json!({"slugs": {
            "head": {"section": "Batch", "order": 2},
            "child": {"section": "Batch", "baseBranch": "head"},
            "grandchild": {"section": "Batch", "baseBranch": "child"},
            "other": {"section": "Batch", "order": 1},
        }, "sectionsOrder": ["Batch"]});
        let mut board = Board {
            rows: vec![row("grandchild"), row("child"), row("head"), row("other")],
            ..Board::default()
        };
        prepare(&mut board, &state, "main", UiSort::Manual);
        // Unlanded: one stack, sorted as a unit at its head's position.
        assert_eq!(
            section_rows(&board, "Batch"),
            ["other", "head", "child", "grandchild"]
        );
        assert_eq!(board.rows[2].stack_prefix, "┌");
        // Landed: the head sorts by its own order with no rail; its
        // children are a stack of their own, at their own position.
        board.rows[2].git.landed_on = Some(wt_tui::LandingKind::Base);
        prepare(&mut board, &state, "main", UiSort::Manual);
        assert_eq!(
            section_rows(&board, "Batch"),
            ["child", "grandchild", "other", "head"]
        );
        assert_eq!(board.rows[2].stack_prefix, "");
        assert_eq!(board.rows[1].stack_prefix, "┌");
        assert_eq!(board.rows[0].stack_prefix, "└");
        assert_eq!(board.rows[1].split_parent_section, None);
        // With the Landed block, only the landed head leaves the section.
        board.display.owed_sections = true;
        board.rows[2].owed_phase = Some(wt_core::OwedPhase::Done);
        board.rows[2].owed_block = Some(wt_tui::OwedBlock::Done);
        prepare(&mut board, &state, "main", UiSort::Manual);
        assert_eq!(
            section_rows(&board, "Batch"),
            ["child", "grandchild", "other"]
        );
        assert_eq!(section_rows(&board, LANDED_SECTION), ["head"]);
    }

    #[test]
    fn archived_rows_are_separate_and_a_trunk_fork_is_not_a_stack() {
        let mut archived = row("old");
        archived.archived = true;
        let mut board = Board {
            rows: vec![archived, row("new")],
            ..Board::default()
        };
        prepare(
            &mut board,
            &json!({"slugs":{"new":{"baseBranch":"main"}}}),
            "main",
            UiSort::Status,
        );
        assert_eq!(
            board
                .sections
                .iter()
                .map(|section| section.key.as_str())
                .collect::<Vec<_>>(),
            [INBOX, ARCHIVED]
        );
        assert_eq!(board.sections[0].rows, [1]);
        assert_eq!(board.sections[1].rows, [0]);
    }

    #[test]
    fn removals_follow_manual_sections_and_precede_archived() {
        let mut removing = row("removing");
        removing.archived = true;
        removing.removal = Some(wt_tui::RemovalView::default());
        let mut failed = row("failed");
        failed.removal = Some(wt_tui::RemovalView {
            error: Some("denied".into()),
            ..Default::default()
        });
        let mut archived = row("archived");
        archived.archived = true;
        let mut board = Board {
            rows: vec![row("active"), removing, failed, archived],
            ..Board::default()
        };
        let state = json!({"slugs": {"removing": {"section": "Batch"}}, "sectionsOrder": [ARCHIVED, REMOVING, "Batch"]});
        prepare(&mut board, &state, "main", UiSort::Status);
        let keys: Vec<_> = board
            .sections
            .iter()
            .map(|section| section.key.as_str())
            .collect();
        assert_eq!(keys, ["Batch", INBOX, REMOVING, ARCHIVED]);
        assert_eq!(board.sections[2].rows, [2, 1]);
        assert!(!board.sections[2].folded);
    }

    /// With the Landed block there is no Removing section: a row being
    /// removed stays where it was drawn, also once the removal has archived
    /// it and deleted its checkout, which takes its Git facts with it.
    #[test]
    fn with_the_landed_block_a_row_being_removed_stays_where_it_was_drawn() {
        // Landed when the removal began; the checkout is gone already.
        let mut gone = row("gone");
        gone.archived = true;
        gone.removal = Some(wt_tui::RemovalView {
            checkout_absent: true,
            landed: true,
            ..Default::default()
        });
        // Not landed, filed in a section; the removal archived it.
        let mut filed = row("filed");
        filed.archived = true;
        filed.removal = Some(wt_tui::RemovalView::default());
        // Archived before its removal.
        let mut old = row("old");
        old.archived = true;
        old.removal = Some(wt_tui::RemovalView {
            was_archived: true,
            ..Default::default()
        });
        let mut failed = row("failed");
        failed.removal = Some(wt_tui::RemovalView {
            error: Some("denied".into()),
            ..Default::default()
        });
        let mut board = Board {
            rows: vec![
                gone,
                filed,
                old,
                failed,
                owing("kept", wt_core::OwedPhase::Done, None),
            ],
            ..Board::default()
        };
        board.display.owed_sections = true;
        let state = json!({"slugs": {"filed": {"section": "Batch"}}, "sectionsOrder": ["Batch"]});
        prepare(&mut board, &state, "main", UiSort::Manual);
        let shown: Vec<(String, Vec<String>)> = layout(&board)
            .into_iter()
            .map(|(_, title, rows, _)| (title, rows))
            .collect();
        let named = |title: &str, rows: &[&str]| {
            (
                title.to_owned(),
                rows.iter().map(|row| (*row).to_owned()).collect::<Vec<_>>(),
            )
        };
        assert_eq!(
            shown,
            [
                named("Batch", &["filed"]),
                named("Inbox", &["failed"]),
                named("Landed", &["gone", "kept"]),
                named("Archived", &["old"]),
            ]
        );
    }
}
