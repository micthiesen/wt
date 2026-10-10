//! Pure preparation of section placement and stack rails from the current
//! inventory and durable layout. No separate stack membership is persisted.
use std::collections::BTreeMap;

use serde_json::Value;
use wt_config::UiSort;
use wt_core::{
    ChainMember, SpineMember, build_stack_index, parse_work_status, spine_layout, work_record_rank,
};
use wt_tui::{Board, BoardSection};

pub const INBOX: &str = "\0inbox";
pub const ARCHIVED: &str = "\0archived";

pub fn prepare(board: &mut Board, state: &Value, trunk: &str, sort: UiSort) {
    let members = board
        .rows
        .iter()
        .map(|row| {
            ChainMember::new(
                &row.slug,
                &row.branch,
                state["slugs"][&row.slug]["baseBranch"]
                    .as_str()
                    .map(str::to_owned),
            )
        })
        .collect::<Vec<_>>();
    let stacks = build_stack_index(&members, trunk);
    let mut buckets = BTreeMap::<String, Vec<usize>>::new();
    buckets.insert(INBOX.into(), Vec::new());
    for (index, row) in board.rows.iter().enumerate() {
        let stack = stacks
            .by_branch
            .get(&row.branch)
            .map(|entry| &stacks.layouts[entry.layout_index]);
        let anchor = stack
            .and_then(|stack| stack.nodes.first())
            .map(|root| root.slug.as_str())
            .unwrap_or(&row.slug);
        let section = state["slugs"][anchor]["section"]
            .as_str()
            .filter(|name| !name.is_empty());
        let key = if row.archived {
            ARCHIVED.to_owned()
        } else if let Some(section) = section {
            section.to_owned()
        } else if let Some(stack) = stack {
            format!("\0stack:{}", stack.stack_id)
        } else {
            INBOX.to_owned()
        };
        buckets.entry(key).or_default().push(index);
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
        let row = &board.rows[index];
        let entry = stacks.by_branch.get(&row.branch);
        let stack = entry.map(|entry| &stacks.layouts[entry.layout_index]);
        let slug = stack
            .and_then(|stack| stack.nodes.first())
            .map(|root| root.slug.as_str())
            .unwrap_or(&row.slug);
        let status = if sort == UiSort::Status {
            stack
                .map(|stack| {
                    stack
                        .nodes
                        .iter()
                        .map(|node| rank(&node.slug))
                        .min()
                        .unwrap_or(99)
                })
                .unwrap_or_else(|| rank(slug))
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
            entry.map_or(0, |entry| entry.node.index),
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
        .map(|(key, rows)| {
            let title = match key.as_str() {
                INBOX => "Inbox".to_owned(),
                ARCHIVED => "Archived".to_owned(),
                _ => key
                    .strip_prefix("\0stack:")
                    .map(|branch| format!("Stack: {branch}"))
                    .unwrap_or_else(|| key.clone()),
            };
            BoardSection {
                folded: folded.iter().any(|value| value.as_str() == Some(&key)),
                title: wt_core::sanitize_terminal_text(&title),
                key,
                rows,
            }
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
        let members = section
            .rows
            .iter()
            .map(|&index| {
                let row = &board.rows[index];
                SpineMember {
                    key: row.key.clone(),
                    branch: row.branch.clone(),
                    parent_branch: stacks
                        .by_branch
                        .get(&row.branch)
                        .and_then(|entry| entry.node.parent_branch.clone()),
                }
            })
            .collect::<Vec<_>>();
        let spine = spine_layout(&members);
        for &index in &section.rows {
            let row = &mut board.rows[index];
            row.stack_prefix = spine
                .get(&row.key)
                .map(|cell| {
                    let mut prefix = String::new();
                    for column in 0..cell.col.min(16) {
                        prefix.push(if cell.trail.get(column).copied().unwrap_or(false) {
                            '│'
                        } else {
                            ' '
                        });
                        prefix.push(' ');
                    }
                    prefix.push(cell.glyph);
                    prefix.push(' ');
                    prefix
                })
                .unwrap_or_default();
        }
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
    fn stacks_stay_contiguous_in_the_roots_manual_section_and_sort_by_stable_identity() {
        let mut board = Board {
            rows: vec![row("child"), row("root"), row("other")],
            ..Board::default()
        };
        let state = json!({"slugs": {
            "root": {"section": "Batch", "baseBranch": "main"},
            "child": {"section": "Ignored child placement", "baseBranch": "root"},
            "other": {"section": "Batch"}
        }, "sectionsOrder": [INBOX, "Batch"], "foldedSections": ["Batch"]});
        prepare(&mut board, &state, "main", UiSort::Manual);
        let batch = board
            .sections
            .iter()
            .find(|section| section.key == "Batch")
            .unwrap();
        assert!(batch.folded);
        assert_eq!(batch.rows, [2, 1, 0]);
        assert!(!board.rows[0].stack_prefix.is_empty());
        assert_eq!(section_for(&board, "child").as_deref(), Some("Batch"));
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
