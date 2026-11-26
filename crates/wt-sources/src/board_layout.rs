//! Pure preparation of section placement and stack rails from the current
//! inventory and durable layout. No separate stack membership is persisted.
use std::collections::BTreeMap;

use serde_json::Value;
use wt_config::UiSort;
use wt_core::{
    ChainMember, SpineMember, build_display_stack_index, parse_work_status, spine_layout,
    work_record_rank,
};
use wt_tui::{Board, BoardRow, BoardSection, OwedBlock};

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

/// The block `[ui] owed_sections` shows a row in instead of its stored
/// section: only a local row on the board whose own work has landed, whose
/// only remaining meaning is what it owes. `None` before the owed overlay
/// has prepared the row.
fn owed_block(board: &Board, row: &BoardRow) -> Option<OwedBlock> {
    if !board.display.owed_sections
        || row.host.is_some()
        || row.is_main
        || row.pending_creation
        || row.git.landed_on.is_none()
    {
        return None;
    }
    row.owed_phase.map(|phase| OwedBlock::of(phase, &row.owes))
}

/// Lay the board out from the stored placement. With
/// `board.display.owed_sections`, landed rows are drawn in derived blocks;
/// nothing stored is read differently or written, so the option can be
/// turned off again.
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
            if row.removal.is_some() {
                REMOVING.to_owned()
            } else if row.archived {
                ARCHIVED.to_owned()
            } else {
                section.map_or_else(|| INBOX.to_owned(), str::to_owned)
            }
        })
        .collect();
    for (index, row) in board.rows.iter_mut().enumerate() {
        let entry = &state["slugs"][&row.key];
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
    // Where each row is drawn: its stored section, or the block derived
    // from what it owes.
    let section_keys: Vec<String> = board
        .rows
        .iter()
        .zip(stored_keys)
        .map(|(row, stored)| match owed_block(board, row) {
            Some(block) if stored != REMOVING && stored != ARCHIVED => block.key().to_owned(),
            _ => stored,
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
        let identity = node_of(anchor).map_or(slug, |entry| entry.node.stack_id.as_str());
        (
            status,
            order,
            identity,
            node_of(index).map_or(0, |entry| entry.node.index),
        )
    };
    for indices in buckets.values_mut() {
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
        let mut positions = Vec::new();
        for &root in &units {
            let entry = &state["slugs"][&board.rows[root].key];
            let Some(after) = entry.get("creationAfter") else {
                continue;
            };
            let started = members_by_unit[&root].iter().any(|&member| {
                parse_work_status(&state["slugs"][&board.rows[member].key]["work"]).is_some()
            });
            if entry["order"].is_number() || (sort == UiSort::Status && started) {
                continue;
            }
            let anchor = after.as_str().and_then(|key| {
                indices
                    .iter()
                    .copied()
                    .find(|&index| board.rows[index].key == key)
                    .map(anchor_of)
            });
            if anchor != Some(root) {
                positions.push((root, anchor));
            }
        }
        apply_creation_positions(&mut units, &positions);
        let mut placed = Vec::with_capacity(indices.len());
        for root in units {
            placed.extend(members_by_unit[&root].iter().copied());
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
            } else if let Some(block) = OwedBlock::from_key(&section.key) {
                // After every stored section and the Inbox, most urgent first.
                usize::MAX - 2 - (OwedBlock::ALL.len() - 1 - block as usize)
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
            let row = &mut board.rows[index];
            row.stack_lane = lane;
            row.split_parent_section = split;
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
        _ => OwedBlock::from_key(key).map_or_else(
            || wt_core::sanitize_terminal_text(key),
            |block| block.title().to_owned(),
        ),
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
    use wt_tui::BoardRow;

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
        use wt_tui::OwedBlock;
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
           "foldedSections": [OwedBlock::Done.key()]});
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
                        "unprepared",
                        "wait-other",
                        "wait-prod"
                    ]
                ),
                ("Removing", &["removing"]),
                ("Archived", &["archived"]),
            ])
        );
        // On: only landed local rows on the board move, into blocks after
        // the stored sections and the Inbox, most urgent first. Empty
        // blocks are not drawn; they fold like any section.
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
                ("Due", &["due"]),
                ("Waiting for staging", &["wait-staging"]),
                ("Waiting for production", &["wait-prod"]),
                ("Waiting", &["wait-other"]),
                ("Done", &["done"]),
                ("Removing", &["removing"]),
                ("Archived", &["archived"]),
            ])
        );
        let done = shown.iter().find(|section| section.1 == "Done").unwrap();
        assert_eq!((done.0.as_str(), done.3), (OwedBlock::Done.key(), true));
        assert!(
            board
                .sections
                .iter()
                .filter(|section| section.owed_block())
                .all(|section| section.key.starts_with('\0'))
        );
        // A row drawn in a block is still filed where it was, and still
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
        // No landed row owes anything in a block that was never needed.
        board.display.owed_sections = true;
        board
            .rows
            .retain(|row| row.key == "open" || row.key == "done");
        prepare(&mut board, &state, "main", UiSort::Manual);
        assert_eq!(
            keys(&layout(&board)),
            named(&[
                ("Batch", &["open"]),
                ("Verify in Staging", &[]),
                ("Empty", &[]),
                ("Done", &["done"]),
            ])
        );
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
        // Grouped by what it owes, only the landed head leaves the section.
        board.display.owed_sections = true;
        board.rows[2].owed_phase = Some(wt_core::OwedPhase::Done);
        prepare(&mut board, &state, "main", UiSort::Manual);
        assert_eq!(
            section_rows(&board, "Batch"),
            ["child", "grandchild", "other"]
        );
        assert_eq!(
            section_rows(&board, wt_tui::OwedBlock::Done.key()),
            ["head"]
        );
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
}
