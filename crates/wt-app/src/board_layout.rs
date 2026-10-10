//! Pure preparation of section placement and stack rails from the current
//! inventory and durable layout. No separate stack membership is persisted.
use std::collections::BTreeMap;

use serde_json::Value;
use wt_config::UiSort;
use wt_core::{
    ChainMember, SpineMember, WORK_STATES, WorkRisk, build_stack_index, parse_work_status,
    spine_layout, work_record_rank,
};
use wt_tui::{Board, BoardSection, SectionRollup, WorkRiskCount, WorkStateCount};

pub const INBOX: &str = "\0inbox";
pub const ARCHIVED: &str = "\0archived";

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
    let stacks = build_stack_index(&members, trunk);
    let nodes: Vec<_> = board
        .rows
        .iter()
        .map(|row| stacks.by_branch.get(&branch_key(row, &row.branch)).cloned())
        .collect();
    let node_of = |index: usize| nodes[index].as_ref();
    // Each row lives in its own stored section. Moving a stack writes every
    // member's placement, and `--only` deliberately splits one off, so the
    // read side must not pull members back to the root.
    let section_keys: Vec<String> = board
        .rows
        .iter()
        .map(|row| {
            let section = state["slugs"][&row.key]["section"]
                .as_str()
                .filter(|name| !name.is_empty());
            if row.archived {
                ARCHIVED.to_owned()
            } else {
                section.map_or_else(|| INBOX.to_owned(), str::to_owned)
            }
        })
        .collect();
    let index_by_branch: BTreeMap<String, usize> = board
        .rows
        .iter()
        .enumerate()
        .map(|(index, row)| (branch_key(row, &row.branch), index))
        .collect();
    let parent_of = |index: usize| {
        node_of(index)
            .and_then(|entry| entry.node.parent_branch.as_ref())
            .and_then(|branch| index_by_branch.get(branch).copied())
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
        let order = state["slugs"][slug]["order"]
            .as_f64()
            .filter(|n| n.is_finite())
            .unwrap_or(0.0);
        (
            status,
            order,
            slug,
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
    }
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
            rollup: SectionRollup::default(),
        })
        .collect();
    board.sections.sort_by(|a, b| {
        let position = |section: &BoardSection| {
            if section.key == ARCHIVED {
                usize::MAX
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
        _ => wt_core::sanitize_terminal_text(key),
    }
}

/// Identical branch names on different machines do not form a shared stack.
pub(crate) fn branch_key(row: &wt_tui::BoardRow, branch: &str) -> String {
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

/// Recompute folded-section summaries from the same typed row facts shown in
/// each member. Called after source overlays have joined session and PR state.
pub fn refresh_rollups(board: &mut Board) {
    for section in &mut board.sections {
        let rows = section
            .rows
            .iter()
            .filter_map(|&index| board.rows.get(index));
        let rows = rows.collect::<Vec<_>>();
        let mut states = Vec::new();
        for state in WORK_STATES {
            let count = rows
                .iter()
                .filter(|row| {
                    row.work.as_ref().and_then(|work| work.effective_state) == Some(state)
                })
                .count();
            if count > 0 {
                states.push(WorkStateCount {
                    state: Some(state),
                    count,
                });
            }
        }
        let unset = rows
            .iter()
            .filter(|row| {
                row.work
                    .as_ref()
                    .and_then(|work| work.effective_state)
                    .is_none()
            })
            .count();
        if unset > 0 {
            states.push(WorkStateCount {
                state: None,
                count: unset,
            });
        }
        let risks = [WorkRisk::High, WorkRisk::Medium, WorkRisk::Low]
            .into_iter()
            .filter_map(|risk| {
                let count = rows
                    .iter()
                    .filter(|row| {
                        row.work.as_ref().is_some_and(|work| {
                            work.record.as_ref().and_then(|record| record.risk) == Some(risk)
                        })
                    })
                    .count();
                (count > 0).then_some(WorkRiskCount { risk, count })
            })
            .collect();
        let unknown_git = rows
            .iter()
            .filter(|row| row.git.tracked_changes.is_none() || row.git.untracked_files.is_none())
            .count();
        let dirty_worktrees = rows
            .iter()
            .filter(|row| {
                row.git.tracked_changes.unwrap_or_default() > 0
                    || row.git.untracked_files.unwrap_or_default() > 0
            })
            .count();
        let blocked_notes = rows
            .iter()
            .filter_map(|row| {
                row.work
                    .as_ref()
                    .and_then(|work| work.record.as_ref())
                    .and_then(|record| record.blocked_on.as_deref())
                    .map(|note| format!("{}: {}", row.slug, note))
            })
            .collect();
        section.rollup = SectionRollup {
            states,
            risks,
            blocked_notes,
            stale_statuses: rows
                .iter()
                .filter(|row| {
                    row.work
                        .as_ref()
                        .is_some_and(|work| work.stale == Some(true))
                })
                .count(),
            verification_owed: rows
                .iter()
                .filter(|row| row.work.as_ref().is_some_and(|work| work.verification_owed))
                .count(),
            verification_overdue: rows
                .iter()
                .filter(|row| {
                    row.work
                        .as_ref()
                        .is_some_and(|work| work.verification_overdue)
                })
                .count(),
            dirty_worktrees: Some(dirty_worktrees),
            unknown_git,
            upstream_ahead: rows
                .iter()
                .filter(|row| row.git.ahead.is_some_and(|count| count > 0))
                .count(),
            upstream_behind: rows
                .iter()
                .filter(|row| row.git.behind.is_some_and(|count| count > 0))
                .count(),
            rebasing: rows.iter().filter(|row| row.git.rebasing).count(),
            conflicted: rows
                .iter()
                .filter(|row| !row.git.conflict_files.is_empty())
                .count(),
            open_prs: rows
                .iter()
                .filter(|row| row.pr.as_ref().and_then(|pr| pr.state.as_deref()) == Some("OPEN"))
                .count(),
            draft_prs: rows
                .iter()
                .filter(|row| {
                    row.pr
                        .as_ref()
                        .is_some_and(|pr| pr.state.as_deref() == Some("OPEN") && pr.draft)
                })
                .count(),
            queued_prs: rows
                .iter()
                .filter(|row| row.pr.as_ref().is_some_and(|pr| pr.merge_queue.is_some()))
                .count(),
            failing_checks: rows
                .iter()
                .filter(|row| row.pr.as_ref().map(|pr| pr.checks) == Some(wt_tui::CheckState::Fail))
                .count(),
            paused_automations: rows.iter().filter(|row| row.automations_paused).count(),
            needs_attention: rows.iter().filter(|row| row.needs_attention).count(),
        };
    }
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
}
